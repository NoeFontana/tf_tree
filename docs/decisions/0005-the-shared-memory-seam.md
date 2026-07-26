# 0005: The shared-memory seam

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** (filled in as work lands)

## Context

`docs/PHASE2.md` §3 specifies zero-config rendezvous: a process calls `tf_tree::open()`
with no arguments and either joins the running arena or creates it. That is also
Phase 3's headline deliverable (`docs/PHASE3.md` §4.1, DoD item 1).

**It cannot happen today, and the reason is a missing edge in the dependency graph
rather than a missing algorithm.**

- `crates/tf_tree_ipc/` implements the entire rendezvous — `RuntimeDir::resolve`
  (`runtime_dir.rs:111`), `Rendezvous` (`rendezvous.rs:106`), `LockFile` with OFD
  locks (`lockfile.rs`), `Identity` records (`identity.rs`), and the §3.4 decision
  machine `Open::from_env().mode(..).create(..).open(..)` (`open.rs:240`), with
  multi-process tests.
- `crates/tf_tree_arena/` implements the mapping — `MappedArena::create`
  (`mapped.rs:160`) and `MappedArena::attach(OwnedFd, AttachMode)` (`mapped.rs:235`).
- **Nothing depends on `tf_tree_ipc`.** `grep -rn tf_tree_ipc --include=Cargo.toml`
  returns only the workspace-member line. The two halves have never touched.
- §3.7 — `SOCK_SEQPACKET` + `SCM_RIGHTS` fd passing — is unimplemented.
  `crates/tf_tree_ipc/src/lib.rs:54` says so; `Rendezvous::sock_path()`
  (`rendezvous.rs:172`) is computed and never bound; the only `ServerProbe` impl is
  `NoServer` (`open.rs:140`), which always answers `Reach::Absent`.

Consequently **no second process can obtain the arena fd**. `Tree::attach_shared`
takes an `OwnedFd` and the only transport in the workspace is fd inheritance by a
child (`crates/tf_tree_bench/src/shm_util.rs`). `CreatePolicy::Never` can never reach
`Joined`.

Amendments A1–A8 are all applied (`FORMAT_VERSION = 2`). This decision does not
revisit them; it is about the seam and the four protocols that cross it.

A **new crate boundary** is exactly what `CLAUDE.md` and this folder's README say must
start as a decision rather than a PR, which is why this document exists.

### Two facts established by measurement, not assumption

1. **`SCM_RIGHTS` requires no `unsafe`.** rustix 1.1.4 — already the pinned version —
   exposes a safe ancillary-data API: `sendmsg` (`net/send_recv/msg.rs:712`),
   `SendAncillaryBuffer::push(SendAncillaryMessage::ScmRights(&[BorrowedFd]))`
   (`:299`), `RecvAncillaryMessage::ScmRights(AncillaryIter<OwnedFd>)` (`:181`). Only
   `RecvAncillaryBuffer::parse` (`:529`) is `unsafe`, and it is not on the required
   path. Enabling rustix's `net`, `event` and `rand` features pulls **no new crates** —
   they only turn on features of `linux-raw-sys`, already in the graph.

   This matters because the obvious argument against putting the seam in `tf_tree` was
   that `tf_tree` is `#![forbid(unsafe_code)]`. That argument is void.

2. **`ArenaHeader` has 56 bytes of implicit padding.** `boot_id` at offset 112 (+16 =
   128), `_reserved: [u8; 8]` at 128..136, `topo_lock` at 192, `size_of == 256`
   (`header.rs:205-209`). An `instance_uuid: [u8; 16]` at 136 leaves **every pinned
   offset and `layout_hash()` unchanged** — the hash covers region sizes, alignments
   and strides, not header fields.

## Decision

### 1. `tf_tree` gains an optional dependency on `tf_tree_ipc`, gated by the existing `shm` feature

```toml
# crates/tf_tree/Cargo.toml
[features]
shm = ["tf_tree_arena/shm", "dep:tf_tree_ipc"]
```

Responsibility split:

- **`tf_tree_ipc`** owns the wire and the lock file, and **never learns what the fd
  is**. §3.7 lands here as `wire.rs` / `server.rs` / `client.rs`, parameterised by a
  plain `Copy` descriptor plus a `BorrowedFd`:

  ```rust
  pub struct SegmentDescriptor {
      pub format_version: u32,
      pub layout_hash: u32,
      pub arena_size: u64,
      pub instance_uuid: [u8; 16],
      pub boot_id: [u8; 16],
  }
  ```

  This keeps §2's "no arena dependency" rule literally true, and lets the wire be
  tested end to end inside `tf_tree_ipc` with a three-line `memfd_create` payload.

