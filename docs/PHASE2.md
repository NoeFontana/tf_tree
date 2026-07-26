# tf_tree — Phase 2 Implementation Specification: Shared Memory

> **Companion documents:** `docs/PROJECT.md` (vision, roadmap, decision log) and `docs/PHASE1.md` (single-process core). Read §1 of this document before writing any Phase 2 code — it contains mandatory amendments to the Phase 1 design that were discovered by working the multi-process failure modes through.

**Deliverable:** the same arena, mapped into N processes, with the *identical unmodified* reader code from Phase 1 running against it. Plus the lifecycle, liveness, and fault-tolerance machinery that makes that safe when processes die at arbitrary points.

**Framing.** Phase 1 was the easy half. In one process, a crash takes down every reader with it, so a torn write is unobservable. Across processes it is not: a writer can be `SIGKILL`ed between two stores and leave a data structure permanently wedged while sixteen readers keep running. **Every mutation protocol in the arena must therefore be crash-consistent — there must be no state a dead process can leave behind that a live process cannot detect and repair.** That single requirement drives most of this document.

Sections marked **NORMATIVE** are requirements. Where a syscall behaviour is asserted, it has been verified on Linux 6.18; the probe is reproduced in Appendix B so you can re-run it on your target kernel.

---

## 0.0 Implementation status

**Partially implemented.** The crash-consistency amendments and the mapping are done; the rendezvous and lifecycle are not.

| Area | Status |
|---|---|
| Amendments A1–A7 (§1) | **Applied** — `FORMAT_VERSION` 2 |
| `MappedArena` — `memfd`, sealed, `MAP_SHARED`, `MADV_DONTFORK`/`HUGEPAGE` (§4) | **Done** (`tf_tree_arena::mapped`, behind `--features shm`) |
| `TreeBuilder::build_shared` / `Tree::attach_shared`, read-only mode (§8) | **Done** |
| Zero-diff read path, proven by the relocation gate (§4) | **Done, and tested** (`just shm-test`) |
| Multi-process read scaling (part of §12.2) | **Done** (`just shm-scaling`; results in `docs/benchmarks/tf2.md`) |
| Amendment A2 — in-arena topology lock | **Applied** — `Tree::reparent` holds it; bounded spin, liveness-gated steal, loom- and multi-process-tested |
| Amendment A8 — bounded intern spin | **Applied** — `claiming` array, bounded spin, takeover of a dead claimant (`layout_hash` 0x9075_90F5) |
| Discovery, rendezvous, `open()`, ownership migration (§3) | Not implemented — fd inheritance stands in |
| Attach protocol — `SOCK_SEQPACKET` + `SCM_RIGHTS` (§3.7) | Not implemented |
| Claims as OFD locks (§6.1); reaping (§6.3) | Not implemented — `ClaimRecord` CAS only |
| Per-edge page population (§7.1) | **Not implemented — currently maps `MAP_POPULATE`, which §7.1 forbids** |
| `instance_uuid` (§3.6 step 4, A7) | Not implemented |
| Participant registry — owner-side slot assignment (§5) | Table exists (A6); slots are self-assigned by CAS, not by the owner |
| `tf_treed`, `tf_tree_record`, `/tf` ingest, diagnostics (§9, §10) | Not implemented |
| Fault injection, `shm_torture` (§11.3, §11.4) | Not implemented |

**What the gap means.** N processes map one arena, read it with byte-identical
results, and see each other's writes, at no per-lookup cost over the
single-process path. The amendments close the crash-consistency holes that made
that unsafe: a killed writer no longer leaks an edge (A3), inverts a slot's
parity (A5), wedges every reader with a permanently odd generation (A1), or
wedges every other mutator by dying mid-topology-mutation (A2).

What is missing is the **rendezvous and lifecycle** half — how processes find
each other without configuration, and who cleans up after a death. Three
consequences are live today:

* Segments are handed over by **fd inheritance**, so only a child of the creator
  can attach. There is no late join, no restart-and-reattach, and no `open()`.
* Nothing reaps. A participant that dies holding a claim leaves it held until the
  arena is destroyed, because §6.1's kernel lock — the thing that would make the
  death observable — is not there yet.
