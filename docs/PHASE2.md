# tf_tree — Phase 2 Implementation Specification: Shared Memory

> **Companion documents:** `docs/PROJECT.md` (vision, roadmap, decision log) and `docs/PHASE1.md` (single-process core). Read §1 of this document before writing any Phase 2 code — it contains mandatory amendments to the Phase 1 design that were discovered by working the multi-process failure modes through.

**Deliverable:** the same arena, mapped into N processes, with the *identical unmodified* reader code from Phase 1 running against it. Plus the lifecycle, liveness, and fault-tolerance machinery that makes that safe when processes die at arbitrary points.

**Framing.** Phase 1 was the easy half. In one process, a crash takes down every reader with it, so a torn write is unobservable. Across processes it is not: a writer can be `SIGKILL`ed between two stores and leave a data structure permanently wedged while sixteen readers keep running. **Every mutation protocol in the arena must therefore be crash-consistent — there must be no state a dead process can leave behind that a live process cannot detect and repair.** That single requirement drives most of this document.

Sections marked **NORMATIVE** are requirements. Where a syscall behaviour is asserted, it has been verified on Linux 6.18; the probe is reproduced in Appendix B so you can re-run it on your target kernel.

---

## 0.0 Implementation status

**Partially implemented.** The mapping works; the lifecycle does not yet exist.

| Area | Status |
|---|---|
| `MappedArena` — `memfd`, sealed, `MAP_SHARED`, `MADV_DONTFORK`/`HUGEPAGE` (§4, §7) | **Done** (`tf_tree_arena::mapped`, behind `--features shm`) |
| `TreeBuilder::build_shared` / `Tree::attach_shared`, read-only mode (§8) | **Done** |
| Zero-diff read path — unmodified Phase 1 reader over a shared segment (§4) | **Done, and tested** (`just shm-test`) |
| Multi-process benchmarks (§12.2) | **Done** (`just shm-scaling`; results in `docs/benchmarks/tf2.md`) |
| Attach protocol — `SOCK_SEQPACKET` + `SCM_RIGHTS`, negotiation (§3.3) | Not implemented — fd inheritance stands in |
| Amendments A1-A4, A6-A8 (§1) | **Not applied** |
| Participant registry, liveness, reaping (§5, §6) | Not implemented |
| `tf_treed`, `tf_tree_record`, `/tf` ingest, diagnostics (§9, §10) | Not implemented |
| Fault injection, `shm_torture` (§11.3, §11.4) | Not implemented |

**What the gap means.** Everything above the line works and is measured: N
processes map one arena, read it with byte-identical results, and see each
other's writes, at no per-lookup cost over the single-process path. What is
missing is entirely the **crash-consistency and lifecycle** half — the machinery
that makes it safe when a participant dies at an arbitrary instruction. A
process killed while holding a claim currently leaks that edge (A3/A4), and one
killed mid-topology-mutation could leave the generation permanently odd and spin
readers forever (A1).

Those failure modes cannot occur in the flows implemented today — topology is
immutable after `build_shared`, per decision `0004`'s builder-time declaration —
but they *will* the moment a long-lived daemon owns the segment, which is §9.
**Apply §1's amendments before anything ships against this.**

---

## 0. Scope

### In scope

| | |
|---|---|
| `MappedArena` | memfd-backed, sealed, `MAP_SHARED` |
| Attach protocol | `SOCK_SEQPACKET` + `SCM_RIGHTS`, with version and layout negotiation |
| Participant registry | in-arena, fixed capacity, PID-reuse-proof identity |
| Liveness and reaping | socket `HUP` primary, `/proc` verification, cooperative and idempotent |
| Crash-consistency | every arena mutation protocol audited and repaired (§1) |
| Read-only attach | `PROT_READ` mapping as a real safety boundary |
| Mapping policy | `MAP_POPULATE`, `MADV_HUGEPAGE`, `MADV_DONTFORK`, optional `mlock` |
| `tf_treed` | reference owner daemon, ~400 lines |
| `tf_tree_record` | MCAP record/replay — the correctness harness for this phase |
| `/tf` ingest bridge | read-only ROS 2 → arena, for real-data benchmarking |
| Diagnostics | `doctor`, `top`, `participants` |

### Out of scope — NORMATIVE

Everything excluded in §0 of `PHASE1.md` remains excluded. Additionally:

| Excluded | Why |
|---|---|
| Network, discovery beyond one host | Phase 6 |
| Python bindings | Phase 3 (but see §14 for the handoff constraints you must not break) |
| `tf2_ros::Buffer` API shim | Phase 4. The *ingest bridge* here is one-way and does not implement the tf2 API. |
| macOS / Windows shared memory | §2. Those platforms get in-process only until Phase 6. |
| Multi-arena federation on one host | One arena per domain. Multiple domains are independent and do not interact. |
| Any security boundary against a malicious RW peer | §3.3. Say this out loud in the docs. |
| Dynamic arena resize | D4. Still forbidden. Capacity is planned, not grown. |

### Trust model — NORMATIVE, and state it in the public docs

Participants are **mutually trusting, same-user, cooperating processes**. A participant attached read-write can corrupt any part of the arena, and no amount of checksumming changes that. What the design *does* guarantee:

- A participant attached **read-only cannot corrupt anything**, enforced by the MMU. This is the only real boundary and it should be the default for consumers.
- A participant that **crashes** — at any instruction — cannot corrupt anything or wedge any other participant. This is a hard requirement, tested by fault injection (§11.3).
- A participant that **hangs** (SIGSTOP, GC pause, priority inversion) cannot corrupt anything, and cannot be mistaken for a crashed one (§6.4).

Do not oversell it beyond that. "Shared memory IPC is not a sandbox" belongs in the README.

---

## 1. Phase 1 amendments — NORMATIVE, apply before Phase 1 is frozen

`PROJECT.md` states that if Phase 2 requires changes outside `tf_tree_arena`, the Phase 1 design was wrong. Working through the crash matrix found eight places where it was. Seven are cheap; one (A6) changes the arena layout.

