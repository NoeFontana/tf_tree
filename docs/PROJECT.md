# tf_tree — Project Overview

> **Read this before `docs/PHASE1.md`.** This document explains *what* and *why*; the phase specs explain *how*. When a spec does not answer a question, consult the decision log in §5.

## 1. What this is

`tf_tree` is a transform tree engine: it stores time-stamped rigid-body transforms between named coordinate frames and answers *"where was frame A relative to frame B at time t?"*

In ROS the answer is `tf2`. `tf_tree` targets the workloads `tf2` was not designed for: kilohertz sensor edges, many concurrent readers, multiple processes on one host, and multiple hosts on one robot.

**Non-goal:** this is a *tree*, not a pose graph. Each frame has exactly one parent, and **`tf_tree` carries no uncertainty at all**: a composed covariance would understate the truth wherever the composed edges are correlated. **If you need joint uncertainty you need a factor graph** ([`0009`](./decisions/0009-descoping-phase-6.md), which also cut copy-on-write branches).

## 2. The problems being solved

| Problem | Consequence | Our answer |
|---|---|---|
| String-keyed frames hashed per lookup | Allocation and hashing in the hot path | Interned `FrameId`, resolved once at plan compilation |
| One `std::mutex` over the whole buffer | Lookups serialize; N reader threads do not scale | Wait-free reads, per-edge single-writer publish |
| Path re-resolved on every lookup | O(depth) topology walk per call | Compiled `Plan` — resolve topology once, sample many times |
| Every node holds a full tree replica | `/tf` is a firehose | Shared memory intra-host; interest-based replication inter-host |
| LERP + SLERP interpolation | Not right-invariant; not the SE(3) geodesic | ScLerp (screw-linear) default, LerpSlerp for compatibility |
| Static transforms via a latched topic | Timestamps meaningless, storage wasteful | First-class static edge kind, constant-folded at plan time |
| Opaque error strings | Extrapolation of *which* edge? | Typed errors that name the offending edge |
| Anyone may publish any edge | Two nodes fighting over `map→odom` produce silent garbage | Exclusive claim per edge, enforced |
| No batch API | Per-sample lookup loops | `at_many` and `at_adaptive` |
| No derivatives, no continuous-time model | Cannot serve as a trajectory backbone | Cumulative B-splines (Phase 6); body-frame twists in Phase 4 |

## 3. Architecture in one page

**Query layer.** A `Plan` is a compiled query: topology resolved, static edges folded into constants. A `Guard` pins a topology generation so a batch of lookups sees one consistent view. `plan.at(&guard, t)` does *d* binary searches, *d* interpolations and *d−1* compositions.

**Storage layer.** A flat arena of fixed-capacity ring buffers with no pointers, only `u32` offsets. Stamps and poses are stored separately (SoA); each pose slot is one cacheline with a seqlock sequence number.

**Transport layer** decides where the arena lives:
- *in-process*: a heap allocation shared by `Arc`.
- *intra-host*: the same bytes, `mmap`'d from a `memfd`; the identical reader code runs against it.
- *inter-host*: replication of only the edges a subscriber declared interest in, with bounded, *reported* staleness.

Above the query layer sit the bindings: Rust, PyO3, a C ABI with a C++ RAII header, and a `tf2_ros::Buffer`-compatible shim plus a `/tf` bridge.

**The load-bearing consequence:** shared memory is a constraint on the core layout, not a later transport. If Phase 2 requires changes outside `tf_tree_arena`, Phase 1 was built wrong.

## 4. Roadmap

- **Phase 1 — single-process core.** Interning, topology, arena, ring buffers, plan compilation, ScLerp, typed errors, CLI diagnostics, benchmarks against `tf2`. `docs/PHASE1.md`.
- **Phase 2 — shared memory.** `MappedArena` via sealed `memfd`, FD passing over a Unix socket that doubles as the liveness signal, cooperative crash-consistent reaping, read-only attach as an MMU-enforced boundary. Includes eight mandatory Phase 1 amendments (A1–A8). `docs/PHASE2.md`.

> **Status: the engine half is complete, §3.5 included**, under [`0005`](./decisions/0005-the-shared-memory-seam.md). `tf_tree_record` is **declined** by [`0047`](./decisions/0047-the-recording-this-reader-would-refuse.md). Of §11.4's four continuous invariants, two are checked continuously, a third at teardown, the fourth is not implemented (carry that qualifier when quoting). `tf_tree serve`, which [`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md) put in place of §9's `tf_treed`, is optional. `docs/PHASE2.md` §0.0 is the authoritative status table.

