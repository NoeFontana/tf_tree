# tf_tree — Phase 2 Implementation Specification: Shared Memory

> **Companion documents:** `docs/PROJECT.md` (vision, roadmap, decision log) and `docs/PHASE1.md` (single-process core). Read §1 of this document before writing any Phase 2 code — it contains mandatory amendments to the Phase 1 design that were discovered by working the multi-process failure modes through.

**Deliverable:** the same arena, mapped into N processes, with the *identical unmodified* reader code from Phase 1 running against it. Plus the lifecycle, liveness, and fault-tolerance machinery that makes that safe when processes die at arbitrary points.

**Framing.** Phase 1 was the easy half. In one process, a crash takes down every reader with it, so a torn write is unobservable. Across processes it is not: a writer can be `SIGKILL`ed between two stores and leave a data structure permanently wedged while sixteen readers keep running. **Every mutation protocol in the arena must therefore be crash-consistent — there must be no state a dead process can leave behind that a live process cannot detect and repair.** That single requirement drives most of this document.

Sections marked **NORMATIVE** are requirements. Where a syscall behaviour is asserted, it has been verified on Linux 6.18; the probe is reproduced in Appendix B so you can re-run it on your target kernel.

---

## 0.0 Implementation status

**Implemented**, except for the daemon/tooling surface (§9, §10) and the
long-running fault harness (§11.3, §11.4). The rendezvous, the attach protocol,
ownership migration, claims-as-leases, reaping, fork poisoning and per-region
population all landed under
[`0005`](./decisions/0005-the-shared-memory-seam.md).

