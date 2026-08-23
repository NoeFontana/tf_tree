# tf_tree — Phase 1 Implementation Specification

> **Companion document:** `docs/PROJECT.md` holds the project overview, the full eight-phase roadmap, and the decision log with rationale. When this spec does not answer a question, check the decision log there before choosing — several obvious-looking simplifications are deliberately excluded and the reasons are recorded.

**Deliverable:** a single-process transform tree engine in Rust, benchmarked against ROS 2 `tf2`.

**Critical framing:** Phase 2 maps the sample storage into shared memory so that a second process runs the *identical, unmodified* reader code against an `mmap`'d arena. Phase 1 is therefore not "the simple version" — it is the shared-memory version backed by a heap allocation instead of a `memfd`. Every layout decision in this document exists to make that swap a one-line change in `tf_tree_arena`. Do not simplify them away.

Sections marked **NORMATIVE** are requirements. Code blocks are illustrative unless the section says otherwise; signatures and layouts are normative even when bodies are sketches.

---

## 0. Non-goals and guardrails — read first

**NORMATIVE.** Do not implement any of the following in Phase 1. Each is either a later phase or a deliberate permanent exclusion.

| Excluded | Why |
|---|---|
| `async` / `tokio` / any runtime | The core is synchronous. A lookup is a pure function. |
| Generic scalar `T: RealField` | f64 only. Generics double the test matrix and the monomorphized code size for a benefit not yet measured. Revisit after benchmarks. |
| `serde` in `tf_tree_core` | Wire formats are Phase 6. Serialization pulls in allocation and breaks `no_std`. |
| Dynamic capacity growth | Growth means remapping, which invalidates reader mappings in Phase 2. Capacity is fixed at construction. |
| `String` in any error type or hot path | Errors carry IDs; `Display` resolves names by consulting the arena. Keeps errors `Copy` and `no_std`. |
| Covariance / uncertainty | Phase 5. Do not add fields for it now; the slot layout is exactly one cacheline and must stay that way. |
| Copy-on-write branches | Phase 5. |
| Any GPU code, CUDA dependency, or point-cloud apply | Permanently out of core. The engine's product is a sampled trajectory, not transformed points. |
| Network, discovery, multicast | Phase 6. |
| Deskew / point-cloud helpers | See above. `sample_many` is the whole surface. |

**Dependency budget for `tf_tree_core`:** `libm` (no_std transcendentals), `bytemuck` (checked POD casts), and `blake3`. Nothing else. `tf_tree_arena` adds `rustix` in Phase 2 only. Test/bench-only dependencies are unrestricted.

`blake3` is a **deliberate third entry**, resolving what was an outright
contradiction: §5.1 mandates `BLAKE3-256 truncated to 64 bits` for frame-name
hashing and justifies it with a collision analysis, while this budget listed two
crates. The hash cannot be swapped for a `std` hasher — Phase 2 has two
*processes* interning into one arena, so it must be deterministic across
processes, builds and toolchain versions, which rules out anything randomly
seeded, and the 64-bit truncation is only safe with cryptographic-quality
avalanche.

**Open cost, to be settled with the Phase 2 `FORMAT_VERSION` bump, not before:**
`blake3` pulls five runtime crates and a `cc` build dependency, and a C build
step is exactly what the safety-critical integrator D14 is written for would
object to. Replacing it with an inlined non-cryptographic hash of adequate
avalanche would change `FrameRecord::name_hash` and therefore the arena format,
so it is not a change to make casually mid-phase. Revisit it when the format
version moves anyway.

**Unsafe budget:** `#![forbid(unsafe_code)]` on `tf_tree_math`, `tf_tree_cli`. `tf_tree` is `#![deny(unsafe_code)]` with exactly one `#[allow]`. `unsafe` is permitted only in `tf_tree_arena` and in exactly two modules of `tf_tree_core` (`buffer.rs`, `arena_view.rs`), each of which must carry a module-level `// SAFETY:` doc block stating its invariants. Every `unsafe` block gets its own `// SAFETY:` comment naming which invariant it relies on.

> **`tf_tree`'s entry was corrected in place**, and this paragraph's crate list
> is Phase 1's only; the rest of the budget is unchanged. It said `forbid` for
> `tf_tree` until
> [`0017`](./decisions/0017-owned-handles-and-the-lifetime-rule.md) moved the
> crate to `deny` for one lifetime extension, in `OwnedWriter` — which exists to
> replace two hand-rolled `extend_to_static` helpers in crates the Rust test
> suite cannot instrument (`tf_tree_c`, `tf_tree_py`). **`0017` steps 6–7 have
> deleted both**, so it is now the workspace's only lifetime extension rather
> than one of three. The rule the list is a snapshot of is
> [`0007`](./decisions/0007-the-unsafe-budget-and-the-c-abi.md)'s criterion —
> `unsafe` only at a boundary the compiler cannot see across — and the phases
> after this one added `tf_tree_ipc`, `tf_tree_py` and `tf_tree_c` under it.
> Corrected rather than annotated because this section is normative and a rule
> that says `forbid` while the code says `deny` is worse than either.

**If a design question is not answered by this document, stop and ask rather than choosing.** The most expensive failure mode here is an agent that picks a reasonable-looking simplification in the concurrency or layout sections.

---

## 1. Workspace layout

```
tf_tree/
├── Cargo.toml                  # workspace, resolver = "2"
├── rust-toolchain.toml         # pinned stable
├── docs/
│   ├── PROJECT.md              # project overview — read this first
│   └── PHASE1.md               # this document
├── crates/
│   ├── tf_tree_math/           # no_std. SE(3)/SO(3), dual quats. zero unsafe.
│   │   └── src/{lib,quat,iso3,dualquat,interp,reference}.rs
│   ├── tf_tree_arena/          # no_std+alloc. Arena abstraction + layout math.
│   │   └── src/{lib,layout,heap,header}.rs
│   ├── tf_tree_core/           # no_std+alloc. The engine.
│   │   └── src/{lib,frame,topology,edge,buffer,arena_view,plan,sample,error}.rs
│   ├── tf_tree/                # std facade. Re-exports + ergonomic helpers.
│   ├── tf_tree_bench/          # criterion, incl. tf2 comparison harness
│   └── tf_tree_cli/            # binary `tf_tree` (alias `tft`)
└── xtask/                        # loom, miri, bench-gate runners
```

Crate names use underscores throughout (as `serde_json` and `parking_lot` do), so the import path matches the project name: `use tf_tree::...`.

`tf_tree_math` and `tf_tree_arena` are separately publishable and separately testable. Keeping the math crate free of unsafe and free of the arena means its property tests run under Miri in seconds.

---

## 2. Load-bearing invariants

**NORMATIVE.** These are the invariants every other section depends on. Encode each as a debug assertion where cheap, and as a documented `// INVARIANT:` comment at the definition site.