- **Phase 3 — Python bindings.** PyO3 binding the Rust core directly (not the C ABI), abi3 wheels, GIL released on lookup, zero-copy NumPy `(N, 4, 4)`, DLPack export. `docs/PHASE3.md`.
- **Phase 4 — dogfooding integration.** C ABI frozen in two tiers, C++ RAII wrapper, a **one-way** `/tf` → arena ingest bridge, `sample_with_derivatives`. **Its exit criterion is operational**: a real node on real hardware for two weeks and a written log of every surprise. `docs/PHASE4.md`.
- **Phase 5 — offline, observability, adoption wedge.** The frozen `.tft` arena, bag ingestion, `FORMAT_VERSION = 3` (Phase 6 **header fields** reserved; a region break is still owed: [`0032`](./decisions/0032-the-region-table-was-not-part-of-the-purchase.md)), counters, a diagnostics catalogue, `tf_tree top`. `docs/PHASE5.md`.
- **Phase 6 — continuous-time interpolation.** Cumulative B-splines with analytic derivatives. [`0009`](./decisions/0009-descoping-phase-6.md) descoped covariance and copy-on-write branches and moved URDF parsing out of the engine.
- **Phase 7 — the compatibility layer, gated (D21).** `tf2_ros::Buffer` shim and arena → `/tf` egress. `docs/PHASE7.md` is a **requirements artifact, not an implementation authorization**.
- **Phase 8 — inter-host replication.** Interest-based subscription, delta-coded wire format, clock-domain alignment, pluggable transport.

> The roadmap was re-cut from six phases to eight by [`0006`](./decisions/0006-the-eight-phase-roadmap.md), which holds the decision-number alias table: `PHASE4.md`/`PHASE5.md` cite **D28/D29** for **D21**, **D30** for **D20**, **D34** for **D22**.


## 5. Decision log

- **D1 — Rust for the core, C++ only as a wrapper.** Do not add a parallel C++ implementation; C++ users get the C ABI plus a header-only RAII wrapper.
- **D2 — A tree, not a pose graph.** One parent per frame. **No uncertainty is stored** ([`0009`](./decisions/0009-descoping-phase-6.md)). *Do not* add multi-parent support for loop closure, nor reintroduce it through copy-on-write branches.
- **D3 — Compiled plan, separate from temporal sampling.** Topology resolution and static folding happen once; only sampling is per-query. *Do not* add a convenience API that re-resolves topology per call without going through the plan cache.
- **D4 — Shared memory is a layout constraint, not a transport.** Drives: no pointers in the arena, fixed capacity, `#[repr(C)]` everywhere, seqlock per slot, claim table, `layout_hash` in the header. *Do not* simplify any of these on the grounds that one process does not need them. **`ArcSwap` for the topology is forbidden** — `Arc` refcounts do not cross a process boundary.
- **D5 — ScLerp is the default interpolator.** LERP+SLERP is left-invariant but *not* right-invariant; ScLerp is the SE(3) geodesic and is invariant under both. `LerpSlerp` stays for differential testing against `tf2` and latency-critical plans. *Do not* remove it; *do not* make it the default without a measurement (`just interp-accuracy` upholds the default).

- **D6 — f64 only in v1.** Decide f32 with numbers.
- **D7 — Exclusive writer claim per edge, enforced at runtime and in the type system.** `Publisher` is `Send + !Sync`. A second claim on a live edge is an error. *Do not* add a "force" flag without an accompanying loud diagnostic.
- **D8 — The engine samples trajectories; it does not transform points.** `at_adaptive` emits a small knot array and the consumer LERPs between knots on its own device. *Do not* add a `deskew()` helper, a point-cloud type, or GPU compute to the core.
- **D9 — Time domains are typed.** `Stamp<D>` with a phantom domain plus a runtime tag on each edge. Cross-domain lookup is an error until Phase 8 supplies alignment. *Do not* add an implicit coercion.
- **D10 — Frame and edge identity is append-only.** Removal is tombstoning; indices are never reused, so a stale `Plan` may fail the generation check but can never go out of bounds. *Do not* add index recycling.
- **D11 — Every error names the offending edge.** Errors are `Copy`, allocation-free and carry IDs; a `Display` wrapper resolves names against the arena.
- **D12 — Numerics are measured, not assumed.** `log_SO3` must go through the quaternion `atan2` form, and the small-angle series threshold is θ < 0.1 with four terms (`PHASE1.md` §3.3). *Do not* adjust either without re-running the verification.
- **D13 — Reference implementation plus fast implementation, forever.** Every non-obvious numeric routine keeps an obvious slow version in the tree and a fast version tested against it by proptest.
- **D14 — `no_std` + `alloc` core, minimal dependencies.** `tf_tree_core` depends on `libm`, `bytemuck` and `blake3`, and nothing else. *Do not* add `serde`, `tokio`, `nalgebra`, or a logging framework to the core.

