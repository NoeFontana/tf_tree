# 0005: The shared-memory seam

**Status:** implemented
**Owner:** @NoeFontana

**Implementation.** Twelve steps, twelve-plus PRs, in order:

## Decision

### 1. `tf_tree` gains an optional dependency on `tf_tree_ipc`, gated by `shm`

`shm = ["tf_tree_arena/shm", "dep:tf_tree_ipc"]`. `tf_tree_ipc` owns the wire and lock file and never learns what the fd is: it is parameterised by a `Copy` `SegmentDescriptor { format_version, layout_hash, arena_size, instance_uuid, boot_id }` plus a `BorrowedFd`. `tf_tree_arena` gains `instance_uuid` (header offset 136), `descriptor()` and §7.1 population. `tf_tree` owns composition (`open()`, owner thread, leases, reaping, fork poisoning).

### 2. `tf_tree::open()`

`open()`, `open_named(name)` and `Open` (`mode` DEFAULT `ReadOnly` (D18); `create` DEFAULT `IfAbsent`; `timeout` DEFAULT `tf_tree_ipc::DEFAULT_OPEN_TIMEOUT`; `layout_if_creating(TreeBuilder)`, per `0004`). `OpenError` is `Copy`, `String`-free, `#[non_exhaustive]`.

### 3. §3.7 — the owner serves from a thread in the owning process

Not a daemon: ownership is a role a survivor inherits (§3.5). `OwnerServer` `epoll`s over its listener, a shutdown `eventfd` and client fds; **`EPOLLHUP` on a client fd is the reap trigger** (D17). Bind as `<name>.sock.<pid>`, `chmod 0600`, `rename` into place; reject any datagram whose length is not `size_of::<Hello*>()` before parsing; statuses pinned by `wire_status_codes_are_pinned`. Client: `ENOENT` / `ECONNREFUSED` / `EAGAIN` or a peer that HUPs mid-handshake is `Absent` (the ownership byte is the real discriminator); a **rejection** is **terminal, not retried**.

### 4. `ServerProbe` widens; it is not removed

`fn probe(&mut self, sock: &Path) -> Result<Reach<Self::Attached>, IpcError>` with `enum Reach<T> { Serving(T), Absent, Rejected(HelloStatus) }`. `NoServer` keeps `Attached = ()`.

### 5. The claim protocol: the arena CAS is the decision, the OFD lock is the lease

> **Acquire:** `edge::claim(rec, slot)` CAS → `F_OFD_SETLK(CLAIM_BASE + edge_id)` → **re-read `rec.epoch`**; if it changed, a reaper ran inside the window: `edge::release`, unlock, retry. A contended SETLK backs the CAS out and returns `ClaimLeaseContended`.
> **Release:** clear the record (`edge::release`, a CAS) → **then** unlock.
> **Invariant:** `record held ∧ lock free` ⟺ the holder is dead or inside the acquire window.

### 6. Reaping, with a self-skip

Any read-write participant reaps (D15/D17): the owner on `EPOLLHUP`, lazily a claimer that gets `EdgeAlreadyClaimed`, and `Tree::reap_dead()`. Per edge: skip if `owner == 0`, if `slot_of(owner)` is the reaper's own slot, or if `probe_claim(edge)` is held; else `edge::reap` (epoch++ then owner = 0) and A5 parity repair. **One syscall per *dead* edge — NORMATIVE:** the `EPOLLHUP` trigger passes `only_slot = Some(slot)`; only `reap_dead()` passes `None`.

### 7. Fork poisoning

`FORK_GEN` is bumped by `pthread_atfork(after_in_child)` (the single `unsafe`, in `tf_tree_ipc::fork::generation()`); `Tree` stores `fork_gen_at_open`. A detached `Tree::view()` returns a process-wide **poison arena** and `Guard::poisoned(view, err)` answers `ChildDetached`. Each destructor stands itself down by `fork_gen` compare, except `MappedArena::drop` (`getpid()`).

### 8. D16 is amended, not silently contradicted

D16 rejects *negotiated* ownership; the §3.5 heir is whoever wins an uncontended `F_OFD_SETLK`.

## Consequences

1. No epoch re-check ⇒ `the_acquire_window_backs_out`; inverted acquire order ⇒ two writers.
2. No reaper self-skip ⇒ a process revokes its own claims (`a_reaper_does_not_reap_itself`, `a_reaper_does_not_reap_its_own_live_claim`); none in liveness ⇒ `a_tree_never_reports_itself_dead`.
3. **`CreatePolicy::Always`** creates a second arena against the same lock file, so claim and participant bytes alias; it needs an instance-scoped lock path.
4. **Fork** ⇒ `MADV_DONTFORK` leaves the child unmapped, so the participant release faults unless guarded.
5. `crates/tf_tree_bench/src/bin/fork_child.rs` carries the real `fork()` (`scripts/unsafe-budget.txt`). Miri covers none of this.

## Implementation plan

1-4. `instance_uuid`; `register_at`; §3.7 wire in `tf_tree_ipc`; wire into `Open`.
5. `tf_tree::open()`, tested by `crates/tf_tree/tests/rendezvous.rs` (PHASE2 §11.2 scenarios 4, 6, 7, 9, 10, 11).
6-8. §5.1 liveness; §6.1 claims as leases, the window tested by injection (`test-hooks`, `CLAIM_WINDOW_HOOK`); §6.3 reaping.
9. Fork poisoning: `fork_child` asserts `WIFEXITED`, `ChildDetached` and exit 0.
10. §7.1 per-region population: `MADV_POPULATE_WRITE`/`_READ`, falling back on `EINVAL` to a read touch. Tests: `declared_headroom_is_not_charged`, `declared_content_is_charged`, `the_first_lookup_after_attach_does_not_fault`.
11. CLI adoption: `--attach`/`--domain`/`--name`/`--rw`/`--create`/`--timeout`; `--rw` opt-in, `--create` defaults to never (D18) (`crates/tf_tree_cli/tests/attach.rs`).
12. Docs close-out.
