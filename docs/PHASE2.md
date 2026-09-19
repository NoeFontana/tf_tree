# tf_tree — Phase 2 Implementation Specification: Shared Memory

> **Companion documents:** `docs/PROJECT.md` (vision, roadmap, decision log) and `docs/PHASE1.md` (single-process core). Read §1 before writing any Phase 2 code: it amends the Phase 1 design.

**Deliverable:** the same arena, mapped into N processes, with the *identical unmodified* reader code from Phase 1 running against it, plus the lifecycle, liveness and fault-tolerance machinery that makes that safe when processes die at arbitrary points.

**Framing.** A writer can be `SIGKILL`ed between two stores and leave a structure permanently wedged while sixteen readers keep running. **Every mutation protocol in the arena must be crash-consistent: no state a dead process can leave behind may be undetectable or unrepairable by a live one.**

Sections marked **NORMATIVE** are requirements. Syscall behaviour asserted here was verified on Linux 6.18; the probe is in Appendix B.

## 0.0 Implementation status

**Implemented**, except `tf_tree serve` (§9), declined and not scheduled. §10's recorder and §7.4's memory locking are **declined by record** ([`0047`](./decisions/0047-the-recording-this-reader-would-refuse.md), [`0049`](./decisions/0049-the-flag-that-prefaults-the-arena.md)): work not owed. Where a row and any prose disagree, the row wins.