**If Phase 1 has not yet been frozen, apply all of these to it.** If it has shipped, they constitute `FORMAT_VERSION = 2` and no version-1 arena may be attached.

---

### A1 — Pack the topology generation and active index into one atomic word

**Problem.** Phase 1 §5.2 uses a seqlock: bump `topo_generation` to odd, copy the block, flip `topo_active`, bump to even. A writer `SIGKILL`ed after the first bump leaves the generation permanently odd. Every reader then spins forever in plan compilation. **This wedges the entire arena and there is no recovery.**

**Fix.** There is no need for an odd state at all. The writer mutates an *inactive* block, which no reader is looking at; the active block is never mutated in place. So publication is a single store, and there is nothing to make atomic across a window.

```rust
/// bits 63..8 = generation (monotone), bits 7..0 = active block index
#[repr(C)]
pub struct TopoWord(pub AtomicU64);

#[inline] pub const fn pack(gen: u64, active: u8) -> u64 { (gen << 8) | active as u64 }
#[inline] pub const fn unpack(w: u64) -> (u64, u8) { (w >> 8, (w & 0xff) as u8) }
```

Writer:

```
hold the topology lock (A2)
w    = topo.load(Relaxed); (gen, active) = unpack(w)
next = (active + 1) % TOPO_BLOCKS
copy block[active] -> block[next]; apply mutation; recompute depths
fence(Release)
topo.store(pack(gen + 1, next), Release)     // single publishing store
release the lock
```

Reader (plan compilation only; `Plan::at` never touches this):

```
for _ in 0..TOPO_RETRY_LIMIT {
    w1 = topo.load(Acquire); (gen, active) = unpack(w1)
    ...walk block[active], bounds-checked, step budget max_frames...
    fence(Acquire)
    if topo.load(Relaxed) == w1 { return plan.with_generation(gen) }
}
return Err(TopologyChurn)
```

A dead writer now leaves the arena in a state that is *indistinguishable from no write having happened*. Readers are wait-free and never spin on a writer.

**`TOPO_BLOCKS = 4`, not 2.** With two blocks a reader is only hit if the writer flips twice mid-read. Four blocks require four flips. At `max_frames = 256` a block is 1.5 KB, so four cost 6 KB — free. Mutations happen a few hundred times per process lifetime; this makes `TopologyChurn` effectively unreachable outside a torture test.

**Topology block arrays become `[AtomicU32]` and `[AtomicU16]`.** A reader racing a writer on the same block reads garbage and discards it — but reading a non-atomic `u32` while another process writes it is a data race and therefore UB, even when the value is thrown away. Relaxed atomic loads compile to the same instruction. **Every index read from a topology block must be bounds-checked before use and the parent walk must be capped at `max_frames` steps**, because garbage from a losing race must not panic or index out of bounds before the validity check catches it.

---

### A2 — The topology mutation lock lives in the arena and is reapable

Phase 1 serializes topology mutations with a Rust `Mutex`, which is per-process and therefore does nothing across processes.

```rust
#[repr(C, align(64))]
pub struct TopoLock {
    /// 0 = free, else participant_slot + 1
    pub owner: AtomicU64,
    pub acquired_at_nanos: AtomicI64,
    _pad: [u8; 48],
}
```

Acquire is `compare_exchange(0, slot + 1, AcqRel, Acquire)` with bounded spin. On failure, resolve the owning participant (§5) and check liveness (§6.2); if dead, `compare_exchange(stale, slot + 1)` to steal. Because A1 makes an abandoned mutation leave *no trace*, stealing the lock is safe with no rollback: the new holder simply re-copies from the current active block. This is the payoff for A1 — recovery is a no-op.

---

### A3 — Claim ownership is a participant slot, not a PID

Phase 1's `ClaimRecord` stores `owner_pid` and `owner_boot_id` as separate fields written *after* the state CAS. A writer killed between the CAS and the PID store leaves `state = HELD, owner_pid = 0` — held by nobody, reapable by nobody. Permanently leaked edge.

**Fix.** One atomic word carries both the state and the full identity, because the identity is an *indirection* into a participant record that was fully written at attach time, long before any claim.

```rust
#[repr(C, align(64))]
pub struct ClaimRecord {
    /// 0 = free, else participant_slot + 1. Claim and identity publish atomically.
    pub owner: AtomicU64,
    /// Incremented on every reap and every successful claim. Fences zombies (A4).
    pub epoch: AtomicU64,
    /// Advisory only. NEVER a reaping trigger on its own (§6.4).
    pub heartbeat: AtomicU64,
    pub last_push_nanos: AtomicI64,
    _pad: [u8; 32],
}
```

Claim: `owner.compare_exchange(0, slot + 1, AcqRel, Acquire)`, then `epoch.fetch_add(1, AcqRel)`, and the `Publisher` records the resulting epoch.

---

### A4 — `push` must verify the claim epoch

**The zombie writer.** A process `SIGSTOP`ped, or stalled in a GC pause or on a page fault against a slow device, can be judged dead, have its claim reaped, and then *resume* and continue pushing to an edge another process now owns. Two writers, silent corruption — precisely the failure the claim model exists to prevent.

**Fix.** One relaxed load per push, on a cacheline the writer already touches:

```rust
if self.claim.epoch.load(Ordering::Relaxed) != self.epoch {
    return Err(PushError::ClaimRevoked { edge: self.id });
}
```

Cost is ~1 ns and it is not optional. Reaping bumps the epoch *before* freeing the claim (§6.3), so the window is closed from both ends.

---

### A5 — The slot sequence writer forces parity instead of incrementing

A writer killed between `seq.store(s+1)` (odd) and `seq.store(s+2)` (even) leaves the slot permanently odd. Every reader that reaches it burns `SEQ_RETRY_LIMIT` and returns `SlotContended`. The sample was never published — `head` was not bumped — so no *correct* reader looks at it, but when the ring wraps, the next writer reads an odd `s` and its `s+1` lands even, inverting the protocol for that slot.

**Fix.** Force the parity rather than incrementing:

