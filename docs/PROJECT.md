# tf_tree — Project Overview

> **Read this before `docs/PHASE1.md`.** This document explains *what* we are building and *why*. The phase spec explains *how*. When the phase spec does not answer a question, consult the decision log in §5 before choosing — several obvious-looking simplifications are deliberately excluded and the reasons are recorded there.

---

## 1. What this is

`tf_tree` is a transform tree engine: it stores time-stamped rigid-body transforms between named coordinate frames and answers the question *"where was frame A relative to frame B at time t?"*

Every robot needs this. In ROS the answer is `tf2`, which is competent, ubiquitous, and around fifteen years old. `tf_tree` targets the workloads `tf2` was not designed for: kilohertz sensor edges, many concurrent readers in one process, multiple processes on one host, and multiple hosts on one robot — with a query path fast enough to sit inside a control loop and diagnostics good enough to debug at 3 a.m.

**Non-goal, stated first because it constrains everything else:** this is a *tree*, not a pose graph. Each frame has exactly one parent. Uncertainty, when it arrives in Phase 5, will be a marginal — the structure cannot represent cross-correlation between sibling branches. If you need the joint distribution you need a factor graph, and `tf_tree` should say so loudly in its docs rather than hand out an optimistic covariance.

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
| No derivatives, no continuous-time model | Cannot serve as a VIO/SLAM trajectory backbone | Pluggable interpolation incl. cumulative B-splines (Phase 5) |

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

> **Status: the engine half is complete** — amendments A1–A8, rendezvous, attach protocol, ownership migration, liveness, claim leases, reaping, fork poisoning, page population, and CLI adoption, all under [`0005`](./decisions/0005-the-shared-memory-seam.md). What remains is the **tooling** half: `tf_treed`, `tf_tree_record`, `/tf` ingest, and the long-running fault harness. `docs/PHASE2.md` §0.0 is the authoritative status table.

**Phase 3 — Python bindings.** PyO3 binding the Rust core directly (not through the C ABI — that would cost error types and zero-copy ergonomics), abi3 wheels via maturin, GIL released on lookup, `at_many` returning zero-copy NumPy `(N, 4, 4)`, `__dlpack__` and `__cuda_array_interface__` export.

**Phase 4 — C ABI and ROS 2.** `cbindgen` C ABI, C++ RAII header wrapper with Eigen conversions, `tf2_ros::Buffer`-compatible shim, bidirectional `/tf` bridge. By volume this is the largest phase and the first point at which an ABI is frozen. Budget accordingly.

**Phase 5 — remaining engine features.** Covariance with adjoint transport, copy-on-write branches for loop-closure and multi-hypothesis evaluation, cumulative B-spline interpolation with analytic derivatives, MCAP record/replay, URDF parsing and typed-frame codegen, Rerun and Foxglove output.

**Phase 6 — inter-host replication.** Interest-based subscription, delta-coded wire format, clock-domain alignment with reported uncertainty, pluggable transport (Zenoh default).

**Pulled forward deliberately:** the `tf_tree doctor` diagnostics land at the end of Phase 1 (they are how Phase 2 gets debugged), and MCAP record/replay lands early in Phase 2 (deterministic replay is the correctness harness for the shared-memory layer).

## 5. Decision log

Each entry records a decision, why it was made, and what not to do. **These are the entries most likely to be "helpfully" reversed by someone who has not read the rationale.**

**D1 — Rust for the core, C++ only as a wrapper.**
The memory-safety story is the point: the concurrency in §6 of the phase spec is where every bug in this project will live. Do not add a parallel C++ implementation. C++ users get the C ABI plus a header-only RAII wrapper.

**D2 — A tree, not a pose graph.**
One parent per frame. Keeps topology to two dense arrays and the lookup to an array walk. Uncertainty is a marginal. *Do not* add multi-parent support to "handle" loop closure — that is a factor graph and a different project. Document the limitation prominently instead.

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
`Stamp<D>` with a phantom domain plus a runtime tag on each edge. Mixing sensor clock and host clock is the most common robotics bug and the compiler can prevent it. Cross-domain lookup is an error until Phase 6 supplies alignment. *Do not* add an implicit coercion.

**D10 — Frame and edge identity is append-only.**
Removal is tombstoning; indices are never reused. This is what makes a stale `Plan` safe: it may index a valid record and fail the generation check, but it can never go out of bounds. *Do not* add index recycling to save memory.

**D11 — Every error names the offending edge.**
`tf2`'s "lookup would require extrapolation into the future" without naming the edge is its most-complained-about behaviour, and fixing it costs one struct field. Errors are `Copy`, allocation-free, and carry IDs; a `Display` wrapper resolves names against the arena.