| Area | Status |
|---|---|
| Amendments A1–A8 (§1) | **Applied** — `FORMAT_VERSION` 2 |
| `MappedArena` — `memfd`, sealed, `MAP_SHARED`, `MADV_DONTFORK`/`HUGEPAGE` (§4) | **Done** (`tf_tree_arena::mapped`, behind `--features shm`) |
| `TreeBuilder::build_shared` / `Tree::attach_shared`, read-only mode (§8) | **Done** |
| Zero-diff read path, proven by the relocation gate (§4) | **Done, and tested** (`just shm-test`) |
| Multi-process read scaling (part of §12.2) | **Done** (`just shm-scaling`; results in `docs/benchmarks/tf2.md`) |
| Amendment A2 — in-arena topology lock | **Applied** — `Tree::reparent` holds it; bounded spin, liveness-gated steal, loom- and multi-process-tested |
| Amendment A8 — bounded intern spin | **Applied** — `claiming` array, bounded spin, takeover of a dead claimant |
| `instance_uuid` (§3.6 step 4, A7) | **Done** — header offset 136, in pre-existing alignment padding |
| Discovery, rendezvous, `open()` (§3.1–§3.4) | **Done** (`tf_tree_ipc`, `tf_tree::open`) |
| §3.4's `--force-new` escape hatch | **The capability shipped; the flag never existed.** It is `CreatePolicy::Always`, settable on `tf_tree::Open`, and it skips the split-brain check as §3.4 asks. §3.4's other adjective is not met: the policy is explicit, but nothing in `tf_tree_ipc` or `tf_tree` is *loud* about taking it — no holder is named on the way past — because loudness belongs to whatever offers the escape hatch to a human, and nothing does. No binary in the workspace carries a flag of that name, and `tf_tree_cli` cannot usefully grow one: it supplies no `layout_if_creating`, so every create path it can reach ends in `OpenError::NoLayoutToCreate` — measured, against a rendezvous wedged with a held participant byte — and it exits, so an arena it did create would not outlive the command. A flag arrives with the subcommand that owns a topology and stays running, which is [`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md) §1's `tf_tree serve`. An earlier revision of this row gave a second reason to hold — that this policy is one of the paths on which a process's lock byte and its arena participant record get different indices (#201) — and **that was wrong, so it is retracted here rather than deleted.** Measured: an owner plus two read-write survivors holds bytes `[0, 1, 2]`, `SIGKILL`ing the owner leaves `[1, 2]`, and the forced creator then takes byte 0 against record 0. They agree, because the owner holds record 0 and byte 0 for its whole life and the assigner skips slot 0 for every joiner, so no survivor can hold byte 0 when the owner dies. #201 is real, and the live non-owner holder of byte 0 it needs is **no longer hypothetical**: `tf_tree_ipc::Session::release_ownership` produces one from a documented §3.5 call on a published crate, and the divergence has since been reproduced through shipped public API — see the participant-registry row below. The retraction above stands for exactly what it retracted, the owner-death route; what was wrong was concluding from it that nothing outside a test produces the state. `RUNBOOK.md` names the policy; §3.4's prose still names the flag, and this row is what it is read against (#189); §5.1's sentence was corrected against this row. |
| Attach protocol — `SOCK_SEQPACKET` + `SCM_RIGHTS` (§3.7) | **Done** — owner serves from a thread, not a daemon |
| Ownership migration (§3.5) | **Half done, and the half that is missing is the visible one.** The lock-file protocol is there and tested: ownership is byte 0, the kernel releases it when the owner dies, `tf_tree_ipc::Open::already_attached` takes the takeover path (step 3, skipping the split-brain check because a process that already holds the arena cannot create a second one), and `tf_tree_ipc`'s `migrate` rendezvous test covers it. D16 amended accordingly. **What does not exist is the trigger**: nothing watches the client socket for `HUP`, so no participant ever *calls* that path — `crates/tf_tree/src/open.rs`'s module documentation states this. Observable consequence, measured with `shm_torture` (§11.4): kill the owner and the arena keeps serving lookups exactly as §3.5 promises, but no new process can join it. They win the ownership byte, meet §3.4's split-brain check against the surviving participants' bytes, back off, and time out with `ArenaHeldButUnreachable` — for as long as any survivor lives. |
| Participant registry — owner-side slot assignment (§5) | **Done, and resting on an invariant nothing asserts (#201).** "The arena slot and the lock byte are the same integer" is *established* rather than lucky on the two ordinary paths: §3.4 step 4 refuses to create while any participant byte is held (`crates/tf_tree_ipc/src/open.rs:369`), so a normal creator runs against an empty lock file and takes byte 0 while `build_shared` hands it a fresh arena whose first `FREE` record is 0 (`crates/tf_tree/src/tree.rs:516`); and a joiner is handed one number for both, which `register_participant_at`'s doc comment (`crates/tf_tree/src/tree.rs:2893–2898`) gives the reason for in as many words — “two independently-chosen numbers would make every liveness answer be about somebody else”. **It is checked now, and the check refuses** ([`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md) plan step 0c). `Open::attempt` compares `Session::slot` with `Tree::participant_slot` at the single `hold_ownership` call site and returns `OpenError::ParticipantSlotDiverged` instead of a tree whose every liveness answer would be about somebody else. The comparison sits two statements *before* `Tree::hold_ownership` (`crates/tf_tree/src/tree.rs:2397`), which is where both numbers first meet and still compares neither, because `spawn_owner_server` binds the rendezvous socket between them: refusing after the bind would tear an arena out from under a joiner that had already attached, so the refusal has to land while the arena is still private. **Not** in `register_any` — `tf_tree_ipc` has no arena dependency and cannot see a record index at all. **And it is reachable through shipped public API — reproduced 2026-08-19, unstaged.** `tf_tree_ipc::Session::release_ownership` (`crates/tf_tree_ipc/src/open.rs:525`) gives up the ownership byte while keeping participant byte 0, which is exactly what §3.5 asks of it (“give up the owner role while staying attached”), so a live **non-owner** is left holding byte 0. A second process opening with `CreatePolicy::Always` then skips step 4's guard, `register_any` (`crates/tf_tree_ipc/src/open.rs:419`) hands it the first *free* byte — 1 — and `build_shared` still registers it at arena record **0**. Measured: `arena record = 0, lock byte = Some(1)`, and after the first process exits a joined peer reports `participant_alive(0) = false` about a process that is live, holds record 0, and has just pushed a sample, while that process's own tree reports `true`. That is the corrupting direction §6.2 forbids. A second producer of the same state needs no library call at all: `tf_tree_ipc_child hold-participant <lock> 0`, a `[[bin]]` target of the published crate. **What the retraction in the `--force-new` row above still gets right** is the *operator scenario* #201 was filed on — kill the owner, then force-create — which does not diverge, because an owner death frees byte 0 along with the ownership byte. What was wrong was concluding from that that nothing produces the state; the route above is a different producer of it. #201's second path, the takeover arm, remains unreached — nothing sets `Open::already_attached`. **The reproduction above is history as of step 0c**, kept because it is the evidence the check exists on: both of the tests that carried it (`defect_201_release_ownership_strands_a_live_non_owner_on_byte_0` and `defect_201_a_forced_creators_record_reads_dead_while_it_is_publishing`, `crates/tf_tree/tests/rendezvous.rs`) now assert the refusal, and the false-dead verdict itself can no longer be retaken through public API — `use_ofd_liveness` is installed only by the two arms of `Open::attempt`, one of which now refuses, so no `Tree` whose byte and record disagree can be constructed. |
| §5.1 liveness from `F_OFD_GETLK` | **Done for a tree from `tf_tree::open`** — both arms of `Open::attempt` install the probe, and nothing else does. Every other tree keeps `/proc`, and the row below is what "survives as a diagnostic" is read against. |
| §5.1's "no longer on any correctness-critical path" | **False in three places, all `crates/tf_tree/src/tree.rs`** (#205, reported out of #194). **(1)** `use_ofd_liveness`'s fallback, `probe.is_held(slot).unwrap_or_else(\|\| record_is_alive(rec))` — whenever `F_OFD_GETLK` declines to answer, the triple decides A8's claim liveness. **(2)** `liveness_for` — for a tree with no probe (a heap tree, one from `TreeBuilder::build_shared` called directly, or one from `Tree::attach_shared`), `record_is_alive` is the *entire* predicate handed to `ArenaView::with_liveness`, so it decides A8's intern takeover and `Tree::participant_alive`. **(3)** `Tree::reparent` — A2's topology-lock steal calls `participant_is_alive`, which reads `/proc` and never the probe **even on a rendezvous tree that has one**, so there the triple is the whole predicate on every tree; #194 named the first two, and this one was found verifying them. Each false "dead" is the corrupting direction §6.2 forbids: a topology lock stolen from a live mutator, or an intern entry taken from a running interner. The predicate is biased against it since #204 — `alive_given` returns alive wherever it cannot *prove* death — so this row records what §5.1 licenses, not a miscount in the code. **§3.10 is *a* dependency that keeps this survivable, and it is not sufficient.** It is load-bearing for correctness rather than convenience: `hidepid=2` answers `ENOENT` for another user's `/proc/<pid>`, indistinguishable at the call site from a pid that does not exist, and what makes that harmless is that a hidden entry cannot belong to a participant — they are same-user by construction. That much is stated on `proc_answers_here` and, until this row, nowhere in the spec, which is the shape #194 fixed one layer down. **The second dependency is stated nowhere at all: participants must share a PID namespace.** `read_start_time` resolves a *namespace-local* pid in the reader's namespace, and §3.1 recommends sharing the runtime directory across containers by volume mount — which does not share a PID namespace, and §3.1's own warning is about the *network* namespace only. Two outcomes, both dead-about-a-live-process: `ENOENT` while `/proc/self/stat` reads fine, which the bias at least classifies as unprovable; and a **collision** with an unrelated local pid, `Known(st) != stored`, which is not `ENOENT`-shaped, so no "cannot prove death" bias catches it. `ParticipantRecord` carries no namespace discriminator and `boot_id` is identical across namespaces on one host, so neither guard fires. **Derived from the code and from §3.1's own text, not reproduced** — `unshare --fork --pid` is unavailable in the environment this was written in. §5.1's `start_time` paragraph is read against this row — **all three of its clauses**, not only the last. It reads "It is still parsed — carefully — because `doctor` reports it and the takeover path prints it, but it is no longer on any correctness-critical path", and `doctor` does **not** report it (`crates/tf_tree_cli/src/doctor.rs` records both extra fields as captured, never read, and deliberately removed) and **no takeover path prints it** — the only live producer is `client_start_time` on the wire, which no reader consumes. **No amendment is proposed here:** §5.1's wording is `0028` step 0's ground, and the owner's answer to that record's question 1 (#195) retired the amendment step 0 planned without touching these three paths, so moving them off the triple is a decision record, not a docs edit. |
| Claims as OFD leases (§6.1) | **Done** — the arena CAS is the decision, the lease makes death observable |
| Reaping (§6.3) | **Done for both objects, by any read-write participant — and the participant half is on demand, not automatic.** Claims: `Tree::reap_dead` / `reap_participant`. Participant *records*: `Tree::reap_participants` ([`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md) plan step 5), which sweeps the table through the one reclamation predicate — the state word observed **before** the OFD byte is probed — and `ParticipantTable::reclaim`s each slot the kernel reports free, `RESERVED` records included. It is refused on a read-only tree and on a tree with no lock file, and it never judges its own slot. **§6.3's "reaping must not be owner-only" is now a property of the code**: the case that proves it is the owner's own slot, which no `HUP` can reach because no socket of the owner's closes, and `a_survivor_reaps_the_killed_owners_slot_which_no_hangup_can` (`crates/tf_tree/tests/rendezvous.rs`) kills the owner and has a surviving joiner return slot 0 to `FREE`. §3.9's "the owner reaps its arena-side records" is met by the hangup callback (#191) for a joiner and by this for everything else. **What is not done:** the sweep runs when a participant *calls* it — [`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md) plan step 3, which makes the owner's slot assigner sweep before it grants, has not landed, so nothing reclaims a leaked record on its own except the hangup fast path. And the fork case is **deliberately** not reclaimed (§6.2): a forked child keeps the parent's open file description, so the kernel says the byte is held and this refuses to act, which is `0030`'s ground. |
| Fork poisoning (§7.3) | **Done** — `pthread_atfork` counter; five destructors guarded |
| Per-edge page population (§7.1) | **Done** — measured 66.3 MiB → 3.8 MiB on an over-provisioned arena |
| CLI adoption — `--attach`, `tf_tree participants` | **Done** |
| `tf_tree serve` (was `tf_treed`), `tf_tree_record`, `/tf` ingest, diagnostics (§9, §10) | Not implemented; §9 superseded by [`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md) |
| Fault injection (§11.3) | **Not implemented.** There is no `crash-points` feature and no `TF_TREE_CRASH_AT`, so none of §11.3's eleven named mid-protocol abort sites can be reached. `shm_torture --crash-points` **refuses** rather than running the SIGKILL test and calling it §11.3 coverage: `SIGKILL` lands wherever the scheduler puts it, which is a different and much shallower set of states. §11.3's `intern.after_hash_cas_before_id_store` row is the exception — its fix is amendment A8, which *is* applied. |
| `shm_torture` (§11.4) | **Done, minus §11.3's crash points and minus killing the owner.** `crates/tf_tree_bench/src/bin/shm_torture.rs`: N processes on one arena doing random attach/detach/claim/reap/push/lookup through the real rendezvous, with the driver `SIGKILL`ing one of them several times a second and replacing it. Every reader validates every transform (§11.4's non-unit-quaternion and NaN rules, unconditionally — which is what `TF_TREE_PARANOID=1` was for, so there is no such switch), and after the run a participant that was never killed reclaims and checks that no claim and no participant slot leaked. `just shm-torture` is §13's 30-minute nightly and `just shm-torture-asan` is its "under ASan" half; **`just shm-torture-self-test` is the part that runs on a branch** — it asserts an injected corrupt transform is caught *by a process that did not write it*, and that a run which validated too little **fails** rather than printing the same `0 violations` a healthy one does. That test is in `just shm-check`. **The killed processes are joiners; the driver owns the rendezvous and is never killed.** That is forced by §3.5: takeover is specified but not wired into `tf_tree::open` (that module's own docs say so), so when the owner dies nothing takes over, every joiner is turned away by §3.4's split-brain check, and the run wedges in `ArenaHeldButUnreachable` — which is exactly what the first revision of this harness did, for most seeds, while printing `PASS`. **§12.3 gate 3 is therefore partly met**: zero corrupt reads across a torture run, measured, over ~250 composed `map -> tool` reads per observation round and with a floor that fails the run below 16; "every §11.3 crash point recovers" is not, because the crash points do not exist, and "the owner dies mid-run" is not, because §3.5 is not wired up. |
| §3.8's generous default layout | **Superseded by decision `0004`**, which sizes the arena from declared edges. Reconciling the two is its own decision; `0005` records the conflict rather than resolving it silently. |

**What the remaining gap means.** There is no daemon and no recorder, so "an
owner always exists" is the operator's job rather than something a service
guarantees — which is exactly what D16 says it should be. And there is no
long-running fault harness, so the crash matrix in §11.3 is covered by targeted
tests at each crash point rather than by randomised injection over hours. Both
are additions, not corrections: nothing in the shipped protocol is waiting on
them.

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
| ~~`tf_treed`~~ | **Not a crate.** [`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md) makes it `tf_tree serve`, a subcommand of `tf_tree_cli`, so the workspace gains no member |
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
// A consumer. The defaults are the consumer: read-only, and `CreatePolicy::Never`.
let tree = tf_tree::open()?;                 // join, or ArenaAbsent
let tree = tf_tree::open_named("robot")?;

// A consumer that may start before the publisher's process does.
let tree = tf_tree::Open::new()
    .domain(7)
    .name("robot")
    .await_open(Duration::from_secs(5))?;    // 0019 §2b

// A creator. Read-write is required: a read-only attach *cannot* create.
let tree = tf_tree::Open::new()
    .domain(7)
    .name("robot")
    .mode(AttachMode::ReadWrite)
    .create(CreatePolicy::IfAbsent)          // IfAbsent | Never | Always
    .layout_if_creating(layout)              // sizes the arena if we turn out to be the creator
    .open()?;
```

> **Amended by [`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md) §2a.**
> `Open::new()`'s `create` default was `IfAbsent`; it is **`Never`**, so the builder's own defaults are
> the consumer. And `AttachMode::ReadOnly` with any creating policy is now
> `OpenError::ReadOnlyCannotCreate` rather than a request that could only ever produce an arena its
> creator cannot write. The sample above used to show exactly that combination as the ordinary consumer
> spelling.