* **Liveness is a `/proc` heuristic, not a kernel fact.** A2's lock steals from a
  dead holder, and it asks *something* whether the holder is dead. §5.1 says the
  answer must come from the participant's OFD lock byte; that file does not exist
  yet, so the `tf_tree` facade supplies §6.2's `(pid, start_time, boot_id)` check
  instead. The lock itself takes the answer as an **injected predicate**
  (`TopoLockView::acquire`'s `is_alive`) and `tf_tree_core` never learns how it is
  reached — §2 forbids it the dependency — so replacing the heuristic with
  `F_OFD_GETLK` is a change to one function in `tf_tree::tree` and to nothing
  else. Until then the predicate fails **safe** in every branch: an unreadable
  `/proc` reports "alive", so the worst case is a caller that retries, never a
  live mutator that is stolen from.

**§7.1 and the code disagree, deliberately and temporarily.** `MappedArena` maps
`MAP_POPULATE`, which was correct against the previous draft and is forbidden by
this one. It is harmless at the current arena sizes (~1.3 MiB) and becomes a real
problem the moment §3.8's generous default layout lands, because it would fault
in and charge hundreds of megabytes nobody declared. Fix it with §3.8, not before
— the two changes are one change.

## 0. Scope

### In scope

| | |
|---|---|
| `MappedArena` | memfd-backed, sealed, `MAP_SHARED` |
| Discovery & rendezvous | zero-config `open()`; runtime dir as the sharing boundary; kernel file locks as the election |
| Attach protocol | `SOCK_SEQPACKET` + `SCM_RIGHTS`, with version and layout negotiation |
| Ownership migration | owner death is inherited by a surviving participant; lookups never pause |
| Participant registry | in-arena advisory records; OFD locks authoritative for liveness |
| Liveness and reaping | claims as kernel locks — no heartbeats, no `/proc`, no zombie window |
| Crash-consistency | every arena mutation protocol audited and repaired (§1) |
| Read-only attach | `PROT_READ` mapping as a real safety boundary |
| Mapping policy | per-edge population, `MADV_HUGEPAGE`, `MADV_DONTFORK`, optional `mlock` |
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
| Multi-arena federation on one host | One arena per `(runtime_dir, domain, name)`. Distinct triples are fully independent (§3.1). |
| Any security boundary against a malicious RW peer | §3.10. Say this out loud in the docs. |
| Dynamic arena resize | D4. Still forbidden. Capacity is planned, not grown. |

### Trust model

See §3.10. Summary: mutually trusting same-user processes; read-only attach is the only real boundary; crash and hang are both non-corrupting.

---

## 1. Phase 1 amendments — NORMATIVE, apply before Phase 1 is frozen

`PROJECT.md` states that if Phase 2 requires changes outside `tf_tree_arena`, the Phase 1 design was wrong. Working through the crash matrix found eight places where it was. Seven are cheap; one (A6) changes the arena layout. A8's motivating crash point is described in §11.3; the amendment itself is specified below with the rest.

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

**§6.1 supersedes the reaping role of this record.** Claims are held as kernel file locks and the lock is authoritative; `ClaimRecord` is retained for diagnostics and for readers asking who publishes an edge. The slot indirection is still required, because a record written after a partial CAS must never be mistaken for a valid identity.

---

### A4 — `push` must verify the claim epoch

**The zombie writer.** A process `SIGSTOP`ped, or stalled in a GC pause or on a page fault against a slow device, can be judged dead, have its claim reaped, and then *resume* and continue pushing to an edge another process now owns. Two writers, silent corruption — precisely the failure the claim model exists to prevent.

**Fix.** One relaxed load per push, on a cacheline the writer already touches:

```rust
if self.claim.epoch.load(Ordering::Relaxed) != self.epoch {
    return Err(PushError::ClaimRevoked { edge: self.id });
}
```

Cost is ~1 ns. **§6.1 changes why this exists:** with claims held as kernel locks, a stalled writer keeps its lock and cannot be reaped, so the zombie is impossible by construction. Keep the check anyway as defence in depth against a bug in the `ClaimRecord` path — but do not describe it in comments as the sole barrier, or someone will delete it after reading §6.1.

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

`instance_uuid: [u8; 16]` joins them (§3.6 step 4). It is what makes a split-brain detectable after the fact: two processes that believe they share an arena but print different `instance_uuid`s are on different arenas, and no other field distinguishes them.

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

**NORMATIVE.** Shared memory is **Linux-only** in Phase 2, requiring **kernel ≥ 3.17** for `memfd_create` and `F_ADD_SEALS`, and **≥ 3.15** for OFD locks (§3.3). Target and test on 5.15 (Ubuntu 22.04 / JetPack 6) and current stable. `MADV_POPULATE_WRITE` (§7.1) needs ≥ 5.14 and has a documented fallback.

Do not build a POSIX abstraction layer. macOS and Windows keep `HeapArena` and in-process operation; a file-backed unsealed `MappedArena` for macOS developer ergonomics is acceptable *later*, explicitly labelled dev-only, and must not shape the Linux design.

| Crate | New dependencies |
|---|---|
| `tf_tree_arena` | `rustix` (feature `shm`, `mm`, `fs`, `net`) — no libc crate, no C build step |
| `tf_tree_ipc` (new) | `rustix`, **and `libc` for `fcntl(F_OFD_*)` only**. Attach protocol, participant registry, reaping. |
| `tf_tree_record` (new) | `mcap`, `serde` — **isolated here specifically so D14 holds for the core** |
| `tf_tree_core` | **none.** Unchanged. |

`tf_tree_core` gaining a dependency in this phase is a design failure, not a tradeoff. If you find yourself needing one, stop and report it.

**The `libc` exception, recorded rather than hidden.** This section originally said "no libc crate". `rustix` 1.1 turned out to have **no OFD locking at all** — its `fcntl_lock` is the classic, whole-file `F_SETLK` that §3.3 rejects by name, and `flock` is whole-file too. The alternatives were to hand-roll the `fcntl` syscall or to take `libc`. Hand-rolling was implemented first and then rejected on review: it pinned syscall numbers and `struct flock`'s layout by hand and refused to compile on any architecture except x86-64 and aarch64 — including riscv64 and ppc64le. A hand-maintained kernel ABI underneath the primitive the whole rendezvous depends on is a worse risk than one more dependency, and `libc` introduces **no C build step**, which is what this rule was protecting against. Scope it to `tf_tree_ipc` and to that one call.

---

## 3. Discovery, rendezvous, and ownership

**This is the seam that makes everything else usable.** A process calls `tf_tree::open()` and either joins the arena that already exists on this machine or creates it. No configuration file, no daemon, no start-order requirement, and no possibility of two processes silently ending up on different arenas.

The design principle: **do not implement leader election — borrow the kernel's.** A rendezvous needs exactly three properties: mutual exclusion, automatic release when the holder dies, and a way to ask whether anyone holds it. Linux file locks provide all three, maintained by the kernel, with no timeouts, no heartbeats, and no stale state that can survive a `SIGKILL`. Every distributed-consensus flavoured problem in this section dissolves into one `fcntl` call.

### 3.1 The sharing boundary is the runtime directory — NORMATIVE

Two processes share an arena **if and only if they resolve to the same runtime directory, domain, and name.** That is the whole mental model, and it should be the first sentence of the user-facing documentation.

```
<runtime_dir>/<domain>/<name>.lock     # rendezvous + kernel-managed liveness
<runtime_dir>/<domain>/<name>.sock     # SOCK_SEQPACKET, owner-bound, FD passing
```

Resolution order for `runtime_dir`, first hit wins:

1. `$TF_TREE_RUNTIME_DIR`
2. `$XDG_RUNTIME_DIR/tf_tree` (normally `/run/user/<uid>/tf_tree`, tmpfs, per-user, cleaned on logout)
3. `/run/tf_tree` if writable (system services)
4. `/tmp/tf_tree-<uid>`, created mode `0700`

**Containers:** sharing the runtime directory is a volume mount (`-v /run/tf_tree:/run/tf_tree`), and not sharing it is complete isolation. This is deliberately the same idiom people already use for X11 and D-Bus sockets, and it means the isolation boundary is inspectable with `ls` rather than being an implicit property of a namespace. Do **not** use abstract Unix sockets, which would tie the boundary to the network namespace — an invisible, surprising, and usually wrong place to put it.

**NORMATIVE check:** `statfs` the runtime directory at open and reject NFS (`0x6969`) and CIFS. File locks over network filesystems have subtly different semantics and the entire rendezvous depends on them being exact.

### 3.2 Identity and defaults

```
domain: $TF_TREE_DOMAIN, else $ROS_DOMAIN_ID, else 0
name:   $TF_TREE_NAME,   else "default"
```

Falling back to `ROS_DOMAIN_ID` is deliberate: a ROS 2 system already has its isolation configured, and inheriting it means `tf_tree` partitions exactly the way the rest of the stack does with no additional setup. Two robots on one bench, or a simulator alongside hardware, stay separated because they were already separated.

The zero-argument case must work:

```rust
let tree = tf_tree::open()?;                 // join or create, defaults throughout
let tree = tf_tree::open_named("robot")?;

let tree = tf_tree::Open::new()
    .domain(7)
    .name("robot")
    .mode(AttachMode::ReadOnly)              // default for consumers (§8)
    .layout_if_creating(layout)              // used only if we turn out to be the creator
    .create(CreatePolicy::IfAbsent)          // IfAbsent | Never | Always
    .open()?;
```

`CreatePolicy::Never` is for consumers that should fail fast rather than accidentally create an empty arena when the estimator has not started — worth recommending for anything running in a supervised deployment.

### 3.3 The lock file — NORMATIVE

A small regular file used as a lock substrate with **open file description locks** (`F_OFD_SETLK`, Linux ≥ 3.15). OFD locks, not classic POSIX `F_SETLK`, because classic locks are dropped when *any* file descriptor to the file is closed anywhere in the process — an unfixable footgun for a library that shares an address space with code it does not control.

| Offset | Meaning |
|---|---|
| byte 0 | **Ownership.** Exclusive. The holder serves the socket. |
| bytes 1–15 | reserved |
| bytes 16 + *i* | **Participant liveness** for slot *i*. Exclusive, held for the lifetime of the attachment. |
| 4096 + 64·*i* | **Identity record** for slot *i*: pid, start_time, boot_id, mode, name. Written with `pwrite` before taking the slot lock. Advisory; diagnostics only. |

Verified behaviour on Linux 6.18:

| Operation | Result |
|---|---|
| Second process takes a held byte | `EAGAIN` |
| Holder dies without unlocking | lock released by the kernel, immediately |
| `F_OFD_GETLK` on a free byte | `l_type = F_UNLCK` |
| `F_OFD_GETLK` on a held byte | held, but **`l_pid = -1`** |

**That last row matters.** An OFD lock belongs to an open file description, not a process, so `GETLK` cannot report a PID. The lock file therefore answers *"is anyone alive?"* — which is all the rendezvous needs — while *"who?"* comes from the identity records, which is why they exist as plain `pwrite` data rather than living only in the arena. A process that cannot reach the arena can still run `tf_tree doctor` and get names and PIDs.

Because liveness is now a kernel fact rather than an inference, **`/proc` parsing and PID-reuse defence are no longer on the rendezvous path at all.** They remain only for the arena's advisory participant table (§5).

### 3.4 `open()` — NORMATIVE algorithm

```
deadline = now + open_timeout (default 5 s)
loop {
    // 1. Someone is already serving. Join.
    if connect(sock) succeeds {
        Hello handshake -> recv arena fd -> validate -> mmap
        pwrite identity record; F_OFD_SETLK participant byte
        return Joined
    }

    // 2. Nobody is serving. Try to become the owner.
    if F_OFD_SETLK(byte 0, exclusive) fails {
        // another process is mid-bind; it will be serving shortly
        backoff; continue
    }

    // 3. We hold ownership. Do we already have an arena?
    if we hold an arena fd (we are an existing participant taking over) {
        goto 5                              // reuse it -- never create a second one
    }

    // 4. SPLIT-BRAIN CHECK. Is any participant byte locked?
    if any participant byte is held {
        // an arena exists and is alive, but its holder has not taken over yet.
        release byte 0; backoff; continue    // yield to the real participant
    }

    // 5. Serve.
    if creating { memfd_create; ftruncate; mmap; init header; seal (§3.6) }
    unlink stale sock; bind sock.tmp; chmod; rename -> sock; listen
    pwrite identity; F_OFD_SETLK our participant byte
    return Created | TookOver
}
on timeout -> Err(ArenaHeldButUnreachable { holder_slots, identities })
```

**Step 4 is the whole design.** Without it, this sequence is possible: the owner dies; a fresh process starts, finds no socket, wins the ownership lock before any surviving participant notices the `HUP`, and creates a *second* arena. The surviving participants keep using the first. Two arenas, both live, silently diverging — worse than any failure to start, because nothing reports an error and the robot's transform tree is quietly inconsistent between nodes.

The check is **deterministic, not a grace period.** If any participant byte is locked, a live arena exists; a fresh process must not create one, full stop. No timing assumption, no window to tune.

The timeout case is also correct behaviour rather than a limitation. If a participant is `SIGSTOP`ped and never takes over, no new process can join — and that is the right answer, because the alternative is divergence. The error names the stuck slots and their identities, so an operator can see exactly what to kill. Provide `--force-new` as an explicit, loud escape hatch that abandons the existing arena; never take that path automatically.

### 3.5 Ownership migrates; the data plane never pauses — NORMATIVE

Ownership is a **role**, not a property of the arena. The arena is the memfd, which lives as long as any mapping does; the owner is merely whichever participant currently holds byte 0 and the listening socket.

When a participant's client socket reports `HUP`:

```
keep serving lookups from the existing mapping -- untouched, uninterrupted
try F_OFD_SETLK(byte 0)
  acquired -> unlink stale sock; bind; listen; serve OUR existing fd
  contended -> another participant is taking over; retry connect with backoff
```

**Lookups do not stop, slow down, or observe anything during a takeover.** `Plan::at` touches the mapping and nothing else; ownership lives entirely in the control plane. State this explicitly in the docs, because "what happens when the owner dies" is the first question any integrator will ask, and the answer — *nothing observable* — is a strong one.

This supersedes the previous draft's "ownership is configured, not negotiated". Ownership is neither configured nor negotiated: it is *inherited*, and the kernel picks the heir.

### 3.6 Creation sequence — verified on Linux 6.18

```
1. memfd_create("tf_tree.<domain>.<name>", MFD_CLOEXEC | MFD_ALLOW_SEALING)
2. ftruncate(fd, arena_size)
3. mmap(NULL, arena_size, PROT_READ|PROT_WRITE, MAP_SHARED, fd, 0)     // NOT MAP_POPULATE (§7.1)
4. initialize header: magic, format_version, layout_hash, arena_size, instance_uuid, boot_id
5. fcntl(fd, F_ADD_SEALS, F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_SEAL)
6. madvise(base, len, MADV_DONTFORK)      // §7.3 -- must precede any fork
7. madvise(base, len, MADV_HUGEPAGE)      // best-effort
```

Step 5 is load-bearing:

| Operation | Result |
|---|---|
| `F_ADD_SEALS SHRINK\|GROW` while a writable mapping is held | **succeeds** — the owner keeps write access |
| `F_ADD_SEALS WRITE` while a writable mapping is held | `EBUSY` — cannot over-seal by accident |
| `ftruncate` shrink after sealing | `EPERM` |
| `F_ADD_SEALS` anything after `F_SEAL_SEAL` | `EPERM` |
| `F_GET_SEALS` | `0x7` |

**Sealing against shrink is what makes `SIGBUS` structurally impossible.** Without it any holder of the fd could truncate the segment and every reader touching a truncated page would fault inside a lookup — unrecoverable, in the middle of a control loop, with nothing a library can do about it. Do not skip step 5, and do not substitute `shm_open`, which cannot be sealed and additionally leaves stale segments in `/dev/shm` after a crash.

### 3.7 Attach

```
1. connect SOCK_SEQPACKET
2. send HelloRequest
3. recvmsg -> HelloResponse + SCM_RIGHTS fd   (or a rejection carrying no fd)
4. fstat(fd).st_size == response.arena_size
   F_GET_SEALS & (F_SEAL_SHRINK|F_SEAL_GROW) == both     // refuse an unsealed segment
5. mmap(PROT_READ [| PROT_WRITE], MAP_SHARED)
6. verify header magic, format_version, layout_hash, arena_size, boot_id
7. madvise DONTFORK, HUGEPAGE
8. pwrite identity record; F_OFD_SETLK participant byte
9. KEEP THE SOCKET OPEN for the lifetime of the attachment
```

**Step 9:** the socket is not a handshake channel to be closed after use — it is how a participant learns the *owner* has died, in microseconds, with no polling. Participant death is detected by the lock file; owner death is detected by the socket. Both are kernel-maintained; neither involves a timeout.

Message structs are fixed-size `#[repr(C)]`, little-endian, over `SOCK_SEQPACKET` so framing comes from the kernel:

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
    pub client_start_time: u64,
    pub client_boot_id: [u8; 16],
    pub client_name: [u8; 32],
}