| Area | Status |
|---|---|
| Amendments A1–A8 (§1) | **Applied** — `FORMAT_VERSION` 3 |
| `MappedArena` — `memfd`, sealed, `MAP_SHARED`, `MADV_DONTFORK`/`HUGEPAGE` (§4) | **Done** (`tf_tree_arena::mapped`, behind `--features shm`) |
| `TreeBuilder::build_shared` / `Tree::attach_shared`, read-only mode (§8) | **Done** |
| Zero-diff read path, proven by the relocation gate (§4) | **Done, and tested** (`just shm-test`) |
| Multi-process read scaling (part of §12.2) | **Done** (`just shm-scaling`; results in `docs/benchmarks/tf2.md`) |
| Amendment A2 — in-arena topology lock | **Applied, and no longer only in-arena** ([`0029`](./decisions/0029-the-topology-lock-is-a-kernel-lock.md)). On a tree with a lock file `Tree::reparent` takes §3.3's byte 1 first and the arena word second, so a live holder is refused by a kernel fact; the `/proc` predicate remains only as a residual that may withhold a steal. §11.3's `topo.holding_lock` row is the crash walk. |
| Amendment A8 — bounded intern spin | **Applied** — `claiming` array, bounded spin, takeover of a dead claimant |
| `instance_uuid` (§3.6 step 4, A7) | **Done** — header offset 136, in existing alignment padding |
| Discovery, rendezvous, `open()` (§3.1–§3.4) | **Done** (`tf_tree_ipc`, `tf_tree::open`). There is no `tf_tree::open_named`; the named open is `Open::new().name(n)?.open()`. |
| §3.4's `--force-new` escape hatch | **The capability shipped; the flag never existed.** It is `CreatePolicy::Always` on `tf_tree::Open`, and passes iff nothing is serving **and** the ownership byte is free **and** participant byte 0 is free (`the_escape_hatch_creates_over_a_stranded_participant`, `a_live_byte_0_refuses_both_policies`, `a_held_ownership_byte_refuses_the_hatch_and_freeing_it_lets_one_through`, all `crates/tf_tree/tests/rendezvous.rs`). No binary carries a flag of that name: `tf_tree_cli` supplies no `layout_if_creating` and exits, so a flag belongs to the subcommand that owns a topology ([`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md) §1, `tf_tree serve`). `IpcError::ArenaHeldButUnreachable` carries `ownership_held`; `RUNBOOK.md` names the policy. §3.4 and §5.1 are read against this row. |
| Attach protocol — `SOCK_SEQPACKET` + `SCM_RIGHTS` (§3.7) | **Done** — owner serves from a thread, not a daemon |
| Ownership migration (§3.5) | **Done as of 2026-08-28; the mechanism is complete and the *trigger* is the caller's, by design.** `Session::take_over_ownership` takes byte 0 on the description the session already holds and `Tree::inherit_ownership` binds and serves the existing segment, returning `Inheritance::{Inherited, OwnerAlive, Contended, ReadOnly, NotApplicable}` ([`0037`](./decisions/0037-a-takeover-is-not-a-second-open.md)). `Tree::owner_lost` polls the attach socket for hangup and then `F_OFD_GETLK`s byte 0, so it answers "the arena has no owner", not "my socket is dead" ([`0043`](./decisions/0043-owner-lost-is-a-question-about-the-owner.md)); a `false` or a single non-`Inherited` answer is never final ([`0057`](./decisions/0057-an-owner-is-not-dead-until-its-files-close.md)). Tests: `a_survivor_inherits_ownership_and_the_arena_becomes_joinable_again`, `two_survivors_race_and_exactly_one_inherits`, `a_read_only_survivor_reports_that_it_cannot_inherit`, `a_survivor_that_did_not_inherit_stops_being_told_the_owner_is_gone`, `scenario_3_an_owner_dying_leaves_readers_working_and_joins_refused`, `a_guard_may_be_held_across_inheriting_ownership`. Owed by the fleet, not the crate: calling the trigger ([`0055`](./decisions/0055-the-recovery-capacity-a-fleet-cannot-add-later.md), §3.5). §3.5's literal reconnect is not implemented; its residue is reclamation latency ([`0043`](./decisions/0043-owner-lost-is-a-question-about-the-owner.md)). |
| Participant registry — owner-side slot assignment (§5) | **Done; #201 is closed (2026-08-27).** "The arena slot and the lock byte are the same integer" is checked: `Open::attempt` compares `Session::slot` with `Tree::participant_slot` and returns `OpenError::ParticipantSlotDiverged` before binding ([`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md) step 0c). The path that could break it (the takeover arm, `register_any`) is deleted ([`0035`](./decisions/0035-the-creators-slot-is-taken-not-found.md), [`0037`](./decisions/0037-a-takeover-is-not-a-second-open.md)). A `LIVE` record at index *i* whose byte *i* is unheld reads dead to every probe-carrying observer, so `Tree::reap_participants` must not run in a process tree where anything served a `build_shared` arena by hand — **out of contract, permanently** ([`0031`](./decisions/0031-the-participant-record-with-no-byte.md), 2026-09-18). |
| §5.1 liveness from `F_OFD_GETLK` | **Done for a tree from `tf_tree::open`** — both arms of `Open::attempt` install the probe and nothing else does. Every other tree keeps `/proc`. |
| §5.1's "no longer on any correctness-critical path" | **False in two places, both `crates/tf_tree/src/tree.rs`** (#205): `use_ofd_liveness`'s fallback when `F_OFD_GETLK` declines to answer, and `liveness_for` for a tree with no probe (heap, `build_shared`, `attach_shared`), where `record_is_alive` decides A8's intern takeover and `Tree::participant_alive`. A third path, `Tree::reparent`'s steal, is closed for a tree with a lock file by [`0029`](./decisions/0029-the-topology-lock-is-a-kernel-lock.md) and open for one without (`a_live_holder_that_proc_calls_dead_keeps_the_topology_lock`). The predicate fails toward "alive" wherever it cannot prove death. §3.10 (same-user, so `hidepid` cannot hide a participant) is a dependency; a second is that participants must share a PID namespace, since `ParticipantRecord` carries no namespace discriminator and adding one is a `FORMAT_VERSION` bump ([`0033`](./decisions/0033-the-identity-record-cannot-name-a-namespace.md) fixed `doctor` only). `doctor` does not report `start_time`, and no takeover path prints it. Moving these predicates off the `/proc` triple is a decision record, not a docs edit. |
| Claims as OFD leases (§6.1) | **Done** — the arena CAS is the decision, the lease makes death observable |
| Reaping (§6.3) | **Done for claims and participant records, by any read-write participant; the participant sweep is on demand.** `Tree::reap_dead` / `reap_participant` for claims; `Tree::reap_participants` ([`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md) step 5) sweeps the table through the one reclamation predicate (state word observed **before** the byte is probed) and `reclaim`s each slot the kernel reports free. It is refused on a read-only tree or one with no lock file and never judges its own slot. The owner's slot assigner and hangup callback are the other two collectors. `a_survivor_reaps_the_killed_owners_slot_which_no_hangup_can` pins §6.3's "must not be owner-only". A forked child keeps the parent's description, so its byte reads held and is deliberately not reclaimed (§6.2, [`0030`](./decisions/0030-the-atfork-handler-and-inherited-descriptors.md)). |
| Fork poisoning (§7.3) | **Done** — `pthread_atfork` counter; five destructors guarded |
| Per-edge page population (§7.1) | **Done** — measured 66.3 MiB → 3.8 MiB on an over-provisioned arena |
| Memory locking (§7.4) | **Declined by [`0049`](./decisions/0049-the-flag-that-prefaults-the-arena.md).** No `LockPolicy` or `mlock` exists; `MappedArena` applies `MADV_DONTFORK`, `MADV_HUGEPAGE` and §7.1's per-edge populate. Reason: `docs/API.md` §8.3, a library cannot see the `RLIMIT_MEMLOCK` budget it would spend. The shipped half is diagnostic: `TFT016` compares `RLIMIT_MEMLOCK` to arena size (`fn tft016`'s `match host.memlock` arm in `crates/tf_tree_cli/src/checks.rs`). |
| CLI adoption — `--attach`, `tf_tree participants` | **Done** |
| `tf_tree serve` (was `tf_treed`, §9) | **Not implemented, not scheduled.** §9 is superseded by [`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md) (steps 6–7 not built); §15's box is `~`. |
| `tf_tree_record` (§10) | **Declined by [`0047`](./decisions/0047-the-recording-this-reader-would-refuse.md).** §10(c), the NORMATIVE heap-against-mapped bit-identity test, **is met**: `crates/tf_tree_cli/tests/replay_bit_identity.rs`, run by `just shm-check`, not `just test`. §10(a)/(b) are declined because the recorder's channels carry no `tf2_msgs` schema and `tf_tree_ingest` accepts a channel only by schema. |
| `/tf` ingest bridge | **Done**, as `docs/PHASE4.md` §5; `ros/tf_tree_ros/` is the `ament_cmake` half. |
| Diagnostics (§9's `doctor` / `top` / `participants`) | **Done.** `tf_tree top` is `docs/PHASE5.md` §0.0's §7 row. |
| Fault injection (§11.3) | **Implemented.** Thirteen of the fourteen rows carry a `crash-points` site (seven in `tf_tree_core`, six in the facade) and all thirteen are driven by a test that fires them. Completeness gates: `crash_tests::the_published_site_list_is_the_one_the_tests_arm` (core) and `the_facade_site_list_is_pinned_by_index_and_every_site_has_a_test` (facade, pins index to name). The fourteenth, `reclaim.probe_then_reoccupied`, is an interleaving of two live processes, not an abort site; `loom` and §11.4 are what reach it. `shm_torture --crash-points` arms these sites and refuses only in a build that compiled them out; a run without it must not be quoted as §11.3 coverage. |
| `shm_torture` (§11.4) | **Done, and since 2026-09-04 it kills the rendezvous owner. Two of §11.4's four continuous invariants are checked continuously, a third at teardown, and the fourth is not implemented — carry that qualifier when quoting “invariants checked continuously”.** `crates/tf_tree_bench/src/bin/shm_torture.rs`: N processes on one arena doing random attach/detach/claim/reap/push/lookup through the real rendezvous, the driver `SIGKILL`ing one several times a second and replacing it. The owner is a child; every child evaluates `owner_lost` and calls `inherit_ownership`; each owner kill must be followed by a *fresh* `Open::new().create(Never)` join inside a 10 s deadline and a recorded inheritance, or the run fails (`--no-inherit` is the negative control). Invariants: *no non-unit quaternion or NaN* — every read; *no two writers on one edge* — every push, by the holder (a push that succeeds while the claim word names another slot); *no slot leaks* — teardown only, in `check_recovery`, which holds records of processes that ever held the rendezvous to the strict path; *arena hash stable across quiescent points* — **not implemented** (no safe accessor for the arena's bytes, and “quiescent” is undefined under continuous kills). Recipes: `just shm-torture` (30 min nightly), `shm-torture-asan` (`--duration 30m --children 4 --kill-hz 4` in nightly), `shm-torture-crash-points` (own nightly job; `--features shm,crash-points`), and `shm-torture-self-test` (runs in `just shm-check`; asserts an injected corrupt transform is caught by a process that did not write it, that a run which validated too little **fails**, that a run in which nothing inherits fails, that a run which never migrates passes with every worker record on the strict path, and that a short run whose owner kills all defer fails). A kill of the owner is deferred, retried every 250 ms (`OWNER_KILL_DEFERRAL_RETRY`), and fails only after `3 × --owner-kill-every` of unbroken deferral; `--defer-owner-kills N` is its positive control. The floor counts owner-kill *attempts*. `--crash-points` refuses when nothing was armed and, separately, when nothing fired; **`aborted` is a floor rather than a count**, because the driver's victim path discards the child's exit status. A `kill.in_progress` marker makes a child stay instead of detaching for the width of one reap, because a `SIGKILL`ed owner is undetectably dead until `exit_files()` runs and a survivor that leaves cannot return (§3.4 step 4); the run prints the suppressed detaches. `--victim-ballast-mb` and `--stop-owner-ms` are the positive controls (the first is void under `transparent_hugepage=always`; the second is what the regression test uses). The recipes run the binary under `prlimit --core=1:1` ([`0057`](./decisions/0057-an-owner-is-not-dead-until-its-files-close.md) Decision 6) so an armed abort does not run the host's crash helper before its files close; that suppression is verified on kernel 6.8 and pending on the runner. **Twelve of thirteen sites fire in this workload** (measured with `--crash-site NAME[:nth]`): eight in a live arena with the repair met by live peers (`push.*` ×3, `claim.after_cas`, `attach.after_slot_assigned_before_publish`, `takeover.after_ownership_lock_before_bind`, `reclaim.after_probe_before_cas`, `hangup.after_probe_before_cas`); `open.after_ownership_lock_before_bind` and `open.after_create_before_bind` in the creating owner, where `spawn_owner`'s retry is the next `open()`; `topo.after_copy_before_publish` and `intern.after_hash_cas_before_id_store` only where the state their row names cannot exist; `topo.holding_lock` never (nothing reparents). Covering all thirteen is not a bar this workload can clear; per-site coverage lives in the targeted tests. Recovery-across-a-core-dump is measured by `0057`, not by the recipes. §12.3 gate 3's “every §11.3 crash point recovers” is partly measured here and fully by the targeted tests. |
| §3.8's generous default layout | **Superseded by decision `0004`**, which sizes the arena from declared edges. |

## 0. Scope

### In scope

`MappedArena` (memfd, sealed, `MAP_SHARED`); zero-config discovery and rendezvous with kernel file locks as the election; the `SOCK_SEQPACKET` + `SCM_RIGHTS` attach protocol; ownership migration; the advisory participant registry with OFD locks authoritative for liveness; claims as kernel locks; crash-consistency of every mutation protocol (§1); read-only attach; mapping policy (per-edge population, `MADV_HUGEPAGE`, `MADV_DONTFORK`); the `/tf` ingest bridge; `doctor`, `top`, `participants`. Not crates: `tf_treed` is `tf_tree serve` ([`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md)), `tf_tree_record` is declined ([`0047`](./decisions/0047-the-recording-this-reader-would-refuse.md); §10(c) is met by `crates/tf_tree_cli/tests/replay_bit_identity.rs`).

### Out of scope — NORMATIVE

Everything excluded in §0 of `PHASE1.md` remains excluded. Additionally:

| Excluded | Why |
|---|---|
| Network, discovery beyond one host | Phase 6 |
| Python bindings | Phase 3 (see §14) |
| `tf2_ros::Buffer` API shim | Phase 4. The ingest bridge here is one-way. |
| macOS / Windows shared memory | §2. In-process only until Phase 6. |
| Multi-arena federation on one host | One arena per `(runtime_dir, domain, name)`; distinct triples are independent (§3.1). |
| Any security boundary against a malicious RW peer | §3.10. Say this out loud in the docs. |
| Dynamic arena resize | D4. Capacity is planned, not grown. |

## 1. Phase 1 amendments — NORMATIVE

`PROJECT.md` states that if Phase 2 requires changes outside `tf_tree_arena`, the Phase 1 design was wrong. Working the crash matrix found eight such places; A6 changes the layout. If Phase 1 is not yet frozen, apply all of them; if shipped, they constitute `FORMAT_VERSION = 2` and no version-1 arena may be attached.

### A1 — Pack the topology generation and active index into one atomic word

**Problem.** Phase 1 §5.2's seqlock bumps `topo_generation` to odd, copies, flips `topo_active`, bumps to even. A writer `SIGKILL`ed after the first bump leaves the generation odd forever and every reader spins in plan compilation. This wedges the arena with no recovery.

**Fix.** The writer mutates an *inactive* block; the active block is never mutated in place, so publication is a single store.

`TopoWord(AtomicU64)`: bits 63..8 = generation (monotone), bits 7..0 = active block index; `pack(gen, active) = (gen << 8) | active`, `unpack(w) = (w >> 8, w & 0xff)`.

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

A dead writer leaves the arena indistinguishable from no write having happened; readers never spin on a writer.

**`TOPO_BLOCKS = 4`.** A reader is hit only if the writer flips `TOPO_BLOCKS` times mid-read; at `max_frames = 256` four blocks cost 6 KB, making `TopologyChurn` unreachable outside a torture test.

**Topology block arrays are `[AtomicU32]` and `[AtomicU16]`.** A non-atomic read racing another process's write is UB even when the value is discarded. **Every index read from a topology block must be bounds-checked and the parent walk capped at `max_frames` steps**, because garbage from a lost race must not panic or index out of bounds before the validity check.

### A2 — The topology mutation lock lives in the arena and is reapable

Phase 1's `Mutex` is per-process.

```rust
#[repr(C, align(64))]
pub struct TopoLock {
    /// 0 = free, else participant_slot + 1
    pub owner: AtomicU64,
    pub acquired_at_nanos: AtomicI64,
    _pad: [u8; 48],
}
```

Acquire is `compare_exchange(0, slot + 1, AcqRel, Acquire)` with bounded spin. On failure, resolve the owner (§5) and check liveness (§6.2); if dead, CAS to steal. A1 makes an abandoned mutation leave no trace, so stealing needs no rollback: the new holder re-copies from the active block.

### A3 — Claim ownership is a participant slot, not a PID

A writer killed between the state CAS and a separate PID store would leave `state = HELD, owner_pid = 0`: a permanently leaked edge.

**Fix.** One atomic word carries state and identity; the identity is an indirection into a participant record fully written at attach.

```rust
#[repr(C, align(64))]
pub struct ClaimRecord {
    /// 0 = free, else participant_slot + 1. Claim and identity publish atomically.
    pub owner: AtomicU64,
    /// Incremented on every reap and every successful claim. Fences zombies (A4).
    pub epoch: AtomicU64,
    /// Advisory only. NEVER a reaping trigger on its own (§6.4).
    pub heartbeat: AtomicU64,
    pub clock_offset_nanos: AtomicI64,
    _pad: [u8; 32],
}
```

Claim: `owner.compare_exchange(0, slot + 1, AcqRel, Acquire)`, then `epoch.fetch_add(1, AcqRel)`; the `Publisher` records the epoch. §6.1 supersedes this record's reaping role; the slot indirection stays required.

### A4 — `push` must verify the claim epoch

A `SIGSTOP`ped or stalled process can be judged dead, reaped, then resume and push to an edge another process owns.

```rust
if self.claim.epoch.load(Ordering::Relaxed) != self.epoch {
    return Err(PushError::ClaimRevoked { edge: self.id });
}
```

~1 ns, one relaxed load on an already-touched line. With §6.1 the zombie is impossible by construction; keep the check as defence in depth, and do not describe it in comments as the sole barrier.

### A5 — The slot sequence writer forces parity instead of incrementing

A writer killed between the odd and even `seq` stores leaves the slot odd; when the ring wraps the next writer's `s+1` lands even, inverting the protocol.

```rust
let s    = slot.seq.load(Ordering::Relaxed);
let odd  = s | 1;                                    // self-heals a stale odd
slot.seq.store(odd, Ordering::Relaxed);
core::sync::atomic::fence(Ordering::Release);
// ...write stamp and pose data...
slot.seq.store(odd.wrapping_add(1), Ordering::Release);
```

Any reader that saw the stale odd retried without reading. Additionally, **claim acquisition normalizes the slot at `head & mask`**.

### A6 — The arena gains a participant table (layout change)

New region of `max_participants * 128` bytes between the claim table and the edge table. `ArenaHeader` gains `participant_table_off: u32`, `max_participants: u32` and `participant_count: AtomicU32` from `_reserved`. Default `max_participants = 64`. **The only amendment that changes the layout.**

### A7 — Header identity fields

`ArenaHeader` gains `owner_start_time: u64` beside `creator_pid`, and `boot_id: [u8; 16]` replaces the `u64`. `instance_uuid: [u8; 16]` joins them (§3.6 step 4): two processes that believe they share an arena but print different `instance_uuid`s are on different arenas. All fit in `_reserved`.

### A8 — Interning must not spin forever on a dead claimant

Phase 1 §5.1's interning waits for `ids[i] != U32_MAX` with an unbounded spin. A process that wins the hash-slot CAS and dies before publishing the id wedges every future interner of that name. This is `intern.after_hash_cas_before_id_store` (§11.3).

**Fix.** Record the claimant beside the hash, bound the spin, take over from a dead claimant:

```rust
/// Parallel to `hashes`/`ids`: the participant slot that won the CAS, + 1.
/// Written BEFORE the hash is published.
claiming: [AtomicU32],

// waiter, after INTERN_SPIN_LIMIT iterations:
let owner = claiming[i].load(Acquire);
if owner != 0 && !is_alive(&participants[(owner - 1) as usize], boot) {
    if claiming[i].compare_exchange(owner, my_slot + 1, AcqRel, Acquire).is_ok() {
        write_record(id); ids[i].store(id, Release);
    }
}
```

The takeover is idempotent and CAS-guarded. `is_alive` is §6.2's predicate and fails **safe**: an unreadable `/proc` means "alive". Phase 1's `ID_FAILED` sentinel handles the *capacity* failure; A8 handles the *crash* failure.

## 2. Platform, dependencies, feature gating

**NORMATIVE.** Shared memory is **Linux-only**, requiring **kernel ≥ 3.17** (`memfd_create`, `F_ADD_SEALS`) and **≥ 3.15** (OFD locks, §3.3). Target 5.15 (Ubuntu 22.04 / JetPack 6) and current stable. `MADV_POPULATE_WRITE` (§7.1) needs ≥ 5.14 and has a fallback.

No POSIX abstraction layer. macOS and Windows keep `HeapArena`; a file-backed unsealed `MappedArena` for macOS is acceptable *later*, labelled dev-only, and must not shape the Linux design.

| Crate | New dependencies |
|---|---|
| `tf_tree_arena` | `rustix` (feature `shm`, `mm`, `fs`, `net`) |
| `tf_tree_ipc` (new) | `rustix`, **and `libc` for `fcntl(F_OFD_*)` only** |
| ~~`tf_tree_record` (new)~~ | **Retired by [`0047`](./decisions/0047-the-recording-this-reader-would-refuse.md).** MCAP arrived in `tf_tree_ingest`; no crate here declares `serde` |
| `tf_tree_core` | **none.** |

`tf_tree_core` gaining a dependency here is a design failure; stop and report it.

**The `libc` exception.** `rustix` 1.1 has no OFD locking (its `fcntl_lock` is classic whole-file `F_SETLK`, which §3.3 rejects). Hand-rolling the syscall pinned syscall numbers and `struct flock` by hand and refused to compile off x86-64/aarch64; `libc` adds no C build step. Scope it to `tf_tree_ipc` and that one call.

## 3. Discovery, rendezvous, and ownership

A process calls `tf_tree::open()` and either joins the arena on this machine or creates it: no config file, no daemon, no start-order requirement, and no possibility of two processes silently on different arenas.

**Do not implement leader election — borrow the kernel's.** A rendezvous needs mutual exclusion, automatic release on holder death, and a way to ask whether anyone holds it. Linux file locks give all three with no timeouts, heartbeats or stale state surviving a `SIGKILL`.

### 3.1 The sharing boundary is the runtime directory — NORMATIVE

Two processes share an arena **if and only if they resolve to the same runtime directory, domain, and name.** This should be the first sentence of the user-facing docs.

```
<runtime_dir>/<domain>/<name>.lock     # rendezvous + kernel-managed liveness
<runtime_dir>/<domain>/<name>.sock     # SOCK_SEQPACKET, owner-bound, FD passing
```

`runtime_dir`, first hit wins:

1. `$TF_TREE_RUNTIME_DIR`
2. `$XDG_RUNTIME_DIR/tf_tree`
3. `/run/tf_tree` if writable
4. `/tmp/tf_tree-<uid>`, created mode `0700`

**Containers:** sharing the runtime directory is a volume mount (`-v /run/tf_tree:/run/tf_tree`); not sharing it is complete isolation, inspectable with `ls`. Do **not** use abstract Unix sockets, which tie the boundary to the network namespace.

**NORMATIVE check:** `statfs` the runtime directory at open and reject NFS (`0x6969`) and CIFS; the rendezvous depends on exact lock semantics.

**An arena no runtime directory names sits outside this boundary, and putting it back inside by hand is out of contract.** `TreeBuilder::build_shared` creates a segment whose fd is the capability: no `.lock`, no `.sock`, and so no byte for its participant record to be judged by. Binding an `OwnerServer` over it publishes it into a rendezvous anyway, and every peer that joins judges the creator **dead** while it is publishing and reclaims its record and claims. [`0031`](./decisions/0031-the-participant-record-with-no-byte.md) decided that composition **out of contract** (2026-09-18); no single call can refuse it. The supported way to serve a created arena is `tf_tree::Open`'s create arm, which takes its lock byte *before* it builds (§3.4).

### 3.2 Identity and defaults

```
domain: $TF_TREE_DOMAIN, else $ROS_DOMAIN_ID, else 0
name:   $TF_TREE_NAME,   else "default"
```

Falling back to `ROS_DOMAIN_ID` is deliberate: tf_tree partitions exactly as the rest of the ROS 2 stack does.

```rust
// A consumer. The defaults are the consumer: read-only, and `CreatePolicy::Never`.
let tree = tf_tree::open()?;                 // join, or ArenaAbsent
let tree = tf_tree::Open::new().name("robot")?.open()?;

// A consumer that may start before the publisher: `.domain(7).name("robot")?.await_open(Duration::from_secs(5))?` (0019 §2b).

// A creator (read-write is required; a read-only attach cannot create):
let tree = tf_tree::Open::new().domain(7).name("robot")?.mode(AttachMode::ReadWrite)
    .create(CreatePolicy::IfAbsent)   // IfAbsent | Never | Always
    .layout_if_creating(layout).open()?;
```

`CreatePolicy::Never` is the default ([`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md) §2a): a consumer that creates an arena the estimator has not populated looks healthy and finds nothing. `AttachMode::ReadOnly` with a creating policy is `OpenError::ReadOnlyCannotCreate`.

### 3.3 The lock file — NORMATIVE

A small regular file used as a lock substrate with **open file description locks** (`F_OFD_SETLK`, Linux ≥ 3.15). Not classic POSIX locks, which are dropped when *any* fd to the file closes anywhere in the process.

| Offset | Meaning |
|---|---|
| byte 0 | **Ownership.** Exclusive. The holder serves the socket. |
| byte 1 | **Topology mutation** (A2). Exclusive, held for one `Tree::reparent`. |
| bytes 2–15 | reserved |
| bytes 16 + *i* | **Participant liveness** for slot *i*. Exclusive, held for the lifetime of the attachment. |
| 4096 + 64·*i* | **Identity record** for slot *i*: pid (`0..4`), start_time (`4..12`), boot_id (`12..28`), mode (`28`), name (`32..48`), pid_ns_inode (`48..56`). `29..32` and `56..64` are padding and read zero. Written with `pwrite` before taking the slot lock. Advisory; diagnostics only. |

**`pid_ns_inode` is the `nsfs` inode of the PID namespace its writer's pid is drawn from; `0` means *unknown*** ([`0033`](./decisions/0033-the-identity-record-cannot-name-a-namespace.md)). A recorded `pid` is namespace-local and `boot_id` is identical across namespaces, so an observer compares it against its **own** namespace. Zero must keep the pre-`0033` behaviour. This is the lock file, not the arena: no arena field moved and no layout hash changed.

Verified on Linux 6.18:

| Operation | Result |
|---|---|
| Second process takes a held byte | `EAGAIN` |
| Holder dies without unlocking | released by the kernel at the end of the holder's exit — after any core dump and teardown of its address space ([`0057`](./decisions/0057-an-owner-is-not-dead-until-its-files-close.md)) |
| `F_OFD_GETLK` on a free byte | `l_type = F_UNLCK` |
| `F_OFD_GETLK` on a held byte | held, but **`l_pid = -1`** |

An OFD lock belongs to a description, so `GETLK` cannot report a PID: the lock file answers *"is anyone alive?"* and *"who?"* comes from the identity records, which is why they are plain `pwrite` data. Liveness is a kernel fact, so **`/proc` parsing and PID-reuse defence are off the rendezvous path**; they remain only for the arena's advisory participant table (§5).

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
        // another process holds byte 0: an owner mid-bind, or another open()
        // passing through steps 2-4, which will hand it back (0057)
        backoff; continue
    }

    // 3. DELETED (#275, 0037). It skipped step 4 for a process declaring it
    //    already held the arena, which no new file description can verify.
    //    Do not re-add it.

    // 4. SPLIT-BRAIN CHECK. Is any participant byte locked?
    if any participant byte is held {
        // an arena exists and is alive, but its holder has not taken over yet.
        release byte 0; backoff; continue    // yield to the real participant
    }

    // 5. Serve.
    if creating {
        // The creator's slot is 0, and the ACQUIRE is the check: step 4 is a
        // separate pass, so participant byte 0 can change hands between them.
        if F_OFD_SETLK(participant byte 0, exclusive) fails {
            release the ownership byte; backoff; continue   // step 4, arriving late
        }
        pwrite identity for slot 0
        memfd_create; ftruncate; mmap; init header; seal (§3.6)
    }
    unlink stale sock; bind sock.tmp; chmod; rename -> sock; listen
    return Created
}
on timeout -> Err(ArenaHeldButUnreachable { holder_slots, first_slot, first_pid, ownership_held })
```

There is no takeover branch: a heir is already a participant and registers nothing ([`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md) question 3); §3.5 is implemented as a method on the session it already holds, never as a re-entry into this algorithm ([`0037`](./decisions/0037-a-takeover-is-not-a-second-open.md)). A fresh `open()` against an ownerless arena with surviving participants therefore times out until a survivor inherits.

