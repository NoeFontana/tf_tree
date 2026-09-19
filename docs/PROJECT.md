# tf_tree — Project Overview

> **Read this before `docs/PHASE1.md`.** This document explains *what* and *why*; the phase specs explain *how*. When a spec does not answer a question, consult the decision log in §5 — several obvious simplifications are deliberately excluded there.

## 1. What this is

`tf_tree` is a transform tree engine: it stores time-stamped rigid-body transforms between named coordinate frames and answers *"where was frame A relative to frame B at time t?"*

In ROS the answer is `tf2`. `tf_tree` targets the workloads `tf2` was not designed for: kilohertz sensor edges, many concurrent readers in one process, multiple processes on one host, and multiple hosts on one robot — with a query path fast enough for a control loop and diagnostics good enough to debug at 3 a.m.

**Non-goal:** this is a *tree*, not a pose graph. Each frame has exactly one parent, and **`tf_tree` carries no uncertainty at all**. The structure cannot represent cross-correlation between sibling branches, so a composed covariance would understate the truth wherever the composed edges are correlated (`map → odom` and `odom → base_link` routinely are). **If you need joint uncertainty you need a factor graph.** Recorded in [`0009`](./decisions/0009-descoping-phase-6.md), which also cut copy-on-write branches.

## 2. The problems being solved

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

Three layers, each optional, each preserving the one above unchanged.

**Query layer.** A `Plan` is a compiled query: topology resolved, static edges folded into constants, reduced to a short list of steps. A `Guard` pins a topology generation so a batch of lookups sees one consistent view. `plan.at(&guard, t)` does *d* binary searches, *d* interpolations and *d−1* compositions, and nothing else.

**Storage layer.** A flat arena of fixed-capacity ring buffers with no pointers — only `u32` offsets. Stamps and poses are stored separately (SoA); each pose slot is one cacheline and carries a seqlock sequence number.

**Transport layer** decides where the arena lives:
- *in-process* — a heap allocation shared by `Arc`.
- *intra-host* — the same bytes, `mmap`'d from a `memfd`. The layout is position-independent POD, so the identical reader code runs against it: **no copy, no deserialization, no middleware.**
- *inter-host* — replication of only the edges a subscriber declared interest in, delta-coded and quantized, with bounded, *reported* staleness.

Above the query layer sit the bindings: Rust, PyO3 (NumPy/DLPack), a C ABI with a C++ RAII header, and a `tf2_ros::Buffer`-compatible shim plus a `/tf` bridge.

**The load-bearing consequence:** shared memory is a constraint on the core layout, not a later transport. If Phase 2 requires changes outside `tf_tree_arena`, Phase 1 was built wrong.

## 4. Roadmap

Phases are ordered by *what constrains what*, not by user-visible value.

- **Phase 1 — single-process core.** Interning, topology, arena, ring buffers, plan compilation, ScLerp, typed errors, CLI diagnostics, benchmarks against `tf2`. Ends at a measured go/no-go gate. `docs/PHASE1.md`.
- **Phase 2 — shared memory.** `MappedArena` via sealed `memfd`, FD passing over a Unix socket that doubles as the liveness signal, PID-reuse-proof participant registry, cooperative crash-consistent reaping, read-only attach as an MMU-enforced boundary. Highest technical risk. Includes eight mandatory Phase 1 amendments (A1–A8). `docs/PHASE2.md`.

> **Status: the engine half is complete, §3.5 included since 2026-08-28**, under [`0005`](./decisions/0005-the-shared-memory-seam.md). `tf_tree_record` is **declined** by [`0047`](./decisions/0047-the-recording-this-reader-would-refuse.md): §10's Record and Replay halves are retired and §10(c)'s bit-identity test is met. `/tf` ingest shipped as Phase 4 §5. The fault harness is `shm_torture`; two of §11.4's four continuous invariants are checked continuously, a third at teardown, the fourth is not implemented (carry that qualifier when quoting). `tf_tree serve`, which [`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md) put in place of §9's `tf_treed`, is optional. `docs/PHASE2.md` §0.0 is the authoritative status table.