**D12 — Numerics are measured, not assumed.**
Two results from high-precision verification, both contrary to common practice: `log_SO3` must go through the quaternion `atan2` form (the `acos(trace)` form loses nine digits near θ = π, which is a rear-facing camera), and the small-angle series threshold is θ < 0.1 with four terms (the closed forms cancel catastrophically far above the 1e-8 threshold most libraries use). Full error tables are in §3.3 of the phase spec. *Do not* adjust either without re-running the verification.

**D13 — Reference implementation plus fast implementation, forever.**
Every non-obvious numeric routine gets an obvious slow version that is kept in the tree and a fast version tested against it by proptest. This applies to ScLerp, `mul_inv`, and anything added later. It is the cheapest correctness insurance available.

**D14 — `no_std` + `alloc` core, minimal dependencies.**
`tf_tree_core` depends on `libm` and `bytemuck` and nothing else. This keeps the engine viable on microcontrollers and, more immediately, keeps the dependency graph small enough that a safety-critical integrator will accept it. *Do not* add `serde`, `tokio`, `nalgebra`, or a logging framework to the core.

**D15 — Crash-consistency is a hard requirement, not a quality bar.**
In one process, a crash takes down every reader with it, so a torn write is unobservable. Across processes it is not. There must be no state a dead process can leave behind that a live process cannot detect and repair — no stuck seqlock, no unreapable claim, no wedged interning slot. This drives the single-store topology publish, the participant-slot indirection for claims, and the parity-forcing sample writer. Every mutation protocol added from here on must be walked through the crash matrix and covered by a named crash point. *Do not* add a mutation protocol without one.

**D16 — Ownership is configured, not negotiated.**
One process owns the arena; others attach. No leader election, no consensus, no takeover. On a real robot there is always a natural owner, and negotiation would add a distributed-consensus problem to a project whose value is a fast local lookup — it would also be the least-tested code in the system. `tf_treed` exists so that "configure an owner" is a trivial ask.

> **Amended by [`0005`](./decisions/0005-the-shared-memory-seam.md) §8 — "no takeover" does not survive; the rest does.** `PHASE2.md` §3.5 makes ownership a role the kernel reassigns on owner death, and `OpenOutcome::TookOver` ships. That is not negotiation: the heir is whichever process wins an uncontended `F_OFD_SETLK` on one byte, with no message exchanged between candidates and no quorum. What D16 rejects — election, consensus, a distributed-agreement protocol — stays rejected. D17 already assumes an owner can die without leaking every claim.

**D17 — The attach socket is the liveness signal.**
Participants hold their Unix socket open for the lifetime of the attachment. Process death of any kind closes it, and the owner sees `EPOLLHUP` in microseconds — exact, immediate, with no timeout to tune. Consequently **heartbeat staleness never triggers reaping** (a legitimately slow publisher is indistinguishable from a hung one), and reaping is cooperative rather than owner-only so that an owner's death does not leak every claim. *Do not* add heartbeat-based reaping by default, and *do not* close the socket after the handshake.

**D18 — Read-only attach is the default for consumers.**
A `PROT_READ` mapping means a buggy consumer *cannot* corrupt the transform tree, enforced by the MMU. For an industrial integrator this is a more compelling argument than any latency number, because it converts a class of whole-system failures into a single-process fault. It is also the only real security boundary the design has — a read-write peer is trusted completely, and the docs must say so plainly.

**D19 — Interest-based replication, never broadcast (Phase 6).**
A subscriber declares which `(target, source)` pairs it needs at what rate and precision; the daemon subscribes to exactly the union of required edges. This is the structural fix for the `/tf` firehose.

**D20 — Apache-2.0 / MIT dual license.**
The Rust ecosystem norm and the only choice compatible with industrial adoption. Not GPL, not BSL.

## 6. Design smells — stop if you catch yourself doing these

- Reaching for `ArcSwap`, `Arc`, `Box`, `Vec`, or any pointer inside a structure that lives in the arena (D4)
- Adding a `String` to an error type or a hot path
- Adding a dependency to `tf_tree_core`
- Writing `unsafe` outside `tf_tree_arena`, `buffer.rs`, or `arena_view.rs`
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

## 7. Glossary

| Term | Meaning |
|---|---|
| Frame | A named coordinate system. Interned to a `FrameId`. |
| Edge | The relationship between a frame and its parent, storing `T_parent_child`. One per non-root frame. |
| Arena | The single flat allocation holding all records and buffers. Position-independent; relocatable by `memcpy`. |
| Plan | A compiled query: topology resolved, static edges folded, reduced to ≤16 steps. |
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