`CreatePolicy::Never` is what a consumer gets without asking. It is the default because a consumer that creates an arena the estimator has not populated looks healthy and finds nothing — worth stating explicitly in a supervised deployment even so.

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

**Slot assignment and record population are done by different processes, and this section used to say otherwise.** The *owner* assigns the slot: its accept loop scans for an index whose arena record carries no identity and whose lock byte the kernel reports free, and returns it as `HelloResponse.participant_slot` (§3.7). The *joiner* writes the record — its own, **with a CAS**, and **after** it has taken the lock byte for that slot. A creator or a process taking ownership has no owner to ask, so it finds its own free record the same way and through the same CAS, again after its byte. **“After its byte” holds on every path that *has* a byte, and one public path still does not**: a directly-called `TreeBuilder::build_shared` (`crates/tf_tree/src/tree.rs:504`, registering at `:516`) opens no lock file, so there the CAS is the only ordering there is — which is what §11.3's `attach.after_slot_assigned_before_publish` row is distinguishing. That shape is supported and stays: it is how an arena gets created. **This sentence read “three public ones” until [`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md)'s plan step 0b, and the other two have since stopped registering at all.** `Tree::attach_shared` / `attach_shared_at` (`:2227`, `:2254`) still open no lock file, but their `AttachMode::ReadWrite` arm now returns `ShmError::ReadWriteNeedsRendezvous` before the segment is mapped, and their `ReadOnly` arm writes no participant record — the `is_writable` branch at `:2302` hands a non-writable backing the `u32::MAX` sentinel (`:2313`) instead of registering — so neither has a record whose ordering could be at issue. `TreeBuilder::build` (`:460`) is a heap tree and never had one.