```rust
let s    = slot.seq.load(Ordering::Relaxed);
let odd  = s | 1;                                    // self-heals a stale odd
slot.seq.store(odd, Ordering::Relaxed);
core::sync::atomic::fence(Ordering::Release);
// ...write stamp and pose data...
slot.seq.store(odd.wrapping_add(1), Ordering::Release);
```

The seqlock still works: any reader that observed the stale odd value retried without reading, so no reader can be mid-read holding it. Additionally, **claim acquisition normalizes the slot at `head & mask`** — one store, once, at claim time.

---

### A6 — The arena gains a participant table (layout change)

New region, sized `max_participants * 128`, placed between the claim table and the edge table. `ArenaHeader` gains `participant_table_off: u32`, `max_participants: u32`, and `participant_count: AtomicU32`, taken from `_reserved`. Default `max_participants = 64`.

**This is the only amendment that changes the layout, and therefore the only one with a `FORMAT_VERSION` consequence.**

---

### A7 — Header identity fields

`ArenaHeader` gains `owner_start_time: u64` alongside the existing `creator_pid`, and `boot_id: [u8; 16]` replaces the `u64` (a boot ID is a 128-bit UUID; truncating it to 64 bits loses the property that makes it useful). Both fit in `_reserved`.

---

### A8 — Interning must not spin forever on a dead claimant

Phase 1 §5.1's interning waits for `ids[i] != U32_MAX` with an **unbounded**
spin. A process that wins the hash-slot CAS and dies before publishing the id
leaves that slot claimed and unpublished forever, and **every future interner of
that name spins forever**. In one process this is unobservable — the dead
process took its readers with it. Across processes it wedges every live
participant that touches the name.

This is the same class of defect as A1, and it is the crash point
`intern.after_hash_cas_before_id_store` in §11.3.

**Fix.** Record the claimant alongside the hash, bound the spin, and take the
entry over if the claimant is dead:

```rust
/// Parallel to `hashes`/`ids`: the participant slot that won the CAS, + 1.
/// Written BEFORE the hash is published, so a reader that sees the hash can
/// always resolve who claimed it.
claiming: [AtomicU32],

// waiter, after INTERN_SPIN_LIMIT iterations:
let owner = claiming[i].load(Acquire);
if owner != 0 && !is_alive(&participants[(owner - 1) as usize], boot) {
    // Take over: republish `claiming`, write the record, publish the id.
    if claiming[i].compare_exchange(owner, my_slot + 1, AcqRel, Acquire).is_ok() {
        write_record(id); ids[i].store(id, Release);
    }
}
```

The takeover is idempotent and CAS-guarded, so concurrent rescuers cannot both
publish. `is_alive` is §6.2's predicate, which fails **safe** — an unreadable
`/proc` means "alive", so a slow interner is never stolen from.

Note this interacts with Phase 1's `ID_FAILED` sentinel, which already handles
the *capacity* failure by publishing a terminal marker. A8 handles the *crash*
failure, which has no such marker because the claimant never got to write one.

## 2. Platform, dependencies, feature gating

**NORMATIVE.** Shared memory is **Linux-only** in Phase 2, requiring **kernel ≥ 3.17** for `memfd_create` and **≥ 3.17** for `F_ADD_SEALS`. Target and test on 5.15 (Ubuntu 22.04 / JetPack 6) and current stable.

Do not build a POSIX abstraction layer. macOS and Windows keep `HeapArena` and in-process operation; a file-backed unsealed `MappedArena` for macOS developer ergonomics is acceptable *later*, explicitly labelled dev-only, and must not shape the Linux design.

| Crate | New dependencies |
|---|---|
| `tf_tree_arena` | `rustix` (feature `shm`, `mm`, `fs`, `net`) — no libc crate, no C build step |
| `tf_tree_ipc` (new) | `rustix`. Attach protocol, participant registry, reaping. |
| `tf_tree_record` (new) | `mcap`, `serde` — **isolated here specifically so D14 holds for the core** |
| `tf_tree_core` | **none.** Unchanged. |

`tf_tree_core` gaining a dependency in this phase is a design failure, not a tradeoff. If you find yourself needing one, stop and report it.

---

## 3. Segment lifecycle

### 3.1 Ownership is configured, not negotiated — NORMATIVE

One process **owns** the arena: it creates the memfd, initializes the header, and serves the attach socket. Others attach. There is no leader election, no consensus, no takeover.

This is a deliberate refusal to build machinery that configuration solves. On a real robot there is always a natural owner — the state estimator, or a supervisor. Ownership negotiation would add a distributed-consensus problem to a project whose entire value proposition is a fast local lookup, and it would be the least-tested code in the system.

Three supported topologies, in order of preference:

1. **`tf_treed` owns** (§8). Recommended for production. The arena outlives every node; nodes attach and detach freely.
2. **The state estimator owns.** Fewer moving parts for small systems. If it restarts, existing attachers keep working (§3.4) but new ones cannot attach until it is back.
3. **In-process only.** `HeapArena`, unchanged from Phase 1. Still a first-class configuration and the default.

### 3.2 Creation — NORMATIVE sequence, verified on Linux 6.18

```
1. memfd_create("tf_tree.<domain>.<name>", MFD_CLOEXEC | MFD_ALLOW_SEALING)
2. ftruncate(fd, arena_size)
3. mmap(NULL, arena_size, PROT_READ|PROT_WRITE, MAP_SHARED|MAP_POPULATE, fd, 0)
4. initialize header, zero regions, write magic / format_version / layout_hash /
   arena_size / owner identity / boot_id
5. fcntl(fd, F_ADD_SEALS, F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_SEAL)
6. madvise(base, len, MADV_DONTFORK)     // §7.3 — must precede any fork
7. madvise(base, len, MADV_HUGEPAGE)     // best-effort, ignore EINVAL
8. bind SOCK_SEQPACKET at <runtime>/<domain>/<name>.sock.<pid>.tmp, chmod, rename(2)
9. listen(backlog = max_participants); serve on a dedicated thread
```

Step 5 is the load-bearing one. Verified behaviour:

| Operation | Result |
|---|---|
| `F_ADD_SEALS SHRINK\|GROW` while a writable mapping is held | **succeeds** — the owner keeps write access |
| `F_ADD_SEALS WRITE` while a writable mapping is held | `EBUSY` — cannot over-seal by accident |
| `ftruncate` shrink after sealing | `EPERM` |
| `F_ADD_SEALS` anything after `F_SEAL_SEAL` | `EPERM` |
| `F_GET_SEALS` | `0x7` = `SEAL \| SHRINK \| GROW` |

**Sealing against shrink is what makes `SIGBUS` structurally impossible.** Without it, any process with the fd could `ftruncate` the segment and every reader touching a truncated page would take `SIGBUS` from inside a lookup — an unrecoverable fault in the middle of a control loop, with no way for a library to handle it sanely. With it, the size is immutable for the life of the fd. Do not skip step 5, and do not "simplify" to `shm_open`, which cannot be sealed.

Step 8 uses create-then-`rename(2)` because `rename` is atomic: a concurrent attacher either sees no socket or a fully-bound one, never a half-created path.

### 3.3 Attach

```
1. connect SOCK_SEQPACKET to <runtime>/<domain>/<name>.sock
2. send HelloRequest
3. recvmsg -> HelloResponse + SCM_RIGHTS fd  (or a rejection with no fd)
4. verify fstat(fd).st_size == response.arena_size
   verify F_GET_SEALS & (F_SEAL_SHRINK|F_SEAL_GROW) == both   // refuse an unsealed segment
5. mmap(NULL, size, PROT_READ [| PROT_WRITE], MAP_SHARED|MAP_POPULATE, fd, 0)
6. verify header magic, format_version, layout_hash, arena_size, boot_id
7. madvise DONTFORK, HUGEPAGE
8. write the participant record at the assigned slot; publish it
9. KEEP THE SOCKET OPEN for the lifetime of the attachment
```

**Step 9 is the design's best idea and is easy to miss.** The socket is not a handshake channel to be closed after use — **it is the liveness signal.** When a participant dies for any reason, the kernel closes its socket and the owner's `epoll` reports `EPOLLHUP` within microseconds. That gives crash detection that is immediate, exact, and free, with no heartbeat timeout to tune and no possibility of misjudging a slow process as dead. Heartbeats degrade to a diagnostic (§6.4).

Message structs are fixed-size `#[repr(C)]`, little-endian, sent over `SOCK_SEQPACKET` so message framing comes from the kernel and there is no framing code to get wrong:

```rust
#[repr(C)]
pub struct HelloRequest {
    pub magic: [u8; 8],            // b"TF_TREE\0"
    pub format_version: u32,
    pub layout_hash: u32,
    pub mode: u8,                  // 0 = ReadOnly, 1 = ReadWrite
    _pad: [u8; 7],
    pub client_pid: u32,
    _pad2: u32,
    pub client_start_time: u64,    // /proc/self/stat field 22
    pub client_boot_id: [u8; 16],
    pub client_name: [u8; 32],     // diagnostics only, NUL-padded
}

#[repr(C)]
pub struct HelloResponse {
    pub magic: [u8; 8],
    pub status: u32,               // 0 = Ok
    pub format_version: u32,
    pub layout_hash: u32,
    pub participant_slot: u32,
    pub arena_size: u64,
    pub owner_pid: u32,
    _pad: u32,
}
```

Rejection statuses, each of which must produce a message naming both sides' values: `VersionMismatch`, `LayoutMismatch`, `BootIdMismatch`, `NoParticipantSlots`, `ModeNotPermitted`, `Malformed`.

`LayoutMismatch` is the one operators will hit, and it means a binary built against a different struct layout tried to attach. **The error message must say so in those words** and name both hashes, because the raw symptom — attach failing on a machine where everything looks fine — is otherwise a multi-hour debugging session. This is why `layout_hash` was computed in Phase 1 despite nothing reading it then.

### 3.4 Detach, owner death, teardown

The memfd is refcounted by its mappings, so:

- **A participant dies** → its mapping drops, its socket HUPs, the owner reaps its slot and claims (§6). Nothing else notices.
- **The owner dies** → the socket disappears. **Every existing participant keeps working indefinitely**, because the segment lives as long as any mapping does. New attachments fail with `ConnectionRefused`. Reaping continues, cooperatively, without the owner (§6.3).
- **The last mapping drops** → the kernel frees the segment. **No stale segments, ever.** This is the second reason to prefer memfd over `shm_open`: a `/dev/shm` file outlives every process that used it, and stale segments after a crash are an operational plague.

A stale *socket path* can be left behind, and is handled by: connect first; on `ECONNREFUSED`, `unlink` and proceed to create.

---

## 4. `MappedArena`

**NORMATIVE:** the diff against Phase 1 outside `tf_tree_arena` and the new `tf_tree_ipc` crate must be **zero lines in the read path**. `PoseSlot`, `EdgeBuffer`, `Plan::at`, bracket search, and interning are byte-identical code operating on a different base pointer.

**The premise is now tested, not merely asserted.** "A different base pointer" only works if nothing in the arena is an absolute address, which Phase 1 documented but never checked. `crates/tf_tree_bench/tests/relocation.rs` is that check: it byte-copies a populated arena to a different address, wraps the copy in a minimal `Arena` impl standing in for `MappedArena`, and requires **bit-identical** results — not approximate — across every frame pair in the fixture, plus frame-name resolution and header validation. It guards against a vacuous pass twice (the copy must land at a different address; more than 1000 queries must actually be compared).

It passes today, which is the evidence that the "zero lines in the read path" claim above is achievable rather than aspirational. Keep it green: a regression that caches one resolved address would otherwise surface for the first time in another process, as a wild read rather than an error.

```rust
pub struct MappedArena {
    base: NonNull<u8>,
    len: usize,
    fd: OwnedFd,
    mode: AttachMode,
    _socket: Option<OwnedFd>,      // liveness — dropping this signals detach
    participant_slot: u32,
}

unsafe impl Arena for MappedArena {
    fn base(&self) -> *mut u8 { self.base.as_ptr() }
    fn len(&self) -> usize { self.len }
}
```

`Drop` order is fixed and matters: publish detach in the participant record, then `munmap`, then close the socket, then close the fd. Publishing detach before unmapping means the owner's reap path never races a half-torn-down participant.