- **Phase 3 — Python bindings.** PyO3 binding the Rust core directly (not through the C ABI), abi3 wheels via maturin, GIL released on lookup, `at_many` returning zero-copy NumPy `(N, 4, 4)`, `__dlpack__` and `__cuda_array_interface__` export.
- **Phase 4 — dogfooding integration.** `cbindgen` C ABI frozen in two tiers, C++ RAII wrapper with Eigen and Sophus conversions, a **one-way** `/tf` → arena ingest bridge, `sample_with_derivatives`. The first frozen ABI. **Its exit criterion is operational** — a real node on real hardware for two weeks and a written log of every surprise. `docs/PHASE4.md`.
- **Phase 5 — offline, observability, adoption wedge.** The frozen `.tft` arena (memory-mapped, shared across sixteen dataloader workers), bag ingestion, `FORMAT_VERSION = 3` with the Phase 6 **header fields** reserved (the region table is not, so a region break is still owed: [`0032`](./decisions/0032-the-region-table-was-not-part-of-the-purchase.md)), diagnostic counters, a diagnostics catalogue, `tf_tree top`. `docs/PHASE5.md`.
- **Phase 6 — continuous-time interpolation.** Cumulative B-splines with analytic derivatives, answering §2's last row. [`0009`](./decisions/0009-descoping-phase-6.md) descoped covariance and copy-on-write branches and moved URDF parsing out of the engine (an optional converter, owed by no phase).
- **Phase 7 — the compatibility layer, gated (D21).** `tf2_ros::Buffer` shim and arena → `/tf` egress. `docs/PHASE7.md` is a **requirements artifact, not an implementation authorization**: §4 states the semantic judgements as questions and §0.0 lists four unmet gates.
- **Phase 8 — inter-host replication.** Interest-based subscription, delta-coded wire format, clock-domain alignment with reported uncertainty, pluggable transport (Zenoh default).

> The roadmap was re-cut from six phases to eight by [`0006`](./decisions/0006-the-eight-phase-roadmap.md), which also holds the decision-number alias table: `PHASE4.md`/`PHASE5.md` cite **D28/D29** for **D21**, **D30** for **D20**, **D34** for **D22**.


## 5. Decision log

**These entries are the ones most likely to be "helpfully" reversed by someone who has not read the rationale.**

- **D1 — Rust for the core, C++ only as a wrapper.** The concurrency in the phase specs is where every bug will live. Do not add a parallel C++ implementation; C++ users get the C ABI plus a header-only RAII wrapper.
- **D2 — A tree, not a pose graph.** One parent per frame: two dense arrays and an array walk. **No uncertainty is stored** ([`0009`](./decisions/0009-descoping-phase-6.md)). *Do not* add multi-parent support for loop closure, nor reintroduce it through copy-on-write branches.
- **D3 — Compiled plan, separate from temporal sampling.** Topology resolution and static folding happen once; only sampling is per-query. *Do not* add a convenience API that re-resolves topology per call without going through the plan cache.
- **D4 — Shared memory is a layout constraint, not a transport.** Drives: no pointers in the arena, fixed capacity, `#[repr(C)]` everywhere, seqlock per slot, claim table, `layout_hash` in the header. *Do not* simplify any of these on the grounds that one process does not need them. **`ArcSwap` for the topology is forbidden** — `Arc` refcounts do not cross a process boundary.
- **D5 — ScLerp is the default interpolator.** LERP+SLERP is left-invariant but *not* right-invariant: `T₀C, T₁C` interpolated does not equal `interp(T₀,T₁)·C`. ScLerp is the SE(3) geodesic and is invariant under both. `LerpSlerp` stays for bit-compatible differential testing against `tf2` and latency-critical plans. *Do not* remove it; *do not* make it the default without a measurement justifying it.

> **The measurement exists and upholds the default** (`just interp-accuracy`, `crates/tf_tree_bench/examples/interp_accuracy.rs`). ScLerp buys **position only**: both policies SLERP the rotation (≤0.06 µrad apart, `f64` noise), while `LerpSlerp` draws a chord where the truth is a helix, so the error is lever arm × θ²/8. For a sensor 0.5 m off the turn centre at 180 °/s: 1 kHz 0.001 mm, 100 Hz 0.062 mm, 10 Hz 6.16 mm (model 6.17 mm). Where `LerpSlerp` would save time the answers agree to under a micrometre; where they differ by millimetres nobody is counting nanoseconds.