**Step 4 is the whole design.** Without it the owner dies, a fresh process wins the ownership lock before survivors notice the `HUP`, and creates a *second* arena while the survivors keep the first: two live, silently diverging arenas. The check is **deterministic, not a grace period**: if any participant byte is locked a live arena exists and a fresh process must not create one.

**A creator takes participant slot 0, and the acquire is the check.** The arena's first `FREE` record is 0 and the facade indexes lock byte and arena record with **one** integer (#201). `any_participant_held` probes byte 0 first and then 63 more, so byte 0 can be taken for the rest of that scan (measured: 2242 of 4000 toggled iterations took a non-zero byte; `LockFile::try_take_participant` is public API). So steps 4 and 5 share one `F_OFD_SETLK` on participant byte 0; contention is step 4's condition arriving late and takes step 4's branch.

**The escape hatch (`CreatePolicy::Always`, §0.0's `--force-new` row) skips step 4 by design** and creates iff nothing is serving **and** the ownership byte is free **and** participant byte 0 is free. Byte 0 is the owner's for its whole life and joiners get slots `>= 1`, so the usual reason the three are free is that the owner is gone and non-owner participants survive — the stranded-participant case. "The owner is gone" is not the rule: `Session::release_ownership` (§3.5) frees the ownership byte and keeps byte 0, and the hatch still refuses. It cannot pass a live holder of byte 0 or the ownership byte; the remedy is to stop that process, after which an ordinary `IfAbsent` create works (`a_live_participant_prevents_a_second_arena`, `crates/tf_tree_ipc/tests/multiprocess.rs`). A wedge *requires* a live holder: if every holder were dead no participant byte would be held. Never take the path automatically.

The timeout case is correct behaviour: if a participant is `SIGSTOP`ped and never takes over, no new process can join, because the alternative is divergence. The error names the stuck slots and identities.

### 3.5 Ownership migrates; the data plane never pauses — NORMATIVE

Ownership is a **role**: the arena is the memfd, which lives as long as any mapping; the owner is whichever participant holds byte 0 and the listening socket. A surviving **read-write** participant that observes the owner's hangup inherits the role:

```
keep serving lookups from the existing mapping -- untouched, uninterrupted

poll(OUR OWN attach socket, timeout 0) for POLLHUP/POLLERR   -- the caller's loop
  no hangup -> owner alive; nothing to do
  hangup    -> F_OFD_GETLK(byte 0)                             -- 0043: is the ROLE vacant?
    held -> somebody took over, is mid-bind, or a fresh open() holds it in
            passing through §3.4 steps 2-4 (0057). Nothing to do THIS call.
    free -> read-only attachment: cannot be the heir (D18).
            else F_OFD_SETLK(byte 0) ON THE DESCRIPTION THIS SESSION ALREADY HOLDS
              acquired  -> bind a pid-suffixed socket, listen, rename it over the
                           rendezvous path, serve OUR EXISTING segment fd; on any
                           failure release byte 0 and stay a plain participant
              contended -> another survivor or a passing open() holds it; KEEP OUR
                           SLOT and retry on the next poll -- no single
                           non-Inherited answer is final (0043, 0057)
```

Five requirements:

1. **The ownership lock is taken on the file description the session already holds. A takeover may not be expressed as a second `open()`.** From a fresh description `F_OFD_GETLK` cannot distinguish "I hold byte *n*" from "a live peer holds byte *n*", so the declaration a takeover rests on cannot be verified; taking the lock on the existing description makes the invariant structural.
2. **The heir keeps its existing participant slot, byte and arena record and does not register again.** The slot is baked into every claim and topology guard it holds (A3); a second registration would arrange for its own live claims to be reaped.
3. **The heir serves the segment it already has**, never a fresh `memfd_create`, which would fork the tree.
4. **Publication is a `rename`, not unlink-then-bind**, so a client sees the old socket or a listening one, never a half-built one (§3.7).
5. **Serving must stop before byte 0 is released**, or a successor can bind while the old owner still answers handshakes: two servers on one path. Expressed as **field declaration order** on `Attachment::Owner` (RFC 1857 drop order), not an `impl Drop`.

The shipped spelling: `tf_tree_ipc::peer_hung_up` and `tf_tree::Tree::owner_lost` are the poll, `Session::ownership_held` the `GETLK` that follows a hangup, `Session::take_over_ownership` the lock, and `Tree::inherit_ownership` the seam that binds and serves, reporting `Inheritance::{Inherited, OwnerAlive, Contended, ReadOnly, NotApplicable}`.

**NORMATIVE.** `owner_lost()` answers `true` once the survivor's attach connection has hung up and the last open file description holding byte 0 has closed ([`0043`](./decisions/0043-owner-lost-is-a-question-about-the-owner.md)). For a dying owner that is the end of its exit, including any core dump and address-space teardown; a `fork` child sharing those descriptions holds them until it exits. tf_tree adds no delay, heartbeat or timeout (D17) ([`0057`](./decisions/0057-an-owner-is-not-dead-until-its-files-close.md) Decision 5; §3.7 step 9 carries one host's figures).