#[repr(C)]
pub struct HelloResponse {
    pub magic: [u8; 8],
    pub status: u32,               // 0 = Ok
    pub format_version: u32,
    pub layout_hash: u32,
    pub participant_slot: u32,     // matches the lock-file byte the client must take
    pub arena_size: u64,
    pub instance_uuid: [u8; 16],
    pub owner_pid: u32,
    _pad: u32,
}
```

Rejections: `VersionMismatch`, `LayoutMismatch`, `BootIdMismatch`, `NoParticipantSlots`, `ModeNotPermitted`, `Malformed`. Each must name both sides' values.

`LayoutMismatch` is the one operators will hit — a binary built against a different struct layout. **The message must say exactly that** and print both hashes, because the raw symptom is attach failing on a machine where everything looks fine, which is otherwise a multi-hour debugging session. This is why `layout_hash` was computed in Phase 1 despite nothing reading it then.

### 3.8 Capacity without planning — NORMATIVE

Fixed capacity (D4) is in tension with zero-configuration startup: whoever creates the arena fixes the layout, and a process that joins later cannot grow it. Resolving that tension by making users plan capacity would destroy the seamlessness this section exists to provide.

**The resolution is that virtual capacity is nearly free.** Measured on Linux 6.18 with a 1 GiB memfd:

| State | Pages charged |
|---|---|
| After `ftruncate` to 1 GiB | **0 KiB** |
| After `mmap` without `MAP_POPULATE` | **0 KiB** |
| After touching 16 MiB | exactly 16 MiB |

So the default layout is **generous** — 1024 frames, 1024 edges, 8192 samples per edge, roughly 600 MiB of address space — and resident cost is only what is actually declared and used. A robot with 24 frames and 4 dynamic edges pays for 24 frames and 4 dynamic edges.

**Per-edge population, not whole-arena population.** `MADV_WILLNEED` does *not* pre-fault a memfd region (measured: no change in charged pages). Use `madvise(MADV_POPULATE_WRITE)` (Linux ≥ 5.14, measured working) on an edge's stamp and pose ranges at `declare_dynamic` time, falling back to an explicit zeroing write on older kernels. This moves faulting to declaration — startup, where it belongs — instead of into the first lookup, without paying for capacity nobody declared.

`doctor` warns at 80% occupancy of frames, edges, or participants, and the error on exhaustion (`ArenaFull`) must state the current limit and that raising it requires recreating the arena.

### 3.9 Teardown

- **A participant dies** → its lock byte releases, its mapping drops, and the owner reaps its arena-side records (§6). Nothing else notices.
- **The owner dies** → surviving participants take over (§3.5). Lookups never pause.
- **The last mapping drops** → the kernel frees the segment. **No stale segments, ever** — the second reason to prefer memfd over `shm_open`.
- A stale **socket path** may persist; it is unlinked by whoever wins ownership. A stale **lock file** is harmless: it holds no state, only locks, and locks cannot be stale.

### 3.10 Trust model — NORMATIVE, and state it in the public docs

Participants are **mutually trusting, same-user, cooperating processes**. A read-write participant can corrupt any part of the arena, and no checksum changes that. What the design does guarantee:

- A **read-only** participant cannot corrupt anything, enforced by the MMU (§8). This is the only real boundary, and it is the default.
- A participant that **crashes**, at any instruction, cannot corrupt anything or wedge any other participant. Hard requirement, tested by fault injection (§11.3).
- A participant that **hangs** cannot corrupt anything and cannot be mistaken for a crashed one (§6).

"Shared memory IPC is not a sandbox" belongs in the README.

---

## 4. `MappedArena`

**NORMATIVE:** the diff against Phase 1 outside `tf_tree_arena` and the new `tf_tree_ipc` crate must be **zero lines in the read path**. `PoseSlot`, `EdgeBuffer`, `Plan::at`, bracket search, and interning are byte-identical code operating on a different base pointer.

**The premise is tested, not merely asserted.** "A different base pointer" only works if nothing in the arena is an absolute address, which Phase 1 documented but never checked. `crates/tf_tree_bench/tests/relocation.rs` is that check: it byte-copies a populated arena to a different address, wraps the copy in a minimal `Arena` impl, and requires **bit-identical** results across every frame pair in the fixture, plus frame-name resolution and header validation. Keep it green: a regression that caches one resolved address would otherwise surface for the first time in another process, as a wild read rather than an error.

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

### 5.1 Identity is advisory; the lock file is authoritative — NORMATIVE

**Liveness comes from the participant's OFD lock byte (§3.3), never from these records.** A `ParticipantRecord` describes *who* a slot belongs to; whether it is live is a kernel fact. Any code deciding liveness from `state` or `heartbeat` is a bug.

`(pid, start_time, boot_id)` remains the identity triple for diagnostics and for the `--force-new` path. A bare PID is not an identity: PIDs are recycled, and on an embedded system with a low `pid_max` they recycle fast.

`start_time` is field 22 of `/proc/<pid>/stat`, in clock ticks since boot. It is still parsed — carefully — because `doctor` reports it and the takeover path prints it, but it is no longer on any correctness-critical path.

**The parsing trap — NORMATIVE.** Field 2 is `comm`, the executable name in parentheses, and it may contain spaces *and parentheses*. Splitting the line on whitespace and taking index 21 is wrong and will silently return a different field for any process whose name contains `) (`. Always locate the **last** `)` in the line and parse fields from there:

```rust
let rp = raw.rfind(')').ok_or(ProcParseError)?;
let field22 = raw[rp + 2..].split_ascii_whitespace().nth(19).ok_or(ProcParseError)?;
```

A demonstration of the naive parse returning the wrong value is in Appendix B. Include that exact case as a unit test against a fixture string — you cannot easily create a process with such a name, but you can test the parser.

---

## 6. Liveness and reaping

### 6.1 Claims are kernel locks — NORMATIVE

A5's parity fix, A3's slot indirection, and A4's epoch check were all built to answer one question: *how do you tell a dead writer from a slow one, without ever getting it wrong?* The rendezvous design answers it outright, so claims move to the same primitive.

**`claim(edge)` takes an exclusive OFD lock on `CLAIM_BASE + edge_id` in the lock file**, held for the life of the `Publisher`. Consequences:

| Previously | Now |
|---|---|
| heartbeat freshness heuristics | none — the lock is the liveness |
| `/proc` liveness checks, PID-reuse defence | none on this path |
| reaping algorithm with epoch ordering | `F_OFD_GETLK` says free ⇒ definitively dead |
| the zombie writer (§A4) | **impossible by construction** |

The zombie case is worth stating plainly, because it was the nastiest hazard in the previous draft. A `SIGSTOP`ped or GC-stalled writer **still holds its kernel lock**, so it cannot be reaped while alive, and another process attempting to claim the edge gets `EdgeAlreadyClaimed` rather than silently becoming a second writer. There is no window, no timeout to tune, and no heuristic that can be wrong. A heuristic that is wrong once in a thousand hours is exactly the kind of bug that ships.

One syscall per claim, on a path that runs at startup. Free.

**Two sources of truth, one authoritative.** The arena's `ClaimRecord` remains for diagnostics and for readers asking who publishes an edge, but **the lock file is authoritative**. Claim = take the lock, then write the record. Reap = if the lock is free and the record says held, clear the record. The record may lag; the lock never does. Any code that makes a decision from `ClaimRecord` alone is a bug.

**A4 is retained but downgraded.** The epoch check in `push` is now defence in depth against a bug in the record path rather than the sole barrier against a zombie. Keep it — one relaxed load — and update its comment so nobody removes it believing it was only there for the zombie case.

### 6.2 Fork is still the exception — NORMATIVE

OFD locks are held by the open file description, which **survives `fork` and is shared with the child**. Parent and child therefore both "hold" every claim, and both would pass A4's epoch check.

`MADV_DONTFORK` (§7.3) is what closes this: the child has no mapping, so it faults immediately and loudly rather than corrupting quietly. `MADV_DONTFORK` and OFD claims are a matched pair — neither is safe without the other, and a comment at each site must say so.

### 6.3 What remains of reaping

Arena-side cleanup after a death, performed by any read-write participant, all steps idempotent:

```
for each edge whose ClaimRecord says held:
    if F_OFD_GETLK(CLAIM_BASE + edge) reports free {      // holder is definitively dead
        claim.epoch.fetch_add(1, AcqRel);                  // fence a buggy Publisher
        normalize_slot_parity(edge, head & mask);          // A5 repair
        claim.owner.compare_exchange(stale, 0, ...);       // racing reapers are harmless
    }