1. **Append-only identity.** `FrameId` and `EdgeId` are never reused. Removal is tombstoning (`kind = Tombstone`). A stale `Plan` may therefore index a valid, in-bounds record; it will fail the generation check, but it can never cause out-of-bounds access.
2. **No pointers in the arena.** Every intra-arena reference is a `u32` element index or byte offset relative to the arena base. The arena must be relocatable by `memcpy`.
3. **Fixed capacity.** `max_frames`, `max_edges`, and every per-edge ring capacity are set at construction. Ring capacities are powers of two.
4. **Single writer per edge.** Enforced by the claim table, not by convention. A second claim on a live edge is an error, never a silent success.
5. **Monotone head.** `EdgeRecord::head` is a monotonically increasing count of samples ever published. It is never masked in storage, only at access time. `u64` never wraps in practice (1 MHz for 500 000 years).
6. **Stamps are non-decreasing per edge.** `push` with a stamp `<` the current head stamp is rejected with `NonMonotonicStamp`. Equal stamps are accepted and the newer value wins (this is required for idempotent replay).
7. **All multi-byte arena fields are little-endian.** Phase 6 wire encoding is separate; the arena is host-native but must assert LE at construction.
8. **Every heap allocation happens at construction.** `push` and `at` allocate nothing. Enforced by a counting allocator in tests, not by inspection.

---

## 3. `tf_tree_math`

### 3.1 Types

**NORMATIVE layouts.**

```rust
#[repr(C)] #[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vec3 { pub x: f64, pub y: f64, pub z: f64 }              // 24 B

/// Hamilton convention, w first, right-handed, active rotation.
/// INVARIANT: callers of unchecked constructors guarantee |q| == 1 within 1e-12.
#[repr(C)] #[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quat { pub w: f64, pub x: f64, pub y: f64, pub z: f64 }  // 32 B

/// T_parent_child. Applying to a point in `child` yields the point in `parent`.
#[repr(C, align(64))] #[derive(Clone, Copy, Debug, PartialEq)]
pub struct Iso3 { pub q: Quat, pub t: Vec3, _pad: [u8; 8] }         // 64 B
```