**The trigger is the caller's, and this is NORMATIVE too: no background thread, no daemon, no watcher a user must run** ([`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md)). A survivor evaluates `owner_lost()` in its own loop and nothing evaluates it on its behalf. A fleet whose survivors never call it ends up with an ownerless arena and joiners that time out on `ArenaHeldButUnreachable`; `docs/RUNBOOK.md` says so where an operator will meet it.

**Recovery capacity is whatever was attached and eligible at the instant the ownership role fell vacant, and it cannot be added afterwards — NORMATIVE** ([`0055`](./decisions/0055-the-recovery-capacity-a-fleet-cannot-add-later.md) part 1). An ownerless arena with any participant byte held admits no new **rendezvous** attachment: nothing serves, so §3.7's join cannot start and §3.4's split-brain check refuses an ordinary create. `Tree::attach_shared` and `attach_shared_at` refuse `AttachMode::ReadWrite` (`ShmError::ReadWriteNeedsRendezvous`), so the set of processes that could inherit that arena only shrinks from that instant. `CreatePolicy::Always` still creates in this state but *abandons* the held arena and its creator is an `Attachment::Owner`, so `inherit_ownership()` answers `NotApplicable`.

The axis is eligibility, not mode; three ways to be ineligible (a census shows only the first):

1. **Read-only.** `inherit_ownership()` answers `ReadOnly`: an owner writes the participant table on every grant and a `PROT_READ` mapping cannot (D18).
2. **Read-write and never polling.** An attachment that never evaluates `owner_lost()` is not recovery capacity.
3. **Not attached at that instant.** A supervisor-restarted publisher is read-write and polling and still not capacity, because the door closed while it was down.

A participant built before 2026-08-28 cannot inherit at all; that is a release question, carried in `docs/RUNBOOK.md` as its own triage bullet. Nothing in the library enforces any of this: `0055` answered *"no mechanism"* because every candidate is the thread or daemon this section refuses.

**Lookups do not stop, slow down, or observe anything during a takeover.** `Plan::at` touches the mapping and the `Guard` and nothing else; ownership lives in the control plane. `inherit_ownership` takes `&self` ([`0044`](./decisions/0044-recovery-the-languages-a-robot-is-written-in-cannot-reach.md)), so a control loop may hold a `Guard` across it (`a_guard_may_be_held_across_inheriting_ownership`, a compile-time property). Ownership is neither configured nor negotiated: it is *inherited*, and the kernel picks the heir.

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

Step 5 is load-bearing (Appendix B: sealing `SHRINK|GROW` succeeds with a writable mapping, `WRITE` is `EBUSY`, shrinking after sealing is `EPERM`, `F_GET_SEALS` is `0x7`). **Sealing against shrink makes `SIGBUS` structurally impossible.** Do not skip step 5 and do not substitute `shm_open`, which cannot be sealed and leaves stale segments in `/dev/shm`.

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

**Step 9:** the socket is how a participant learns the *owner* has died, with no polling — **once its attach connection has hung up and the last description holding byte 0 has closed (§3.5), not when the owner is signalled.** The kernel closes a dying process's files only after any core dump and address-space teardown. Measured on one host ([`0057`](./decisions/0057-an-owner-is-not-dead-until-its-files-close.md)): a `SIGKILL`ed 2.7 MiB owner was seen in ~0.24 ms median, a 1 GiB one in ~98 ms (dirty 4 KiB pages), and a 2.7 MiB `abort()` dumping through a pipe `core_pattern` in ~1.1 s; for that window no survivor could inherit and every fresh join was refused. A `fork` child sharing the owner's descriptions holds the socket and byte 0 until it exits (§6.2; `RUNBOOK.md`). Participant death is detected by the lock file, owner death by the socket; neither involves a timeout.

Message structs are fixed-size `#[repr(C)]`, little-endian, over `SOCK_SEQPACKET`:

```rust
#[repr(C)] pub struct HelloRequest {
    magic: [u8; 8] /* b"TF_TREE\0" */, format_version: u32, layout_hash: u32,
    mode: u8 /* 0 = ReadOnly, 1 = ReadWrite */, _pad: [u8; 7],
    client_pid: u32, _pad2: u32, client_start_time: u64,
    client_boot_id: [u8; 16], client_name: [u8; 32],
}
#[repr(C)] pub struct HelloResponse {
    magic: [u8; 8], status: u32 /* 0 = Ok */, format_version: u32, layout_hash: u32,
    participant_slot: u32 /* matches the lock-file byte the client must take */,
    arena_size: u64, instance_uuid: [u8; 16], owner_pid: u32, _pad: u32,
}
```

Rejections: `VersionMismatch`, `LayoutMismatch`, `BootIdMismatch`, `NoParticipantSlots`, `ModeNotPermitted`, `Malformed`. `LayoutMismatch` is the one operators will hit (a binary built against a different struct layout) and should print both hashes.

> **Erratum ([`0055`](./decisions/0055-the-recovery-capacity-a-fleet-cannot-add-later.md) step 7):** `IpcError::HandshakeRejected` carries the owner's `format_version` and `layout_hash` and not the client's (`tf_tree_ipc` cannot read this build's constants), and per-status remedies moved from the message to `RUNBOOK.md`'s `HandshakeRejected` table because rendered messages overflowed the 256-byte C buffer. The rule is [`0059`](./decisions/0059-the-arena-errors-that-cannot-describe-themselves.md)'s convention (g): facts plus the variant name as search key, remedy in the runbook.

### 3.8 Capacity without planning — NORMATIVE

Fixed capacity (D4) is in tension with zero-config startup: whoever creates the arena fixes the layout. **Virtual capacity is nearly free.** On Linux 6.18 a 1 GiB memfd charges 0 KiB after `ftruncate` and after `mmap` without `MAP_POPULATE`, and exactly 16 MiB after touching 16 MiB. So the default layout is generous (1024 frames, 1024 edges, 8192 samples per edge, ~600 MiB of address space) and resident cost is what is declared and used. `MADV_WILLNEED` does *not* pre-fault a memfd; use per-edge `MADV_POPULATE_WRITE` (§7.1). `doctor` warns at 80% occupancy of frames, edges or participants, and `ArenaFull` must state the limit and that raising it requires recreating the arena.

### 3.9 Teardown

- **A participant dies** → its lock byte releases, its mapping drops, and the owner reaps its arena-side records (§6). The owner's hangup callback collects both the participant *record* and every **claim** it held, so a restarted node is not refused its own edges with `EdgeAlreadyClaimed`. The remaining stale-claim producers are a dead **owner** (no hangup observed) and a byte-less `build_shared` participant (out of contract, [`0031`](./decisions/0031-the-participant-record-with-no-byte.md)); for those `Tree::reap()` from a surviving read-write participant is the only collector, reachable from Rust, C, C++ and Python (`tft_tree_reap_dead`, [`0044`](./decisions/0044-recovery-the-languages-a-robot-is-written-in-cannot-reach.md)).
- **The owner dies** → surviving participants take over (§3.5). Lookups never pause.
- **The last mapping drops** → the kernel frees the segment. No stale segments, ever.
- A stale **socket path** may persist; the winner of ownership unlinks it. A stale **lock file** is harmless: it holds only locks, which cannot be stale.

### 3.10 Trust model — NORMATIVE, and state it in the public docs

Participants are **mutually trusting, same-user, cooperating processes**. A read-write participant can corrupt any part of the arena. The design does guarantee:

- A **read-only** participant cannot corrupt anything, enforced by the MMU (§8). This is the only real boundary, and the default.
- A participant that **crashes**, at any instruction, cannot corrupt anything or wedge any other participant (tested by fault injection, §11.3).
- A participant that **hangs** cannot corrupt anything and cannot be mistaken for a crashed one (§6).

"Shared memory IPC is not a sandbox" belongs in the README.

## 4. `MappedArena`

**NORMATIVE:** the diff against Phase 1 outside `tf_tree_arena` and `tf_tree_ipc` must be **zero lines in the read path**. `PoseSlot`, `EdgeBuffer`, `Plan::at`, bracket search and interning are byte-identical code on a different base pointer.

**The premise is tested.** `crates/tf_tree_bench/tests/relocation.rs` byte-copies a populated arena to a different address, wraps the copy in a minimal `Arena` impl, and requires **bit-identical** results across every frame pair in the fixture plus frame-name resolution and header validation. It guards against a vacuous pass twice (the copy must land at a different address; more than 1000 queries must be compared). Keep it green: a cached absolute address would otherwise surface in another process as a wild read.

`Drop` order is fixed: publish detach in the participant record, `munmap`, close the socket, close the fd, so the owner's reap path never races a half-torn-down participant.

Test that `Tree` is generic over `A: Arena` with no `MappedArena`-specific branches, and (compile-fail) that `Publisher` cannot be constructed from a `ReadOnly` arena.

## 5. Participant registry

`ParticipantRecord` is `#[repr(C, align(64))]`, 128 bytes (asserted at compile time): `state: AtomicU32`, `pid: AtomicU32`, `start_time: AtomicU64` (`/proc/<pid>/stat` field 22, defeats PID reuse), `incarnation: AtomicU64`, `attached_at_nanos: AtomicI64`, `heartbeat: AtomicU64`, padding. **`state` is a packed word, not a plain enum:** `state_of(word) = word & 0b11` is the lifecycle (FREE = 0, RESERVED = 1, LIVE = 2) and the incarnation sits above it, `live_word(inc) = (inc << 2) | LIVE`, so testing `state == 2` finds nothing (`fill_slot` publishes `incarnation + 1`). Published last; there is no `detaching` state.

A departing participant goes straight to `FREE`; "leaving" versus "gone" is the socket's job (D17). Every field is atomic, because two processes read while a third publishes; neither Miri nor loom crosses a process boundary (§11.1), so what holds this is the type plus the review rule that an arena field two processes touch is atomic. `incarnation` makes a reaped-then-reused slot distinguishable from the same slot still held. Read-only versus read-write is not in the record; D18's enforcement is the MMU.

**The owner assigns the slot; the joiner writes the record.** The owner's accept loop scans for an index whose lock byte the kernel reports free and whose arena record is absent or **collectable** (§5.1's predicate), reclaiming a collectable one — `reclamation_verdict` then `ParticipantTable::reclaim` — *before* granting it, because `fill_slot` CASes from `FREE` ([`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md) step 3). It returns the slot as `HelloResponse.participant_slot` (§3.7). The *joiner* writes its own record **with a CAS, after** taking the lock byte for that slot. A **creator** finds its own free record through the same CAS after its byte, on the byte `Open::register_creator` *takes* ([`0035`](./decisions/0035-the-creators-slot-is-taken-not-found.md)). **There is no third registrant**: a taker-over is already a participant and keeps its slot, byte and record. "After its byte" holds on every path that has a byte; a directly-called `TreeBuilder::build_shared` opens no lock file, so there the CAS is the only ordering (§11.3's `attach.after_slot_assigned_before_publish` row), which costs nothing while the record is unobserved — the shape supported *for* handing an fd to a child (§3.1). `Tree::attach_shared` / `attach_shared_at` refuse `ReadWrite` and write no record on `ReadOnly`; `TreeBuilder::build` is a heap tree.

`fill_slot` opens with `compare_exchange(FREE, RESERVED)`, writes identity fields under `RESERVED` (where no reader may trust them), and release-stores the live word last. **That publication order makes A3's indirection sound**: a claim can only name a slot some process drove to `LIVE`, and whoever sees `LIVE` sees every field. A process killed in between leaves `RESERVED`: distinguishable garbage, the state §11.3's `attach.after_slot_assigned_before_publish` is about.

Ordering differs between the two records deliberately: for the lock-file identity record it is lock-then-write (write-then-lock loses the race it exists to win); for the arena record it is byte, CAS, fields, publish, so nothing is left in the arena on a retry. Neither leaves a record a reader can mistake for a live participant.

### 5.1 Identity is advisory; the lock file is authoritative — NORMATIVE

**Liveness comes from the participant's OFD lock byte (§3.3), never from these records.** Any code deciding liveness from `state` or `heartbeat` is a bug.

`reclamation_verdict` (`crates/tf_tree/src/open.rs`, [`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md) step 2) is the single predicate every reclamation decision goes through and answers from the lock byte alone — no `/proc`, no `heartbeat`. It reads `state` only to ask *is there a record here*, never *is its process alive*: **a `FREE` word is very often a live process** (a read-only joiner takes its byte in the handshake and registers no record, since a `PROT_READ` mapping cannot write the table), so the predicate reports such a slot *unknown*. Properties a changer needs:

- It **skips this process's own slot**, because `F_OFD_GETLK` reports only conflicting locks.
- It **observes the `state` word before it probes the byte**: the `Acquire` load of a live word synchronises with `fill_slot`'s `Release`, so a later byte probe must see the byte held. Reversed, or taken from one up-front `held_participants()` mask, it erases a published record (`0028` question 6).
- It is **sound only because steps 0b and 0c landed**: every rendezvous participant holds a byte, and the byte at index `slot` belongs to the record at index `slot`.
- It is **total over the rendezvous population, not over the table**: a `build_shared` creator served by hand reads dead to it (`a_byteless_creators_record_reads_dead_and_is_reaped_while_it_publishes`), which [`0031`](./decisions/0031-the-participant-record-with-no-byte.md) answered *out of contract*.

A second copy of this predicate is the defect `0028` was opened about.

**The ordering above binds a *probe*; A2's topology lock takes an *acquire* — NORMATIVE.** `Tree::reparent` takes an exclusive `F_OFD_SETLK` on **byte 1** and holds it for the whole mutation, excluding every subsequent take, so its order is byte-then-word ([`0029`](./decisions/0029-the-topology-lock-is-a-kernel-lock.md)). The invariant: **the topology word is CASed non-zero only while its process holds byte 1, and byte 1 is released only after the word is; a process holding byte 1 that observes a non-zero word is looking at a holder that is either dead or has no lock file.** The `/proc` triple decides only the second, and only to withhold a steal. Anything that reverses either order, or uses `held_participants()`'s up-front mask on this path, gives that invariant up.

`(pid, start_time, boot_id)` remains the identity triple for diagnostics and the forced-create path (`CreatePolicy::Always`; §0.0's `--force-new` row is authoritative). A bare PID is not an identity: PIDs recycle fast under a low `pid_max`.

`start_time` (field 22 of `/proc/<pid>/stat`, clock ticks since boot) is parsed carefully. `doctor` does not report it (`ParticipantInfo` in `crates/tf_tree_cli/src/doctor.rs` carries only fields its checks read, and composing `(pid, start_time)` against `/proc` in the CLI would be a second liveness spelling). No takeover path prints it: inheritance forms no verdict about anyone else's process. What parses it is `read_start_time` (`crates/tf_tree/src/tree.rs`) feeding `alive_given` — the predicate §0.0's row records, so §5.1's original "no longer on any correctness-critical path" is false there — and `client_start_time` in the attach `Hello`, which no reader consumes.

**The parsing trap — NORMATIVE.** Field 2 is `comm`, which may contain spaces *and parentheses*; splitting on whitespace and taking index 21 silently returns another field. Locate the **last** `)` and parse from there:

```rust
let rp = raw.rfind(')').ok_or(ProcParseError)?;
let field22 = raw[rp + 2..].split_ascii_whitespace().nth(19).ok_or(ProcParseError)?;
```

Appendix B has the failing naive parse; include that case as a unit test against a fixture string.

## 6. Liveness and reaping

### 6.1 Claims are kernel locks — NORMATIVE

**`claim(edge)` takes an exclusive OFD lock on `CLAIM_BASE + edge_id` in the lock file**, held for the life of the `Publisher`.

This removes heartbeat freshness heuristics, `/proc` liveness checks and PID-reuse defence from the path; reaping is `F_OFD_GETLK` says free ⇒ definitively dead; and the zombie writer (§A4) is **impossible by construction**.

A `SIGSTOP`ped or GC-stalled writer **still holds its lock**, so it cannot be reaped while alive and a second claimer gets `EdgeAlreadyClaimed`. No window, no timeout, no heuristic that can be wrong.

**Two sources of truth, one authoritative.** `ClaimRecord` remains for diagnostics and for readers asking who publishes an edge, but **the lock file is authoritative**: claim = take the lock, then write the record; reap = lock free and record held ⇒ clear the record. The record may lag; the lock never does. Any decision from `ClaimRecord` alone is a bug. **A4 is retained but downgraded** to defence in depth; update its comment so nobody removes it believing it was only for the zombie case.

### 6.2 Fork is still the exception — NORMATIVE

OFD locks are held by the open file description, which **survives `fork`**: parent and child both "hold" every claim and both pass A4's epoch check. `MADV_DONTFORK` (§7.3) closes this: the child has no mapping and faults loudly. `MADV_DONTFORK` and OFD claims are a matched pair; a comment at each site must say so.

### 6.3 What remains of reaping

Arena-side cleanup after a death, by any read-write participant, all steps idempotent:

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

The owner runs this on socket `HUP`; others run it lazily when a claim appears held. **Reaping must not be owner-only** — that would leak every claim held when the owner died.

### 6.4 Heartbeats are diagnostics only — NORMATIVE

`heartbeat` and `clock_offset_nanos` remain in `ClaimRecord` and are **never** a reaping trigger. Neither detects a hang: a live process that stopped publishing is found from its *stamps* (`TFT009`, `TFT008`).

**Write schedules differ** ([`0036`](./decisions/0036-the-receipt-time-the-format-already-reserved.md)). `heartbeat` is bumped on **every** push inside `SampleRing::push` ([`0014`](./decisions/0014-the-push-heartbeat-is-a-store.md)). `clock_offset_nanos` needs a wall-clock reading (38.4 ns against a ~5 ns push), so `tf_tree`'s `EdgeWriter` **samples** it on a claim's **first** push and then once every `max(nominal_rate_mhz / 1000, 1)` pushes (one per second of data at ≥ 1 Hz, one per push below that); an edge declaring no rate gets a fixed **1024**, which at 10 Hz is 102 s. A claim clears the field it inherits. **`0` means never sampled and is written by nothing**: an offset computing to zero is stored as `1`. The clock is read **in the facade, after the ring write returns**, never inside `SampleRing::push`, where it would widen the seqlock window into readers' `SlotContended` retries. **Only a `SystemDomain` (tag 0) edge records anything**, since `wall clock - stamp` is an offset only where both share an epoch.

It stores the *offset* `wall clock - stamp`, not a receipt time: sampling means the ring's newest stamp belongs to a later push than the receipt, so a bare receipt time cannot be paired with a stamp by any reader. Sampling costs +1.0–1.1 ns per push at the 1024 default (`just push-sampler-cost`); `docs/PHASE1.md` §11.2 tabulates both ends and `docs/benchmarks/EVIDENCE.md` carries provenance.

Reaping on staleness would be actively unsafe: a 0.2 Hz map-to-odom correction is indistinguishable from a hung writer under any timeout short enough to be useful. With claims as kernel locks, **do not add such a policy**, not even opt-in.

## 7. Mapping policy

### 7.1 Page population is per-edge, not per-arena — NORMATIVE

A minor fault costs single-digit microseconds against a 150 ns p50 gate. But §3.8's generous layout means `MAP_POPULATE` over ~600 MiB would charge hundreds of megabytes nobody declared. So populate at **take-up** granularity:

- `mmap` **without** `MAP_POPULATE`.
- When an edge is **taken up**, `madvise(MADV_POPULATE_WRITE|READ)` (Linux ≥ 5.14) over its stamp and pose ranges; older kernels touch one byte per page. **Amended by [`0024`](./decisions/0024-population-is-per-edge-at-take-up.md):** the moments are `Tree::claim` for a writer and plan compilation for a reader, both off the query path by D3 ([`0004`](./decisions/0004-builder-time-edge-declaration.md) deleted `declare_dynamic`). Populating every declared ring at attach is *per-arena* population and was measured at **5.2×** on a process using 4 of 64 declared edges.
- On attach, populate the header, frame table, topology blocks, claim table, participant table, edge table and both counter regions. **Not the two ring arenas**, 99.8% of a large arena.

**`MADV_WILLNEED` does not work here** (zero change in charged pages on a memfd). §12 requires first-access-after-attach rows with population on and off.

### 7.2 Huge pages

`madvise(MADV_HUGEPAGE)`, best-effort. A 260 MB arena needs ~63 000 TLB entries on 4 KB pages, 130 on 2 MB. THP must be `madvise` or `always`; `doctor` reports the setting and the benchmark reports both configurations.

### 7.3 `MADV_DONTFORK` — NORMATIVE, and easy to forget

A `MAP_SHARED` mapping survives `fork()`, and the child inherits the parent's `Publisher` structs with their claim epochs, so both processes pass A4 and write the same edge. `madvise(base, len, MADV_DONTFORK)` at attach, before any fork, removes the mapping from the child, which faults loudly. The child must re-attach for its own slot and claims. **Document this prominently**: Python's `multiprocessing` defaults to `fork` on Linux (§14).

### 7.4 Memory locking

> **AMENDED by [`0049`](./decisions/0049-the-flag-that-prefaults-the-arena.md). There is no `LockPolicy` and no `mlock` call in this library, and none is owed. This section is history.**
>
> The reason is `docs/API.md` §8.3's second bullet: a library that locks memory spends an `RLIMIT_MEMLOCK` budget it cannot see, and the embedding application knows how much of the over-provisioned arena a node touches. `TFT016` reports the limit against arena size, which is one term of two, and its message says its silence is not a clearance. `MLOCK_ONFAULT` does not prefault, but it does not "add nothing over §7.1": §7.1 establishes PTEs once and the flag keeps them (measured with `VM_LOCKED` as the only variable). Whether that matters on a swapless host is **undetermined**; `crates/tf_tree_bench/examples/mlock_probe.rs` is the executor.

The original `LockPolicy::{ None, Populate, Locked }` design is kept because `0049` argues against it.

## 8. Read-only attachment

**NORMATIVE:** `AttachMode::ReadOnly` maps `PROT_READ` only and is **the default for any participant that does not declare an intent to publish.** A buggy perception node *cannot* corrupt the transform tree, enforced by hardware; lead with this in the documentation.

| Operation | ReadOnly |
|---|---|
| `plan`, `at`, `at_many`, `at_adaptive` | permitted, identical code path |
| resolve an existing frame name | permitted |
| **intern a new frame** | `Err(FrameNotDeclared)` — interning writes |
| `claim` / `push` | not expressible: `Publisher` construction requires `ReadWrite` (compile-fail test) |
| reaping | not permitted — reaping writes |
| heartbeat | not written; the socket carries liveness |

`FrameNotDeclared` must say "no publisher has declared this frame yet", not "unknown frame".

## 9. `tf_treed`

> **SUPERSEDED by [`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md). There is no `tf_treed` binary. The capability is `tf_tree serve`, a subcommand of the shipped binary, and it is an escalation, not a prerequisite.**
>
> The daemon was to create and seal the segment, pre-declare the topology, serve the attach socket, reap on `HUP` and export metrics; it must not publish or interpret transforms. Liveness, reaping and owner death need no daemon: ownership is lock-file byte 0, which the kernel releases when the owner dies, and a survivor inherits it (§3.5, caller-driven). What remained was pre-declaration, so a consumer can attach and plan before any publisher runs; `0019` §2 fixes that without a daemon: read-only attach implies `CreatePolicy::Never`, consumers wait with `Open::await_open` and `Tree::await_frames`, and `frame_headroom`/`edge_headroom` cover late frames. What survives, as `tf_tree serve --config <topology.toml>`: create and seal from the config, pre-declare, hold the arena open, export metrics, drain on `SIGTERM` leaving the segment alive. Retired: `--lock` and `--socket-mode`, which the rendezvous owns. Per [`0009`](./decisions/0009-descoping-phase-6.md), `--config` takes the topology config only; URDF is owed by no phase and is converted first.

## 10. Recording and replay — the correctness harness

> **§10(a) *Record* and §10(b) *Replay* are DECLINED by [`0047`](./decisions/0047-the-recording-this-reader-would-refuse.md). There is no `tf_tree_record` crate and no `record`/`replay` subcommand, and none is owed. §10(c), the NORMATIVE test, is met — `crates/tf_tree_cli/tests/replay_bit_identity.rs`, §15's box for it.**
>
> The recorder's channels (`tf_tree/topology`, `tf_tree/samples`) carry no `tf2_msgs` schema and `crates/tf_tree_ingest/src/source.rs` accepts a channel only by schema, so its output would be refused by the only MCAP reader here. The read half shipped a phase later as `tf_tree_ingest` under [`0006`](./decisions/0006-the-eight-phase-roadmap.md). Unmet: the regression corpus and real robot data for §12; read `docs/PHASE5.md` §0.0's §3 row before assuming a substitute.

The rest of this section is kept because `0047` argues against it.

- **Record.** A read-only participant tapping every edge into MCAP (`tf_tree/topology`, `tf_tree/samples`). **Replay.** Reconstruct an arena and re-publish deterministically.
- **The test that matters — NORMATIVE.** Replay one recording into a `HeapArena` and a `MappedArena`, run an identical query set against both, and assert **bit-identical `f64` results**. Lookups are pure functions of `(plan, stamp, buffer contents)`, so any difference means the shared-memory path is not the same code.

## 11. Test plan

### 11.1 What Miri and loom can and cannot do

**Neither crosses a process boundary**, so `MappedArena` cannot be tested by either. Run every Phase 1 loom test against a `HeapArena` with A1–A5 applied, cover the multi-process dimension by fault injection, and add loom cases for: two threads racing `try_reap` (at most one `Reaped::Yes`, epoch bumped at least once); reap concurrent with `push` from the reaped `Publisher` (`ClaimRevoked`, or completes before the epoch bump); topology mutation concurrent with plan compilation across four blocks (one consistent block or `TopologyChurn`); claim, reap, re-claim, zombie push (the zombie always fails).

### 11.2 Multi-process integration harness

`tf_tree_test_harness` spawns real child processes, coordinates via pipes, and asserts on arena state. Required scenarios:

1. 1 owner, 1 writer, 14 read-only readers. Sustained 1 kHz for 60 s. Zero errors, zero divergence.
2. Attach/detach churn: 32 processes for 60 s while a writer publishes. Slots must not leak.
   - **2b. Slot recycling under abnormal exit.** Attach and `SIGKILL` a read-write participant 128 times against a 64-slot arena. Every attach must succeed (`slot_recycling_under_abnormal_exit`, `crates/tf_tree/tests/rendezvous.rs`; falsifier for #184). Two mechanisms satisfy it and it does not distinguish them: the owner's hangup callback (#191) and the assigner's reclaim before granting ([`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md) step 3). `the_assigner_reclaims_a_stale_record_no_hangup_will_ever_clear` pins the assigner alone; `the_assigner_collects_a_record_left_reserved_by_a_killed_registrant` and `the_hangup_collects_a_record_left_reserved_by_a_killed_registrant` pin the `RESERVED` half, one per collector, both staged.
