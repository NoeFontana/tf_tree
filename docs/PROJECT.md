# tf_tree — Project Overview

> **Read this before `docs/PHASE1.md`.** This document explains *what* we are building and *why*. The phase spec explains *how*. When the phase spec does not answer a question, consult the decision log in §5 before choosing — several obvious-looking simplifications are deliberately excluded and the reasons are recorded there.

---

## 1. What this is

`tf_tree` is a transform tree engine: it stores time-stamped rigid-body transforms between named coordinate frames and answers the question *"where was frame A relative to frame B at time t?"*

Every robot needs this. In ROS the answer is `tf2`, which is competent, ubiquitous, and around fifteen years old. `tf_tree` targets the workloads `tf2` was not designed for: kilohertz sensor edges, many concurrent readers in one process, multiple processes on one host, and multiple hosts on one robot — with a query path fast enough to sit inside a control loop and diagnostics good enough to debug at 3 a.m.

**Non-goal, stated first because it constrains everything else:** this is a *tree*, not a pose graph. Each frame has exactly one parent, and **`tf_tree` carries no uncertainty at all** — not now and not in a later phase.

That is a decision, not a gap. The structure cannot represent cross-correlation between sibling branches, so a composed covariance would be valid only where the composed edges are independent — and on a real robot they routinely are not, because `map → odom` and `odom → base_link` come from one estimator and their errors are correlated through it. Composing marginals as if independent understates the true covariance: wrong in the optimistic direction, on a number whose whole purpose is to gate a decision. Documenting the caveat is not enough, because the caller who most wants a covariance is the least likely to re-derive whether independence holds for their chain. **If you need joint uncertainty you need a factor graph, and this is not one.** Recorded in [`0009`](./decisions/0009-descoping-phase-6.md), which also cut copy-on-write branches for the same family of reasons.

## 2. The problems being solved

These are the specific `tf2` behaviours that define the design targets. Each maps to a section of the Phase 1 spec.

| Problem | Consequence | Our answer |
|---|---|---|
| String-keyed frames hashed per lookup | Allocation and hashing in the hot path | Interned `FrameId`, resolved once at plan compilation |
| One `std::mutex` over the whole buffer | Lookups serialize; N reader threads do not scale | Wait-free reads, per-edge single-writer publish |
| Path re-resolved on every lookup | O(depth) topology walk per call | Compiled `Plan` — resolve topology once, sample many times |
| Every node holds a full tree replica | `/tf` is a firehose regardless of what you consume | Shared memory intra-host; interest-based replication inter-host |
| LERP + SLERP interpolation | Not right-invariant; not the SE(3) geodesic | ScLerp (screw-linear) default, LerpSlerp for compatibility |
| Static transforms via a latched topic | Timestamps meaningless, storage wasteful | First-class static edge kind, constant-folded at plan time |
| Opaque error strings | "Extrapolation into the future" — of *which* edge? | Typed errors that name the offending edge |
| Anyone may publish any edge | Two nodes fighting over `map→odom` produces silent garbage | Exclusive claim per edge, enforced |
| No batch API | Per-sample lookup loops for sweep deskewing | `at_many` and `at_adaptive` |
| No derivatives, no continuous-time model | Cannot serve as a VIO/SLAM trajectory backbone | Pluggable interpolation incl. cumulative B-splines (Phase 6); body-frame twists in Phase 4 |

## 3. Architecture in one page

Three layers, stacked. Each is optional; each preserves the layer above it unchanged.

**The query layer** is the product. A `Plan` is a compiled query: topology resolved, static edges folded into precomputed constants, reduced to a short list of steps. A `Guard` pins a topology generation so a batch of lookups sees one consistent view and pays the validation cost once. `plan.at(&guard, t)` does *d* binary searches, *d* interpolations, and *d−1* compositions, and nothing else.