- **D6 — f64 only in v1.** A generic `T: RealField` doubles the test matrix and code size for an unmeasured benefit. Decide f32 with numbers.
- **D7 — Exclusive writer claim per edge, enforced at runtime and in the type system.** `Publisher` is `Send + !Sync`. A second claim on a live edge is an error. *Do not* add a "force" flag without an accompanying loud diagnostic.
- **D8 — The engine samples trajectories; it does not transform points.** `at_adaptive` emits a small knot array (tens of poses for a 100 ms sweep), the consumer LERPs between knots on whatever device its points live on, and the error is bounded by construction. This keeps CUDA out of the dependency tree. *Do not* add a `deskew()` helper, a point-cloud type, or GPU compute to the core.
- **D9 — Time domains are typed.** `Stamp<D>` with a phantom domain plus a runtime tag on each edge. Cross-domain lookup is an error until Phase 8 supplies alignment. *Do not* add an implicit coercion.
- **D10 — Frame and edge identity is append-only.** Removal is tombstoning; indices are never reused, so a stale `Plan` may fail the generation check but can never go out of bounds. *Do not* add index recycling.
- **D11 — Every error names the offending edge.** Errors are `Copy`, allocation-free and carry IDs; a `Display` wrapper resolves names against the arena.
- **D12 — Numerics are measured, not assumed.** `log_SO3` must go through the quaternion `atan2` form (`acos(trace)` loses nine digits near θ = π), and the small-angle series threshold is θ < 0.1 with four terms. Error tables: `PHASE1.md` §3.3. *Do not* adjust either without re-running the verification.
- **D13 — Reference implementation plus fast implementation, forever.** Every non-obvious numeric routine keeps an obvious slow version in the tree and a fast version tested against it by proptest (ScLerp, `mul_inv`, anything added later).
- **D14 — `no_std` + `alloc` core, minimal dependencies.** `tf_tree_core` depends on `libm`, `bytemuck` and `blake3`, and nothing else. *Do not* add `serde`, `tokio`, `nalgebra`, or a logging framework to the core.

> `blake3` is deliberate: `PHASE1.md` §5.1 mandates BLAKE3-256 truncated to 64 bits for frame-name hashing, and the hash cannot be a `std` hasher because two **processes** intern into one arena.

- **D15 — Crash-consistency is a hard requirement, not a quality bar.** There must be no state a dead process can leave behind that a live process cannot detect and repair — no stuck seqlock, no unreapable claim, no wedged interning slot. This drives the single-store topology publish, the participant-slot indirection for claims, and the parity-forcing sample writer. Every new mutation protocol must be walked through the crash matrix and covered by a named crash point.
- **D16 — Ownership is configured, not negotiated.** One process owns the arena; others attach. No leader election, no consensus. On a real robot there is always a natural owner.