3. Owner dies mid-run: participants continue for 60 s; new attach fails cleanly; reaping still functions.
4. `FORMAT_VERSION` / `layout_hash` mismatch: rejected with the correct status and a message naming both values.
5. Read-only participant attempts every write operation: all fail, arena bytes unchanged (hash before and after).
6. 64 participants, then a 65th: `NoParticipantSlots`, and the message says how to raise the limit.
7. **Thundering herd:** 32 processes `open()` simultaneously with no arena. Exactly one creates; 31 join; all 32 see the same `instance_uuid`.
8. **Ownership migration:** kill the owner mid-run. A survivor takes over; a new process joins the *same* arena (identical `instance_uuid`); a reader thread running throughout observes zero failed lookups and no excursion beyond steady-state p99.9.
9. **Split-brain attempt:** kill the owner and immediately start a fresh process. It must block on §3.4 step 4 and then join. **Two distinct `instance_uuid`s on one `(runtime_dir, domain, name)` is a hard failure.** Run a thousand times; it is the most important race in the phase.
10. **Stuck participant:** `SIGSTOP` the only participant, then `open()` from a fresh process. Must fail with `ArenaHeldButUnreachable` naming the stuck slot — never create a second arena. `SIGCONT`, then a subsequent `open()` succeeds.
11. **Domain isolation:** two arenas under different domains, and two under different runtime dirs, never observe each other.

### 11.3 Fault injection — the core of this phase

**NORMATIVE.** A build-time `crash-points` feature places named, deterministic abort sites in every mutation protocol:

```rust
#[cfg(feature = "crash-points")]
macro_rules! crash_point { ($name:literal) => { $crate::crash::maybe_abort($name) }; }
```

Armed by `TF_TREE_CRASH_AT=<name>:<nth_hit>`, which `abort()`s (not `panic!`, whose unwinding runs `Drop` and defeats the test). One test per site:

| Crash point | The state it leaves behind must be repairable |
|---|---|
| `push.after_seq_odd` | slot odd, `head` unbumped → A5 self-heals on next claim |
| `push.after_data_before_seq_even` | as above; sample invisible because `head` never moved |
| `push.after_seq_even_before_head` | sample fully written but unpublished → invisible, then overwritten |
| `topo.after_copy_before_publish` | inactive block dirty, word unchanged → **no observable effect** (A1) |
| `topo.holding_lock` | **Placed and executed** in `Tree::reparent`, after A2's word is CASed and *before* `set_parent` (`a_killed_topology_holder_leaves_a_word_the_next_acquirer_steals`); placed before the mutation so the test cannot pass on a build that never took the byte. Byte released by the kernel; word left stale and overwritten by the next acquirer ([`0029`](./decisions/0029-the-topology-lock-is-a-kernel-lock.md)); stealing needs no rollback (A1). Holding the byte means a non-zero word belongs to a holder that is dead or has no lock file, so `/proc` may only ever *withhold* a steal. Holder classes: **live with a lock file** — refused by the byte whatever `/proc` says (#213); **dead with a lock file** — stealable; **no lock file** (`build_shared`, [`0031`](./decisions/0031-the-participant-record-with-no-byte.md)) — decided by the triple alone; **fork inheritor** ([`0030`](./decisions/0030-the-atfork-handler-and-inherited-descriptors.md)) — a dead parent's lock is not stealable while the child lives, an availability failure reported by `TFT014`, far narrower than the claim byte's exposure because this byte is held for two `fcntl`s and one block copy |
| `claim.after_cas` | claim held by a dead participant → reapable via slot indirection (A3) |
| `intern.after_hash_cas_before_id_store` | hash slot claimed, id unpublished → next interner spins, then takes over (A8, below) |
| `attach.after_slot_assigned_before_publish` | **Placed and executed** in `participant::fill_slot`, between the `FREE -> RESERVED` CAS and the `live_word` store (`attach_after_slot_assigned_before_publish_aborts_at_the_named_point`). Slot `RESERVED`; on the rendezvous path the byte was taken and then released by the kernel; on the byte-less `build_shared` path no byte was ever taken. → record cleared by any reaper: `ParticipantTable::reclaim` accepts any observed word, `RESERVED` included ([`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md) step 1), and both the hangup callback and the slot assigner act on one. The window is ~12 ns, so §11.2's `..._collects_a_record_left_reserved_by_a_killed_registrant` tests *stage* the word and cover the recovery, not the crash. On the byte-less path no reclaimer ever runs |
| `hangup.after_probe_before_cas` | **Placed** in the owner's hangup callback between the `state` load and `reclaim`; armed by `a_killed_owner_in_its_hangup_callback_leaves_the_role_inheritable`, which needs a joiner to hang up while **this owner** is armed. One CAS, no torn state: the reclamation happened or did not. Repairable only because the assigner (next grant) and `Tree::reap_participants` form the same verdict later |
| `reclaim.after_probe_before_cas` | **Placed** in `Tree::reap_participants` between `reclamation_verdict` and the CAS; armed by `a_killed_sweeper_leaves_the_record_for_the_next_one`, which needs a sweeper that is not the test binary (`rendezvous_child`'s `join-sweep` arm) and a fixture that kills the **owner**. Nothing published → idempotent; racing reclaimers are harmless, at most one CAS succeeds |
| `reclaim.probe_then_reoccupied` | **Not an abort site**: it names an interleaving between two *live* processes, which killing one cannot produce; `loom` and §11.4 are what reach it. A reclaimer holding a verdict formed before the slot was freed, re-granted and re-occupied: for `live_word(inc)` the CAS fails on the differing incarnation; for `RESERVED`, which carries none, it **can** succeed, bounded by the byte rather than the word (the record belongs to a joiner holding the matching byte and about to publish `live_word`), so the outcome is a **spurious free, never a second occupant**. `ParticipantTable::reclaim`'s doc comment carries the precondition. `Tree::reap_participants` runs in another process, so the byte is the whole of the bound |
| `open.after_ownership_lock_before_bind` | **Placed and executed** with `open.after_create_before_bind` by `a_creator_killed_before_or_after_the_arena_exists_leaves_nothing_behind`. Ownership lock released by the kernel → the next `open()` proceeds; **no arena created twice** |
| `open.after_create_before_bind` | **Placed and executed.** Placed before `use_ofd_liveness`, so the abandoned tree holds only the segment ("no participant byte held"). Arena exists, nothing serving, no byte held → next `open()` creates fresh; the orphan memfd is freed with its last mapping |
| `takeover.after_ownership_lock_before_bind` | ownership released; another participant takes over; joiners retry. **`another participant` is a precondition, not an outcome**: the corpse leaves nothing but an `F_OFD_SETLK` on byte 0 that the kernel undoes, but the role is re-taken only by a process *already attached*. With **zero** attached read-write survivors the state is absorbing; `a_killed_heir_leaves_the_role_for_the_next_survivor` keeps a second heir attached and `shm_torture` defers an owner kill rather than taking the last one |

**`intern.after_hash_cas_before_id_store` needs amendment A8** (§1): bounded spin plus takeover of a dead claimant, covered by a loom test.

### 11.4 `shm_torture`

Nightly CI, 30 minutes: N processes, random attach/detach/claim/reap/push/lookup, random `SIGKILL` at 1–10 Hz, a random crash point armed in 10% of children. Invariants checked continuously: no reader ever observes a non-unit quaternion or a NaN; no two writers ever hold one edge; participant and claim slots never leak; the arena hash is stable across quiescent points. Run it under ASan (works across processes) and with `TF_TREE_PARANOID=1`, which validates quaternion normalization and stamp monotonicity on every read. §0.0's `shm_torture` row states which invariants are checked how.

> **Amendment — the crash-point clause runs nightly.** It is a different build (`--features shm,crash-points`, since a site compiled out of the driver is compiled out of every child) with gentler parameters (5 minutes, 10 children, 2 Hz; a high kill rate beats the site more often than not), so `nightly.yml`'s `crash-points` job is its own job. Because a job reads an exit status, not the `§11.3:` line, the binary **refuses** a `--crash-points` run with `armed 0` and separately one with `armed N, aborted 0`: two "at least one" bounds, without which the job would go green on a build that cannot arm a site.

> **Amendment — the harness pauses its own churn for the width of one reap.** The driver's `kill()`-to-`wait()` interval is exactly the interval in which the owner is dead and *undetectably* dead (`do_exit` runs `exit_mm()` before `exit_files()`, and §3.5's trigger is a socket hangup). Survivors keep drawing the 2%-per-operation detach arm and a survivor that leaves cannot return (§3.4 step 4), so the pool can drain inside the vacancy. The interval scales with the victim's dirty pages (~0.09 ms/MB), not the sanitizer: the plain build wedges at `--children 4` too. A `kill.in_progress` marker, written before the signal and removed after the post-reap census, makes a child stay instead of detaching. That is a reduction in what "random attach/detach" means here, and the run prints the suppressed count; it suppresses a detach, never a kill, an inheritance or a violation. `--victim-ballast-mb` and `--stop-owner-ms` are the positive controls (deliberate failures, so a fix can be shown to *stop* something).

> **Amendment — the torture recipes run their children without core dumps** ([`0057`](./decisions/0057-an-owner-is-not-dead-until-its-files-close.md) Decision 6). An armed `abort()` defaults to *core*, and on a pipe `core_pattern` the crash helper runs **before** the child's files close, so an armed owner or heir holds its socket and byte 0 for the helper's whole run. `just shm-torture`, `shm-torture-crash-points` and `shm-torture-asan` run under `prlimit --core=1:1 --` (a soft limit of 0 does not stop a pipe dump). Measured on kernel 6.8 with apport; pending on the runner (`0057` step 4). The recipes therefore no longer exercise recovery across a core dump, and a green crash-points run is not evidence about the 2026-09-12 wedge. The bare binary, outside a recipe, still inherits the shell's limit.

## 12. Benchmarks and the gate

### 12.1 Fixture

The Phase 1 24-frame robot tree, plus 1 writer process (4 dynamic edges) and 1–16 read-only consumer processes each running 4 reader threads, cores pinned, `isolcpus` if available. Compare against ROS 2 `tf2` with an equivalent tree over the default DDS, same rates.

### 12.2 Required measurements

| Benchmark | Report | Measured |
|---|---|---|
| depth-3 cross-process lookup, warm | p50, p99, p99.9 vs the Phase 1 in-process baseline | — |
| first access after attach, per-edge population on vs off | p99.9, both | **Half done — `just attach-bench`.** Population **on**: the first lookup after attach is **130–170 ns p50** over 25 runs, indistinguishable from steady state. Its tail is one sample (nearest-rank p99.9 over 201 cycles *is* the maximum), 1.2–4.7 us across runs. The **off** arm is absent: `populate_hot()` is unconditional inside `attach_shared_inner`, and it arrives with `0022`'s B2-prime. |
| THP `madvise` vs `never` | p50, p99.9, both | — |
| aggregate read throughput, 1→16 consumer processes | scaling curve | **Done — `just shm-scaling`; the curve is in [`docs/benchmarks/tf2.md`](./benchmarks/tf2.md).** 1/2/4/8 reader processes: **4.66 → 9.04 → 15.43 → 18.17 M lookups/s** (1.00x → 3.90x) at 213 → 431 ns a lookup, unique resident 3.5 → 18.7 MiB against N × 1.4 MiB for private tf2 buffers. The bend is cores (4 physical): 4 × 213 ns is an 18.8 M/s roofline. The curve stops at 8 because 16 processes on 4 cores measures the scheduler. |
| **CPU per consumer at 1 kHz × 20 edges, vs ROS 2 `/tf`** | %CPU per consumer, both | — |
| **total RSS across 16 consumers, vs ROS 2 `/tf`** | MB, both | — |
| publish → visible-to-consumer latency, vs ROS 2 `/tf` | p50, p99.9, both | — |
| `SIGKILL` writer → claim reapable → re-claimed | p50, p99, of a small `SIGKILL`ed victim: a claim lease is released at the same point in the victim's exit as byte 0, so a large or dumping victim adds its teardown or dump ([`0057`](./decisions/0057-an-owner-is-not-dead-until-its-files-close.md)) | — |
| attach time, cold and warm | p50 | **Done — `just attach-bench`, which does not run §3.7's rendezvous** (`Tree::attach_shared(dup, ReadOnly)` on a duplicated memfd: map, validate, take a slot, `populate_hot`). On the §11.1 fixture, 201 cycles a run: attach **12.3–14.2 us p50**, first plan compile (where ring population went) **66.3–92.3 us p50**, observed extremes over 28 runs rounded outward, **not a bound**; cold-cycle and maximum figures are single scheduler-tail draws. The halves sum to **79.3–106.4 us**, unchanged by `0024` moving population from attach to take-up. The arena is 1 401 472 B (343 pages). A real join (`Open::open()` against a live owner) measured ~133 us p50 out of tree, reproduced by nothing here. "Cold" means fresh VMA and page tables, not a cold page cache. |
| `open()` when the arena exists vs when creating | p50, both | — |
| owner kill → new owner serving | p50, p99 | **Done — `just owner-migration`.** Timed from the outside: the driver stamps its `SIGKILL` and retries `Open::new().create(Never)` until one succeeds. **0.6–1.2 ms p50, 1.1–2.0 ms p99** over five runs of five migrations, of a small `SIGKILL`ed owner only ([`0057`](./decisions/0057-an-owner-is-not-dead-until-its-files-close.md)): an owner that dumps core or holds a large dirty address space is not inheritable until its exit ends (~1.1 s pipe dump, ~98 ms for 1 GiB; §3.7 step 9). |
| lookup latency across an ownership migration | p99.9 during vs steady-state | **Measured — `just owner-migration` — and the quotient is only weakly evaluable.** It reads **0.976, 1.000, 1.025, 1.025, 1.093** at 5 migrations (one past gate 4b's 1.05) and **1.000** at `--repeat 15`: its sensitivity falls as its sample count rises, so it cannot be both stable and sensitive, and choosing the passing repeat count would choose the vacuous end. Re-cutting the criterion is a decision record. **Load-bearing:** *zero failed lookups* in every run, and stalls (lookups past 10× the steady p99.9) at **510–542 per million steady against 517–531 during**. The readers are read-only and make no control-plane call. |

Third-column dashes mean this table holds no figure, not that none exists (`docs/benchmarks/tf2.md`); `just artifact-versions` fails on a row whose cell count disagrees with its header.

### 12.3 The gate — NORMATIVE

Proceed to Phase 3 if:

1. **Cross-process depth-3 p50 within 10% of the in-process baseline, p99.9 within 25%.** The central claim of the phase; if it fails, the mapping policy (§7) is wrong, not the design.
2. **Aggregate read throughput scales ≥ 12× from 1 to 16 consumer processes.**
3. **Zero corrupt reads across the full `shm_torture` run**, and every §11.3 crash point recovers. Not negotiable.
4. **Kill → re-claimable p99 under 10 ms. MET, and gated in CI — `just gate reclaim-latency may-refuse`, in `ci.yml`'s `shm` job on both matrix rows.** 200 trials: p50 0.112 ms, p99 0.159 ms, a ~60x margin scheduler noise on a hosted runner cannot flip, per [`PHASE5.md` §9.3](./PHASE5.md#93-honesty-requirements--normative)'s one-sided-budget amendment. The figure is of a small `SIGKILL`ed victim; a 1 GiB dirty victim would pay ~100 ms of teardown first and a dumping one its crash helper, neither the library's ([`0057`](./decisions/0057-an-owner-is-not-dead-until-its-files-close.md)). **`may-refuse`, not `must-pass`:** `reclaim_latency` exits `2` for its own non-vacuity refusal (INVALID, not FAIL), a statement about the runner; `must-pass` would map that to a red job indistinguishable from a real regression. The refusal emits a `::warning::`; `scripts/gate-run.sh` owns the policy so the workflow cannot re-spell it.
4b. **Ownership migration is invisible to the data plane:** lookup p99.9 during a migration within 5% of steady state, and zero failed lookups. **Partly met, on a criterion that is only weakly evaluable** (`just owner-migration`, wired into no CI workflow): zero failed lookups holds in every run; the quotient reads 0.976–1.093 at the default 5 migrations and exactly 1.000 at `--repeat 15` (§12.2's row carries the argument). Re-cutting it is a decision record, as `0023` did for `PHASE4` §7 gate 1.

    **A finding from building it, not about ownership:** a lookup against an actively-written ring transiently refuses with an *inverted* window (`oldest` a few ms **past** `newest`) at ~1 in 4 × 10⁷, at the same rate in steady state as in migration. `LookupError::Extrapolation` names a *single* `edge`, so the pair is one ring's bounds. `SampleCursor::sample` (`crates/tf_tree_core/src/sample.rs`) loads `head` and then reads `t_old = stamp_at(lo_logical)` and `t_new = stamp_at(newest)` with two independent `Relaxed` loads and no seqlock, deliberately, being bounds probes; a writer that laps the ring between them inverts the pair by the slots it advanced. The refusal is correct; only the reported pair is inconsistent. The pair is consumed: `counter_of` files it as `extrap_before` and `worst_extrap_gap_ns` (read by `TFT011`) takes the bogus gap, ~50 ms against an ~8 s retained span, three orders short of mattering. Making the bounds consistent changes a hot-path read under a concurrent writer, which is a decision record (`CLAUDE.md`), so it is recorded here and not fixed. `owner_migration` counts these separately and excludes them from 4b's tally.
4c. **Scenario 9 of §11.2 passes 1000 consecutive runs with a single `instance_uuid`.**
5. Total RSS across 16 consumers under 1.2 × arena size.

### 12.4 What the numbers are actually for

Latency is the engineering gate; **CPU-per-consumer and RSS are the industrial argument** and belong in the README. Under `/tf` every consumer deserializes every transform into a private replica (O(consumers × edges × rate)); under `tf_tree` CPU is O(1) in consumers and RSS is one arena. Lead with the resource ratio.

## 13. Failure modes and runbook

Ship this table as `docs/RUNBOOK.md`. Every row must correspond to a `doctor` check and a distinct error type.

| Symptom | Cause | Response |
|---|---|---|
| `LayoutMismatch` on attach | binaries built from different commits | rebuild all participants; layout changes require a full restart |
| `BootIdMismatch` | arena predates a reboot (only possible with a file-backed dev arena) | recreate the arena. `HelloStatus::BootIdMismatch` from the §3.7 handshake is **not** this case: a serving owner answered, so the two processes disagree about which boot this is (one could not read `/proc/sys/kernel/random/boot_id`, or the parsers diverged); `RUNBOOK.md`'s `HandshakeRejected` row is the triage ([`0055`](./decisions/0055-the-recovery-capacity-a-fleet-cannot-add-later.md) step 7) |
| `ConnectionRefused` | owner not running, or a stale socket path | start the owner; the stale path is unlinked automatically |
| `NoParticipantSlots` | more than `max_participants` attached | find the leak. Capacity is fixed at construction ([`PROJECT.md`](./PROJECT.md) §5 D4) and there is no flag: `ArenaLayout` sets `max_participants` from `tf_tree_arena::layout::DEFAULT_MAX_PARTICIPANTS`, and header validation refuses a differently-sized arena. Raising it means changing that constant and rebuilding every participant together |
| `FrameNotDeclared` on a read-only participant | startup ordering: no publisher has declared it yet | wait for it — `Tree::await_frames` ([`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md) §2). Check the consumer is not creating the arena itself |
| `ClaimRevoked` during `push` | this writer was judged dead and reaped | the process was stalled; investigate scheduling, GC, or page-fault stalls |
| `EdgeAlreadyClaimed` | two nodes configured to publish one edge | a genuine configuration error — `doctor` names both PIDs |
| `SlotContended` / `SlotRecycled` | reader starved, or ring too shallow for the publish rate | increase edge capacity; `doctor` warns at 80% occupancy |
| `TopologyChurn` | topology mutated ≥ 4 times during one plan compilation | almost certainly a bug: topology should be near-static after startup |
| `SIGBUS` in a lookup | **structurally impossible with sealing (§3.6)** | if it ever happens, the segment was not sealed — file a bug |

## 14. Phase 3 handoff — constraints you must not break

> **Superseded in part by [`PHASE3.md`](./PHASE3.md) §1**, which corrects item 5 (`abi3` alone does not cover free-threaded builds) and adds a constraint this section missed. Read it before acting on items 4 or 5.