for each participant slot whose record is populated:
    if its lock byte is free { clear the record }
```

The owner runs this on socket `HUP`; others run it lazily when a claim appears held. **Reaping must not be owner-only** — an owner-only design leaks every claim held at the moment the owner died.

### 6.4 Heartbeats are diagnostics only — NORMATIVE

`heartbeat` and `last_push_nanos` remain in `ClaimRecord`, bumped on every push, and are **never** a reaping trigger. They detect the *hang* case — a live process that has stopped publishing — which `doctor` reports and an operator resolves.

Reaping on staleness would be actively unsafe: an edge legitimately published at 0.2 Hz, such as a map-to-odom correction from a slow global localizer, is indistinguishable from a hung writer under any timeout short enough to be useful. With claims as kernel locks there is no reason to offer such a policy at all, so **do not add one**, not even opt-in.

---

## 7. Mapping policy

### 7.1 Page population is per-edge, not per-arena — NORMATIVE

A minor page fault costs single-digit microseconds. The Phase 1 gate is a **150 ns p50, with a p99.9 that matters more** — one fault in the lookup path blows that budget by two orders of magnitude.

But §3.8 makes the default layout deliberately generous so that zero-configuration startup works, and `MAP_POPULATE` over a 600 MiB address space would fault in — and charge — hundreds of megabytes nobody declared. The two requirements are reconciled by populating at **declaration** granularity:

- `mmap` **without** `MAP_POPULATE`. Untouched regions of a memfd cost nothing (measured, §3.8).
- At `declare_dynamic`, `madvise(MADV_POPULATE_WRITE)` (Linux ≥ 5.14, measured working) over that edge's stamp and pose ranges. On older kernels, fall back to an explicit zeroing write.
- On attach, populate the header, frame table, topology blocks, and edge table — small, always touched, always hot.

**`MADV_WILLNEED` does not work here** (measured: zero change in charged pages on a memfd). Do not substitute it.

§12 requires benchmark rows for first-access-after-attach with per-edge population on and off, because it is the clearest demonstration of why this exists and it stops someone removing it to speed up startup.

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
7. **Thundering herd:** 32 processes call `open()` simultaneously with no arena present. Exactly one creates; 31 join; all 32 see the same `instance_uuid`.
8. **Ownership migration:** kill the owner mid-run. A surviving participant takes over; a new process joins the *same* arena (identical `instance_uuid`); and a reader thread running throughout observes zero failed lookups and no latency excursion beyond its steady-state p99.9.
9. **Split-brain attempt:** kill the owner, and immediately — before any participant can notice the `HUP` — start a fresh process. It must block on §3.4 step 4 and then join the existing arena. **Two distinct `instance_uuid`s on one `(runtime_dir, domain, name)` is a hard test failure.** Run this one a thousand times in a loop; it is the single most important race in the phase.
10. **Stuck participant:** `SIGSTOP` the only participant, then `open()` from a fresh process. Must fail with `ArenaHeldButUnreachable` naming the stuck slot — never create a second arena. `SIGCONT`, then confirm a subsequent `open()` succeeds.
11. **Domain isolation:** two arenas under different domains, and two under different runtime dirs, never observe each other.

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
| `attach.after_slot_assigned_before_publish` | participant slot in `ATTACHING`, lock byte never taken → record cleared by any reaper |
| `open.after_ownership_lock_before_bind` | ownership lock released by the kernel → the next `open()` proceeds; **no arena created twice** |
| `open.after_create_before_bind` | arena exists, nothing serving, no participant byte held → next `open()` finds nothing alive and creates fresh; the orphan memfd is freed with its last mapping |
| `takeover.after_ownership_lock_before_bind` | ownership released; another participant takes over; joiners retry |

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
| first access after attach, per-edge population on vs off | p99.9, both |
| THP `madvise` vs `never` | p50, p99.9, both |
| aggregate read throughput, 1→16 consumer processes | scaling curve |
| **CPU per consumer at 1 kHz × 20 edges, vs ROS 2 `/tf`** | %CPU per consumer, both |
| **total RSS across 16 consumers, vs ROS 2 `/tf`** | MB, both |
| publish → visible-to-consumer latency, vs ROS 2 `/tf` | p50, p99.9, both |
| `SIGKILL` writer → claim reapable → re-claimed | p50, p99 |
| attach time, cold and warm | p50 |
| `open()` when the arena exists vs when creating | p50, both |
| owner kill → new owner serving | p50, p99 |
| lookup latency across an ownership migration | p99.9 during vs steady-state |

### 12.3 The gate — NORMATIVE

Proceed to Phase 3 if:

1. **Cross-process depth-3 p50 within 10% of the in-process baseline, p99.9 within 25%.** This is the central claim of the phase: the same code, the same speed, in another process. If it fails, the mapping policy is wrong (§7), not the design.
2. **Aggregate read throughput scales ≥ 12× from 1 to 16 consumer processes.**
3. **Zero corrupt reads across the full `shm_torture` run**, and every §11.3 crash point recovers. Not negotiable; a single failure here means the arena is not crash-consistent and the phase is not done.
4. **Kill → re-claimable p99 under 10 ms.**
4b. **Ownership migration is invisible to the data plane:** lookup p99.9 during a migration within 5% of steady state, and zero failed lookups.
4c. **Scenario 9 of §11.2 passes 1000 consecutive runs with a single `instance_uuid`.**
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
| `SIGBUS` in a lookup | **structurally impossible with sealing (§3.6)** | if it ever happens, the segment was not sealed — file a bug |

---

## 14. Phase 3 handoff — constraints you must not break

> **Superseded in part by [`PHASE3.md`](./PHASE3.md) §1.** That document's §1.1
> corrects item 5 below: `abi3` alone is not a sufficient distribution target,
> because it does not work on free-threaded builds — and §1.2 adds the constraint
> that turned out to matter most, which this section missed entirely. Read
> `PHASE3.md` §1 before acting on items 4 or 5.

Phase 3 binds Python directly to the Rust core. Five Phase 2 properties must be preserved or Python users will hit them hard:

1. **`fork` safety.** `multiprocessing` defaults to `fork` on Linux. `MADV_DONTFORK` means the child's mapping is gone and any inherited handle is a fault waiting to happen. Phase 3 must register an `os.register_at_fork(after_in_child=...)` hook that poisons every inherited `Tree` handle so the child gets a clear Python exception rather than a segfault.
2. **GIL and liveness.** The socket carries liveness, not the heartbeat, so a long GIL-held pause does not risk reaping. Preserve that: do not add heartbeat-based reaping to make Python "safer" — it would make it strictly less safe (§6.4).
3. **Read-only by default.** The Python `attach()` default must be `ReadOnly`. Most Python consumers are analysis and visualization tools; they should be incapable of corrupting a robot's transform tree, and the default is what determines whether that is true in practice. Pair it with `CreatePolicy::Never` so a notebook started before the robot fails loudly instead of creating an empty arena that a later publisher then refuses to join.
4. **`tf_tree.open()` with no arguments must work in a notebook.** Zero-config discovery (§3) is most of the perceived quality of the Python binding; if a user has to pass paths, the seam has leaked.
5. **Distribution: `abi3` wheels via maturin.** ~~One wheel per platform.~~ **Corrected by `PHASE3.md` §1.1** — `abi3` does not cover free-threaded builds, so the matrix needs a version-specific `cp314t` wheel alongside it (built by a *second* maturin invocation, not a flag), and an `abi3.abi3t` job (PEP 803) for 3.15 onward. `cp313t` is not buildable on PyO3 0.29 and is deliberately not in the matrix.

Write these into `docs/PHASE3.md` as you finish, alongside the measured numbers from §12.

---

## 15. Definition of done

- [ ] Amendments A1–A8 applied to Phase 1; all Phase 1 tests still pass unchanged
- [ ] `FORMAT_VERSION` bumped if Phase 1 had already been frozen, with a documented compatibility table
- [ ] Diff in `tf_tree_core`'s read path against Phase 1: **zero lines**
- [ ] `tf_tree_core` dependency list unchanged (D14)
- [ ] All §11.2 integration scenarios pass in CI on x86-64 **and aarch64**
- [ ] Scenario 9 (split-brain) passes 1000 consecutive runs
- [ ] `tf_tree::open()` with no arguments joins-or-creates correctly from any start order
- [ ] `doctor` prints `instance_uuid` and the resolved runtime dir, and works without the arena
- [ ] Every §11.3 crash point has a test proving recovery
- [ ] `shm_torture` runs 30 minutes nightly, clean, under ASan
- [ ] `HeapArena` / `MappedArena` replay produces **bit-identical** results (§10)
- [ ] §12.3 gate met, or a written explanation of which criterion failed and by how much
- [ ] `tf_treed` ships with a systemd unit and a container example
- [x] `docs/RUNBOOK.md` complete; every row maps to a `doctor` check (rows for unimplemented Phase 2 errors are marked as such)
- [~] `docs/PHASE3.md` written and carrying §14 forward; the measured numbers land with §12

---

## Appendix A — implementation order

Steps 1–3 are the phase. Everything after them is comparatively mechanical.

1. **Amendments A1–A8 against `HeapArena`**, with the loom tests. Do this before touching a single syscall. Every one of these is a correctness fix that is testable single-process, and finding a bug here after the IPC layer exists costs ten times as much to diagnose.
2. **The lock file and `open()`.** Runtime-dir resolution, OFD ownership and participant bytes, the §3.4 algorithm including the split-brain check, ownership migration. Build §11.2 scenarios 7–11 alongside it; scenario 9 in particular should exist before the code it tests.
3. **`MappedArena` + attach protocol.** Owner and attacher, sealing, `SCM_RIGHTS`, header validation. Assert the zero-line-diff property in the read path.
4. **Claims as OFD locks; arena-side reaping; crash-point harness** — the harness built alongside, not after.
5. `tf_tree_record` and the bit-identical replay test.
6. `tf_treed`.
7. `doctor` / `top` / `participants` extensions.
8. `/tf` ingest bridge. **Note the impedance mismatch:** ROS permits any number of publishers per edge; `tf_tree` permits one. The bridge must claim each edge on first sight and adopt a documented, configurable policy on conflict — `FirstWriterWins` (default, with a loud diagnostic naming both ROS publishers) or `LastWriterWins`. This mismatch is a feature, not a bug: it surfaces multi-publisher conflicts that `tf2` silently averages into garbage, and the bridge is where users will first discover their robot has one.
9. Benchmarks and the gate.

Do not proceed past step 4 until every §11.3 crash point recovers and §11.2 scenario 9 passes a thousand consecutive runs. Everything after assumes crash-consistency and a single arena identity; a violation in either will present as an impossible numerical result somewhere around step 8.

## Appendix B — kernel behaviour probe

Verified on Linux 6.18. Re-run on the target kernel; the sealing results in §3.6 are load-bearing.

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

**OFD locks and lazy allocation.** Measured on Linux 6.18:

```
OFD exclusive lock on a byte                      -> 0
same byte from another process                    -> -1 EAGAIN
holder killed without unlocking, then GETLK       -> F_UNLCK   (kernel released it)
GETLK on a byte held by a live process            -> held, l_pid = -1   (see §3.3)

memfd ftruncate to 1 GiB                          -> 0 KiB charged
mmap without MAP_POPULATE                         -> 0 KiB charged
touch 16 MiB                                      -> 16384 KiB charged
madvise(MADV_WILLNEED, 16 MiB)                    -> 0 KiB charged   (does NOT populate)
madvise(MADV_POPULATE_WRITE, 16 MiB)              -> 16384 KiB charged
```

**The `/proc` parsing trap (§5.1), as a test fixture.** For a process whose `comm` is `evil) proc`, the naive whitespace split returns field 12's value where field 22 was intended:

```
raw    = "1234 (evil) proc) S 1 1234 1234 0 -1 4194304 1 2 3 ... 39"
naive  : raw.split()[21]                       -> 12    WRONG
robust : raw[raw.rindex(')')+2:].split()[19]   -> 13    correct
```