So the CAS is load-bearing rather than avoidable. `fill_slot` (`crates/tf_tree_core/src/participant.rs:154`) opens with `compare_exchange(FREE, RESERVED)`, which wins the slot exclusively; writes the identity fields under `RESERVED`, where no reader may trust them; and release-stores the live word last. **That publication order is what makes A3's indirection sound**, not owner-side population: a claim can only name a slot some process drove to `LIVE`, and the `Release`/`Acquire` pair means whoever sees `LIVE` sees every field written above it. A process killed in between leaves `RESERVED` — distinguishable garbage rather than a plausible-looking record, which is the state §11.3's `attach.after_slot_assigned_before_publish` row is about.

**The deviation is recorded rather than corrected silently**, in the same shape as `Open::register_any`'s doc comment (`crates/tf_tree_ipc/src/open.rs:399-418`), which records the analogous deviation from §3.3's "written with `pwrite` before taking the slot lock" for the *lock file's* identity record. The two deviations point opposite ways and both are deliberate. For the lock-file record on a self-chosen slot it is lock-then-write, because write-then-lock loses the race it exists to win and leaves a byte held under the losing process's name. For the arena record it is byte, then CAS, then fields, then publish. The code gives the reason as retry-cleanliness — “nothing was written to the arena, so nothing is left behind, which is the point of taking the byte before touching it” (`crates/tf_tree_ipc/src/open.rs:334`); that it also keeps a record from reading live before its byte is held, which is what §5.1 needs, is an inference this section draws and no comment states. Neither ordering leaves a record a reader can mistake for a live participant.