**The storage layer** is a flat arena of fixed-capacity ring buffers with no pointers in it — only `u32` offsets. Stamps and poses are stored separately (SoA) so a bracket search touches eight timestamps per cacheline without pulling in pose data. Each pose slot is exactly one cacheline and carries a seqlock sequence number.

**The transport layer** decides where that arena lives:

- *in-process* — a heap allocation shared by `Arc`. Zero cost.
- *intra-host* — the same bytes, `mmap`'d from a `memfd`. Because the layout is position-independent POD, the identical reader code runs against it. **No copy, no deserialization, no middleware.** This is the single biggest win over `tf2` and the reason the arena is shaped the way it is.
- *inter-host* — replication of only the edges a subscriber declared interest in, delta-coded and quantized. Eventually consistent with bounded, *reported* staleness.

Above the query layer sit the bindings: a Rust-native API, PyO3 bindings with NumPy/DLPack output, a C ABI with a C++ RAII header, and a `tf2_ros::Buffer`-compatible shim plus a `/tf` bridge.

**The load-bearing consequence:** shared memory is not a transport bolted on later, it is a constraint on the core layout. Phase 1 is the shared-memory design backed by a heap allocation. If Phase 2 requires changes outside `tf_tree_arena`, Phase 1 was built wrong.

## 4. Roadmap

Phases are ordered by *what constrains what*, not by user-visible value.

**Phase 1 — single-process core.** Interning, topology, arena, ring buffers, plan compilation, ScLerp, typed errors, CLI diagnostics, benchmarks against `tf2`. Fully specified in `docs/PHASE1.md`. Ends at a measured go/no-go gate.

**Phase 2 — shared memory.** `MappedArena` via sealed `memfd`, FD passing over a Unix socket that doubles as the liveness signal, a participant registry with PID-reuse-proof identity, cooperative crash-consistent reaping, and read-only attach as an MMU-enforced safety boundary. Highest technical risk in the project. Also ships `tf_tree_record` (the bit-identical replay harness) and a `/tf` ingest bridge so benchmarks run against real robot data. Fully specified in `docs/PHASE2.md`, including eight mandatory amendments to Phase 1 that the multi-process crash matrix exposed.