1. **`fork` safety.** `multiprocessing` defaults to `fork` on Linux; `MADV_DONTFORK` means the child's mapping is gone. Phase 3 must register an `os.register_at_fork(after_in_child=...)` hook that poisons every inherited `Tree` handle so the child gets a Python exception, not a segfault.
2. **GIL and liveness.** The socket carries liveness, so a long GIL-held pause does not risk reaping. Do not add heartbeat-based reaping (§6.4).
3. **Read-only by default.** Python `attach()` defaults to `ReadOnly` with `CreatePolicy::Never`, so a notebook started before the robot fails loudly instead of creating an empty arena a later publisher refuses to join.
4. **`tf_tree.open()` with no arguments must work in a notebook.**
5. **Distribution: `abi3` wheels via maturin**, plus (per `PHASE3.md` §1.1) a version-specific `cp314t` wheel from a second maturin invocation and an `abi3.abi3t` job (PEP 803) for 3.15 onward. `cp313t` is not buildable on PyO3 0.29 and is not in the matrix.

Write these into `docs/PHASE3.md` as you finish, alongside the measured numbers from §12.

## 15. Definition of done

- [x] Amendments A1–A8 applied to Phase 1; all Phase 1 tests still pass unchanged — §0.0's first row
- [x] `FORMAT_VERSION` bumped if Phase 1 had already been frozen, with a documented compatibility table — executable: `tf_tree doctor --explain-version` prints the build's `format_version` and `layout_hash`, what a mismatch requires (rebuild every participant from one commit and restart together; there is no compatibility layer), and that a version-2 arena cannot be attached
- [x] Diff in `tf_tree_core`'s read path against Phase 1: **zero lines** — structural: the read path is written against `ArenaView` and never names a backend. `another_process_reads_the_same_arena_bit_identically` (`crates/tf_tree_bench/tests/multiprocess.rs`) is the executable half
- [x] `tf_tree_core` dependency list unchanged (D14) — normal-kind third-party dependencies are exactly `blake3`, `bytemuck` and `libm`, plus `tf_tree_arena` and `tf_tree_math`
- [x] All §11.2 integration scenarios pass in CI on x86-64 **and aarch64** — all eleven are in `crates/tf_tree/tests/rendezvous.rs` except 4 (`a_layout_mismatch_names_the_owners_hash_and_sends_no_fd`, `a_version_mismatch_outranks_a_layout_mismatch` in `tf_tree_ipc`) and 5 (`read_only_refuses_mutation_instead_of_faulting`); 8 is `a_survivor_inherits_ownership_and_the_arena_becomes_joinable_again`. They run under `just shm-rendezvous` in the `shared memory (${{ matrix.os }})` job, matrix `[ubuntu-latest, ubuntu-24.04-arm]`. Scenario 1's 60 s at 1 kHz and scenario 2's 60 s of churn are soak shapes not reproduced (`just shm-torture` is the sustained arm); scenario 2 churns three tablefuls, eight at a time
- [x] Scenario 9 (split-brain) passes 1000 consecutive runs — **1000/1000 clean**, `just split-brain-soak`; one run is `scenario_9_a_split_brain_attempt_never_produces_a_second_arena`. Mutation-verified: a racer using `CreatePolicy::Always` fails it. **§11.2's prediction is wrong and the test says so:** the newcomer is **refused** with `ArenaHeldButUnreachable`, not joined, because migration is caller-driven ([`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md)). The test asserts refused is fine, joined is fine, a second `instance_uuid` never is
- [x] `tf_tree::open()` with no arguments joins-or-creates correctly from any start order — `scenario_7_a_thundering_herd_produces_exactly_one_arena` starts sixteen openers (not §11.2's thirty-two; the race is among the first few) and asserts every one sees **one** `instance_uuid`
- [x] `doctor` prints `instance_uuid` and the resolved runtime dir, and works without the arena — the directory is resolved independently of the source and a failure omits the line; both renderers carry the rule that produced it. `crates/tf_tree_cli/tests/doctor_runtime_dir.rs` pins it (mutation-verified via `the_reported_dir_follows_the_environment`)
- [x] Every §11.3 crash point has a test proving recovery — thirteen of fourteen rows carry a site and all thirteen are driven by a test; the fourteenth is not an abort site (§0.0's fault-injection row, which also names the two completeness gates)
- [~] `shm_torture` runs 30 minutes nightly, clean, under ASan — **the ASan arm is not clean, and the box is `[~]`.** It failed on the scheduled 2026-09-10 run on the population condition [`0055`](./decisions/0055-the-recovery-capacity-a-fleet-cannot-add-later.md) is about (`Classification: POPULATION`, `Heirs attached at this kill: 0`), not on an ASan report or an invariant; `0055` carries the margin (one kill in six leaves exactly one heir) and the discriminator for a future red. `0055` was answered 2026-09-18 — no mechanism, a NORMATIVE fleet requirement — which does not tick this box: what it waits on is whether `shm_torture` guarantees an heir at every kill, a harness decision nobody has taken. Three arms: the 30-minute `SIGKILL` soak (`nightly.yml`'s `torture`), the `crash-points` job, and ASan in the `sanitizers` matrix at `--duration 30m --children 4 --kill-hz 4` (four children because ASan makes six a different amount of work; the recipe's own default stays two minutes)
- [x] `HeapArena` / `MappedArena` replay produces **bit-identical** results (§10) — `crates/tf_tree_cli/tests/replay_bit_identity.rs`. Both arenas are filled by the same `replay` function from the same in-memory `Vec<FixtureMessage>`, so the backing store is the one variable; the test writes `run.mcap` but does not read it back, so no round trip (CDR, MCAP chunking, the reader) is proven. 5 edge pairs x 300 stamps = 1500 answers, 865 successful, compared as raw bits; mutation-verified by a one-ULP perturbation. The test is `#![cfg(all(feature = "shm", target_os = "linux"))]`: **`just test` does not run it; `just shm-check` does**. §10's tooling is DECLINED ([`0047`](./decisions/0047-the-recording-this-reader-would-refuse.md))
- [x] §12.3 gate met, or a written explanation of which criterion failed and by how much — on a 4-physical-core / 8-thread host (2026-08-30). Three states: **met**, **failed by a stated margin**, and **not evaluable on this hardware** (a fact about the host, not a code failure). The vocabulary is `docs/PHASE5.md` §9.3's `Sensitivity` axes and one-sided-budget amendment, and `tf_tree_bench`'s `Ground` enum.

  | criterion | verdict | evidence |
  |---|---|---|
  | 1. cross-process depth-3 p50 within 10%, p99.9 within 25% | **not evaluable here** | `just mp-bench` refuses to run: the host was 13% busy against its 10% threshold |
  | 2. aggregate read throughput scales ≥ 12× from 1 to 16 processes | **not evaluable here, and the ceiling is arithmetic** | `just shm-scaling`: 1.90× at 2, 2.98× at 4, **3.72× at 8** on 4 physical cores; 12× at 16 needs ≥ 16 cores to be *possible* |
  | 3. zero corrupt reads, and every §11.3 crash point recovers | **met** | `just shm-torture` PASSes with `0 violation(s)`; crash points per the box above |
  | 4. kill → re-claimable p99 under 10 ms | **met, 0.182 ms** | `just reclaim-latency`, 200 trials: p50 0.122 ms, p99 0.182 ms, max 0.400 ms. The first revision was vacuous (with `wait()` inside the timed region the edge was takeable first try in 50 of 50 trials); with `wait` moved out it is 0/200 and that counter is a **refusal** (`INVALID, not FAIL`) |
  | 4b. migration invisible to the data plane | **partly met, weakly evaluable** | zero failed lookups always; quotient 0.976–1.093 at the default and exactly 1.000 at `--repeat 15` (§12.3, §12.2) |
  | 4c. split-brain, 1000 consecutive runs, one `instance_uuid` | **met** | `just split-brain-soak`: **1000/1000 clean** |
  | 5. total RSS across 16 consumers under 1.2 × arena size | **not evaluable at 16, and the criterion's arithmetic does not survive contact** | `just shm-scaling` at 8 processes: unique resident **19.9 MiB** against an arena of **1368 KiB**. The arena is `MAP_SHARED` and resident once (`unique res` grows ~2.3 MiB per process), but the literal criterion compares whole-process RSS against the arena alone and per-process overhead dominates at any arena this size; it should be *re-cut*, not measured |

  **So: three met, one partly met, three not evaluable on this host, none failed.** The two needing ≥ 16 cores and the one needing a quiet machine are hardware gaps of the kind `docs/PHASE5.md` §9.3 handles by printing `UNAVAILABLE` with a reason.
- [~] `tf_tree serve` ships with a systemd unit and a container example — **out of scope by its own text and by [`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md)** (steps 6–7 not built, not scheduled). `~` because an unticked box reads as work owed and this is work declined. It is also the home the `--force-new` flag was deferred to (§0.0's §3.4 row)
- [x] `docs/RUNBOOK.md` complete; every row maps to a `doctor` check (rows for unimplemented Phase 2 errors are marked as such)
- [~] `docs/PHASE3.md` written and carrying §14 forward; the measured numbers land with §12

## Appendix A — implementation order

Steps 1–3 are the phase.

1. **Amendments A1–A8 against `HeapArena`**, with the loom tests, before touching a syscall: each is testable single-process, and a bug found after the IPC layer exists costs ten times as much.
2. **The lock file and `open()`.** Runtime-dir resolution, OFD ownership and participant bytes, the §3.4 algorithm including the split-brain check, ownership migration. Build §11.2 scenarios 7–11 alongside; scenario 9 should exist before the code it tests.
3. **`MappedArena` + attach protocol.** Owner and attacher, sealing, `SCM_RIGHTS`, header validation. Assert the zero-line-diff property in the read path.
4. **Claims as OFD locks; arena-side reaping; crash-point harness**, the harness built alongside.
5. ~~`tf_tree_record`~~ (declined, [`0047`](./decisions/0047-the-recording-this-reader-would-refuse.md)) and the bit-identical replay test (done). 6. `tf_tree serve` — last, possibly never ([`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md) §2). 7. `doctor` / `top` / `participants` extensions.
8. `/tf` ingest bridge. ROS permits any number of publishers per edge; `tf_tree` permits one. The bridge claims each edge on first sight and applies a configurable conflict policy — `FirstWriterWins` (default, with a loud diagnostic naming both ROS publishers) or `LastWriterWins`. This surfaces multi-publisher conflicts that `tf2` silently averages into garbage.
9. Benchmarks and the gate.

Do not proceed past step 4 until every §11.3 crash point recovers and §11.2 scenario 9 passes a thousand consecutive runs.

## Appendix B — kernel behaviour probe

Verified on Linux 6.18. Re-run on the target kernel; the sealing results in §3.6 are load-bearing.

```c
#define _GNU_SOURCE
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>
#include <fcntl.h>
#include <stdio.h>
#define P(l, e, x) printf("%-34s %d (expect %s)\n", l, (int)(x), e)
int main(void) {
    int fd = syscall(SYS_memfd_create, "tf_tree.probe", MFD_CLOEXEC | MFD_ALLOW_SEALING);
    ftruncate(fd, 1 << 20);
    void *p = mmap(NULL, 1 << 20, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_POPULATE, fd, 0);
    P("seal SHRINK|GROW w/ writable map", "0", fcntl(fd, F_ADD_SEALS, F_SEAL_SHRINK | F_SEAL_GROW));
    P("seal WRITE w/ writable map", "-1 EBUSY", fcntl(fd, F_ADD_SEALS, F_SEAL_WRITE));
    P("ftruncate shrink after seal", "-1 EPERM", ftruncate(fd, 1 << 10));
    P("seal SEAL", "0", fcntl(fd, F_ADD_SEALS, F_SEAL_SEAL));
    P("F_GET_SEALS", "7", fcntl(fd, F_GET_SEALS));
    P("MADV_DONTFORK", "0", madvise(p, 1 << 20, MADV_DONTFORK));
    P("MADV_HUGEPAGE", "0", madvise(p, 1 << 20, MADV_HUGEPAGE));
    return 0;
}
```

**The `/proc` parsing trap (§5.1), as a test fixture.** For a process whose `comm` is `evil) proc`, the naive whitespace split returns field 12's value where field 22 was intended:

```
raw    = "1234 (evil) proc) S 1 1234 1234 0 -1 4194304 1 2 3 ... 39"
naive  : raw.split()[21]                       -> 12    WRONG
robust : raw[raw.rindex(')')+2:].split()[19]   -> 13    correct
```