### 5.1 Identity is advisory; the lock file is authoritative — NORMATIVE

**Liveness comes from the participant's OFD lock byte (§3.3), never from these records.** A `ParticipantRecord` describes *who* a slot belongs to; whether it is live is a kernel fact. Any code deciding liveness from `state` or `heartbeat` is a bug.

**Where that rule is implemented for participant records, once.** `reclamation_verdict` (`crates/tf_tree/src/open.rs:298`, [`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md) plan step 2) is the single predicate every reclamation decision goes through, and it answers from the lock byte and nothing else — no `/proc`, no `heartbeat`. It does read `state`, and the line this section draws is the one that makes that legitimate: the word answers *is there a record here*, never *is its process alive*. **A `FREE` word is very often a live process** — a read-only joiner takes its lock byte in the handshake and then registers no arena record, because the table is in the arena and a `PROT_READ` mapping cannot be written — so the predicate reports such a slot *unknown*, and a revision that reported it reclaimable would be issuing a death verdict about a running consumer, on the shape D18 makes the default. Three properties of it are not stylistic and a reader changing it needs all three. It **skips this process's own slot unconditionally**, because `F_OFD_GETLK` reports only conflicting locks and a description does not see its own. It **observes the `state` word before it probes the byte**: under that order the `Acquire` load of a live word synchronises-with `fill_slot`'s publishing `Release` store, so a byte probe sequenced after it must see the byte held — reversed, or taken from one up-front `held_participants()` mask, a model erases a published record (`0028` open question 6). And it is **sound only because plan steps 0b *and* 0c both landed**: 0b buys *every participant holds a byte*, 0c buys *the byte at index `slot` is the byte of the record at index `slot`*, and question 6's resolution is explicitly conditional on both. It is scoped to a tree carrying a liveness probe, which only `tf_tree::Open` installs; a directly-called `TreeBuilder::build_shared` has no lock file, therefore no probe, and never reaches it. **A second copy of this predicate is the defect `0028` was opened about, re-created.**

`(pid, start_time, boot_id)` remains the identity triple for diagnostics and for the forced-create path — which is `CreatePolicy::Always` (`crates/tf_tree_ipc/src/open.rs:114`), **not** a `--force-new` flag: no binary in the workspace carries one, and `tf_tree_cli` cannot usefully grow one. §0.0's row for §3.4's `--force-new` escape hatch is the authoritative statement of what shipped, of why the flag did not, and of what a flag would have to arrive with; this sentence is read against it (#189). A bare PID is not an identity: PIDs are recycled, and on an embedded system with a low `pid_max` they recycle fast.

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
- At the moment an edge is **taken up**, `madvise(MADV_POPULATE_WRITE|READ)` (Linux ≥ 5.14, measured working) over that edge's stamp and pose ranges. On older kernels, fall back to touching one byte per page. **Amended by [`0024`](./decisions/0024-population-is-per-edge-at-take-up.md);** this bullet used to say "at `declare_dynamic`", and [`0004`](./decisions/0004-builder-time-edge-declaration.md) deleted that function when it moved declaration to build time. The two moments that replace it are `Tree::claim` for a writer and plan compilation for a reader, both off the query path by D3. Populating every declared ring at attach instead — which is what the code did while this bullet named a dead function — is *per-arena* population, which the title of this section forbids, and it charged every reader for every edge on the vehicle: measured at **5.2×** on a process using 4 of 64 declared edges.
- On attach, populate the header, frame table, topology blocks, claim table, participant table, edge table and both counter regions — small, always touched, always hot. **Not the two ring arenas:** they are 99.8% of a large arena and are the previous bullet's business.

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

> **SUPERSEDED by [`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md).
> There is no `tf_treed` binary. The capability is `tf_tree serve`, a subcommand
> of the binary that already ships — and it is an escalation, not a prerequisite.**
>
> Two things dissolved this section. First, **most of its responsibilities were
> already discharged elsewhere**: D16 said ownership is *configured, not
> negotiated*, and this daemon existed to make configuring it trivial —
> [`0005`](./decisions/0005-the-shared-memory-seam.md) §8 then retired the "no
> takeover" half, so ownership is a role the kernel reassigns on an uncontended
> `F_OFD_SETLK` and `OpenOutcome::TookOver` ships. Liveness, reaping and owner
> death need no daemon.
>
> Second, **the part that did remain is not a lifecycle problem.** What this
> section was really for is the last paragraph below: pre-declaration, so a
> consumer can attach and plan before any publisher runs. `0019` §2 fixes that
> without a daemon and without a config file — a read-only attach implies
> `CreatePolicy::Never` (`Open::new()` *defaulted* to read-only **and**
> create-if-absent, a configuration no correct program wants — it failed
> `NoLayoutToCreate` rather than creating an empty arena, so what §2a removed
> was a latent class; the default is now `Never`), a consumer waits for the
> arena with
> `Open::await_open` and for topology with `Tree::await_frames`, and
> `frame_headroom`/`edge_headroom` cover frames that arrive later.
>
> What survives, as `tf_tree serve --config <topology.toml>`: create and seal
> from the config, pre-declare, hold the arena open, export metrics, drain on
> `SIGTERM` leaving the segment alive. What is retired: `--lock` and
> `--socket-mode`, both of which the rendezvous owns and neither of which was ever
> daemon-specific. [`0009`](./decisions/0009-descoping-phase-6.md)'s amendment
> below travels with it — URDF is still owed by no phase.
>
> The rest of this section is kept as written, because `0019`'s §*Rationale*
> argues against it and an argument needs its opponent on the page.

The reference owner. Target ~400 lines. Deliberately boring: it holds no application logic, so it is the process least likely to crash.

```
tf_treed --domain <n> --name <n> --config <file.toml> [--participants 64]
         [--lock] [--socket-mode 0600] [--metrics-port <p>]
```

> **Amended by [`0009`](./decisions/0009-descoping-phase-6.md):** `--config` took
> `<file.toml|urdf>` here. URDF parsing is no longer owed by any phase — it
> leaves the engine and becomes an optional converter that emits the topology
> config Phase 4 already ships, built on the existing `urdf-rs` crate. So this
> daemon takes the config format and nothing else; a user with a URDF converts
> it first. Costs no code, since §9 is unimplemented, but it does retract a
> promise rather than leaving it to be discovered.

Responsibilities: create and seal the segment; declare frames and static edges from the config so the tree exists before any node starts; serve the attach socket; `epoll` participant sockets and reap on `HUP`; serve `doctor`/`top` queries; export Prometheus metrics; on `SIGTERM`, drain and exit, leaving the segment alive for existing participants.

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
| `attach.after_slot_assigned_before_publish` | participant slot in `ATTACHING` (`RESERVED` in the code), with **two possible byte histories** — on the rendezvous path the byte was **taken first and then released by the kernel** at death, because the joiner CASes its record only after holding it (§5); on the one byte-less path left — a directly-called `TreeBuilder::build_shared`, `Tree::attach_shared` / `attach_shared_at` having left this list at `0028`'s plan step 0b, where their `ReadWrite` arm began refusing and their `ReadOnly` arm registers no record, so neither reaches this crash point at all — no lock file is opened and the byte was **never taken** → record cleared by any reaper |
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

| Benchmark | Report | Measured |
|---|---|---|
| depth-3 cross-process lookup, warm | p50, p99, p99.9 vs the Phase 1 in-process baseline | — |
| first access after attach, per-edge population on vs off | p99.9, both | **Half done — `just attach-bench`.** With population **on** (the shipped path): the first lookup after attach is **130–170 ns p50** over 25 runs (sixteen in the sitting that rewrote this row, nine in the review before it), indistinguishable from a steady-state lookup — which is the guarantee §7.1 exists to buy, now demonstrated rather than asserted. **Its tail is one sample, and is quoted as one:** nearest-rank p99.9 over 201 cycles *is* the maximum (`pct`, in the binary), so what moves between runs is one draw from the scheduler's tail and not a distribution — 1.2–4.7 us across the sixteen runs of this sitting, ~1.3 us in the sitting that first filled this row, and the cold cycle to the nanosecond in fifteen of those sixteen but not in the sixteenth. The **off** arm is absent and deliberately so: `populate_hot()` is unconditional inside `attach_shared_inner`, and manufacturing an "off" arm out of a different code path would measure something else. It arrives with `docs/decisions/0022`'s B2-prime, the change that gives the attach path a policy. |
| THP `madvise` vs `never` | p50, p99.9, both | — |
| aggregate read throughput, 1→16 consumer processes | scaling curve | **Done — `just shm-scaling`; the curve is in [`docs/benchmarks/tf2.md`](./benchmarks/tf2.md).** §0.0 has recorded it **Done** since it was measured, which is why this cell is not a dash. 1/2/4/8 reader processes on one arena: **4.66 → 9.04 → 15.43 → 18.17 M lookups/s** aggregate (1.00x → 1.94x → 3.31x → 3.90x) at 213 → 219 → 257 → 431 ns a lookup, with unique resident 3.5 → 18.7 MiB (Pss, one arena) against the N × 1.4 MiB a private tf2 buffer per process would cost. **The bend is cores, not contention**: this host has 4 physical cores, 4 × 213 ns is an 18.8 M/s roofline, the 8-process row measures 18.2, and its per-lookup cost doubles exactly as 2:1 oversubscription predicts (426 ns predicted, 431 measured). The curve stops at 8 for the same reason — 16 processes on 4 cores measures the scheduler, and the roofline argument is already made. |
| **CPU per consumer at 1 kHz × 20 edges, vs ROS 2 `/tf`** | %CPU per consumer, both | — |
| **total RSS across 16 consumers, vs ROS 2 `/tf`** | MB, both | — |
| publish → visible-to-consumer latency, vs ROS 2 `/tf` | p50, p99.9, both | — |
| `SIGKILL` writer → claim reapable → re-claimed | p50, p99 | — |
| attach time, cold and warm | p50 | **Done — `just attach-bench`, which does not run §3.7's rendezvous.** The binary calls `Tree::attach_shared(dup, AttachMode::ReadOnly)` on a duplicated memfd: no `connect`, no version handshake, no `SCM_RIGHTS`, no assign closure. What it times is map, validate, take a participant slot, `populate_hot` — and on a read-only mapping that advice is `MADV_POPULATE_READ`, not `POPULATE_WRITE`: `MappedArena::populate` follows the mapping's protection, because `POPULATE_WRITE` on `PROT_READ` is `EINVAL`. On the §11.1 fixture, 201 cycles a run: attach **12.3–14.2 us p50**, and the first plan compile — which is where the ring population went — **66.3–92.3 us p50**. Those are **observed extremes over 28 runs on this host, rounded outward** (sixteen in the sitting that rewrote this row, nine in the review that falsified its first draft, three in [`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md)), at load averages of 4 to 7 wherever the run recorded one — a record of what was seen, **not a bound**. The width is the host: `0028` watched three unpaired repeats of its join arm drift 132 → 138 → 180 us "as the other agents' load rose", and the first draft of this row published ranges over five and eight runs, of which nine fresh runs fell outside eight of ten. **Only the p50s are given as ranges, and that is the point:** `cold` is cycle 0 and nearest-rank p99.9 over 201 cycles *is* the maximum, so each of those is a single sample whose run-to-run movement is one draw from the scheduler's tail — attach's cold cycle has been seen at 16–25 us and its maximum at 19–127 us, the first compile's at 74–113 us and 107–171 us, and a rerun landing outside those intervals says nothing about the code. **This row read `97.5 us p50` until 2026-08-19, and that is a split rather than a regression**: it was measured on `1e18234` (2026-08-14), and [`0024`](./decisions/0024-population-is-per-edge-at-take-up.md) moved ring population off attach and onto edge take-up on `0f17fb8` (2026-08-16). The two halves still sum to **79.3–106.4 us p50** — per run, paired — on this fixture, whose plan walks essentially every edge, and the pre-`0024` pair summed to 100.3 us (99 791 + 550 ns, `0024`'s own before column), so the total did not move. That before column measuring attach at 99.8 us where this row said 97.5 is the same run-to-run width, two days apart. The arena is 1 401 472 B — 343 pages at 4 KiB, printed by the recipe so the division is reproducible — and that sum over 343 is ~230–310 ns/page, which is arithmetic over the whole arena and not a per-page measurement: attach populates the tables, the first plan compile populates the rings its plan reaches. **Attach alone is not that number and neither is a real join**: `0028` measured the whole §3.7 path — `Open::open()` against a live owner, assign closure included — at ~133 us p50, out of tree, so nothing in this repository reproduces it. "Cold" is the first cycle — fresh VMA and page tables — and **not** a cold page cache, which needs root to arrange. |
| `open()` when the arena exists vs when creating | p50, both | — |
| owner kill → new owner serving | p50, p99 | — |
| lookup latency across an ownership migration | p99.9 during vs steady-state | — |

**The third column is where a result goes, and a dash is not a status.** Three
of these rows carry a measurement. Two of them carried it as a third cell in a
two-column table until 2026-08-19 — which GFM discards silently, so on
github.com the figures in them rendered as nothing at all, and one of the two
was stale for five days behind that, because nobody proofreads what they cannot
see (#208). The third had never been in this table at all: §0.0 records
multi-process read scaling **Done**, and a dash here against a **Done** there
would be this table contradicting the §0.0 that outranks it. A dash means *this
table* holds no figure for that row, not that none exists: more are measured in
[`docs/benchmarks/tf2.md`](./benchmarks/tf2.md) (`just mp-bench`,
`just tf2-bench`), and that register rather than this list is where the `tf2`
comparison lives. `just artifact-versions` now fails on a row whose cell count
disagrees with its header, so the next one is caught before it is invisible.

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
| `FrameNotDeclared` on a read-only participant | startup ordering: no publisher has declared it yet | wait for it — `Tree::await_frames` ([`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md) §2). Check the consumer is not creating the arena itself: a read-only attach implies `CreatePolicy::Never`, and the builder's default is now `Never` too |
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
- [ ] `tf_tree serve` ships with a systemd unit and a container example (§9, superseded by [`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md); not first-release scope)
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
6. `tf_tree serve` — last, and possibly never: [`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md) §2 is shaped so the daemon may never become urgent.
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
