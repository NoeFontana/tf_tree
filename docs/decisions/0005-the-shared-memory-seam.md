# 0005: The shared-memory seam

**Status:** implemented
**Owner:** @NoeFontana

**Implementation.** Twelve steps, twelve-plus PRs, in order:

| step | what | PR |
|---|---|---|
| — | this decision record | #27 |
| 1 | `instance_uuid` + `SegmentDescriptor` | #28 |
| 2 | `ParticipantTable::register_at` | #29 |
| 3 | §3.7 wire, `SOCK_SEQPACKET` + `SCM_RIGHTS` | #30, #31 |
| 4 | wire §3.7 into `Open` | #32 |
| 5 | `tf_tree::open()` — the seam | #33 |
| 6 | §5.1 liveness from `F_OFD_GETLK` | #39, #43 |
| 7 | §6.1 claims as leases | #52, #53 |
| 8 | §6.3 reaping | #54 |
| 9 | fork poisoning | #55 |
| 7b | `the_acquire_window_backs_out`, owed from step 7 | #56 |
| 10 | §7.1 per-region population | #57 |
| 11 | CLI adoption | #58 |
| 12 | docs close-out | this one |

Milestone B (layout kernels, #34) and Phase 3 Python work ran alongside and are not part of this decision.

## Decision

### 1. `tf_tree` gains an optional dependency on `tf_tree_ipc`, gated by `shm`

`shm = ["tf_tree_arena/shm", "dep:tf_tree_ipc"]`. Nothing depended on `tf_tree_ipc` and §3.7 was unimplemented (`crates/tf_tree_ipc/src/lib.rs:54`). `tf_tree_ipc` owns the wire and lock file and never learns what the fd is: it is parameterised by a `Copy` `SegmentDescriptor { format_version, layout_hash, arena_size, instance_uuid, boot_id }` plus a `BorrowedFd`. `tf_tree_arena` gains `instance_uuid` (header offset 136, existing padding), `descriptor()` and §7.1 population. `tf_tree` owns composition (`open()`, owner thread, leases, reaping, fork poisoning) and, seeing both `MAX_PARTICIPANTS` and `DEFAULT_MAX_PARTICIPANTS`, asserts their equality in a `const _`. `SCM_RIGHTS` needs no `unsafe`. The two `unsafe` sites (`pthread_atfork`, the test helper's `fork()`) live outside `tf_tree`; `0017` later moved `tf_tree` to `deny` with one `#[allow]`.

### 2. `tf_tree::open()`

`open()`, `open_named(name)` and `Open` (`mode` DEFAULT `ReadOnly` (D18); `create` DEFAULT `IfAbsent`; `timeout` DEFAULT `tf_tree_ipc::DEFAULT_OPEN_TIMEOUT`, re-exported not restated; `layout_if_creating(TreeBuilder)`, per `0004`).

`OpenError` is `Copy`, `String`-free, `#[non_exhaustive]`, with `From<IpcError>`, `From<ShmError>`, `From<BuildError>` and `Rejected(HelloStatus)`.

### 3. §3.7 — the owner serves from a thread in the owning process

Not a daemon: ownership is a role a survivor inherits (§3.5). `OwnerServer` `epoll`s over its listener, a shutdown `eventfd` and client fds; **`EPOLLHUP` on a client fd is the reap trigger** (D17). Required: bind as `<name>.sock.<pid>`, `chmod 0600`, `rename` into place; `SO_RCVTIMEO`/`SO_SNDTIMEO` both sides; reject any datagram whose length is not `size_of::<Hello*>()` before parsing, checking `magic` then `format_version` first; `RecvFlags::CMSG_CLOEXEC`; rejection statuses pinned by `wire_status_codes_are_pinned`. Client reachability:

| Observation | Verdict |
|---|---|
| `connect` → `ENOENT` / `ECONNREFUSED` | `Absent`; the ownership byte is the real discriminator. |
| `connect` succeeds, peer HUPs or times out mid-handshake | `Absent`; the §3.4 loop proceeds. |
| `connect` → `EAGAIN` (backlog full) | `Absent`; the existing back-off covers it. |
| A **rejection** (`VersionMismatch`, `LayoutMismatch`, …) | **Terminal, not retried.** |

### 4. `ServerProbe` widens; it is not removed

`trait ServerProbe { type Attached; fn probe(&mut self, sock: &Path) -> Result<Reach<Self::Attached>, IpcError>; }` with `enum Reach<T> { Serving(T), Absent, Rejected(HelloStatus) }`, so the fd arrives on the same connection. `NoServer` keeps `Attached = ()`; the eight pre-existing `open.rs` tests pass unchanged. `Open` gains `register_at(slot)` (joiner) beside `register_any()` (creator).

### 5. The claim protocol: the arena CAS is the decision, the OFD lock is the lease

PHASE2 §6.1's "the lock file is authoritative" is not implementable: two files have no atomic cross-update.

> **Acquire:** `edge::claim(rec, slot)` CAS → `F_OFD_SETLK(CLAIM_BASE + edge_id)` → **re-read `rec.epoch`**; if it changed, a reaper ran inside the window: `edge::release`, unlock, retry. A contended SETLK backs the CAS out and returns `ClaimLeaseContended`.
> **Release:** clear the record (`edge::release`, a CAS) → **then** unlock.
> **Invariant:** `record held ∧ lock free` ⟺ the holder is dead or inside the acquire window.

### 6. Reaping, with a self-skip

Any read-write participant reaps (D15/D17): the owner on `EPOLLHUP`, lazily a claimer that gets `EdgeAlreadyClaimed`, and `Tree::reap_dead()`. Precondition: `self.participant != u32::MAX`, asserted. For each edge: skip if `owner == 0`; **skip if `slot_of(owner)` is the reaper's own slot** (compare `slot_of`, never the packed word against `slot + 1`); skip if `probe_claim(edge)` is held; else `edge::reap` (epoch++ then owner = 0) and A5 parity repair. Then `force_free` each recorded participant slot other than its own whose byte is not held.

**One syscall per *dead* edge, not per edge — NORMATIVE.** The owner-`EPOLLHUP` trigger passes `only_slot = Some(slot)`; the lazy trigger probes one edge; only `reap_dead()` passes `None`. A `SIGSTOP`ped process still holds its byte; the epoch re-check closes the acquire window.

### 7. Fork poisoning

`FORK_GEN` is bumped by `pthread_atfork(after_in_child)`; `Tree` stores `fork_gen_at_open`; the check is one Relaxed load. The single `unsafe { pthread_atfork(..) }` lives in `tf_tree_ipc` behind `fork::generation()`. `Tree::view()` returns a process-wide **poison arena** (allocated at first shared open) when detached, and `Guard::poisoned(view, err)` answers `ChildDetached`. Each destructor stands itself down independently: `Publisher::abandon` (called by `EdgeWriter::drop`); `ClaimLease`, `OwnerThread::stop` and `OwnerServer::drop` by `fork_gen` compare; `MappedArena::drop` by `getpid()` compare (a destructor can afford the syscall).

### 8. D16 is amended, not silently contradicted

D16's "no takeover" does not survive §3.5 inheritance; D16 rejects *negotiated* ownership, and the heir is whoever wins an uncontended `F_OFD_SETLK`, decided by the kernel. D16 carries an amendment note.

## Consequences

Each rule below has a named test:

1. No epoch re-check ⇒ `the_acquire_window_backs_out`; inverted acquire order ⇒ two writers.
2. No reaper self-skip ⇒ `F_OFD_GETLK` reports only conflicting locks, so a process revokes its own claims (`a_reaper_does_not_reap_itself`, `a_reaper_does_not_reap_its_own_live_claim`).
3. No self-skip in liveness ⇒ `a_tree_never_reports_itself_dead`.
4. **`CreatePolicy::Always`** creates a second arena against the same lock file, so claim and participant bytes alias; it needs an instance-scoped lock path.
5. **Fork** ⇒ `MADV_DONTFORK` leaves the child unmapped, so the participant release faults unless guarded.

### What we commit to

- One dependency edge, `tf_tree → tf_tree_ipc`, one-directional and `shm`-gated; rustix `net`+`event`+`rand`, no new crates.
- The fork test helper needs a real `fork()` without `exec`, so `crates/tf_tree_bench/src/bin/fork_child.rs` carries `unsafe { libc::fork() }` with a `// SAFETY:` block. **"So this is a separate bin target"** stands: `forbid` on `src/lib.rs` governs no bin, test, bench or example. Per `0048` the site count is stale (`scripts/unsafe-budget.sh`) and these are kinds 2 and 5 in `scripts/unsafe-budget.txt`.
- Miri covers none of this (`memfd_create`, `F_ADD_SEALS`, `fcntl(F_OFD_*)`); `just miri` says so.

## Implementation plan

Steps 1-4: `instance_uuid` at header offset 136 (`FORMAT_VERSION` stays 2; `getrandom` retried on `EINTR`/short reads); `ParticipantTable::register_at` (arena slot and lock byte are one integer); §3.7 wire in `tf_tree_ipc` (an omitted `ScmRights` push must fail with `NoFdReceived`); wire into `Open` with `IpcError::SocketPathTooLong` at `Rendezvous` construction.
5. `tf_tree::open()`, tested by `crates/tf_tree/tests/rendezvous.rs` (PHASE2 §11.2 scenarios 4, 6, 7, 9, 10, 11).
6-8. §5.1 liveness from `F_OFD_GETLK`; §6.1 claims as leases, the acquire window tested by injection (`test-hooks`, `CLAIM_WINDOW_HOOK`, a second participant as reaper); §6.3 reaping, where the owner's slot assigner also consults `probe_participant` because a read-only joiner holds a byte with no arena record.
9. Fork poisoning: `fork_child` asserts `WIFEXITED`, `ChildDetached` and exit 0; the parent re-validates itself, including `probe_claim` from an independent description (OFD locks are self-blind).
10. §7.1 per-region population: no `MAP_POPULATE`; `MADV_POPULATE_WRITE`/`_READ`, falling back on `EINVAL` to a read touch (never a write into a live segment); runs after `build_with`. Tests: `declared_headroom_is_not_charged`, `declared_content_is_charged`, `the_first_lookup_after_attach_does_not_fault`.
11. CLI adoption: `--attach`/`--domain`/`--name`/`--rw`/`--create`/`--timeout`; `--rw` opt-in, `--create` defaults to never (D18); `doctor` on a live arena drops multi-writer and short-buffer and says so (`crates/tf_tree_cli/tests/attach.rs`).
12. Docs close-out.