> `blake3` is deliberate: `PHASE1.md` §5.1 mandates it for frame-name hashing, because two **processes** intern into one arena.

- **D15 — Crash-consistency is a hard requirement, not a quality bar.** There must be no state a dead process can leave behind that a live process cannot detect and repair. Every new mutation protocol must be walked through the crash matrix and covered by a named crash point.
- **D16 — Ownership is configured, not negotiated.** One process owns the arena; others attach. No leader election, no consensus. On a real robot there is always a natural owner.

> **Amended by [`0005`](./decisions/0005-the-shared-memory-seam.md) §8 and [`0037`](./decisions/0037-a-takeover-is-not-a-second-open.md): "no takeover" does not survive.** `PHASE2.md` §3.5 makes ownership a role the kernel reassigns on owner death: the heir is whichever survivor wins an uncontended `F_OFD_SETLK` on lock-file byte 0, and no message is exchanged. `Session::take_over_ownership`, `Tree::inherit_ownership` and the trigger `Tree::owner_lost` implement it. **Nothing calls the trigger for you** ([`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md)), so an arena whose survivors never call it stays ownerless. `OpenOutcome` is `Joined | Created`. `PHASE2.md` §0.0's ownership-migration row is authoritative.

> **Further amended by [`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md): the §9 daemon is not built.** A consumer waits with `Open::await_open` and `Tree::await_frames`; the capability survives as the optional `tf_tree serve`.

- **D17 — The attach socket is the liveness signal.** Participants hold their Unix socket open for the whole attachment; process death closes it. **Heartbeat staleness never triggers reaping**, and reaping is cooperative rather than owner-only. *Do not* add heartbeat-based reaping by default, and *do not* close the socket after the handshake.

> **Amended by [`0057`](./decisions/0057-an-owner-is-not-dead-until-its-files-close.md): the hangup happens when the kernel closes the dying process's files**, after any core dump and address-space teardown. `PHASE2.md` §3.5 states the survivor's half as NORMATIVE: `owner_lost()` answers `true` once the attach connection has hung up and the last description holding byte 0 has closed.

- **D18 — Read-only attach is the default for consumers.** A `PROT_READ` mapping means a buggy consumer *cannot* corrupt the tree. It is the only real security boundary; a read-write peer is trusted completely, and the docs must say so.
- **D19 — Interest-based replication, never broadcast (Phase 8).** A subscriber declares which `(target, source)` pairs it needs ; the daemon subscribes to exactly the union.
- **D20 — Apache-2.0 / MIT dual license.** Cited as **D30** by `PHASE5.md` §10 ([`0006`](./decisions/0006-the-eight-phase-roadmap.md)).
- **D21 — The compatibility layer is Phase 7, and it is gated on evidence, not scheduled.** `tf2_ros::Buffer` compatibility and arena → `/tf` egress wait for operating experience (`PHASE4.md` §1, `PHASE5.md` §0); each of the shim's semantic judgements made without it is a guess shipped as a promise. Phase 4's bridge is **ingress-only** for the same reason. *Do not* schedule the shim; gate it. Cited as **D28**/**D29** by the Phase 4 and 5 specs.

> [`docs/PHASE7.md`](./PHASE7.md) did not open the gate; a J-row answered from it rather than from the surprise log is the failure this decision prevents. Two judgements became records: [`0018`](./decisions/0018-blocking-waits-belong-in-the-shim.md) and [`0017`](./decisions/0017-owned-handles-and-the-lifetime-rule.md); their core-side halves are not gated.

- **D22 — A disabled feature never forks the layout hash.** When a cargo feature is compiled out, the arena *regions* it would use stay declared and counted by `layout_hash`; only the code disappears. First consumers: `PHASE5.md` §5.5 (`counters`) and §1.2 (the spline region, declared absent with offset `0`). Cited as **D34** by the Phase 5 spec.

> §1.2's two covariance fields were descoped by [`0009`](./decisions/0009-descoping-phase-6.md) but their eight bytes stay reserved **in place**: `spline_region_off` and `spline_degree` sit at 168 and 172, and `layout_hash` does not cover header fields.

### 5.1 The scheduled format-break ledger

**The queue a request to spend an arena byte joins** ([`0032`](./decisions/0032-the-region-table-was-not-part-of-the-purchase.md) part 2). `FORMAT_VERSION = 3` bought the **header** and not the region table (`PHASE5.md` §1.2), so one more break is owed. **Do not add arena fields opportunistically.**

**Nothing in this table is authorised.** A row is where the argument goes when the break is scheduled, not a promise it ships. Adding a row is not a decision; removing one is.

| Entry | Source | Why it is queued |
|---|---|---|
| Phase 6's spline region | `PHASE5.md` §1.2, [`0009`](./decisions/0009-descoping-phase-6.md) (Rationale, *B-splines*) | No region slot is reserved; whether a spline needs a region at all is open. If it does, it costs a `FORMAT_VERSION`. |
| An `EdgeMeta` provenance byte | no record | Listed so a future request has somewhere to be argued. |

## 6. Design smells — stop if you catch yourself doing these

- Reaching for `ArcSwap`, `Arc`, `Box`, `Vec` or any pointer inside an arena structure (D4); adding a `String` to an error type or hot path, a dependency to `tf_tree_core`, growth or reallocation, an async runtime, or a second parent (D2, D11, D14)
- Writing `unsafe` outside the boundaries of [`0007`](./decisions/0007-the-unsafe-budget-and-the-c-abi.md) as amended by [`0048`](./decisions/0048-a-kind-is-not-a-crate-name.md). The kinds are properties, not crate names; `scripts/unsafe-budget.txt` indexes the file set and `just lint` checks it. Or a second `#[allow(unsafe_code)]` in `tf_tree`, whose only one is `OwnedWriter`'s lifetime extension ([`0017`](./decisions/0017-owned-handles-and-the-lifetime-rule.md)); a second is a new decision record
- Weakening an atomic ordering because a test passes on x86-64 (the loom tests exist for this)
- Adding a point-cloud type, GPU kernel or `deskew` helper (D8); recycling a `FrameId` or `EdgeId` (D10); "fixing" `LerpSlerp` so the right-invariance test passes (D5)
- Adding a mutation protocol without walking the crash matrix (D15); closing the attach socket after the handshake, reaping on heartbeat staleness, or trusting a bare PID as an identity (D17)
- Defaulting a consumer to read-write attach (D18); using `shm_open` instead of a sealed `memfd`, or skipping `MADV_DONTFORK`
- Adding public API without checking it against [`API.md`](./API.md) §1's six rules, or a second spelling of an existing path
- Putting a blocking wait, futex or notification primitive in the arena ([`0018`](./decisions/0018-blocking-waits-belong-in-the-shim.md))

## 7. Glossary

| Term | Meaning |
|---|---|
| Edge | The relationship between a frame and its parent, storing `T_parent_child`. One per non-root frame. |
| Arena | The single flat allocation holding all records and buffers. Position-independent; relocatable by `memcpy`. |
| Plan | A compiled query: topology resolved, static edges folded, reduced to ≤`MAX_DEPTH` steps (**32**; the raw walk is bounded at `MAX_PATH_EDGES` = 64, [`0034`](./decisions/0034-the-depth-bound-priced-two-slots-the-same.md)). |
| Claim | Exclusive write ownership of an edge, held by a `Publisher`. |
| Claim epoch | Counter bumped on every claim and reap; a `Publisher` checks it on every push. |

## 8. Document map

`PHASE1.md`–`PHASE5.md` and `PHASE7.md` are the normative phase specs (Phase 7 is **gated by D21**). `RUNBOOK.md` is the operator-facing failure-mode list, each row mapping to a `doctor` check. **`API.md` is not a phase**: the API contract (six rules §1, per-binding surface §2–§5, delta table §6, new-surface check §7), read before adding public API.