Write a test asserting `Tree` is generic over `A: Arena` with no `MappedArena`-specific branches, and a compile-fail test asserting `Publisher` cannot be constructed from a `ReadOnly` arena.

---

## 5. Participant registry

```rust
#[repr(C, align(64))]
pub struct ParticipantRecord {
    /// 0 = free, 1 = attaching, 2 = live, 3 = detaching. Published last.
    pub state: AtomicU32,
    pub mode: u8,                  // 0 RO, 1 RW
    _pad0: [u8; 3],
    pub pid: u32,
    _pad1: u32,
    pub start_time: u64,           // /proc/<pid>/stat field 22 — defeats PID reuse
    pub attach_nanos: i64,
    pub heartbeat: AtomicU64,
    pub name: [u8; 32],
    _pad2: [u8; 24],
}
```

Slot assignment is done by the owner under its accept loop, so it needs no CAS protocol. The record is written by the *owner* (which knows the client's identity from `HelloRequest`) before the response is sent, so a participant record is always fully populated before any claim can reference it. That ordering is what makes A3's indirection sound.

### 5.1 PID-reuse-proof identity — NORMATIVE

`(pid, start_time, boot_id)` is the identity triple. A bare PID is not an identity: PIDs are recycled, and on a busy embedded system with a low `pid_max` they recycle fast. Reaping a live process's claim because it inherited a dead one's PID would be a silent-corruption bug of exactly the kind this phase exists to eliminate.

`start_time` is field 22 of `/proc/<pid>/stat`, in clock ticks since boot.

**The parsing trap — NORMATIVE.** Field 2 is `comm`, the executable name in parentheses, and it may contain spaces *and parentheses*. Splitting the line on whitespace and taking index 21 is wrong and will silently return a different field for any process whose name contains `) (`. Always locate the **last** `)` in the line and parse fields from there:

```rust
let rp = raw.rfind(')').ok_or(ProcParseError)?;
let field22 = raw[rp + 2..].split_ascii_whitespace().nth(19).ok_or(ProcParseError)?;
```

A demonstration of the naive parse returning the wrong value is in Appendix B. Include that exact case as a unit test against a fixture string — you cannot easily create a process with such a name, but you can test the parser.

---

## 6. Liveness, reaping, and the zombie

### 6.1 Detection sources, in priority order

| Source | Latency | Catches |
|---|---|---|
| Socket `EPOLLHUP` | microseconds | any process death: crash, kill, exit, OOM |
| `/proc` liveness check | on demand | death when the owner is absent or the socket was never established |
| Heartbeat staleness | seconds | **hangs only — never triggers reaping (§6.4)** |

### 6.2 The liveness predicate — NORMATIVE

```rust
fn is_alive(p: &ParticipantRecord, boot: &[u8; 16]) -> bool {
    if &arena.header.boot_id != boot { return false; }        // arena predates a reboot
    match read_start_time(p.pid) {
        Ok(st) => st == p.start_time,                          // PID reuse -> false
        Err(NotFound) => false,
        Err(_) => true,                                        // fail SAFE: assume alive
    }
}
```

**Unreadable `/proc` returns "alive".** A false negative reaps a working writer and corrupts data; a false positive leaks one edge until the next check. The asymmetry is enormous and the default must always favour leaving the claim alone.

### 6.3 Reaping is cooperative and idempotent — NORMATIVE

Any read-write participant may reap. The owner is merely the fastest, because it has the `EPOLLHUP` signal; others reap lazily when `claim()` finds an edge held. **There is no single point of failure for reaping** — an owner-only design would leak every claim held at the moment the owner died.

```
fn try_reap(claim: &ClaimRecord) -> Reaped {
    let owner = claim.owner.load(Acquire);
    if owner == 0 { return NotHeld }
    let p = &participants[(owner - 1) as usize];
    if p.state.load(Acquire) == LIVE && is_alive(p, boot) { return OwnerAlive }

    // 1. Fence the zombie FIRST. A4's epoch check now fails for the old Publisher
    //    even if it resumes mid-reap.
    claim.epoch.fetch_add(1, AcqRel);

    // 2. Repair anything the dead writer may have left mid-protocol (A5).
    normalize_slot_parity(edge, head & mask);

    // 3. Release. A concurrent reaper that already did 1 and 2 is harmless:
    //    both steps are idempotent, and only one CAS can win.
    match claim.owner.compare_exchange(owner, 0, AcqRel, Acquire) {
        Ok(_)  => Reaped::Yes,
        Err(_) => Reaped::RacedAndLost,
    }
}
```

Ordering is not negotiable: **bump the epoch before freeing the claim.** Freeing first opens a window in which a new writer claims the edge and a resurrected zombie still passes its epoch check — two live writers on one edge, which is the exact failure the whole claim mechanism exists to prevent.

### 6.4 Heartbeats never trigger reaping — NORMATIVE

A heartbeat is bumped on every `push`. An edge legitimately published at 0.2 Hz — a map-to-odom correction from a slow global localizer — looks identical to a hung writer under any timeout short enough to be useful. Reaping it would break a working system in a way that is intermittent, load-dependent, and nearly impossible to reproduce.

Therefore: **staleness is reported by `doctor` as a warning and never acts.** A `ReapPolicy::HeartbeatTimeout(Duration)` may be offered as explicit opt-in for deployments that know their publish rates; it is off by default and its docs must state the failure mode.

The heartbeat's real job is the *hang* case — a live process that has stopped publishing — which is an operator-visible symptom, not something a library should resolve unilaterally.

---

## 7. Mapping policy

### 7.1 Page population — NORMATIVE

`MAP_POPULATE` on every mapping, owner and attacher. A minor page fault costs single-digit microseconds. The Phase 1 gate is a **150 ns p50 and a p99.9 that matters more than the p50** — a single fault in the lookup path blows that budget by two orders of magnitude, and a lazily-mapped 260 MB arena has tens of thousands of them waiting. Faulting them in at attach time moves that cost to startup where it belongs.

Measure this: §12 requires a benchmark row for first-access-after-attach *with and without* `MAP_POPULATE`, because it is the clearest demonstration of why the flag is there and it stops someone removing it later to speed up attach.

### 7.2 Huge pages

`madvise(MADV_HUGEPAGE)`, best-effort. A 260 MB arena on 4 KB pages needs ~63 000 TLB entries; on 2 MB pages, 130. With a dozen processes walking it, TLB pressure is real. THP must be `madvise` or `always` in `/sys/kernel/mm/transparent_hugepage/enabled` — `doctor` reports the current setting, and the benchmark reports both configurations rather than assuming.

### 7.3 `MADV_DONTFORK` — NORMATIVE, and easy to forget

A `MAP_SHARED` mapping survives `fork()`. A forked child inherits the mapping *and* the parent's `Publisher` structs, including their claim epochs — so both processes pass the A4 check and both write the same edge. Silent corruption, from an entirely ordinary `fork`.

`madvise(base, len, MADV_DONTFORK)` removes the mapping from the child, so a child touching it faults immediately and loudly instead of corrupting quietly. It must be applied at attach, before any fork can occur.

The child must re-attach to use the tree, which is correct: it is a different process and needs its own participant slot and claims. **Document this prominently**, because Python's `multiprocessing` defaults to `fork` on Linux and Phase 3 users will hit it (§14).

### 7.4 Memory locking

`LockPolicy::{ None, Populate (default), Locked }`. `Locked` calls `mlock2(MLOCK_ONFAULT)` and requires `RLIMIT_MEMLOCK`; failure is a warning, not an error, and `doctor` reports the current limit against the arena size. Worth it for hard-real-time consumers on a system with any swap or memory pressure.

---

## 8. Read-only attachment

**NORMATIVE:** `AttachMode::ReadOnly` maps `PROT_READ` only and is **the default for any participant that does not declare an intent to publish.**

This is the strongest safety property in the system and it costs nothing: a buggy or crashing perception node *cannot* corrupt the transform tree, enforced by hardware. Lead with it in the documentation — for an industrial integrator it is a more compelling argument than any latency number, because it converts a class of whole-system failures into a single-process fault.

Consequences, which must be enforced by types where possible and by errors where not:

| Operation | ReadOnly |
|---|---|
| `plan`, `at`, `at_many`, `at_adaptive` | permitted, identical code path |
| resolve an existing frame name | permitted |
| **intern a new frame** | `Err(FrameNotDeclared)` — interning writes |
| `claim` / `push` | not expressible: `Publisher` construction requires `ReadWrite` (compile-fail test) |
| reaping | not permitted — reaping writes |
| heartbeat | not written; the socket carries liveness |

`FrameNotDeclared` must explain itself: a read-only participant asking for a frame nobody has declared is usually a startup-ordering problem, and the message should say "no publisher has declared this frame yet" rather than "unknown frame".

---

## 9. `tf_treed`

The reference owner. Target ~400 lines. Deliberately boring: it holds no application logic, so it is the process least likely to crash.

```
tf_treed --domain <n> --name <n> --config <file.toml|urdf> [--participants 64]
         [--lock] [--socket-mode 0600] [--metrics-port <p>]
```

Responsibilities: create and seal the segment; declare frames and static edges from config or URDF so the tree exists before any node starts; serve the attach socket; `epoll` participant sockets and reap on `HUP`; serve `doctor`/`top` queries; export Prometheus metrics; on `SIGTERM`, drain and exit, leaving the segment alive for existing participants.

It must **not** publish, claim any edge, or interpret transforms. It is a lifecycle daemon.

The config-driven pre-declaration is what makes startup ordering deterministic: with the tree's static structure declared up front, read-only consumers can attach and plan before any publisher exists.

---

## 10. Recording and replay — the correctness harness

`tf_tree_record` lands **early in this phase, not at the end**, because it is how the shared-memory layer gets validated.

- **Record.** A read-only participant tapping every edge, writing MCAP with two channels: `tf_tree/topology` (declaration and mutation events) and `tf_tree/samples` (`edge_id`, `stamp`, `pose`). Recording is itself read-only, so it can be attached to a production system without risk.
- **Replay.** Reconstructs an arena from a recording and re-publishes deterministically.
- **The test that matters — NORMATIVE.** Replay one recording into a `HeapArena` and a `MappedArena`, run an identical query set against both, and assert **bit-identical `f64` results**, not approximate equality. Lookups are pure functions of `(plan, stamp, buffer contents)`, so any difference at all means the shared-memory path is not the same code, which is the central claim of this phase.

This also gives every subsequent phase a regression corpus, and gives §12 real robot data instead of synthetic input.

---

## 11. Test plan

### 11.1 What Miri and loom can and cannot do

**Neither tool crosses a process boundary.** `MappedArena` cannot be tested by either. The mitigation is architectural: the protocols are identical for both arena types, so run every existing Phase 1 loom test unchanged against a `HeapArena` with the A1–A5 amendments applied, and cover the multi-process dimension by fault injection instead. Add loom cases for:

- Two threads racing `try_reap` on the same claim: at most one `Reaped::Yes`, and the epoch is bumped at least once.
- Reap concurrent with `push` from the reaped `Publisher`: the push returns `ClaimRevoked` or completes before the epoch bump; it never lands after a new claimer's first push.
- Topology mutation concurrent with plan compilation across four blocks: readers see one consistent block or return `TopologyChurn`.
- Claim, reap, re-claim, zombie push: the zombie always fails.

### 11.2 Multi-process integration harness

`tf_tree_test_harness` spawns real child processes (not threads), coordinates via pipes, and asserts on arena state. Required scenarios:

1. 1 owner, 1 writer, 14 read-only readers. Sustained 1 kHz for 60 s. Zero errors, zero divergence between readers.
2. Attach/detach churn: 32 processes attaching and detaching randomly for 60 s while a writer publishes. Participant slots must not leak.
3. Owner dies mid-run: existing participants continue for 60 s; new attach fails cleanly; reaping still functions.
4. `FORMAT_VERSION` / `layout_hash` mismatch: attach is rejected with the correct status and a message naming both values.
5. Read-only participant attempts every write operation: all fail, arena bytes unchanged (verify by hashing the arena before and after).
6. 64 participants, then a 65th: `NoParticipantSlots`, and the message says how to raise the limit.

### 11.3 Fault injection — the core of this phase

**NORMATIVE.** A build-time `crash-points` feature places named, deterministic abort sites in every mutation protocol:

```rust
#[cfg(feature = "crash-points")]
macro_rules! crash_point { ($name:literal) => { $crate::crash::maybe_abort($name) }; }
```

Armed by `TF_TREE_CRASH_AT=<name>:<nth_hit>`, which `abort()`s (not `panic!` — a panic unwinds and runs `Drop`, which would clean up and defeat the test). Required sites, one test per site:

| Crash point | The state it leaves behind must be repairable |
|---|---|
| `push.after_seq_odd` | slot odd, `head` unbumped → A5 self-heals on next claim |
| `push.after_data_before_seq_even` | as above; sample invisible because `head` never moved |
| `push.after_seq_even_before_head` | sample fully written but unpublished → invisible, then overwritten |
| `topo.after_copy_before_publish` | inactive block dirty, word unchanged → **no observable effect** (A1) |
| `topo.holding_lock` | lock stuck → stealable after liveness check (A2) |
| `claim.after_cas` | claim held by a dead participant → reapable via slot indirection (A3) |
| `intern.after_hash_cas_before_id_store` | hash slot claimed, id unpublished → next interner spins then... **see below** |
| `attach.after_slot_assigned_before_publish` | participant slot in `ATTACHING` → owner reaps on socket HUP |

**`intern.after_hash_cas_before_id_store` needs a fix that Phase 1 does not have.** Phase 1's interning spins waiting for `ids[i] != U32_MAX`; if the process that won the hash CAS dies before publishing the id, every future interner of that name spins forever. Add a bounded spin plus recovery: after `INTERN_SPIN_LIMIT`, verify the participant that claimed it — record `claiming_slot` alongside the hash — and if dead, take over the entry. This is **amendment A8** in §1; cover it with a loom test.

### 11.4 `shm_torture`

Nightly CI, 30 minutes: N processes, random attach/detach/claim/reap/push/lookup, random `SIGKILL` at 1–10 Hz, a random crash point armed in 10% of children. Invariants checked continuously: no reader ever observes a non-unit quaternion or a NaN; no two writers ever hold one edge; participant and claim slots never leak; the arena hash is stable across quiescent points.

Run it under ASan (works across processes) and with `TF_TREE_PARANOID=1`, a debug mode that validates quaternion normalization and stamp monotonicity on every read.

---

## 12. Benchmarks and the gate

### 12.1 Fixture

The Phase 1 24-frame robot tree, plus: 1 writer process (4 dynamic edges as in Phase 1), 1–16 read-only consumer processes each running 4 reader threads, cores pinned, `isolcpus` if available. Compare against ROS 2 `tf2` with an equivalent tree over the default DDS, same rates.

### 12.2 Required measurements

| Benchmark | Report |
|---|---|
| depth-3 cross-process lookup, warm | p50, p99, p99.9 vs the Phase 1 in-process baseline |
| first access after attach, `MAP_POPULATE` on vs off | p99.9, both |
| THP `madvise` vs `never` | p50, p99.9, both |
| aggregate read throughput, 1→16 consumer processes | scaling curve |
| **CPU per consumer at 1 kHz × 20 edges, vs ROS 2 `/tf`** | %CPU per consumer, both |
| **total RSS across 16 consumers, vs ROS 2 `/tf`** | MB, both |
| publish → visible-to-consumer latency, vs ROS 2 `/tf` | p50, p99.9, both |
| `SIGKILL` writer → claim reapable → re-claimed | p50, p99 |
| attach time, cold and warm | p50 |

### 12.3 The gate — NORMATIVE

Proceed to Phase 3 if:

1. **Cross-process depth-3 p50 within 10% of the in-process baseline, p99.9 within 25%.** This is the central claim of the phase: the same code, the same speed, in another process. If it fails, the mapping policy is wrong (§7), not the design.
2. **Aggregate read throughput scales ≥ 12× from 1 to 16 consumer processes.**
3. **Zero corrupt reads across the full `shm_torture` run**, and every §11.3 crash point recovers. Not negotiable; a single failure here means the arena is not crash-consistent and the phase is not done.
4. **Kill → re-claimable p99 under 10 ms.**
5. Total RSS across 16 consumers under 1.2 × arena size.

### 12.4 What the numbers are actually for

The latency figures are the engineering gate. **The CPU-per-consumer and RSS figures are the industrial argument**, and they are the ones to put in the README.

Under `/tf`, every consumer independently deserializes every transform on the topic and maintains a full private replica: cost scales as O(consumers × edges × rate), and a robot with sixteen perception nodes pays for the same data sixteen times in both CPU and memory. Under `tf_tree`, consumers read shared pages: CPU is O(1) in the number of consumers, and RSS is one arena regardless.

Expect the latency ratio to be dramatic and the resource ratio to be the thing that actually persuades someone to migrate a working system. Lead with the latter.

---

## 13. Failure modes and runbook

Ship this table as `docs/RUNBOOK.md`. Every row must correspond to a `doctor` check and a distinct error type.

| Symptom | Cause | Response |
|---|---|---|
| `LayoutMismatch` on attach | binaries built from different commits | rebuild all participants; layout changes require a full restart |
| `BootIdMismatch` | arena predates a reboot (only possible with a file-backed dev arena) | recreate the arena |
| `ConnectionRefused` | owner not running, or a stale socket path | start the owner; the stale path is unlinked automatically |
| `NoParticipantSlots` | more than `max_participants` attached | raise `--participants`; requires an owner restart |
| `FrameNotDeclared` on a read-only participant | startup ordering: no publisher has declared it yet | pre-declare in `tf_treed` config |
| `ClaimRevoked` during `push` | this writer was judged dead and reaped | the process was stalled; investigate scheduling, GC, or page-fault stalls |
| `EdgeAlreadyClaimed` | two nodes configured to publish one edge | a genuine configuration error — `doctor` names both PIDs |
| `SlotContended` / `SlotRecycled` | reader starved, or ring too shallow for the publish rate | increase edge capacity; `doctor` warns at 80% occupancy |
| `TopologyChurn` | topology mutated ≥ 4 times during one plan compilation | almost certainly a bug: topology should be near-static after startup |
| `SIGBUS` in a lookup | **structurally impossible with sealing (§3.2)** | if it ever happens, the segment was not sealed — file a bug |

---

## 14. Phase 3 handoff — constraints you must not break

Phase 3 binds Python directly to the Rust core. Three Phase 2 properties must be preserved or Python users will hit them hard:

1. **`fork` safety.** `multiprocessing` defaults to `fork` on Linux. `MADV_DONTFORK` means the child's mapping is gone and any inherited handle is a fault waiting to happen. Phase 3 must register an `os.register_at_fork(after_in_child=...)` hook that poisons every inherited `Tree` handle so the child gets a clear Python exception rather than a segfault.
2. **GIL and liveness.** The socket carries liveness, not the heartbeat, so a long GIL-held pause does not risk reaping. Preserve that: do not add heartbeat-based reaping to make Python "safer" — it would make it strictly less safe (§6.4).
3. **Read-only by default.** The Python `attach()` default must be `ReadOnly`. Most Python consumers are analysis and visualization tools; they should be incapable of corrupting a robot's transform tree, and the default is what determines whether that is true in practice.

Write these into `docs/PHASE3.md` as you finish, alongside the measured numbers from §12.

---

## 15. Definition of done

- [ ] Amendments A1–A8 applied to Phase 1; all Phase 1 tests still pass unchanged
- [ ] `FORMAT_VERSION` bumped if Phase 1 had already been frozen, with a documented compatibility table
- [ ] Diff in `tf_tree_core`'s read path against Phase 1: **zero lines**
- [ ] `tf_tree_core` dependency list unchanged (D14)
- [ ] All §11.2 integration scenarios pass in CI on x86-64 **and aarch64**
- [ ] Every §11.3 crash point has a test proving recovery
- [ ] `shm_torture` runs 30 minutes nightly, clean, under ASan
- [ ] `HeapArena` / `MappedArena` replay produces **bit-identical** results (§10)
- [ ] §12.3 gate met, or a written explanation of which criterion failed and by how much
- [ ] `tf_treed` ships with a systemd unit and a container example
- [ ] `docs/RUNBOOK.md` complete; every row maps to a `doctor` check
- [ ] `docs/PHASE3.md` written, carrying §14 forward with the measured numbers

---

## Appendix A — implementation order

Steps 1–3 are the phase. Everything after them is comparatively mechanical.

1. **Amendments A1–A8 against `HeapArena`**, with the loom tests. Do this before touching a single syscall. Every one of these is a correctness fix that is testable single-process, and finding a bug here after the IPC layer exists costs ten times as much to diagnose.
2. **`MappedArena` + attach protocol.** Owner and attacher, sealing, `SCM_RIGHTS`, header validation. Assert the zero-line-diff property in the read path.
3. **Participant registry, liveness, reaping**, with the crash-point harness built alongside — not after.
4. `tf_tree_record` and the bit-identical replay test.
5. `tf_treed`.
6. `doctor` / `top` / `participants` extensions.
7. `/tf` ingest bridge. **Note the impedance mismatch:** ROS permits any number of publishers per edge; `tf_tree` permits one. The bridge must claim each edge on first sight and adopt a documented, configurable policy on conflict — `FirstWriterWins` (default, with a loud diagnostic naming both ROS publishers) or `LastWriterWins`. This mismatch is a feature, not a bug: it surfaces multi-publisher conflicts that `tf2` silently averages into garbage, and the bridge is where users will first discover their robot has one.
8. Benchmarks and the gate.

Do not proceed past step 3 until every §11.3 crash point recovers. Everything after it assumes crash-consistency, and a violation will present as an impossible numerical result somewhere in step 7.

## Appendix B — kernel behaviour probe

Verified on Linux 6.18. Re-run on the target kernel; the sealing results in §3.2 are load-bearing.

```c
#define _GNU_SOURCE
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>
#include <fcntl.h>
#include <stdio.h>

int main(void) {
    int fd = syscall(SYS_memfd_create, "tf_tree.probe",
                     MFD_CLOEXEC | MFD_ALLOW_SEALING);
    ftruncate(fd, 1 << 20);
    void *p = mmap(NULL, 1 << 20, PROT_READ | PROT_WRITE,
                   MAP_SHARED | MAP_POPULATE, fd, 0);

    printf("seal SHRINK|GROW w/ writable map: %d (expect 0)\n",
           fcntl(fd, F_ADD_SEALS, F_SEAL_SHRINK | F_SEAL_GROW));
    printf("seal WRITE w/ writable map:       %d (expect -1 EBUSY)\n",
           fcntl(fd, F_ADD_SEALS, F_SEAL_WRITE));
    printf("ftruncate shrink after seal:      %d (expect -1 EPERM)\n",
           ftruncate(fd, 1 << 10));
    printf("seal SEAL:                        %d (expect 0)\n",
           fcntl(fd, F_ADD_SEALS, F_SEAL_SEAL));
    printf("F_GET_SEALS:                      0x%x (expect 0x7)\n",
           fcntl(fd, F_GET_SEALS));
    printf("MADV_DONTFORK:                    %d (expect 0)\n",
           madvise(p, 1 << 20, MADV_DONTFORK));
    printf("MADV_HUGEPAGE:                    %d (expect 0)\n",
           madvise(p, 1 << 20, MADV_HUGEPAGE));
    return 0;
}
```

**The `/proc` parsing trap (§5.1), as a test fixture.** For a process whose `comm` is `evil) proc`, the naive whitespace split returns field 12's value where field 22 was intended:

```
raw    = "1234 (evil) proc) S 1 1234 1234 0 -1 4194304 1 2 3 ... 39"
naive  : raw.split()[21]                       -> 12    WRONG
robust : raw[raw.rindex(')')+2:].split()[19]   -> 13    correct
```