> **Amended by [`0005`](./decisions/0005-the-shared-memory-seam.md) §8 and [`0037`](./decisions/0037-a-takeover-is-not-a-second-open.md): "no takeover" does not survive; the rest does.** `PHASE2.md` §3.5 makes ownership a role the kernel reassigns on owner death. That is not negotiation: the heir is whichever survivor wins an uncontended `F_OFD_SETLK` on lock-file byte 0, the loser gets `Inheritance::Contended`, and no message is exchanged. `tf_tree_ipc::Session::take_over_ownership` takes the byte on the description the survivor already holds; `tf_tree::Tree::inherit_ownership` binds the rendezvous over the **existing** segment; `tf_tree::Tree::owner_lost` is the trigger. **Nothing calls the trigger for you** — no background thread, no daemon ([`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md)) — so an arena whose survivors never call it stays ownerless and turns new joiners away. `OpenOutcome` is `Joined | Created`; `TookOver` and `TakeoverUnsupported` do not exist. `PHASE2.md` §0.0's ownership-migration row is authoritative.

> **Further amended by [`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md): the §9 daemon is not built.** Pre-declaring topology is done without one: a read-only attach implies `CreatePolicy::Never`, a consumer waits with `Open::await_open` and `Tree::await_frames`, and headroom covers late frames. The capability survives as the optional subcommand `tf_tree serve`.

- **D17 — The attach socket is the liveness signal.** Participants hold their Unix socket open for the whole attachment. Process death closes it and the owner sees `EPOLLHUP` — exact, with no timeout to tune. Consequently **heartbeat staleness never triggers reaping**, and reaping is cooperative rather than owner-only so an owner's death does not leak every claim. *Do not* add heartbeat-based reaping by default, and *do not* close the socket after the handshake.

> **Amended by [`0057`](./decisions/0057-an-owner-is-not-dead-until-its-files-close.md): *exact* and *no timeout to tune* stand; *microseconds* does not.** The hangup happens when the kernel closes the dying process's files, after any core dump and address-space teardown, and not before a `fork` child sharing the description exits. Measured from a survivor: ~0.24 ms median for a `SIGKILL`ed 2.7 MiB process, ~98 ms for 1 GiB, ~1.1 s for a small `abort()` dumping to a pipe. A participant that dumps core keeps its connection and byte for its dump, so the owner's reap of it waits equally; that costs latency, not recoverability. `PHASE2.md` §3.5 states the survivor's half as NORMATIVE: `owner_lost()` answers `true` once the attach connection has hung up and the last description holding byte 0 has closed, with no added delay, heartbeat or timeout.

- **D18 — Read-only attach is the default for consumers.** A `PROT_READ` mapping means a buggy consumer *cannot* corrupt the tree, enforced by the MMU. It is the only real security boundary the design has — a read-write peer is trusted completely, and the docs must say so plainly.
- **D19 — Interest-based replication, never broadcast (Phase 8).** A subscriber declares which `(target, source)` pairs it needs at what rate and precision; the daemon subscribes to exactly the union.
- **D20 — Apache-2.0 / MIT dual license.** Cited as **D30** by `PHASE5.md` §10 ([`0006`](./decisions/0006-the-eight-phase-roadmap.md)).
- **D21 — The compatibility layer is Phase 7, and it is gated on evidence, not scheduled.** `tf2_ros::Buffer` compatibility and arena → `/tf` egress wait for operating experience: a real node on real hardware (`PHASE4.md` §1) and offline users (`PHASE5.md` §0). The shim is a hundred small semantic judgements, and each made without that experience is a guess shipped as a compatibility promise. Phase 4's bridge is **ingress-only** for the same reason. *Do not* schedule the shim; gate it. Cited as **D28**/**D29** by the Phase 4 and 5 specs.

> [`docs/PHASE7.md`](./PHASE7.md) is the requirements artifact and did not open the gate. Every surprise-log entry is filed against a J-row or opens a new one; a row answered from that document rather than from the log is the failure this decision prevents. Two judgements were decidable in advance and became records: the wait ([`0018`](./decisions/0018-blocking-waits-belong-in-the-shim.md)) and the stored claim ([`0017`](./decisions/0017-owned-handles-and-the-lifetime-rule.md)); their core-side halves are not gated by D21.

- **D22 — A disabled feature never forks the layout hash.** When a cargo feature is compiled out, the arena *regions* it would use stay declared and counted by `layout_hash`; only the code disappears. Sizing per feature set would make two correct participants of one version refuse to attach with a mismatch naming no actionable cause. First consumers: `PHASE5.md` §5.5 (`counters`) and §1.2 (the spline region, declared absent with offset `0`). Cited as **D34** by the Phase 5 spec.

> §1.2's two covariance fields were descoped by [`0009`](./decisions/0009-descoping-phase-6.md) but their eight bytes stay reserved **in place**: `spline_region_off` and `spline_degree` sit at 168 and 172, and `layout_hash` hashes region strides, not header fields, so it would not catch a disagreement about where the spline region begins.

### 5.1 The scheduled format-break ledger

**The queue a request to spend an arena byte joins, opened by [`0032`](./decisions/0032-the-region-table-was-not-part-of-the-purchase.md) part 2.** `FORMAT_VERSION = 3` bought the **header** and not the region table (`PHASE5.md` §1.2), so one more break is owed. **Do not add arena fields opportunistically**; "wait for the next break" means join this queue.

**Nothing in this table is authorised.** A row means *this is where the argument goes when the break is scheduled*, not *this ships in it*. Adding a row is not a decision; removing one is. [`0031`](./decisions/0031-the-participant-record-with-no-byte.md)'s row was removed on 2026-09-18: a served `build_shared` arena is out of contract, so no arena byte is wanted.

| Entry | Source | Why it is queued |
|---|---|---|
| Phase 6's spline region | `PHASE5.md` §1.2, [`0009`](./decisions/0009-descoping-phase-6.md) (Rationale, *B-splines*) | A spline *region* is a region and no region slot is reserved. `0009` cuts the other way (*"a spline evaluation needs a wider bracket read, not a new region shape"*), and nobody has designed Phase 6, so whether this is a region at all is open. If it is, it costs a `FORMAT_VERSION`. |
| An `EdgeMeta` provenance byte | no record | No record argues for or against it; listed so a future request has somewhere to be argued. |

## 6. Design smells — stop if you catch yourself doing these

- Reaching for `ArcSwap`, `Arc`, `Box`, `Vec`, or any pointer inside a structure that lives in the arena (D4)
- Adding a `String` to an error type or a hot path, a dependency to `tf_tree_core`, growth/resizing/reallocation anywhere, an async API or runtime, or a second parent or graph search in plan compilation
- Writing `unsafe` anywhere that is not one of the boundaries in [`0007`](./decisions/0007-the-unsafe-budget-and-the-c-abi.md) as amended by [`0048`](./decisions/0048-a-kind-is-not-a-crate-name.md) — the arena's memory, the OS, a foreign runtime **or library**, a foreign caller, our own C ABI called from Rust to exercise or measure it, and a trait the language requires be implemented unsafely in a target that never ships. **They are properties, not crate names**: `scripts/unsafe-budget.txt` indexes the file set and `scripts/unsafe-budget.sh` (inside `just lint`) checks it, so the criterion remains a review rule. Or writing `unsafe` a second time inside `tf_tree`, whose `#![deny(unsafe_code)]` carries exactly one `#[allow]`: `OwnedWriter`'s lifetime extension ([`0017`](./decisions/0017-owned-handles-and-the-lifetime-rule.md)), **the only lifetime extension in the workspace**. Both bindings go through `Tree::claim_owned`; a second extension is a new decision record, not a patch
- Weakening an atomic ordering because a test passes on x86-64 (the loom tests exist for this; aarch64 is a CI target)
- Adding a point-cloud type, GPU kernel or `deskew` helper (D8); recycling a `FrameId` or `EdgeId` (D10); "fixing" `LerpSlerp` so the right-invariance test passes (D5)
- Adding a mutation protocol without walking the crash matrix and adding a named crash point (D15)
- Closing the attach socket after the handshake, reaping on heartbeat staleness or from the owner only, or trusting a bare PID as an identity (D15, D17)
- Defaulting a consumer to read-write attach (D18); using `shm_open` instead of a sealed `memfd`, or skipping `MADV_DONTFORK`
- Adding public API to any binding without checking it against [`API.md`](./API.md) §1's six rules — in particular: an allocating or name-resolving operation on `Plan` (R2), a float stamp (R3), a default for a layout parameter (R4), or a user-storable type carrying a lifetime (§2.1)
- Adding a second spelling of an existing path — a `coverage` beside `span`, a `resample` beside `at(arange(...))` — instead of documenting the one that exists
- Putting a blocking wait, a futex, or any notification primitive in the arena ([`0018`](./decisions/0018-blocking-waits-belong-in-the-shim.md))

## 7. Glossary

| Term | Meaning |
|---|---|
| Edge | The relationship between a frame and its parent, storing `T_parent_child`. One per non-root frame. |
| Arena | The single flat allocation holding all records and buffers. Position-independent; relocatable by `memcpy`. |
| Plan | A compiled query: topology resolved, static edges folded, reduced to ≤`MAX_DEPTH` steps (**32**; the raw walk that produces it is bounded separately at `MAX_PATH_EDGES` = 64 — [`0034`](./decisions/0034-the-depth-bound-priced-two-slots-the-same.md)). |
| Claim | Exclusive write ownership of an edge, held by a `Publisher`. |
| Claim epoch | Counter bumped on every claim and reap. A `Publisher` checks it on every push so a revived zombie writer cannot resurrect a reaped claim. |

## 8. Document map

`PHASE1.md`–`PHASE5.md` and `PHASE7.md` are the normative phase specs (Phase 7 is **gated by D21**; its §4 J-table is what Phase 4's surprise log is filed against). `RUNBOOK.md` is the operator-facing failure-mode list, each row mapping to a `doctor` check. **`API.md` is not a phase**: the API contract — six rules (§1), per-binding surface (§2–§5), delta table (§6), new-surface check (§7) — read before adding public API anywhere.