- **`tf_tree_arena`** keeps `MappedArena`; gains `instance_uuid`, `descriptor()`, and
  §7.1 per-region population.

- **`tf_tree`** owns composition: `open()`, the owner thread, the takeover watcher,
  claims-as-leases, reaping, and fork poisoning. It is the only crate that sees both
  `tf_tree_ipc::MAX_PARTICIPANTS` (`lockfile.rs:48`) and
  `tf_tree_arena::DEFAULT_MAX_PARTICIPANTS` (`layout.rs:92`), so it is the only place
  their required equality can become `const _: () = assert!(..)`.

`tf_tree` stays `#![forbid(unsafe_code)]`.

### 2. `tf_tree::open()`

```rust
pub fn open() -> Result<Tree, OpenError>;
pub fn open_named(name: &str) -> Result<Tree, OpenError>;

pub struct Open { /* .. */ }
impl Open {
    pub fn new() -> Open;                       // domain and name from the environment
    pub fn domain(self, domain: u32) -> Open;
    pub fn name(self, name: &str) -> Open;
    pub fn mode(self, mode: AttachMode) -> Open;         // DEFAULT: ReadOnly  (D18)
    pub fn create(self, policy: CreatePolicy) -> Open;   // DEFAULT: IfAbsent
    pub fn timeout(self, d: Duration) -> Open;           // DEFAULT: tf_tree_ipc::DEFAULT_OPEN_TIMEOUT
    pub fn layout_if_creating(self, builder: TreeBuilder) -> Open;
    pub fn open(self) -> Result<Tree, OpenError>;
}
```

`layout_if_creating` takes a **`TreeBuilder`**, not an `ArenaLayout`: decision `0004`
is still authoritative, the arena is sized from declared edges, and the creator must
also *write* the topology. `Created`/`TookOver` ⇒ `builder.build_shared(name)`
(`tree.rs:315`); `Joined` ⇒ `Tree::attach_shared(fd, mode)` (`tree.rs:791`). Both are
reused as they stand.

`OpenError` is `Copy`, `String`-free, `#[non_exhaustive]`, with `From<IpcError>`,
`From<ShmError>`, `From<BuildError>` and `Rejected(HelloStatus)`. This closes the
current state of three unrelated error families reaching the surface with no bridges.

The default timeout **re-exports `tf_tree_ipc::DEFAULT_OPEN_TIMEOUT`** (`open.rs:159`,
currently 5 s) rather than restating the number. Two constants that must agree and are
written down twice will eventually disagree, and this one governs how long a failing
`open()` blocks — the drift would present as a hang, not as a mismatch.

The `AttachMode` ↔ `AccessMode` conversion cannot be a `From` impl anywhere — both
types are foreign to `tf_tree` and neither dependency may depend on the other. It is a
private exhaustive `match` plus a round-trip test, which is what breaks if either enum
gains a third variant.

### 3. §3.7 — the owner serves from a thread in the owning process

Not a daemon. §3.5 makes ownership a *role* that a surviving participant inherits,
which is only possible if any participant can bind. `tf_treed` (§9) then becomes "a
process that does only this", not a prerequisite for anything.

`OwnerServer` owns the listener fd, a `dup` of the segment fd, the `SegmentDescriptor`,
an `eventfd` for shutdown, and a slot-assignment callback. It `epoll`s over
{listener, eventfd, accepted client fds}. **`EPOLLHUP` on a client fd is the reap
trigger** (D17: "the attach socket is the liveness signal").

Message structs are exactly PHASE2 §3.7's `HelloRequest`/`HelloResponse`, `#[repr(C)]`
little-endian, with explicit `to_bytes`/`from_bytes` reusing the pattern
`identity.rs` already established — which keeps `bytemuck` out of `tf_tree_ipc` and
makes endianness explicit rather than incidental.

Additions §3.7 does not specify but which are required (see *Consequences*):

- Bind as `<name>.sock.<pid>`, `chmod 0600`, then `rename` into place. §3.4 step 5's
  bare `sock.tmp` collides between two takers, or with a stale file from a dead binder.