> **Status: the engine half is complete, §3.5 included since 2026-08-28** — amendments A1–A8, rendezvous, attach protocol, liveness, claim leases, reaping, fork poisoning, page population, and CLI adoption, all under [`0005`](./decisions/0005-the-shared-memory-seam.md). **§3.5's ownership migration now ships**, in the shape [`0037`](./decisions/0037-a-takeover-is-not-a-second-open.md) argued for after #275 deleted the unsound one: `Session::take_over_ownership` takes byte 0 on the description the survivor's session already holds, `Tree::inherit_ownership` binds and serves the existing segment, and `Tree::owner_lost` is the trigger that had never existed. **The trigger is caller-driven** — a survivor polls it in its own loop, nothing polls for it, and an arena whose survivors never call it still ends up ownerless; see D16 below. What else remains is the **tooling** half: `tf_tree_record`, `/tf` ingest, and the long-running fault harness — plus `tf_tree serve`, which [`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md) put in place of §9's `tf_treed` and deliberately made optional rather than owed. `docs/PHASE2.md` §0.0 is the authoritative status table.

**Phase 3 — Python bindings.** PyO3 binding the Rust core directly (not through the C ABI — that would cost error types and zero-copy ergonomics), abi3 wheels via maturin, GIL released on lookup, `at_many` returning zero-copy NumPy `(N, 4, 4)`, `__dlpack__` and `__cuda_array_interface__` export.

**Phase 4 — dogfooding integration.** `cbindgen` C ABI frozen in two tiers, C++ RAII header wrapper with Eigen and Sophus conversions, a **one-way** `/tf` → arena ingest bridge, and `sample_with_derivatives` pulled forward from Phase 6 because ScLerp already computes the twist. The first point at which an ABI is frozen. **Its exit criterion is operational, not a feature list** — a real node on real hardware for two weeks, and a written log of every surprise. Fully specified in `docs/PHASE4.md`.

**Phase 5 — offline, observability, and the adoption wedge.** The frozen `.tft` arena (the arena bytes as a memory-mapped file, shared across sixteen dataloader workers for the price of one), bag ingestion, `FORMAT_VERSION = 3` with the Phase 6 regions reserved so the break happens once, diagnostic counters, a sixteen-check diagnostics catalogue, and `tf_tree top`. Every user of this phase changes nothing about their robot. Fully specified in `docs/PHASE5.md`.

**Phase 6 — continuous-time interpolation.** Cumulative B-spline interpolation with analytic derivatives, answering §2's *"no derivatives, no continuous-time model"* row. Interpolation is already a pluggable axis (ScLerp, LerpSlerp) and Phase 4 shipped `sample_with_derivatives`, so this extends the design rather than adding to it.

> **This phase was four items and is now one.** [`0009`](./decisions/0009-descoping-phase-6.md) descoped **covariance** (a tree cannot compose a correct one — see §1) and **copy-on-write branches** (they serve the loop-closure use case D2 rejects, and contradict fixed capacity, one-writer-per-edge and append-only ids simultaneously), and moved **URDF parsing** out of the engine: it becomes an optional converter emitting the topology config Phase 4 already ships, built on the existing `urdf-rs` crate, and it is not owed by any phase. Phase 6 was the only phase not organised by this section's own "what constrains what" principle, and a remainder is where scope drifts without anyone deciding it should.

**Phase 7 — the compatibility layer, gated (D21).** `tf2_ros::Buffer`-compatible shim and arena → `/tf` egress. Does not begin until Phases 4 and 5 have produced the operating experience its hundred small semantic judgements require. Specified in `docs/PHASE7.md`, which is a **requirements artifact and not an implementation authorization**: its §4 states the semantic judgements as questions, and §0.0 lists the four gates that must be met before §3–§6 are built.

**Phase 8 — inter-host replication.** Interest-based subscription, delta-coded wire format, clock-domain alignment with reported uncertainty, pluggable transport (Zenoh default).

> **The roadmap was re-cut from six phases to eight** by [`0006`](./decisions/0006-the-eight-phase-roadmap.md), which is also where the decision-number alias table lives: `PHASE4.md`/`PHASE5.md` cite **D28/D29** for what is **D21** here, **D30** for what is **D20**, and **D34** for what is **D22**.

**Pulled forward deliberately:** the `tf_tree doctor` diagnostics land at the end of Phase 1 (they are how Phase 2 gets debugged), and MCAP record/replay lands early in Phase 2 (deterministic replay is the correctness harness for the shared-memory layer).

## 5. Decision log

Each entry records a decision, why it was made, and what not to do. **These are the entries most likely to be "helpfully" reversed by someone who has not read the rationale.**

**D1 — Rust for the core, C++ only as a wrapper.**
The memory-safety story is the point: the concurrency in §6 of the phase spec is where every bug in this project will live. Do not add a parallel C++ implementation. C++ users get the C ABI plus a header-only RAII wrapper.

**D2 — A tree, not a pose graph.**
One parent per frame. Keeps topology to two dense arrays and the lookup to an array walk. **No uncertainty is stored — not a joint, not a marginal** ([`0009`](./decisions/0009-descoping-phase-6.md); the earlier "uncertainty is a marginal" wording promised something a tree cannot compose correctly). *Do not* add multi-parent support to "handle" loop closure — that is a factor graph and a different project. *Do not* reintroduce the same use case through copy-on-write branches either, which is how it tried to come back and why `0009` cut them. Document the limitation prominently instead.

**D3 — Compiled plan, separate from temporal sampling.**
The single largest structural win over `tf2`. Topology resolution and static folding happen once; only sampling is per-query. *Do not* add a convenience API that re-resolves topology per call without going through the plan cache.

**D4 — Shared memory is a layout constraint, not a transport.**
Drives: no pointers in the arena, fixed capacity, `#[repr(C)]` everywhere, seqlock per slot, claim table with PID and heartbeat, `layout_hash` in the header. *Do not* simplify any of these in Phase 1 on the grounds that a single process does not need them. In particular: **`ArcSwap` for the topology is forbidden** — `Arc` refcounts do not cross a process boundary, and it is the most tempting simplification in the codebase.

**D5 — ScLerp is the default interpolator.**
LERP+SLERP is left-invariant but *not* right-invariant: interpolating `T₀C, T₁C` does not equal `interp(T₀,T₁)·C`. ScLerp is the SE(3) geodesic and is invariant under both. `LerpSlerp` stays available for bit-compatible differential testing against `tf2` and for latency-critical plans. *Do not* remove `LerpSlerp`; *do not* make it the default without a measurement justifying it.

**D6 — f64 only in v1.**
A generic `T: RealField` doubles the test matrix and the monomorphized code size for an unmeasured benefit. f32 for short-range high-rate edges may be worth it later; decide with numbers.

**D7 — Exclusive writer claim per edge, enforced at runtime and in the type system.**
`Publisher` is `Send + !Sync`. A second claim on a live edge is an error. This eliminates the classic silent-corruption failure where two nodes publish `map→odom`. *Do not* add a "force" flag that bypasses it without an accompanying loud diagnostic.

**D8 — The engine samples trajectories; it does not transform points.**
A LiDAR sweep needs one pose per distinct timestamp, not per point — and with adaptive knot placement bounded by a stated error tolerance, that is *tens* of poses for a 100 ms sweep, not thousands. So `at_adaptive` emits a small knot array, the consumer LERPs between knots on whatever device its points already live on, and the error is bounded by construction. This keeps CUDA out of the dependency tree entirely, which matters for Jetson/x86/ARM heterogeneity. *Do not* add a `deskew()` helper, a point-cloud type, or any GPU compute to the core.

**D9 — Time domains are typed.**
`Stamp<D>` with a phantom domain plus a runtime tag on each edge. Mixing sensor clock and host clock is the most common robotics bug and the compiler can prevent it. Cross-domain lookup is an error until Phase 8 supplies alignment. *Do not* add an implicit coercion.

**D10 — Frame and edge identity is append-only.**
Removal is tombstoning; indices are never reused. This is what makes a stale `Plan` safe: it may index a valid record and fail the generation check, but it can never go out of bounds. *Do not* add index recycling to save memory.

**D11 — Every error names the offending edge.**
`tf2`'s "lookup would require extrapolation into the future" without naming the edge is its most-complained-about behaviour, and fixing it costs one struct field. Errors are `Copy`, allocation-free, and carry IDs; a `Display` wrapper resolves names against the arena.

**D12 — Numerics are measured, not assumed.**
Two results from high-precision verification, both contrary to common practice: `log_SO3` must go through the quaternion `atan2` form (the `acos(trace)` form loses nine digits near θ = π, which is a rear-facing camera), and the small-angle series threshold is θ < 0.1 with four terms (the closed forms cancel catastrophically far above the 1e-8 threshold most libraries use). Full error tables are in §3.3 of the phase spec. *Do not* adjust either without re-running the verification.

**D13 — Reference implementation plus fast implementation, forever.**
Every non-obvious numeric routine gets an obvious slow version that is kept in the tree and a fast version tested against it by proptest. This applies to ScLerp, `mul_inv`, and anything added later. It is the cheapest correctness insurance available.

**D14 — `no_std` + `alloc` core, minimal dependencies.**
`tf_tree_core` depends on `libm`, `bytemuck` and `blake3`, and nothing else. This keeps the engine viable on microcontrollers and, more immediately, keeps the dependency graph small enough that a safety-critical integrator will accept it. *Do not* add `serde`, `tokio`, `nalgebra`, or a logging framework to the core.

> **This entry read "`libm` and `bytemuck` and nothing else" until 2026-08-28, and every other statement of the budget disagreed with it** — `PHASE1.md` §0, [`API.md`](./API.md), `CLAUDE.md`, [`0029`](./decisions/0029-the-topology-lock-is-a-kernel-lock.md) and [`0036`](./decisions/0036-the-receipt-time-the-format-already-reserved.md) all name three. `blake3` is a *deliberate* third rather than a drifted-in fourth: §5.1 mandates BLAKE3-256 truncated to 64 bits for frame-name hashing and argues the collision bound, and the hash cannot be a `std` hasher because Phase 2 has two **processes** interning into one arena, so it must be deterministic across them. The count was the error, not the dependency.

**D15 — Crash-consistency is a hard requirement, not a quality bar.**
In one process, a crash takes down every reader with it, so a torn write is unobservable. Across processes it is not. There must be no state a dead process can leave behind that a live process cannot detect and repair — no stuck seqlock, no unreapable claim, no wedged interning slot. This drives the single-store topology publish, the participant-slot indirection for claims, and the parity-forcing sample writer. Every mutation protocol added from here on must be walked through the crash matrix and covered by a named crash point. *Do not* add a mutation protocol without one.

**D16 — Ownership is configured, not negotiated.**
One process owns the arena; others attach. No leader election, no consensus, no takeover. On a real robot there is always a natural owner, and negotiation would add a distributed-consensus problem to a project whose value is a fast local lookup — it would also be the least-tested code in the system. A daemon existed in §9 so that "configure an owner" would be a trivial ask.

> **Amended by [`0005`](./decisions/0005-the-shared-memory-seam.md) §8 — "no takeover" does not survive; the rest does.** `PHASE2.md` §3.5 makes ownership a role the kernel reassigns on owner death. That is not negotiation: the heir is whichever process wins an uncontended `F_OFD_SETLK` on one byte, with no message exchanged between candidates and no quorum. What D16 rejects — election, consensus, a distributed-agreement protocol — stays rejected. D17 already assumes an owner can die without leaking every claim.

> **That amendment overstated what ships, #275 is what changed, and the 2026-08-28 §3.5 commit is what finally made it true — by a different route from the one it was written for. The whole sequence is kept here, because each step is why the next one has the shape it does.** The amendment above used to end its first sentence "and `OpenOutcome::TookOver` ships"; it does not, and after #275 it has no producer at all. **(i) `0005` amended D16** and its argument stands unaltered: ownership the kernel reassigns is not the negotiation D16 rejects. **(ii) #275 deleted the takeover half** ([`0037`](./decisions/0037-a-takeover-is-not-a-second-open.md)), because `F_OFD_GETLK` answers only *does anyone else hold this byte*, so a second `open()` cannot verify a claim about its own locks — leaving only ownership as lock-file byte 0 and the kernel's release of it at owner death, with no trigger either, since nothing watched the client socket for `HUP`. For one day owner death was terminal **for new joiners**: attached participants kept reading and publishing while every process that tried to join timed out with `ArenaHeldButUnreachable` for as long as any survivor lived. **(iii) The replacement landed on 2026-08-28** and is `0037`'s shape rather than a repair of the old one: `tf_tree_ipc::Session::take_over_ownership` takes byte 0 on the file description the survivor's session already holds, so its slot, byte and arena record cannot move; `tf_tree::Tree::inherit_ownership` binds the rendezvous over the **existing** segment; and `tf_tree::Tree::owner_lost` — a zero-timeout `poll` for `POLLHUP` on the attach socket, the same hangup the owner reads from its end (D17) — is the trigger. **It is still not an election.** The heir is whichever survivor wins one uncontended `F_OFD_SETLK`; the loser is told `Inheritance::Contended` and keeps its slot. No message is exchanged, no quorum is formed, and what D16 rejects stays rejected. Two things did **not** change: `OpenOutcome::TookOver` still has **no producer** and `crates/tf_tree/src/open.rs:1276` still refuses it with `OpenError::TakeoverUnsupported`, because inheritance is a method on an attached session and not an outcome of an `open()` — `0037` question 3 answers `no`, the variant does not survive, and removing it (with `TakeoverUnsupported`) is a breaking change `0037` still owns; and **nothing calls the trigger for you** — there is no background thread and no daemon, per `0019` below, so an arena whose survivors never call `owner_lost()` is still ownerless and still turns new joiners away. `PHASE2.md` §0.0's ownership-migration row is authoritative.

> **Further amended by [`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md) — the daemon this entry leaned on is not built, and does not need to be.** With takeover retired by `0005`, liveness, reaping and owner death need no dedicated process at all, so what was left of §9's `tf_treed` was one thing: pre-declaring topology so a consumer can plan before any publisher runs. `0019` fixes that without a daemon — a read-only attach implies `CreatePolicy::Never` (`Open::new()` *defaulted* to read-only **and** create-if-absent — a configuration no correct program wants; it failed `NoLayoutToCreate` rather than creating an empty arena, so this closed a latent class rather than an observed failure, and the builder's default is now `Never`), a consumer waits with `Open::await_open` and `Tree::await_frames`, and headroom covers frames that arrive later. The capability survives as **`tf_tree serve`**, a subcommand rather than a second binary, and as an escalation rather than a prerequisite: every process a user is *required* to run is a place adoption dies.

**D17 — The attach socket is the liveness signal.**
Participants hold their Unix socket open for the lifetime of the attachment. Process death of any kind closes it, and the owner sees `EPOLLHUP` in microseconds — exact, immediate, with no timeout to tune. Consequently **heartbeat staleness never triggers reaping** (a legitimately slow publisher is indistinguishable from a hung one), and reaping is cooperative rather than owner-only so that an owner's death does not leak every claim. *Do not* add heartbeat-based reaping by default, and *do not* close the socket after the handshake.

**D18 — Read-only attach is the default for consumers.**
A `PROT_READ` mapping means a buggy consumer *cannot* corrupt the transform tree, enforced by the MMU. For an industrial integrator this is a more compelling argument than any latency number, because it converts a class of whole-system failures into a single-process fault. It is also the only real security boundary the design has — a read-write peer is trusted completely, and the docs must say so plainly.

**D19 — Interest-based replication, never broadcast (Phase 8).**
A subscriber declares which `(target, source)` pairs it needs at what rate and precision; the daemon subscribes to exactly the union of required edges. This is the structural fix for the `/tf` firehose.

**D20 — Apache-2.0 / MIT dual license.**
The Rust ecosystem norm and the only choice compatible with industrial adoption. Not GPL, not BSL.

> Cited as **D30** by `PHASE5.md` §10. Same decision; see [`0006`](./decisions/0006-the-eight-phase-roadmap.md).

**D21 — The compatibility layer is Phase 7, and it is gated on evidence, not scheduled.**
`tf2_ros::Buffer` API compatibility and arena → `/tf` egress wait until Phases 4 and 5 have produced operating experience: a real node on real hardware (`PHASE4.md` §1) and offline users who adopted nothing (`PHASE5.md` §0). The shim is a hundred small semantic judgements about what `tf2` does when asked something ambiguous, and each one made without that experience is a guess that ships as a compatibility promise. Phase 4's bridge is **ingress-only** for the same reason: one direction removes every loopback, echo and authority-cycle question from the phase. *Do not* schedule the shim; gate it. Cited as **D28**/**D29** by the Phase 4 and 5 specs.

> **[`docs/PHASE7.md`](./PHASE7.md) is the requirements artifact this gate asks for, and writing it did not open the gate.** Its §4 states the eleven judgements known before operating experience *as questions with an evidence column*, and the discipline it asks for in the meantime is one line: every surprise-log entry is filed against a J-row or opens a new one. A row answered from that document rather than from the log is the failure this decision exists to prevent. Two of the judgements were decidable in advance because they are about *our* design rather than `tf2`'s behaviour, and both were pulled out into their own records rather than left in a gated document: the wait ([`0018`](./decisions/0018-blocking-waits-belong-in-the-shim.md)) and the stored claim ([`0017`](./decisions/0017-owned-handles-and-the-lifetime-rule.md)). Their core-side halves are **not** gated by D21 — see each record's implementation plan.

**D22 — A disabled feature never forks the layout hash.**
When a cargo feature is compiled out, the arena *regions* it would use stay declared in the layout and keep being counted by `layout_hash`; only the code that touches them disappears. Sizing the arena per feature set would make `layout_hash` a function of the build configuration, so two correctly-built participants of the same version would refuse to attach to each other and report a layout mismatch naming no actionable cause. A wasted region in a build that does not use it is cheap; a version-skew diagnostic that lies is not. First consumers: `PHASE5.md` §5.5 (`counters`) and §1.2 (the Phase 6 spline region, declared absent with offset `0`). Cited as **D34** by the Phase 5 spec.

> §1.2 also reserved two covariance fields, which [`0009`](./decisions/0009-descoping-phase-6.md) descoped. Their eight bytes stay reserved **in place** rather than being reclaimed: `spline_region_off` and `spline_degree` are published at 168 and 172, and closing the gap would move them. `layout_hash` hashes region strides and not header fields, so it would not catch two participants disagreeing about where the spline region begins — which is exactly the version-skew diagnostic that lies that this decision exists to prevent.

## 6. Design smells — stop if you catch yourself doing these

- Reaching for `ArcSwap`, `Arc`, `Box`, `Vec`, or any pointer inside a structure that lives in the arena (D4)
- Adding a `String` to an error type or a hot path
- Adding a dependency to `tf_tree_core`
- Writing `unsafe` anywhere that is not one of the four boundaries in [`0007`](./decisions/0007-the-unsafe-budget-and-the-c-abi.md) — the arena's memory, the OS, a foreign runtime, a foreign caller — or a second time inside `tf_tree`, whose `#![deny(unsafe_code)]` carries exactly one `#[allow]`: `OwnedWriter`'s lifetime extension, granted by [`0017`](./decisions/0017-owned-handles-and-the-lifetime-rule.md). **It is the only lifetime extension in the workspace.** `0017` steps 6–7 deleted the two that were not: `tf_tree_c::publisher::extend_to_static` (called from `tft_tree_claim` and from `bridge.rs`'s writer map) and `tf_tree_py`'s copy. Both bindings now go through `Tree::claim_owned`, and the answer to "is there a second lifetime extension here?" is **no** — a second one is a new decision record, not a patch
- Weakening an atomic ordering because a test passes on x86-64 (the loom tests exist for this; aarch64 is a CI target)
- Adding growth, resizing, or reallocation anywhere
- Adding a second parent, a multi-parent edge, or a graph search to plan compilation
- Making the API async, or introducing a runtime
- Adding a point-cloud type, a GPU kernel, or a `deskew` helper (D8)
- Recycling a `FrameId` or `EdgeId` (D10)
- "Fixing" `LerpSlerp` so the right-invariance test passes (D5 — that test is supposed to fail)
- Adding a mutation protocol without walking the crash matrix and adding a named crash point (D15)
- Closing the attach socket after the handshake, or reaping on heartbeat staleness (D17)
- Reaping from the owner only, or trusting a bare PID as an identity (D15, D17)
- Defaulting a consumer to read-write attach (D18)
- Using `shm_open` instead of a sealed `memfd`, or skipping `MADV_DONTFORK`
- Adding public API to any binding without checking it against [`API.md`](./API.md) §1's six rules — in particular: putting an allocating or name-resolving operation on `Plan` (R2), accepting a float stamp anywhere (R3), giving a layout parameter a default (R4), or handing a user a type carrying a lifetime that they will want to store (§2.1)
- Adding a second spelling of an existing path — a `coverage` beside `span`, a `resample` beside `at(arange(...))` — instead of documenting the one that exists
- Putting a blocking wait, a futex, or any notification primitive in the arena ([`0018`](./decisions/0018-blocking-waits-belong-in-the-shim.md))

## 7. Glossary

| Term | Meaning |
|---|---|
| Frame | A named coordinate system. Interned to a `FrameId`. |
| Edge | The relationship between a frame and its parent, storing `T_parent_child`. One per non-root frame. |
| Arena | The single flat allocation holding all records and buffers. Position-independent; relocatable by `memcpy`. |
| Plan | A compiled query: topology resolved, static edges folded, reduced to ≤`MAX_DEPTH` steps (**32**; the raw walk that produces it is bounded separately at `MAX_PATH_EDGES` = 64 — [`0034`](./decisions/0034-the-depth-bound-priced-two-slots-the-same.md)). |
| Guard | A pinned topology generation; makes a batch of lookups consistent and cheap. |
| Generation | Monotone counter on topology mutations. A plan whose generation is stale must be recompiled. |
| Head | Monotone count of samples ever published to an edge. Masked only at access. |
| Claim | Exclusive write ownership of an edge, held by a `Publisher`. |
| Knot | A sampled `(stamp, pose)` pair emitted by `at_adaptive`, spaced so LERP between knots stays within tolerance. |
| ScLerp | Screw-linear interpolation — the SE(3) geodesic, invariant under change of both world and body frame. |
| LatestCommon | The largest stamp for which every dynamic edge on a plan has data. What `tf2`'s `Time(0)` means. |
| Participant | A process attached to the arena, holding a registry slot and an open attach socket. |
| Claim epoch | Counter bumped on every claim and reap. A `Publisher` checks it on every push so a revived zombie writer cannot resurrect a reaped claim. |
| Reaping | Releasing a dead participant's claims. Cooperative and idempotent — any read-write participant may do it. |
| Crash-consistency | The property that no state a dead process leaves behind can wedge or corrupt a live one. |

## 8. Document map

| Document | Contents |
|---|---|
| `docs/PROJECT.md` | This file. Vision, architecture, roadmap, decision log. |
| `docs/PHASE1.md` | Normative implementation spec for Phase 1, including exact layouts, atomic orderings, test plan, and the benchmark gate. |
| `docs/PHASE2.md` | Normative implementation spec for Phase 2: shared memory, lifecycle, liveness, crash-consistency, fault injection. Contains mandatory Phase 1 amendments A1–A8. |
| `docs/RUNBOOK.md` | Operator-facing failure modes, written during Phase 2. Every row maps to a `doctor` check. |
| `docs/PHASE3.md` | Written at the end of Phase 2: Python binding constraints plus the measured Phase 2 numbers. |
| `docs/PHASE4.md` | Normative implementation spec for Phase 4: `sample_with_derivatives`, the two-tier C ABI, the header-only C++ wrapper, the one-way ROS 2 ingest bridge. Its §1 exit criterion is operational, not a feature list. |
| `docs/PHASE5.md` | Normative implementation spec for Phase 5: the frozen `.tft` arena, bag ingestion, `FORMAT_VERSION = 3`, diagnostic counters, the `TFT001`–`TFT019` catalogue, `tf_tree top`. |
| `docs/PHASE7.md` | Requirements artifact for the `tf2`-shaped shim. **Gated by D21, not scheduled** — §0.0 lists four gates, none met. Its §4 J-table is what Phase 4's surprise log is filed against. |
| `docs/API.md` | **Not a phase.** The API contract: six rules that generate every binding (§1), the normative surface of each (§2–§5), the delta table (§6), and the §7 check a new surface passes. Read before adding public API anywhere. |