**Convention lock-in.** Hamilton (not JPL). `w` first (not last — note this differs from Eigen's storage order; the C++ wrapper in Phase 4 must transpose). Active rotations. `Iso3` composition `a * b` means `T_a_x * T_x_b`. Adjoint convention is right-perturbation: `T = T̂ · exp(ξ^)`. Write these five facts in the crate-level doc comment; every downstream bug in this project will trace back to one of them.

Assert layout in a test: `assert_eq!(size_of::<Iso3>(), 64)`, `assert_eq!(align_of::<Iso3>(), 64)`.

### 3.2 SE(3) exponential and logarithm

Required operations: `exp_se3(xi: [f64; 6]) -> Iso3`, `log_se3(t: Iso3) -> [f64; 6]`, with `xi = [ω(3), v(3)]`.

```
exp:  R = exp_so3(ω)              t = V(ω) · v
log:  ω = log_so3(R)              v = V⁻¹(ω) · ω_t   where ω_t is the translation
```

with, for `θ = |ω|` and `W = [ω]×`:

```
V(ω)   = I + c1·W + c2·W²        c1 = (1 − cos θ)/θ²      c2 = (θ − sin θ)/θ³
V⁻¹(ω) = I − ½·W + c3·W²         c3 = 1/θ² − (1 + cos θ)/(2θ·sin θ)
```

### 3.3 Numerical requirements — measured, not assumed

**NORMATIVE.** Two findings drive this section. Both were verified against a 50-digit reference; do not weaken them.

**(a) `log_so3` must go through the quaternion, never through `acos(trace)`.**

Measured relative error of `log_so3` across rotation magnitudes:

| ‖ω‖ | `acos((tr R − 1)/2)` form | `2·atan2(‖q_v‖, q_w)` form |
|---|---|---|
| 1e-7 | 3.2e-16 | 2.0e-16 |
| 1e-3 | 4.4e-16 | 2.2e-16 |
| 1.0 | 2.7e-16 | 2.2e-16 |
| ~π | **1.3e-7** | 3.0e-16 |

The trace form loses nine significant digits near θ = π. A rear-facing camera or a flipped IMU mount is a π rotation, and these are among the most common static transforms in any real robot. Use:

```rust
let n = (q.x*q.x + q.y*q.y + q.z*q.z).sqrt();
let theta = 2.0 * libm::atan2(n, q.w);   // then wrap to (−π, π]
```

**(b) The small-angle series threshold is θ < 0.1, not 1e-8, and needs four terms.**

The closed forms for `c1`, `c2`, `c3` suffer catastrophic cancellation long before they underflow. Measured relative error of the float64 closed form versus the 4-term series:

| θ | c1 closed / series | c2 closed / series | c3 closed / series |
|---|---|---|---|
| 1e-6 | 8.9e-5 / 4.4e-17 | 7.8e-5 / 1.6e-17 | 9.8e-4 / 6.9e-17 |
| 1e-4 | 5.2e-9 / 4.2e-17 | 3.1e-8 / 9.7e-17 | 1.2e-7 / 4.2e-17 |
| 1e-2 | 2.9e-13 / 2.7e-18 | 2.6e-12 / 2.3e-17 | 3.9e-12 / 1.7e-17 |
| 1e-1 | 1.1e-14 / 5.6e-15 | 1.8e-14 / 1.5e-15 | 9.1e-14 / 2.6e-15 |
| 3e-1 | 8.6e-16 / 3.6e-11 | 4.1e-15 / 9.9e-12 | 2.4e-14 / 1.6e-11 |

The crossover where both are accurate is a narrow band around θ ≈ 0.1. A threshold of 1e-8 — the value most SE(3) libraries use, and the one you would naturally reach for — leaves the closed form running in the regime where it has already lost 4 to 11 digits.

**Required constants** (coefficients of θ^0, θ², θ⁴, θ⁶):

```rust
const THETA_SMALL: f64 = 0.1;   // NORMATIVE
// c1 = (1 − cos θ)/θ²
const C1: [f64; 4] = [1.0/2.0,  -1.0/24.0,  1.0/720.0,   -1.0/40320.0];
// c2 = (θ − sin θ)/θ³
const C2: [f64; 4] = [1.0/6.0,  -1.0/120.0, 1.0/5040.0,  -1.0/362880.0];
// c3 = 1/θ² − (1 + cos θ)/(2θ sin θ)
const C3: [f64; 4] = [1.0/12.0,  1.0/720.0, 1.0/30240.0,  1.0/1209600.0];
```

Evaluate by Horner in `θ²`. Four terms are mandatory at this threshold — three terms would need `THETA_SMALL = 0.01`. Add a test that sweeps θ across `[1e-12, π]` on a log grid and asserts relative error against a hardcoded high-precision reference table stays below 1e-14, with no discontinuity exceeding 1e-15 across the branch boundary.

### 3.4 Interpolation

```rust
pub trait Interp {
    fn eval(a: &Iso3, b: &Iso3, s: f64) -> Iso3;
}
pub struct LerpSlerp;   // tf2-compatible
pub struct ScLerp;      // SE(3) geodesic — default
```

**`ScLerp` gets two implementations.**

- `reference::sclerp` — `a * exp_se3(s * log_se3(a.inverse() * b))`. Obvious, slow, and the definition of correct.
- `sclerp` — unit dual quaternion power. Extract screw parameters `(θ, d, l, m)` from `a* ⊗ b`, scale, recompose. One `atan2` and one `sin_cos` total, versus two of each for the log/exp route.

**NORMATIVE:** a differential proptest asserts the two agree to 1e-14 over 10⁵ random pairs including near-identity and near-π cases. This reference/fast pairing is a pattern to repeat everywhere in this project — write the obvious version first, keep it, and test the fast one against it forever.

`LerpSlerp` is `t = (1−s)·t_a + s·t_b`, `q = slerp(q_a, q_b, s)` with the standard shortest-arc sign fix (`if q_a·q_b < 0 { negate q_b }`) and a LERP fallback when the half-angle is below 1e-6.

**Invariance properties**, asserted as tests because they encode the design claim:

| Property | ScLerp | LerpSlerp |
|---|---|---|
| `interp(G·a, G·b, s) == G·interp(a, b, s)` | must hold to 1e-13 | must hold to 1e-13 |
| `interp(a·H, b·H, s) == interp(a, b, s)·H` | must hold to 1e-13 | **must be asserted to FAIL** |

The second row is a positive test that `LerpSlerp` is not right-invariant. Write it as `assert!(max_err > 1e-6)` over a fixed seeded set, with a comment explaining that this is the whole reason `ScLerp` is the default. If someone later "fixes" `LerpSlerp`, this test tells them they have changed its semantics.

---

## 4. `tf_tree_arena`

### 4.1 Header

**NORMATIVE layout.** All offsets are byte offsets from arena base.

```rust
pub const TF_TREE_MAGIC: [u8; 8] = *b"TF_TREE\0";  // byte array, not a u64 literal: no endianness ambiguity
pub const FORMAT_VERSION: u32 = 1;

#[repr(C, align(64))]
pub struct ArenaHeader {
    pub magic: u64,
    pub format_version: u32,
    pub layout_hash: u32,        // compile-time hash of all repr(C) sizes/offsets
    pub arena_size: u64,
    pub max_frames: u32,
    pub max_edges: u32,
    pub stamp_slots: u32,        // total across all edges
    pub pose_slots: u32,
    pub frame_table_off: u32,
    pub frame_hash_off: u32,
    pub topo_block_off: u32,     // 2 blocks, contiguous
    pub topo_block_stride: u32,
    pub claim_table_off: u32,
    pub edge_table_off: u32,
    pub stamp_arena_off: u32,
    pub pose_arena_off: u32,
    pub topo_generation: AtomicU64,  // seqlock: odd = write in progress
    pub topo_active: AtomicU32,      // 0 or 1
    pub frame_count: AtomicU32,
    pub edge_count: AtomicU32,
    pub creator_pid: u32,
    pub creator_boot_id: u64,
    _reserved: [u8; 40],
}
```

`layout_hash` is a `const fn` over `size_of` and `offset_of` for every arena struct. Phase 2 checks it on attach; a mismatch is a hard error, not a warning. Compute it now even though nothing reads it yet — retrofitting it after processes exist in the wild is impossible.

`creator_boot_id` comes from `/proc/sys/kernel/random/boot_id` (Phase 2 uses it to detect a stale segment surviving a reboot). Phase 1 populates it and does nothing else with it.

### 4.2 Arena trait

```rust
pub unsafe trait Arena: Send + Sync {
    fn base(&self) -> *mut u8;
    fn len(&self) -> usize;
}

pub struct HeapArena { /* aligned Vec<u8>, 64-byte aligned */ }
// Phase 2: pub struct MappedArena { /* memfd + mmap */ }
```

`HeapArena::new(layout: &ArenaLayout)` allocates `layout.total_size()` bytes with 64-byte alignment, zeroes them, and writes the header. **The only Phase 2 change in the entire codebase should be adding `MappedArena` and a constructor that selects it.** If you find yourself needing to change anything in `tf_tree_core` to support that, the Phase 1 design was wrong — stop and report it.

### 4.3 Layout computation

```rust
pub struct ArenaLayout {
    pub max_frames: u32,
    pub max_edges: u32,
    pub edge_capacities: Vec<u32>,   // per edge, power of two, 0 for static
}
```

Region sizes, each 64-byte aligned and laid out in header order:

| Region | Size |
|---|---|
| header | 256 |
| frame table | `max_frames * 64` |
| frame hash | `next_pow2(2*max_frames) * (8 + 4)` |
| topology blocks | `TOPO_BLOCKS * align64(max_frames * 10)` |
| claim table | `max_edges * 64` |
| edge table | `max_edges * 128` |
| stamp arena | `sum(capacities) * 8` |
| pose arena | `sum(capacities) * 64` |

A 1000-frame, 1000-edge tree with 4096 samples per edge: ~260 MB of pose arena. Note that in a real robot only a handful of edges are dynamic, so size capacities per edge rather than uniformly. Provide `ArenaLayout::from_edges(&[(FrameName, EdgeKind, Capacity)])`.

**Topology block stride is 10 bytes per frame, not 6.** §5.3 requires
`edge_of_child[c]` to live *in the topology block* so plan compilation is a pure
array walk, and that makes a block `parent: u32` + `edge_of_child: u32` +
`depth: u16` = 10 B/frame. The 6-byte figure predates that requirement and is
wrong. This is not merely an accounting fix: keeping `edge_of_child` inside the
block is what puts it under the *same* double-buffer publish as `parent` and
`depth`, so a reader always observes a consistent `(parent, depth, edge)` triple.
Storing it anywhere else would reintroduce the torn-read this design exists to
prevent. The two `u32` arrays are placed first so both stay 4-byte aligned for
any `max_frames`, with the `u16` `depth` array trailing.

`TOPO_BLOCKS` is **2** in Phase 1. `PHASE2.md` §1 A1 raises it to 4; the stride
is unchanged.

---

## 5. `tf_tree_core` records

### 5.1 Frames

```rust
#[repr(C, align(64))]
pub struct FrameRecord {
    pub name_hash: u64,       // BLAKE3-256 truncated to 64 bits, of the full name
    pub name: [u8; 48],       // UTF-8, NUL-padded, truncated for display only
    pub name_len: u8,
    pub flags: u8,
    _pad: [u8; 6],
}
```

48 bytes covers every real frame name; longer names hash in full but display truncated. `FrameId` is a `NonZeroU32` so `Option<FrameId>` is 4 bytes — index 0 is reserved as "no parent" (root sentinel).

**Interning table:** open addressing, linear probing, `next_pow2(2 * max_frames)` slots. Two parallel arrays: `hashes: [AtomicU64]` (0 = empty) and `ids: [AtomicU32]` (`u32::MAX` = not yet published).

```
intern(name):
  h = blake3_64(name)
  i = h & mask
  loop:
    cur = hashes[i].load(Acquire)
    if cur == h:
       spin until ids[i].load(Acquire) != U32_MAX; return that id
    if cur == 0:
       if hashes[i].compare_exchange(0, h, AcqRel, Acquire).is_ok():
          id = frame_count.fetch_add(1, AcqRel) + 1
          write FrameRecord[id]
          ids[i].store(id, Release)
          return id
       else: continue   // lost the race, re-read this slot
    i = (i + 1) & mask
```

The publish-then-spin dance exists because Phase 2 has two processes interning concurrently. It costs nothing in Phase 1 and cannot be retrofitted. Collision on `h` with a different name is a hard error (`FrameHashCollision`) — check the stored name on hash match. 64 bits at 10⁴ frames gives ~3e-12 collision probability, but detect it rather than corrupt silently.

### 5.2 Topology

```rust
#[repr(C)]
pub struct TopologyBlock {
    // both arrays are max_frames long, indexed by FrameId
    // parent[i] == 0 means root or unattached
    // parent: [u32; max_frames]
    // depth:  [u16; max_frames]
}
```

Two blocks, double-buffered. **`ArcSwap` is forbidden here** — `Arc` refcounts do not cross a process boundary and this is the single most tempting Phase-1 simplification.

**Writer protocol (topology mutation):**

```
g = topo_generation.load(Relaxed);            debug_assert!(g % 2 == 0)
topo_generation.store(g + 1, Release);        // mark unstable
inactive = 1 - topo_active.load(Relaxed)
copy active block -> inactive block, apply mutation, recompute depths
topo_active.store(inactive, Release)
topo_generation.store(g + 2, Release)         // mark stable
```

Topology mutations are serialized by a single `Mutex` on the builder side. They occur at most a few hundred times over a process lifetime.

**Reader protocol (plan compilation only — `at()` never reads topology):**

```
loop {
    g1 = topo_generation.load(Acquire)
    if g1 & 1 != 0 { spin_loop(); continue }
    blk = topo_active.load(Acquire)
    ... read parent/depth, build the step list ...
    fence(Acquire)
    if topo_generation.load(Relaxed) == g1 { return plan_with_generation(g1) }
}
```

`Plan::at()` performs one `Relaxed` load of `topo_generation` and compares to the plan's stored generation. Mismatch is `TopologyChanged`, which is a legitimate, actionable error meaning "re-plan". It is not a failure to hide with a retry loop.

**Cycle detection:** on every mutation, walk from the new child to root with a step budget of `max_frames`; exceeding it is `WouldCreateCycle`. Depth recomputation is a BFS over the whole block — O(max_frames), fine at mutation rates.

### 5.3 Edges

```rust
#[repr(C, align(64))]
pub struct EdgeRecord {
    pub parent: u32,
    pub child: u32,
    pub kind: u8,          // 0 Dynamic, 1 Static, 2 Tombstone
    pub interp: u8,        // InterpPolicy discriminant
    pub domain: u8,        // time domain id
    _pad0: u8,
    pub capacity: u32,     // power of two; 0 for Static
    pub stamp_off: u32,    // element index into stamp arena
    pub pose_off: u32,     // element index into pose arena
    _pad1: u32,
    pub head: AtomicU64,   // monotone total samples published
    pub static_pose: [u64; 7],  // f64 bit patterns; Static only
    _pad2: [u8; 40],
}
```

`EdgeId` indexes this table. The edge for child frame `c` is found via a `u32` side array `edge_of_child[c]` living in the topology block — this makes plan compilation a pure array walk with no search.

### 5.4 Claims

```rust
#[repr(C, align(64))]
pub struct ClaimRecord {
    pub state: AtomicU32,        // 0 free, 1 held
    pub owner_pid: u32,
    pub owner_boot_id: u64,
    pub heartbeat: AtomicU64,    // bumped by the writer on every push
    pub claim_epoch: AtomicU64,  // incremented on every successful claim
    _pad: [u8; 32],
}
```

Claim is a `compare_exchange(0, 1, AcqRel, Acquire)`. Failure is `EdgeAlreadyClaimed { owner_pid }`. Release stores 0 with `Release`. A `Publisher` handle holds the `EdgeId` and the `claim_epoch` it observed; `Drop` releases. Phase 1 implements claim, release, and epoch. The liveness reaper (heartbeat staleness plus PID/boot-id check) is Phase 2 — the fields exist now so the record layout never changes.

---

## 6. Sample buffers — the concurrency core

**This is the section to get right. Everything else is recoverable.**

### 6.1 Slot layout

```rust
#[repr(C, align(64))]
pub struct PoseSlot {
    pub seq: AtomicU32,        // even = stable, odd = write in progress
    _pad: u32,
    pub data: [AtomicU64; 7],  // f64 bit patterns: qw qx qy qz tx ty tz
}
```

Exactly 64 bytes — one cacheline, one slot, no false sharing between adjacent samples during a read.

**Why `[AtomicU64; 7]` and not a plain `[f64; 7]` behind `UnsafeCell`:** the classic seqlock reads data non-atomically and discards the result on a version mismatch. That is a data race, and therefore UB in the Rust and C++ memory models, even though it works on every real CPU. Using relaxed atomic loads makes the protocol sound with zero runtime cost — a `Relaxed` `AtomicU64` load compiles to a plain `mov` on x86-64 and `ldr` on aarch64. Do not "optimize" this into a `memcpy`.

Stamps live in a **separate** array of `AtomicI64`, so binary search touches 8 stamps per cacheline and never pulls in pose data. The seqlock in the pose slot protects the logical sample, covering both arrays.

### 6.2 Publish protocol (single writer)

> **Two lines of the listing below were corrected in place** — it had drifted
> from the shipped `SampleRing::push`, and this section is normative, so the
> listing is now the amended version rather than the original with a note
> attached. What changed, and why, so a reader comparing this against an older
> revision is not left guessing:
>
> - **The odd flip** is `slot.seq.load(Relaxed) | 1`, where this listing used to
>   read `s.wrapping_add(1)`. That is amendment **A5** in
>   [`PHASE2.md`](./PHASE2.md) §1: forcing the parity instead of incrementing it
>   means a writer killed mid-write leaves a stale odd value the next writer
>   heals idempotently, rather than landing its `s+1` on an even value and
>   inverting the protocol for that slot from then on. A5 shipped in the code;
>   this listing had never been updated for it.
> - **The heartbeat** is a plain `store(h + 1)`, where this listing used to read
>   `fetch_add(1)`. The ordering annotation is unchanged — `Relaxed` either way —
>   and no atomic ordering is weakened. Only the atomicity goes, which bought
>   nothing: the ring is single-writer by construction (D7), the same guarantee
>   the neighbouring plain `head.store` already rests on, and `heartbeat` equals
>   `head` at every quiescent point because `head` is written in exactly one
>   place and never reset. The stored value is identical. Measured 8.66 → 4.65
>   ns/push.
>
> The ordering annotations remain NORMATIVE as written; neither correction
> weakens one.

```rust
// NORMATIVE ordering annotations.
fn push(&mut self, stamp: i64, iso: &Iso3) -> Result<(), PushError> {
    let h = self.rec.head.load(Ordering::Relaxed);   // single writer: Relaxed is correct
    if h > 0 {
        let last = self.stamps[((h - 1) & self.mask) as usize].load(Ordering::Relaxed);
        if stamp < last { return Err(PushError::NonMonotonicStamp { last, got: stamp }); }
    }
    let idx = (h & self.mask) as usize;
    let slot = &self.poses[idx];

    let s = slot.seq.load(Ordering::Relaxed) | 1;           // A5: force, do not increment
    slot.seq.store(s, Ordering::Relaxed);                   // -> odd (idempotent if already)
    core::sync::atomic::fence(Ordering::Release);

    self.stamps[idx].store(stamp, Ordering::Relaxed);
    for (i, w) in iso.to_bits().iter().enumerate() {
        slot.data[i].store(*w, Ordering::Relaxed);
    }

    slot.seq.store(s.wrapping_add(1), Ordering::Release);   // -> even, publishes data
    self.rec.head.store(h + 1, Ordering::Release);          // publishes the sample
    self.claim.heartbeat.store(h + 1, Ordering::Relaxed);   // single writer: a store, not an RMW
    Ok(())
}
```

### 6.3 Read protocol

```rust
const SEQ_RETRY_LIMIT: u32 = 64;

fn read_slot(&self, idx: usize) -> Result<Iso3, LookupError> {
    let slot = &self.poses[idx];
    for _ in 0..SEQ_RETRY_LIMIT {
        let s1 = slot.seq.load(Ordering::Acquire);
        if s1 & 1 != 0 { core::hint::spin_loop(); continue; }
        let mut bits = [0u64; 7];
        for i in 0..7 { bits[i] = slot.data[i].load(Ordering::Relaxed); }
        core::sync::atomic::fence(Ordering::Acquire);
        if slot.seq.load(Ordering::Relaxed) == s1 {
            return Ok(Iso3::from_bits(&bits));
        }
    }
    Err(LookupError::SlotContended { edge: self.id })
}
```

**NORMATIVE:** every ordering annotation above is deliberate. The `fence(Acquire)` before the second `seq` load prevents the data loads from being reordered after it on weakly-ordered targets; it is a no-op on x86-64 and a real barrier on aarch64. **Do not weaken any of these to `Relaxed` on the grounds that a test passes on x86.** The loom tests in §10.2 exist to catch exactly that.

### 6.4 Bracket search

```
sample(edge, t, policy):
  h = head.load(Acquire)
  if h == 0                            -> Err(NoData)
  n = min(h, capacity)
  lo_logical = h - n                    // oldest valid logical index
  t_old = stamps[lo_logical & mask]
  t_new = stamps[(h-1) & mask]

  if t < t_old                          -> Err(Extrapolation{ before })
  if t > t_new                          -> per policy: Err(Extrapolation{ after }) | Hold | ConstantTwist
  if t == t_new                         -> read_slot(h-1)

  // binary search over LOGICAL indices in [lo_logical, h-1] for the last index with stamp <= t
  // map each probe through `& mask`
  i = partition_point(...)
  if stamps[i & mask] == t              -> read_slot(i & mask)      // exact hit, no interp
  a = read_slot(i & mask); b = read_slot((i+1) & mask)
  s = (t - stamps[i & mask]) as f64 / (stamps[(i+1) & mask] - stamps[i & mask]) as f64
  result = Interp::eval(&a, &b, s)

  // revalidate: the ring must not have lapped us mid-read
  if head.load(Acquire) - i > capacity  -> Err(SlotRecycled{ edge })
```

The trailing revalidation is what makes the read wait-free-in-practice rather than merely lock-free: with 4096 samples at 1 kHz you have 4 seconds of slack, so it never fires outside a pathological stall. Return the error rather than looping; the caller knows whether a retry makes sense.

Binary search over logical indices with masking on probe is required — searching the physical array directly is wrong once the ring has wrapped, and this is a classic off-by-one source. Add a test that specifically exercises a buffer that has wrapped 3.5 times.

---

## 7. Plan compilation and evaluation

### 7.1 Step representation

```rust
pub const MAX_DEPTH: usize = 32;
pub const MAX_PATH_EDGES: usize = 64;

#[derive(Clone, Copy)]
pub enum Step {
    Static(Iso3),
    Dyn { edge: EdgeId, inverted: bool },
}

pub struct Plan {
    generation: u64,
    steps: [Step; MAX_DEPTH],
    len: u8,
    domain: u8,
}
```

Fixed array, no `SmallVec`, no allocation, no dependency.

**Two bounds, and they price different slots** ([`0034`](./decisions/0034-the-depth-bound-priced-two-slots-the-same.md)).
`MAX_DEPTH` bounds the *compiled* plan, counted **after** §7.2's folding: a slot
there is a `Step`, **128 bytes measured**, carried by value in every `Plan` and
in a 16-slot thread-local cache. `MAX_PATH_EDGES` bounds the *raw walk*: a slot
there is a `u32` in `compile`'s stack frame. 128 bytes against 4 is why one
number cannot price both, and this section said otherwise until `0034` — it read
"combined depth exceeding 16 is `TreeTooDeep` — generous, since real trees are
4–8", which is sound about a moving `/tf` graph and wrong about a rigid
assembly, where a 20-link fixed chain folds to **one step** and was refused
anyway.

Either bound overrun is `TreeTooDeep`, one variant for both because the C ABI's
status table is frozen. Its `depth` field says which: `MAX_PATH_EDGES + 1` is the
walk refusing (the walk stops when it runs out of buffer, so it never learns the
real length), and anything at or below `MAX_PATH_EDGES` is the **exact** folded
step count.

"Real trees are 4–8" is retired as a justification, and what replaces it is a
survey rather than an intuition: 91 real robot descriptions from 26 repositories,
whose worst *graph diameter* — up to the lowest common ancestor and back down,
which is the quantity a lookup pays, not root-to-leaf depth — is **30 joints**,
p95 24, median 10. 32 is the next power of two above 30; 64 is ~1.9× the 30 plus
a deployed `map → odom → base_footprint` prefix.

### 7.2 Compilation

Edge for child `c` stores `T_parent(c)_c`. For `lookup(target, source)`:

```
T_target_source = (T_lca_target)⁻¹ · T_lca_source
```

```
compile(target, source):
  if target == source: return empty plan
  a = target; b = source
  up_t = []; up_s = []
  while depth[a] > depth[b]: up_t.push(a); a = parent[a]
  while depth[b] > depth[a]: up_s.push(b); b = parent[b]
  while a != b:
      if parent[a] == 0 || parent[b] == 0 -> Err(Disconnected{ cut_at: a })
      up_t.push(a); a = parent[a]
      up_s.push(b); b = parent[b]
  // up_t is [target, .., child_of_lca];  emit in that order, inverted
  for f in up_t:            steps.push(Dyn{ edge_of_child[f], inverted: true })
  // up_s is [source, .., child_of_lca]; emit REVERSED, forward
  for f in up_s.rev():      steps.push(Dyn{ edge_of_child[f], inverted: false })
```

Verify the direction by hand once against a three-frame example before writing code; getting it backwards produces a plausible-looking transform that is wrong everywhere.

**Constant folding, applied after compilation:**

1. Replace any `Dyn` whose edge `kind == Static` with `Static(pose)` (pre-inverting if `inverted`).
2. Collapse every run of adjacent `Static` into a single `Static` by composing them.

A depth-6 chain with 4 static edges typically folds to 3 steps. Assert in a test that the canonical URDF fixture folds from 6 steps to 3.

**Folding takes the walk's two `u32` buffers, not an intermediate `[Step; MAX_DEPTH]`
array** ([`0034`](./decisions/0034-the-depth-bound-priced-two-slots-the-same.md)).
The buffers hold everything a step does — an edge id, plus an `inverted` flag
that is `true` iff the edge came from the target side — so the copy was free to
delete, and deleting it is what lets `MAX_PATH_EDGES` be generous without a
second 128-bytes-a-slot array.

Two properties of the fold are load-bearing and neither is obvious:

* **The source half is emitted in reverse of walk order**, which is what makes
  the composition associate `((s[n-1] · s[n-2]) · …)`. `Iso3` composition is not
  associative under rounding and every test in the suite is tolerance-based
  (`TOL = 1e-12`), so a change that folds during the walk — meeting `s[0]` first
  — would produce different bits and pass every test. Verify a change here
  against **bits**, not tolerance.
* **The loop does not stop when the output array fills.** It skips the write,
  keeps counting, and goes on resolving every remaining edge, so (a) `TreeTooDeep`
  reports the true folded length rather than the bound, and (b) `UnknownEdge` and
  `MixedTimeDomains` still win over `TreeTooDeep` for a defect that sits past the
  bound. Stopping early is cheaper and was measured; it is not what ships, and
  `error_precedence_over_defect_kind_position_and_foldability` in
  `crates/tf_tree_core/src/tests.rs` is the table that pins the difference.

### 7.3 Evaluation

```rust
pub fn at(&self, g: &Guard, t: Stamp) -> Result<Iso3, LookupError> {
    let cur = g.header.topo_generation.load(Ordering::Relaxed);
    if cur != self.generation {
        return Err(LookupError::TopologyChanged { plan: self.generation, current: cur });
    }
    let mut acc = Iso3::IDENTITY;
    for step in &self.steps[..self.len as usize] {
        acc = match step {
            Step::Static(m) => acc * *m,
            Step::Dyn { edge, inverted } => {
                let p = g.sample(*edge, t)?;
                if *inverted { acc.mul_inv(&p) } else { acc * p }
            }
        };
    }
    Ok(acc)
}
```

`mul_inv(a, b) = a * b⁻¹` computed directly rather than inverting then composing — saves a negation pass and a rotation. Provide it, and differential-test it against the naive form to 1e-14.

### 7.4 Batch sampling

```rust
pub fn at_many(&self, g: &Guard, stamps: &[Stamp], out: &mut [Iso3]) -> Result<(), LookupError>;
pub fn at_adaptive(&self, g: &Guard, span: (Stamp, Stamp), tol: ErrBound)
    -> Result<(&[Stamp], &[Iso3]), LookupError>;
```

`at_many` detects monotone input and, when monotone, replaces binary search with an exponential (galloping) search resuming from the previous index — O(1) amortized instead of O(log n).

`at_adaptive` emits the minimum knot set such that linear interpolation between knots stays within `tol`. Bisect recursively: evaluate the midpoint exactly, compare against the LERP of the endpoints, subdivide if the error exceeds tolerance. Bound recursion depth at 16 and knot count at 4096. This is the API that replaces the abandoned deskew helper: the consumer LERPs between knots on whatever device the points live on, and the error is bounded by construction. Typical output for a 100 ms sweep at 1 cm / 1e-4 rad tolerance is tens of knots, not thousands.

`at_adaptive` may allocate from a caller-provided scratch buffer only. No global allocation.

---

## 8. Public API surface

**Edges are declared on the builder, before `build()`.** An earlier draft of this
section declared them *after* — `tree.declare_dynamic(odom, base, EdgeCfg { capacity: 8192, .. })`
— which cannot work: the arena is a single fixed allocation whose pose region is
`sum(per-edge capacity) × 64 B`, sized when the bytes are allocated, and invariant
3 forbids growth. A post-`build` declaration would have nowhere to put its ring.
`build()` therefore sizes the arena from exactly the declarations it was given.
The only runtime topology change is [`Tree::reparent`], which reuses an
already-declared edge and allocates nothing. See
[`docs/decisions/0004`](./decisions/0004-builder-time-edge-declaration.md) for the
full argument.

```rust
// construction — topology is declared on the builder, which is what lets
// `build()` size the arena from exactly these edges
let tree = TreeBuilder::new()
    .default_interp(Interp::ScLerp)
    .static_edge("base_link", "camera_mount", &iso)
    .dynamic_edge("odom", "base_link", EdgeCfg::new(Capacity::history(200.0, 10.0)))
    .frame_headroom(8)                        // only if names are interned later
    .build()?;                                // -> Tree (owns a HeapArena)

// resolution
let base: FrameId = tree.frame("base_link")?;
let cam:  FrameId = tree.frame("camera_optical")?;

// writing
let mut pubr: Publisher = tree.claim(odom, base)?;   // exclusive; Drop releases
pubr.push(stamp, &iso)?;                             // wait-free, no alloc

// reading
let plan: Plan = tree.plan(cam, map)?;               // compile once
let g: Guard = tree.guard();                         // pins generation + arena
let t = plan.at(&g, stamp)?;

// convenience path — interned + plan-cached internally, for casual users
let t = tree.lookup("map", "camera_optical", stamp)?;
```

`Tree: Send + Sync`. `Plan: Send + Sync + Copy`. `Publisher: Send + !Sync` (single writer is a type-level property, not a convention). `Guard<'a>` borrows the tree.

The convenience `lookup` keeps a small per-thread plan cache keyed by `(arena, FrameId, FrameId, generation)`, 16 entries, direct-mapped. Progressive disclosure: casual use is fast, expert use is fastest.

The `arena` component was missing from this line, and from the code, until issue #196: the cache is `thread_local!` and shared by every `Tree` the thread touches, while the other three components agree across two trees built from the same names in the same order — ids are handed out in interning order and a fresh tree's generation is its declared edge count — so a second tree was served the first one's compiled plan. It is the arena's identity, not the handle's: two `Tree`s mapping one shared segment share one entry deliberately, because they share one topology.

### Time

```rust
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Stamp<D: Domain = SystemDomain>(i64, PhantomData<D>);  // nanoseconds

pub enum Query { At(Stamp), Latest, LatestCommon, Bracket(Stamp, i64) }
```

Phase 1 implements `At`, `Latest`, `LatestCommon`. `LatestCommon` is the largest stamp for which *every* dynamic edge on the plan has data — compute it as `min` over the plan's edges of their newest stamp. Document it explicitly; this is what tf2's `Time(0)` means and its documentation is the source of endless confusion.

Domains are phantom types in Phase 1 with a runtime `u8` tag stored on the edge. Cross-domain lookup is `TimeDomainMismatch`. The alignment machinery is Phase 6; the type-level separation must exist now so it is not a breaking change later.

---

## 9. Errors

```rust
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum LookupError {
    UnknownFrame { hash: u64 },
    Disconnected { target: FrameId, source: FrameId, cut_at: FrameId },
    TreeTooDeep { depth: u16 },
    NoData { edge: EdgeId },
    Extrapolation { edge: EdgeId, requested: i64, oldest: i64, newest: i64 },
    SlotRecycled { edge: EdgeId },
    SlotContended { edge: EdgeId },
    TopologyChanged { plan: u64, current: u64 },
    TimeDomainMismatch { expected: u8, got: u8 },
}
```

`Copy`, no allocation, `no_std`. **Every variant that can name an edge does name one** — "lookup would require extrapolation into the future" without saying *which edge* is the single most-complained-about thing in tf2, and fixing it costs one field.

`Display` is implemented on a wrapper `Described<'a>(LookupError, &'a Tree)` that resolves IDs to names by consulting the arena. This keeps the error itself allocation-free while giving humans readable messages.

---

## 10. Test plan

### 10.1 Property tests (`proptest`)

Minimum set, each over ≥10⁴ cases with a fixed seed in CI:

1. `(a * b) * c ≈ a * (b * c)` within 1e-12
2. `a * a.inverse() ≈ IDENTITY`
3. `exp_se3(log_se3(T)) ≈ T` within 1e-13, including near-π and near-identity
4. `lookup(X, Y, t) ≈ lookup(Y, X, t).inverse()`
5. `lookup(X, X, t) == IDENTITY` exactly
6. `interp(a, b, 0) == a` and `interp(a, b, 1) == b` exactly (not approximately — endpoints must be exact)
7. ScLerp left-invariance (§3.4)
8. ScLerp right-invariance (§3.4)
9. LerpSlerp right-invariance **fails** (§3.4)
10. `lookup(A, C)` ≈ `lookup(A, B) * lookup(B, C)` for any B in the tree
11. Plan evaluation ≈ manual chain composition
12. Static folding does not change the result
13. `mul_inv(a, b)` ≈ `a * b.inverse()`
14. `sclerp` fast ≈ `reference::sclerp` within 1e-14
15. Round-tripping a wrapped ring buffer: after `3.5 * capacity` pushes, all retained samples read back exactly

### 10.2 Concurrency (`loom`)

Under `cargo xtask loom`, with a reduced buffer (capacity 4) so the state space is tractable:

- One writer pushing 3 samples, one reader sampling concurrently: reader observes either a fully-consistent sample or a documented error, never a torn one.
- Writer wrapping the ring while a reader is mid-`read_slot`: reader returns `SlotRecycled` or a valid sample.
- Two threads racing `intern` on the same name: both get the same `FrameId`.
- Two threads racing `claim` on the same edge: exactly one succeeds.
- Topology mutation concurrent with plan compilation: compilation either sees the old topology or the new one, never a mix, and `at()` reports `TopologyChanged`.
- A reclaimer sweeping the participant table concurrently with a joiner registering: the state word is observed **before** the lock byte is probed, and no record a joiner published is erased. Both are properties of the **caller** of `ParticipantTable::reclaim`; that function's own CAS guard is pinned by the unit test `reclaim_fails_when_the_observed_word_has_changed`, *not* by this model, which measurably never reaches the CAS on a contended slot. Its two failing controls — the reads reversed, and the observation weakened to `Relaxed` — are `#[should_panic]` tests in the same file rather than a paragraph describing them, because a model whose property cannot fail proves nothing (`docs/decisions/0028` open question 6). **`reclaim`'s own `AcqRel`/`Acquire` survives the mutation test below** — `Relaxed`/`Relaxed` passes the whole `tf_tree_core` suite and all of `cargo xtask loom` — so, per that paragraph's own instruction, which of the two it is has been investigated and the answer is written on the function: the strength is required by the byte-as-authority protocol, and nothing here can distinguish it because the byte probe is a syscall.

**Add a mutation test:** weaken each `Acquire`/`Release` in §6.2 and §6.3 to `Relaxed` one at a time and confirm the corresponding loom test fails. If weakening an ordering does not break any test, either the ordering is unnecessary or the test coverage is insufficient — investigate which.

### 10.3 Miri

`cargo +nightly miri test -p tf_tree_arena -p tf_tree_core` with strict provenance. Must be clean.

### 10.4 Allocation

A `CountingAllocator` wrapping the system allocator, with a test that asserts zero allocations across 10⁶ `push` and `at` calls after construction.

### 10.5 Differential against tf2

A harness that drives `tf2::BufferCore` and `tf_tree` with an identical tree and identical sample stream, then compares `lookupTransform` results across 10⁵ random queries with `Interp::LerpSlerp`. Agreement must be within 1e-12. This test is what makes migration credible to anyone currently shipping tf2; treat a failure as a release blocker.

---

## 11. Benchmarks and the go/no-go gate

### 11.1 Fixture

Do not benchmark a synthetic two-frame tree. Use:

- 24 frames, max depth 6, shaped like a real mobile robot: `map → odom → base_link → {imu_link, lidar_mount → lidar, camera_mount → camera_link → camera_optical, ...}`
- 4 dynamic edges: `map→odom` @ 50 Hz, `odom→base_link` @ 200 Hz, `base_link→imu_link` @ 1 kHz, `lidar_mount→lidar` @ 10 Hz
- 19 static edges
- 10 seconds of history pre-populated before measurement begins

### 11.2 Measurements

| Benchmark | Report |
|---|---|
| depth-1, depth-3, depth-6 lookup, hot cache | p50, p99, p99.9 |
| same, cold cache (large-stride flush between iterations) | p50, p99 |
| query mix: 70% `At(t)` uniform in [now−100 ms, now], 20% `Latest`, 10% `LatestCommon` | p50, p99.9 |
| `at_many` with 1024 monotone stamps | ns/sample |
| `push` throughput, single writer | ns/push |
| read scaling: 1/2/4/8/16 reader threads, 4 concurrent writers, cores pinned | aggregate throughput, per-thread p99.9 |
| identical everything against `tf2::BufferCore` | ratio per row |

**p99.9 is the number that matters**, not the mean. A control loop cares about the tail.

### 11.3 Gate

Proceed to Phase 2 if:

- Depth-3 hot lookup p50 under **300 ns with `ScLerp`**, under **220 ns with `LerpSlerp`**, *and* within **25 %** of the committed baseline per percentile.
- Zero allocations confirmed.
- **Read throughput scales at least 2.5× from 1 to 4 threads**, on ≥ 4 physical cores, *and* **tf_tree's 1→4 scaling factor is at least 5× tf2's** over the same sweep.

> **The first and third criteria were re-cut by [`0013`](../decisions/0013-the-benchmark-gate-never-interpolated.md), which is `ready`; its *Resolution* holds the arguments and this is the normative statement of the result.** Neither change is a concession to a regression, and both were previously ungateable rather than merely unmet.
>
> **The first** used to read *"under 150 ns with `ScLerp`, under 100 ns with `LerpSlerp`"*. Those figures were chosen before anything in this repository had measured interpolation: the fixture's query stamp `NOW_NS` was an exact multiple of all four dynamic periods, so every edge took `SampleRing::sample`'s exact-hit branch, `I::eval` never ran, and the row timed `bracket` plus a seqlock read. Off-grid the same rows measure **192.7 ns ScLerp** (band 190.4–268.9, n = 9) and **151.8 ns LerpSlerp** (band 146.2–190.4, n = 9). The new ceilings sit ~1.12× above each observed *maximum*, not above the median: a ceiling under 268.9 would fail about one run in nine on an unchanged engine, and a gate that flaps is one people learn to ignore. They are stated per interpolator because the two differ by 1.27× off-grid and by 1.00× on-grid — a single number written for the slower one would leave `LerpSlerp` effectively ungated. The 25 % regression clause is the half that actually bites, and is not new machinery: `bench_report`'s `lookup_latency` row has been gating exactly that (`LATENCY_SLACK`) all along.
>
> **The third** used to read *"scales at least 6× from 1 to 8 threads"*, and **no host this project has can evaluate it** — 8 threads on 4 physical cores can exceed 4× only through SMT, so the measured 5.35–5.62× (criterion benches) and 5.73× / 5.20× (`contended_scaling`, pinned, four writers) are neither a pass nor a fail. That figure is **retained as informational**, with its measurement, and is the number to re-take on ≥ 8 physical cores. What replaces it is decidable here: tf_tree measured **2.79×** (recorded stream) and **3.09×** (fixture) from 1 to 4 threads, against a 4× ceiling, so 2.5× passes with margin and a slide to 2× fails. The ratio clause is the one carrying the argument this criterion's own prose gives below — tf2 does not merely scale less, it *anti-scales* (0.36× at 4 threads, 0.31× at 8, reproduced by a pure C++ control with our binding deleted), so the measured separation is 2.79 / 0.36 = **7.75×**. An absolute cannot state that and a ratio can.

**Every latency row this gate bounds is measured with the fold *inlined* into its caller — NORMATIVE.** `benches/lookup.rs` measures it that way today. The same depth-3 `LerpSlerp` fold costs **147.6 ns** inlined and **200.3 ns** behind an `#[inline(never)]` call: the call alone is ~51.5 ns, which is larger than the headroom the ceilings above are set with, so a budget that does not pin the call shape has no fixed meaning. The non-inlined, out-of-crate cost is **not** ungated — it is `docs/PHASE5.md` §9.2's `embedding_cross_crate` row, gated at 5 % and currently *failing* at 1.250–1.254×. The split is deliberate: **§11.3 gates the engine, §9.2 gates the boundary.** One number for both could not say which of them had moved.

**"Depth-3" means three *dynamic* steps after constant folding — NORMATIVE.**
The phrase was ambiguous and the two readings differ by ~2.8×, so it is pinned
here. A static edge folds to a precomputed `Iso3` and costs one multiply; a
"depth-3" chain that folds to a single dynamic step measures almost nothing, and
a fixture chosen to be static-heavy would let the gate be passed without the
sampling path ever being exercised. The gate exists to bound the *sampling and
interpolation* path, so the number that matters is the count of `Step::Dyn`
entries in the compiled plan.

**Every reported latency row must state its dynamic-step count**, not just its
nominal depth. A row labelled only "depth 3" is not interpretable.

A note on the first criterion: `ScLerp` costs roughly 150–200 flops with two transcendental pairs via log/exp, or ~80 flops with one pair via the dual-quaternion route. Depth-3 means three of them. The ceiling already assumes the dual-quaternion implementation; if it comes in slower, that is information about the interpolation cost, not a reason to abandon the design — report it and consider making `LerpSlerp` the default for latency-critical plans. **That last clause has now been acted on in one place and not the other:** measured off-grid, ScLerp costs 1.27× LerpSlerp (192.7 against 151.8), and `tf_tree.build`'s Python default moved to `sclerp` to close a divergence from Rust that D5 forbids — so the two bindings agree, and the choice is the caller's per plan, not the binding's.

The third criterion is the one that actually decides the project. `tf2::BufferCore` serializes every lookup on one mutex, so it does not scale at all; if tf_tree scales cleanly, the value proposition is "your perception nodes stop contending," which is a much stronger and more durable claim than raw single-threaded speed. If single-threaded comes in at only 3–5× but scaling is clean, **the project is still justified and the internal pitch should change accordingly.**

**That conditional has resolved, and it resolved the way this paragraph hoped — via its weaker branch, and then some.** Single-threaded is **2.7×** against native C++ tf2, which is *below* the 3–5× this paragraph names as the disappointing case, not inside it. The scaling half is what redeems it, and it is not merely clean but one-sided: tf2 measures 0.50× at 2 threads, 0.36× at 4 and 0.31× at 8, so more threads make it slower than one thread. The tail is where it shows most: at 8 threads tf_tree's p99.9 is 331 ns against tf2's 83 µs, a factor of 252, and that ratio does not depend on core count the way the throughput one does. This is why the re-cut criterion above gates the *ratio* rather than only tf_tree's own factor — the absolute is the weaker statement of the two.

---

## 12. CLI (`tf_tree_cli`, build at the tail of Phase 1)

- `tf_tree tree` — live topology, per-edge rate, buffer occupancy, staleness, writer PID
- `tf_tree echo <target> <source> [--rate]` — continuous lookup
- `tf_tree doctor` — detects: cycles, unclaimed dynamic edges, multi-writer contention, buffers shorter than observed publish latency, frames published at inconsistent rates, unreachable frames, stamps arriving out of order
- `tf_tree bench --gate` — runs §11 and exits non-zero if the gate fails

`doctor` is not a nice-to-have. It is how you will debug Phase 2, and diagnostics are what actually drive tool adoption.

---

## 13. Definition of done

- [ ] All §10 tests pass, including loom and Miri, in CI on x86-64 **and aarch64**
- [ ] §11 benchmark suite runs via `cargo xtask bench-gate` and reports the full table
- [ ] The gate in §11.3 is met, or a written explanation of which criterion failed and by how much
- [ ] `#![forbid(unsafe_code)]` holds on `tf_tree_math`, `tf_tree_cli`; `tf_tree` is `#![deny(unsafe_code)]` with exactly one `#[allow]` (`OwnedWriter`, per [`0017`](./decisions/0017-owned-handles-and-the-lifetime-rule.md) — see §0's amended unsafe-budget note)
- [ ] Every `unsafe` block has a `// SAFETY:` comment naming a §2 invariant
- [ ] `tf_tree doctor` detects all seven listed conditions, each with a test
- [ ] Public API documented with `#![deny(missing_docs)]`
- [ ] Crate-level docs state the five conventions from §3.1 explicitly
- [ ] A `PHASE2.md` listing every place a `MappedArena` will need to differ — ideally the list has one entry

---

## Appendix: suggested implementation order

1. `tf_tree_math` types, `exp`/`log`, the numerical test sweep from §3.3. Get this exactly right before anything else — everything downstream inherits its accuracy.
2. `tf_tree_math` interpolation, both `ScLerp` implementations, the invariance tests.
3. `tf_tree_arena` layout computation and `HeapArena`, with layout assertion tests.
4. `tf_tree_core` frame interning and the topology block, with loom tests.
5. `tf_tree_core` edge records, claims, `PoseSlot`, publish/read protocols, loom tests, the wrapped-ring test.
6. Plan compilation, static folding, evaluation.
7. Public API and the convenience path.
8. `at_many` and `at_adaptive`.
9. Benchmarks and the tf2 differential harness.
10. CLI.

Do not proceed past step 5 until its loom tests pass. Everything after it assumes the buffer protocol is sound, and a bug there will present as a mysterious numerical error somewhere in step 6.