- `SO_RCVTIMEO` / `SO_SNDTIMEO` on both sides.
- Reject any datagram whose length ≠ `size_of::<Hello*>()` **before** parsing; check
  `magic`, then `format_version`, before any other field.
- `RecvFlags::CMSG_CLOEXEC`, so a received fd is never leaked into a concurrent `exec`.
- Rejection statuses get explicit `u32` values, pinned by a table test.

Client-side reachability collapses to three cases, deliberately:

| Observation | Verdict |
|---|---|
| `connect` → `ENOENT` / `ECONNREFUSED` | `Absent`. §3.9 makes a stale path expected; the ownership byte is the real discriminator. |
| `connect` succeeds, peer HUPs or times out mid-handshake | `Absent`. The ownership byte will be free and the §3.4 loop proceeds. |
| `connect` → `EAGAIN` (backlog full) | `Absent`. The existing back-off (`open.rs:300-304`) and the contention branch already cover it; "server busy" is not a distinct state. |
| A **rejection** (`VersionMismatch`, `LayoutMismatch`, …) | **Terminal, not retried.** |

### 4. `ServerProbe` widens; it is not removed

The existing trait (`open.rs:116`) is the right injection point and the module doc
(`open.rs:57-61`) is right that keeping it is what makes the split-brain race
reproducible in a test. But `probe() -> Reach` is too narrow: the real client must
receive the fd on the *same* connection, or it connects twice and re-races.

```rust
pub trait ServerProbe {
    type Attached;
    fn probe(&mut self, sock: &Path) -> Result<Reach<Self::Attached>, IpcError>;
}
pub enum Reach<T> { Serving(T), Absent, Rejected(HelloStatus) }
```

`NoServer` keeps `type Attached = ()`. `Session` gains `take_attached()`.
**All eight existing tests in `open.rs:456-604` must pass unchanged; that is the
regression gate for the change.**

`Open` also gains `register_at(slot)` (a joiner takes the byte the owner named)
alongside `register_any()` (Created/TookOver, where there is no owner to ask). This
retires the deviation documented at `open.rs:308-323`.

### 5. The claim protocol: the arena CAS is the decision, the OFD lock is the lease

PHASE2 §6.1's literal wording — "the lock file is authoritative … any code that makes
a decision from `ClaimRecord` alone is a bug" — **is not implementable.** The lock file
and the arena are two files with no atomic cross-update, so exactly one of them has to
be the linearization point; and A4's epoch check reads the record on every `push` by
design.

> **Acquire, in this order:** `edge::claim(rec, slot)` CAS → on success
> `F_OFD_SETLK(CLAIM_BASE + edge_id)` → **re-read `rec.epoch`**; if it changed, a
> reaper ran inside the window, so `edge::release`, unlock, retry. If the SETLK is
> contended, back the CAS out and return `ClaimLeaseContended`.
>
> **Release, in this order:** clear the record (`edge::release`, already a CAS and not
> a store, `edge.rs:302`) → **then** unlock.
>
> **Invariant bought:** `record held ∧ lock free` ⟺ the holder is dead, or is inside
> the one-syscall acquire window. That is exactly the predicate §6.3's reaper wants,
> and it is the only inconsistent state the protocol can produce. The inverse
> (`record free ∧ lock held`) occurs during release and is ignored by every reader.

This is a **second layer, not a replacement**. A3's slot indirection and A4's epoch
were just implemented on the CAS, are loom-covered, and are the only mechanism that
works for a `HeapArena`.

### 6. Reaping, with a self-skip

Any read-write participant reaps (D15/D17: reaping is cooperative, not owner-only, so
an owner's death does not leak every claim). Triggers: the owner on `EPOLLHUP`; lazily
by any claimer that gets `EdgeAlreadyClaimed`; and `Tree::reap_dead()` for
`tf_tree doctor --repair` and for tests.

```text
# PRECONDITION: self.participant != u32::MAX. A read-only tree never registers
# (tree.rs:801) and cannot reap; assert it rather than assume it, because
# `u32::MAX + 1` overflows — see Consequences.
own_word = self.participant as u64 + 1

for edge in 0..edge_count:
    owner = claim[edge].owner.load(Acquire)
    if owner == 0                        { continue }   # cheap filter, no syscall
    if owner == own_word                 { continue }   # ADDED — see Consequences
    if let Some(dead) = only_slot        {              # see "one syscall per
        if owner != dead + 1 { continue }               #  *dead* edge", below
    }
    if lock.probe_claim(edge)?.held      { continue }   # alive: never reapable
    edge::reap(&claim[edge])                            # epoch++ then owner = 0
    normalize_slot_parity(edge, head & mask)            # A5 repair, §6.3
for slot in 0..MAX_PARTICIPANTS:
    if slot == self.participant          { continue }   # ADDED
    if participants.identity(slot).is_some() && !lock.probe_participant(slot)?.held:
        participants.force_free(slot)                   # incarnation-guarded
```

**One syscall per *dead* edge, not per edge — NORMATIVE.** `probe_claim` is an
`fcntl`, so a naive sweep costs one syscall per claimed edge and an arena with
thousands of edges would make reaping the most expensive operation in the system.
Two things keep it cheap, and both must be implemented:

- The `owner == 0` test is a relaxed load of a word already in the claim table.
  Unclaimed edges cost no syscall, which is the common case.
- **The owner-`EPOLLHUP` trigger knows *which* participant slot died**, so it
  passes `only_slot = Some(slot)` and the loop degenerates to `O(edges)` atomic
  loads plus one syscall per edge that slot actually held. The lazy trigger
  probes exactly one edge. Only `Tree::reap_dead()` — `doctor --repair` and
  tests — passes `None` and pays the full sweep, which is the one caller where
  a whole-arena scan is the point.

With OFD locks the kernel answers liveness authoritatively, so the `/proc`
"unknown ⇒ alive" fail-safe is no longer the defence. The defence is that a
`SIGSTOP`ped or GC-stalled process **still holds its byte**. The only remaining
false-positive source is the acquire window, and it is closed from the claimer's side
by the epoch re-check — strictly better than a grace period, because there is no
timing constant to tune.

`/proc` parsing does not go away: it remains the identity source for `doctor` and the
liveness predicate for `HeapArena` trees and for a `Tree` attached over an inherited fd
with no `Session`, exactly as §5.1 says.

### 7. Fork poisoning

```rust
static FORK_GEN: AtomicU64;                  // bumped by pthread_atfork(after_in_child)
struct Tree { fork_gen_at_open: u64, /* .. */ }
fn alive(&self) -> Result<(), OpenError>     // one Relaxed load
```

Checked at `Tree::guard()`, every mutating entry point (`claim`, `reparent`, `intern`),
the writer facade's `push`, and — decisively — **`Drop`**, which must skip both
`participants().release()` and joining the owner/watch threads.

The single `unsafe { libc::pthread_atfork(..) }` lives in `tf_tree_ipc`, which already
budgets `unsafe` in `ofd.rs`, exposed as a safe `tf_tree_ipc::fork::generation()`.

### 8. D16 is amended, not silently contradicted

`docs/PROJECT.md:117` D16 reads "Ownership is configured, not negotiated. … No leader
election, no consensus, **no takeover**." PHASE2 §3.5 says ownership "is *inherited*,
and the kernel picks the heir"; `OpenOutcome::TookOver` (`open.rs:96`) already ships;
and D17 already presupposes that an owner can die without leaking claims.

D16's *reasoning* survives and its *last clause* does not. What D16 correctly rejects
is **negotiated** ownership — election, consensus, a quorum protocol. What §3.5 does is
not negotiation: the heir is whichever process wins an uncontended `F_OFD_SETLK` on a
single byte, decided by the kernel, with no message exchanged between candidates.

D16 gains an amendment note saying exactly that. D1–D20 are hard constraints per
`CLAUDE.md`, so leaving the contradiction in place would eventually get takeover
"restored" to spec by a reader doing the right thing with the wrong document.

## Rationale

**Why the seam is in `tf_tree` and not a new crate.** `tf_tree::open()` must return a
`Tree`, and `Tree`'s constructor surface is private — the fields at `tree.rs:521-560`
and the `ArenaBacking` enum are both private. Any crate that is not `tf_tree` pays for
the seam by forcing `Tree` to grow a public
`from_parts(ArenaBacking, u32, u64, Box<dyn Fn..>)`, which widens the public API to a
shape whose only consumer is the seam. Secondary: the CLI already depends on `tf_tree`,
and Phase 3's PyO3 crate then binds exactly one crate rather than two.

Rejected: **a `tf_tree_session` crate** — loses on the private-constructor argument
above, and adds a fourth node to a graph whose problem is a missing edge.
Rejected: **the server inside `tf_tree_ipc`** — it would need `tf_tree_arena`, which
§2 forbids.

**Why the wire is parameterised by a descriptor rather than given the arena.** It keeps
`tf_tree_ipc` free of `tf_tree_arena` (§2), and it makes the §3.7 protocol testable
without an arena at all — a `memfd_create` of any size exercises every path.

**Why a thread and not a daemon.** §3.5's inheritance requires that any participant can
become the server. A daemon-only design makes `tf_treed` a hard prerequisite and makes
owner death fatal rather than recoverable.

**Why the CAS stays the decision.** Inverting it — making the lock file authoritative —
would require rewriting A3 and A4 within weeks of landing them, would leave `HeapArena`
with no claim mechanism at all, and cannot be made atomic across the two files anyway.

**Why `getpid()` was rejected for fork detection.** It is a real syscall on Linux, not
vDSO: roughly 50–100 ns against a 150 ns p50 lookup budget (`PHASE1.md` §11). It also
cannot detect a fork that happened while no call was in flight. The atfork counter is a
relaxed load of a few nanoseconds and is correct in both respects.

## Consequences

### Failure modes this protocol is chosen to exclude

Each is a real state reachable if the corresponding rule is dropped, and each becomes a
named test:

1. **Inverted acquire order** ⇒ the record says P2 while the lock says P1. P1 holds the
   lease and cannot write; P2 holds the record and can. Two writers by the back door —
   precisely what D7 and A4 exist to prevent.
2. **No epoch re-check** ⇒ a concurrent reaper clears the record inside the acquire
   window; the claimer then holds a lease on an edge the arena reports free, and a
   third process claims it. `edge::reap` already bumps the epoch *before* clearing the
   owner (`edge.rs:334-337`), which is what makes recovery possible at all.
3. **No self-skip in the reaper** ⇒ `F_OFD_GETLK` reports only *conflicting* locks, so
   a process's own byte always reads free (proven by `lockfile.rs:379`). A literal §6.3
   loop therefore revokes the reaper's own live `Publisher`s, and A4 then correctly
   reports `ClaimRevoked` on the next push: a self-inflicted outage that presents as a
   spurious reap.
4. **No self-skip in the liveness predicate** ⇒ the same blindness makes a `Tree`
   declare *itself* dead and steal the topology lock from itself.
4b. **`self.participant + 1` on a read-only tree** ⇒ arithmetic overflow.
   `u32::MAX` is the read-only sentinel (`tree.rs:801`), so the expression panics
   in a debug build and wraps to `0` in release. The release behaviour is
   *accidentally* harmless — `owner == 0` is filtered one line earlier — and
   accidental correctness is exactly what this project does not accept. The
   precondition is that only a read-write participant reaps; encode it as an
   assertion at the top of the loop, not as a comment.
5. **`CreatePolicy::Always`** creates a second arena against the *same* lock file, so
   arena A's edge 5 and arena B's edge 5 alias on byte `CLAIM_BASE + 5`, as do their
   participant bytes. Requires an instance-scoped lock path.
6. **Fork** ⇒ `MADV_DONTFORK` means the child's mapping is absent, so `Tree::drop`
   (`tree.rs:918-933`) faults at child exit even if every API entry point is guarded.
   `MappedArena::drop`'s `munmap` of an unmapped range is harmless; the fault is the
   participant release.

### What we commit to

- One new dependency edge, `tf_tree → tf_tree_ipc`, and the discipline that it stays
  one-directional and `shm`-gated.
- rustix `net` + `event` + `rand` features. No new crates, so `cargo deny` is
  unaffected.
- `tf_tree` remains `#![forbid(unsafe_code)]`. The two `unsafe` sites this work needs
  (`pthread_atfork`, and the `fork()` in the test helper) live outside it and are named
  here.
- The **fork test helper bends the documented unsafe budget**: it needs a real `fork()`
  without `exec`, which `std::process::Command` cannot do, so
  `crates/tf_tree_bench/src/bin/fork_child.rs` carries one `unsafe { libc::fork() }`
  with a `// SAFETY:` block. `tf_tree_bench` is `publish = false` and its crate root is
  `#![forbid(unsafe_code)]`, so this is a separate bin target, and it is called out
  here rather than discovered later.
- Miri coverage does **not** extend to any of this: miri cannot execute
  `memfd_create`, `F_ADD_SEALS`, or `fcntl(F_OFD_*)`. The `just miri` recipe gains a
  comment saying so, so that nobody "fixes" it by adding `--features shm`.
- aarch64 coverage of the new orderings depends on CI, and **GitHub Actions has
  produced no run for this repository since 2026-07-23**. Until that is resolved, the
  aarch64 half of every ordering claim in this milestone is unverified. This is a known
  gap, recorded rather than papered over.

## Implementation plan

Each step lands as one PR, in order.

1. **`instance_uuid` + `SegmentDescriptor`** — field at header offset 136, which is
   implicit padding created by `TopoLock`'s `align(64)` (`header.rs:77`): `boot_id`
   ends at 128, `_reserved` at 136, and the next 64-byte boundary is 192, so 136..192
   is free and `topo_lock` stays at 192 with `size_of == 256`. `FORMAT_VERSION` stays
   2. Bytes from `rustix::rand::getrandom` — **which must be retried on `EINTR` and
   on a short read**; the kernel does not return partial reads for buffers this small
   except when interrupted, and "except when interrupted" is the whole hazard, since a
   partially-filled uuid would still look random. Verified by
   `instance_uuid_lands_at_136` alongside the existing `key_field_offsets_are_stable`
   (`header.rs:179`), plus `two_creates_have_distinct_uuids` and
   `attach_preserves_the_creator_uuid`.
2. **`ParticipantTable::register_at(slot, ..)`** — §3.7's `participant_slot` requires
   the arena slot and the lock byte to be the same integer; today they are independently
   allocated (`lockfile.rs:175` vs `participant.rs:155`). Verified by a loom test in
   which two threads `register_at(3)` and exactly one wins; mutant: replace the CAS with
   load+store ⇒ loom finds both winning.
3. **§3.7 wire in `tf_tree_ipc`** (`wire.rs`, `server.rs`, `client.rs`, `ipc_child`
   gains `serve`/`attach`). Verified by a child-serves/parent-receives round trip that
   `fstat`s the received fd; mutant: omit the `ScmRights` push ⇒ the client must fail
   with `NoFdReceived`, not hang and not succeed. Plus `layout_mismatch_names_both_hashes`
   (asserting the ancillary iterator is *empty* on rejection) and
   `wire_status_codes_are_pinned`.
4. **Wire §3.7 into `Open`** — widen `ServerProbe`, add `register_at`, make rejections
   terminal, and add `IpcError::SocketPathTooLong` checked at `Rendezvous` construction
   rather than at `bind`. Verified by all eight existing `open.rs` tests passing
   unchanged, plus `a_rejection_does_not_burn_the_timeout`.
5. **`tf_tree::open()`** — the seam. Verified by `crates/tf_tree/tests/rendezvous.rs`
   covering PHASE2 §11.2 scenarios 7 (thundering herd, 32 processes, one `Created` and
   one shared `instance_uuid`), 9 (split-brain), 10 (stuck participant), 11 (domain
   isolation), 4 (layout mismatch) and 6 (65th participant). Mutant for 7 and 9: remove
   §3.4 step 4 (`any_participant_held`, `open.rs:279`) ⇒ distinct uuids appear.
6. **§5.1 liveness from `F_OFD_GETLK`** — verified by `a_sigstopped_holder_is_still_alive`
   (mutant: the current `/proc` predicate ⇒ a stopped process reads as dead),
   `a_sigkilled_holder_is_immediately_dead`, and `a_tree_never_reports_itself_dead`.
7. **§6.1 claims as leases** — verified by two processes racing edge 5;
   `SIGKILL`-then-`probe_claim` with no sleep; `the_acquire_window_backs_out` (mutant:
   delete the epoch re-check ⇒ the claimer publishes onto a reaped record); and a loom
   model of the CAS↔lock ordering with the kernel byte as a loom atomic.
8. **§6.3 reaping** — verified by `a_reaper_does_not_reap_itself` (mutant: delete the
   self-skip ⇒ the process revokes its own live claims — the single most valuable test
   in this milestone), `killed_writer_is_reaped_and_reclaimed` including A5 parity
   repair, and `a_stopped_writer_is_never_reaped` (which is D17/§6.4 as an executable
   assertion).
9. **Fork poisoning** — verified by `fork_child`: the child calls `tree.lookup(..)`,
   must get `ChildDetached`, and must exit 0. Assert `WIFEXITED`, not merely the status
   code; mutant: remove the `Drop` guard ⇒ the child dies with `SIGSEGV` even though
   the API check is present, which an exit-status-only assertion would miss.
10. **§7.1 per-region population** — drop `MapFlags::POPULATE` (`mapped.rs:375`); use
    `Advice::LinuxPopulateWrite` with a zeroing-write fallback on `EINVAL` for kernels
    < 5.14. Verified with `mincore` over an over-provisioned arena: declared edges
    resident, headroom not; mutant: restore `MAP_POPULATE` ⇒ headroom is resident.
    Guard against vacuity by asserting the headroom region is ≥ 2 MiB.
11. **CLI adoption** — `--attach`/`--domain`/`--name`/`--rw`/`--create`/`--timeout`; new
    `tf_tree participants` reading `LockFile::read_identity`, which **must work without
    the arena** (§3.3). Verified by an integration test that starts a publisher and
    asserts a specific `doctor` finding; mutant: point it at another domain ⇒ must
    report "no arena", not a stale snapshot.
12. **Docs close-out** — PHASE2 §0.0 status table, the D16 amendment note, RUNBOOK rows,
    `README.md` gaining §3.10's "shared memory IPC is not a sandbox", and this document
    to `implemented` with PR numbers.

New recipes: `shm-rendezvous` (§11.2 scenarios), `shm-split-brain` (scenario 9 × 1000,
nightly). `shm-check` extends to `-p tf_tree --features shm`, `-p tf_tree_ipc` and
`-p tf_tree_cli --features shm`, because that combination only compiles under `shm` and
`--workspace` never sees it. CI extends the existing `shm` job on **both** x86-64 and
`ubuntu-24.04-arm` rather than adding a job.

## Found while implementing

**A read-only participant holds a lock byte but has no arena record.**
`Tree::attach_shared` skips registration when the mapping is not writable — it
*cannot* write the table — so a `mode="ro"` joiner, which is the consumer
default (D18) and the Python default (`PHASE3.md` §4.1), occupies
`participant` byte *n* in the lock file while arena slot *n* stays `FREE`.

That is the byte/record split step 2's `register_at` exists to close, reappearing
from the other side. Two consequences:

1. `Tree::participant_alive(n)` reports such a peer **dead**, because it checks
   the arena record before consulting the lock. For a reaper that is the safe
   direction — there is nothing to reap — but it means the predicate answers
   "dead" for a live process, and any future code that reads it as "this slot is
   free" would be wrong.
2. The owner's slot assigner scans the *arena* table, so it can hand out a slot
   whose lock byte is already held by a read-only peer. Today the granted-slot
   bitmask (step 5) prevents an immediate re-grant, so the joiner retries and
   gets a different slot — but the bitmask does not survive an owner restart,
   and after a takeover the new owner would name that slot again and the joiner
   would loop.

Neither is reachable as a *correctness* failure today, which is why this is
recorded rather than hot-fixed. The fix belongs with step 8 (reaping), which is
the first code that must decide what a slot's true occupancy is: either the
owner consults `held_participants()` as well as the arena table, or a read-only
attach stops taking an arena-indexed byte at all. **Resolve it there; do not
paper over it by making `participant_alive` consult the lock first**, which
would report a phantom participant as alive and give the reaper nothing to act
on.

## Open questions

None. Items that PHASE2 §3 leaves under-specified are resolved above rather than left
open — §3.4 step 5's socket temp name (§3 of the Decision), the missing handshake
timeout and datagram-length rule (§3), the absent terminal exit for a rejection (§3),
who watches the socket (§3: one watcher thread per attached `Tree`), the reaper's
self-blindness and the missing epoch re-check (§5, §6), `sun_path`'s 108-byte limit
(plan step 4), and the `--force-new` byte aliasing (§5 of *Consequences*).

Two items are deliberately **out of scope** and left for a later decision rather than
called open questions here:

- **§3.8 versus decision `0004`.** §3.8 presumes a generous default layout (~600 MiB)
  for a creator that declares no capacity; `0004` sizes the arena from declared edges
  and is still authoritative. `open()`'s `layout_if_creating` inherits `0004`'s model,
  so §3.8's premise does not arise in this milestone. One of the two must give, and
  that is its own decision.
- **§11.4 `shm_torture`.** The harness skeleton lands with step 12; the 30-minute
  nightly run is a separate piece of work.
