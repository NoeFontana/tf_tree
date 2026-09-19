# tf_tree — Phase 1 Implementation Specification

> **Companion:** `docs/PROJECT.md` holds the overview, roadmap and decision log. Where this spec is silent, check the log first — several obvious simplifications are deliberately excluded.

**Critical framing:** Phase 2 maps sample storage into shared memory so a second process runs the *identical, unmodified* reader code against an `mmap`'d arena. Phase 1 is the shared-memory version backed by a heap allocation; every layout decision exists so that swap is a one-line change in `tf_tree_arena`. Do not simplify them away. Sections marked **NORMATIVE** are requirements; code blocks are illustrative, but signatures and layouts are normative.

## 0. Non-goals and guardrails — read first

**NORMATIVE.** Do not implement in Phase 1: `async`/`tokio`/any runtime (a lookup is a pure function); generic scalar `T: RealField` (f64 only); `serde` in `tf_tree_core` (Phase 6; breaks `no_std`); dynamic capacity growth (remapping invalidates reader mappings); `String` in any error type or hot path (errors carry IDs, `Display` resolves names via the arena); covariance and copy-on-write branches (cut, `0009`; the slot layout is exactly one cacheline); GPU code, point-cloud apply and deskew helpers (permanently out of core; `sample_many` is the whole surface); network, discovery, multicast (Phase 6).

**Dependency budget for `tf_tree_core`:** `libm`, `bytemuck`, and `blake3`. Nothing else. `tf_tree_arena` adds `rustix` in Phase 2 only. Test/bench-only dependencies are unrestricted.

`blake3` is a deliberate third entry: §5.1 mandates `BLAKE3-256 truncated to 64 bits`; two processes intern into one arena, so the hash must be deterministic across processes, builds and toolchains, and truncation is safe only with cryptographic-quality avalanche. Replacing it changes the arena format.

**Unsafe budget:** the rule is [`0007`](./decisions/0007-the-unsafe-budget-and-the-c-abi.md)'s criterion as amended by [`0048`](./decisions/0048-a-kind-is-not-a-crate-name.md): `unsafe` only at a boundary the compiler cannot see across, bound to a crate **root**, indexed in `scripts/unsafe-budget.txt`. In Phase 1: `#![forbid(unsafe_code)]` on `tf_tree_math`, `tf_tree_cli`; `tf_tree` is `#![deny(unsafe_code)]` with exactly one `#[allow]` (`OwnedWriter`, [`0017`](./decisions/0017-owned-handles-and-the-lifetime-rule.md)); `unsafe` in `tf_tree_arena` and exactly two `tf_tree_core` modules (`buffer.rs`, `arena_view.rs`), each with a module-level `// SAFETY:` block stating its invariants. Every `unsafe` block gets its own `// SAFETY:` comment naming the invariant it relies on.

**If a design question is not answered by this document, stop and ask.**

## 1. Workspace layout

```
crates/tf_tree_math/   no_std; SE(3)/SO(3), dual quats; zero unsafe
crates/tf_tree_arena/  no_std+alloc; arena abstraction + layout math
crates/tf_tree_core/   no_std+alloc; the engine (frame, topology, edge, buffer, arena_view, plan, sample, error)
crates/tf_tree/        std facade
crates/tf_tree_bench/  criterion + tf2 comparison harness
crates/tf_tree_cli/    binary `tf_tree` (alias `tft`)
xtask/                 loom, bench-gate, headers runners
```

`tf_tree_math` and `tf_tree_arena` are separately publishable and testable. Keeping the math crate free of `unsafe` and the arena makes it cheap for Miri as a *callee*; its own suite is not run under Miri (`just miri` runs `-p tf_tree_arena -p tf_tree_core` and `-p tf_tree`).

## 2. Load-bearing invariants

**NORMATIVE.** Encode each as a debug assertion where cheap and a `// INVARIANT:` comment at the definition site.

1. **Append-only identity.** `FrameId` and `EdgeId` are never reused. Removal is tombstoning (`kind = Tombstone`). A stale `Plan` may therefore index a valid, in-bounds record; it will fail the generation check, but it can never cause out-of-bounds access.
2. **No pointers in the arena.** Every intra-arena reference is a `u32` element index or byte offset relative to the arena base. The arena must be relocatable by `memcpy`.
3. **Fixed capacity.** `max_frames`, `max_edges`, and every per-edge ring capacity are set at construction. Ring capacities are powers of two.
4. **Single writer per edge.** Enforced by the claim table, not by convention. A second claim on a live edge is an error, never a silent success.
5. **Monotone head.** `EdgeRecord::head` is a monotonically increasing count of samples ever published. It is never masked in storage, only at access time. `u64` never wraps in practice (1 MHz for 500 000 years).
6. **Stamps are non-decreasing per edge.** `push` with a stamp `<` the current head stamp is rejected with `NonMonotonicStamp`. Equal stamps are accepted and the newer value wins (this is required for idempotent replay).
7. **All multi-byte arena fields are little-endian.** Phase 6 wire encoding is separate; the arena is host-native but must assert LE at construction.
8. **Every heap allocation happens at construction.** `push` and `at` allocate nothing. Enforced by a counting allocator in tests, not by inspection.

## 3. `tf_tree_math`

### 3.1 Types

```rust
#[repr(C)] #[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vec3 { pub x: f64, pub y: f64, pub z: f64 }              // 24 B

/// Hamilton convention, w first, right-handed, active rotation.
/// INVARIANT: callers of unchecked constructors guarantee |q| == 1 within 1e-12.
#[repr(C)] #[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quat { pub w: f64, pub x: f64, pub y: f64, pub z: f64 }  // 32 B

/// T_parent_child. Applying to a point in `child` yields the point in `parent`.
#[repr(C)] #[derive(Clone, Copy, Debug, PartialEq)]
pub struct Iso3 { pub q: Quat, pub t: Vec3 }                        // 56 B
```

**Convention lock-in.** Hamilton (not JPL), `w` first (unlike Eigen's storage order; the C++ wrapper transposes), active rotations, `Iso3` composition `a * b` means `T_a_x * T_x_b`, adjoint by right-perturbation `T = T̂ · exp(ξ^)`. Stated in the crate-level doc comment.

Layout tests: `crates/tf_tree_math/src/iso3.rs` (`size_of::<Iso3>() == 56`, `align_of == 8`) and `tf_tree_core`'s `the_sizes_0042_halved_stay_halved`.

### 3.2 SE(3) exponential and logarithm

`exp_se3(xi: [f64; 6]) -> Iso3`, `log_se3(t: Iso3) -> [f64; 6]`, with `xi = [ω(3), v(3)]`.

```
exp:  R = exp_so3(ω)              t = V(ω) · v
log:  ω = log_so3(R)              v = V⁻¹(ω) · ω_t   where ω_t is the translation
```


for `θ = |ω|` and `W = [ω]×`:

```
V(ω)   = I + c1·W + c2·W²        c1 = (1 − cos θ)/θ²      c2 = (θ − sin θ)/θ³
V⁻¹(ω) = I − ½·W + c3·W²         c3 = 1/θ² − (1 + cos θ)/(2θ·sin θ)
```

### 3.3 Numerical requirements

**NORMATIVE.** Both findings were verified against a 50-digit reference.

**(a) `log_so3` goes through the quaternion, never `acos(trace)`.** At ‖ω‖ ≈ π the trace form has relative error **1.3e-7** against 3.0e-16 for `2·atan2(‖q_v‖, q_w)`; elsewhere both are ~2e-16.

Use `theta = 2.0 * libm::atan2(sqrt(q.x² + q.y² + q.z²), q.w)`, then wrap to (−π, π].

**(b) The small-angle series threshold is θ < 0.1, not 1e-8, and needs four terms.** The closed forms of `c1`, `c2`, `c3` lose 4 to 11 digits well above 1e-8 (`c3`: 1.2e-7 at θ = 1e-4, against 4.2e-17 for the series); the series degrades above ~0.3 (1.6e-11 at 0.3). The crossover is θ ≈ 0.1.

**Required constants** (coefficients of θ^0, θ², θ⁴, θ⁶), evaluated by Horner in `θ²`:

```rust
const THETA_SMALL: f64 = 0.1;   // NORMATIVE
// c1 = (1 − cos θ)/θ²
const C1: [f64; 4] = [1.0/2.0,  -1.0/24.0,  1.0/720.0,   -1.0/40320.0];
// c2 = (θ − sin θ)/θ³
const C2: [f64; 4] = [1.0/6.0,  -1.0/120.0, 1.0/5040.0,  -1.0/362880.0];
// c3 = 1/θ² − (1 + cos θ)/(2θ sin θ)
const C3: [f64; 4] = [1.0/12.0,  1.0/720.0, 1.0/30240.0,  1.0/1209600.0];
```

Three terms would need `THETA_SMALL = 0.01`. A test sweeps θ over `[1e-12, π]` on a log grid against a hardcoded high-precision table, error below 1e-14, no discontinuity above 1e-15 across the branch boundary.

### 3.4 Interpolation

`Interp::eval(a: &Iso3, b: &Iso3, s: f64) -> Iso3` has two implementors: `LerpSlerp` (tf2-compatible: LERP translation, shortest-arc slerp with a LERP fallback below 1e-6) and `ScLerp` (SE(3) geodesic, the default).

`ScLerp` has two implementations: `reference::sclerp` — `a * exp_se3(s * log_se3(a.inverse() * b))`, the definition of correct — and `sclerp`, unit dual quaternion power (one `atan2`, one `sin_cos`). **NORMATIVE:** a differential proptest asserts agreement to 1e-14 over 10⁵ random pairs including near-identity and near-π.

**Invariance properties**, asserted as tests:

| Property | ScLerp | LerpSlerp |
|---|---|---|
| `interp(G·a, G·b, s) == G·interp(a, b, s)` | must hold to 1e-13 | must hold to 1e-13 |
| `interp(a·H, b·H, s) == interp(a, b, s)·H` | must hold to 1e-13 | **must be asserted to FAIL** |

The second row is a positive test that `LerpSlerp` is not right-invariant: `assert!(max_err > 1e-6)` over a fixed seeded set. It is the reason `ScLerp` is the default; do not "fix" it.

## 4. `tf_tree_arena`

### 4.1 Header

The normative header is `crates/tf_tree_arena/src/header.rs`: `FORMAT_VERSION` is **3**, the struct is **320 bytes**, and its `key_field_offsets_are_stable` test pins every offset a reader depends on. Phase 1's original 256-byte header lacked the participant registry (`participant_table_off`, `max_participants`, `participant_count`), identity fields (`owner_start_time`, `boot_id[16]`, `instance_uuid[16]`), the counter regions (`edge_counters_off`, `participant_counters_off`), `topo_lock`, and the reserved Phase 6 `spline_region_off`/`spline_degree` (D22); its `topo_generation`/`topo_active` became one packed `topo: AtomicU64`. Fields Phase 1 introduced and still holds: `magic` (`b"TF_TREE\0"`, a byte array to avoid endianness ambiguity), `format_version`, `layout_hash`, `arena_size`, `max_frames`, `max_edges`, region offsets, `frame_count`, `edge_count`, `creator_pid`, `creator_boot_id`. `layout_hash` is a `const fn` over `size_of` and `offset_of` for every arena struct; attach checks it and a mismatch is a hard error. `creator_boot_id` comes from `/proc/sys/kernel/random/boot_id`.

### 4.2 Arena trait

`pub unsafe trait Arena: Send + Sync { fn base(&self) -> *mut u8; fn len(&self) -> usize; }` is implemented by `HeapArena` (64-byte-aligned `Vec<u8>`); Phase 2 adds `MappedArena` (memfd + mmap).

`HeapArena::new(layout: &ArenaLayout)` allocates `layout.total_size()` bytes 64-byte aligned, zeroes them and writes the header. **The only Phase 2 change should be adding `MappedArena` and a constructor selecting it**; if `tf_tree_core` must change, the Phase 1 design was wrong — stop and report it.

### 4.3 Layout computation

`ArenaLayout::new` (`crates/tf_tree_arena/src/layout.rs`) validates that each per-edge capacity is `0` (static) or a power of two and that exactly `max_edges` were supplied. `max_participants` is `DEFAULT_MAX_PARTICIPANTS = 64`, chosen inside `new`. Region sizes, each 64-byte aligned and laid out in header order:

| Region | Size |
|---|---|
| header | **320** |
| frame table | `align64(max_frames * 64)` |
| frame hash | `align64(next_pow2(2*max_frames) * 16)` |
| topology blocks | `TOPO_BLOCKS * align64(max_frames * 12)` |
| claim table | `align64(max_edges * 64)` |
| **participant table** | `align64(max_participants * 128)` |
| edge table | `align64(max_edges * 128)` |
| stamp arena | `align64(sum(capacities) * 8)` |
| pose arena | `align64(sum(capacities) * 64)` |
| **edge counters** | `align64(max_edges * 128)` |
| **participant counters** | `align64(max_participants * 128)` |

> The authority is the `sizes` array in `compute` in `layout.rs`; `layout_hash` folds these strides. `sizes` is not compile-checked (dropping a row is a runtime index error); the `strides: [u32; N_REGIONS + 1]` array in `layout_hash` is a cardinality check only. The counter regions are counted whether or not the `counters` feature is on (D22).

**Topology block stride is 12 bytes per frame.** §5.3 puts `edge_of_child[c]` in the block, so it is `parent: u32` + `edge_of_child: u32` + `depth: u16` = 10 B, rounded to **12** so each triple stays 4-byte aligned. 12 is what `layout_hash` folds; a reader built to another stride is refused at attach. Keeping `edge_of_child` in the block puts it under the same publish as `parent` and `depth`, so a reader always sees a consistent `(parent, depth, edge)` triple. `TOPO_BLOCKS` is **4** (`crates/tf_tree_arena/src/header.rs`): blocks rotate over one packed word (§5.2).

## 5. `tf_tree_core` records

### 5.1 Frames

```rust
#[repr(C, align(64))]
pub struct FrameRecord {
    pub name_hash: u64,       // BLAKE3-256 truncated to 64 bits, of the full name
    pub name: [u8; 48],       // UTF-8, NUL-padded, truncated for display only
    pub name_len: u8, pub flags: u8, _pad: [u8; 6],
}
```

48 bytes covers every real frame name; longer names hash in full but display truncated. `FrameId` is a `NonZeroU32` so `Option<FrameId>` is 4 bytes; index 0 is the "no parent" root sentinel.

**Interning table:** open addressing, linear probing, `next_pow2(2 * max_frames)` slots, **three** parallel arrays: `hashes: [AtomicU64]` (0 = empty), `ids: [AtomicU32]`, `claiming: [AtomicU32]` (`PHASE2.md` §1 A8), stride `FRAME_HASH_STRIDE = 8 + 4 + 4 = 16` bytes — the value §4.3 folds into `layout_hash`. The arrays are zero-initialised as in the production arena.

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

Publish-then-spin exists because two processes intern concurrently. A hash match with a different stored name is `FrameHashCollision`.

### 5.2 Topology

Per-frame `parent: [u32]`, `depth: [u16]` and `edge_of_child: [u32]` arrays, each `max_frames` long and indexed by `FrameId` (`parent == 0` means root or unattached). **Four** blocks, rotated (`TOPO_BLOCKS = 4`, A1). **`ArcSwap` is forbidden here** — `Arc` refcounts do not cross a process boundary.

**Writer protocol (topology mutation):**

```
// A1: generation and active block are ONE packed word, `topo: AtomicU64`
// (`pack_topo(generation, active) = (generation << 8) | active`), so a reader
// cannot observe a generation from one publish beside a block index from
// another. That is what a two-word version could not guarantee.
(g, active) = unpack_topo(topo.load(Relaxed));  debug_assert!(g % 2 == 0)
topo.store(pack_topo(g + 1, active), Release);  // mark unstable
next = (active + 1) % TOPO_BLOCKS               // rotation, not 1 - active
copy block[active] -> block[next], apply mutation, recompute depths
topo.store(pack_topo(g + 2, next), Release)     // publish + mark stable, one store
```

**Reader protocol (plan compilation only — `at()` never reads topology):**

```
loop {
    (g1, blk) = unpack_topo(topo.load(Acquire))   // one load: generation AND block
    if g1 & 1 != 0 { spin_loop(); continue }
    ... read parent/depth, build the step list ...
    fence(Acquire)
    if unpack_topo(topo.load(Relaxed)).0 == g1 { return plan_with_generation(g1) }
}
```

`Plan::at()` does one `Relaxed` load of the generation and compares it to the plan's. Mismatch is `TopologyChanged` — "re-plan", not a failure to hide with a retry loop.

**Cycle detection:** each mutation walks from the new child to root with a step budget of `max_frames`; exceeding it is `WouldCreateCycle`. Depths are recomputed by BFS over the block.

### 5.3 Edges

```rust
#[repr(C, align(64))]
pub struct EdgeRecord {
    pub parent: u32, pub child: u32,
    pub kind: u8,          // 0 Dynamic, 1 Static, 2 Tombstone
    pub interp: u8,        // InterpPolicy discriminant
    pub domain: u8,        // time domain id
    _pad0: u8,
    pub capacity: u32,     // power of two; 0 for Static
    pub stamp_off: u32, pub pose_off: u32,   // element indices into the stamp / pose arenas
    _pad1: u32,
    pub head: AtomicU64,   // monotone total samples published
    pub static_pose: [u64; 7],  // f64 bit patterns; Static only
    _pad2: [u8; 40],
}
```

The edge for child `c` is found via `edge_of_child[c]`, making plan compilation a pure array walk.

### 5.4 Claims

```rust
#[repr(C, align(64))]
pub struct ClaimRecord {
    pub state: AtomicU32,        // 0 free, 1 held
    pub owner_pid: u32, pub owner_boot_id: u64,
    pub heartbeat: AtomicU64,    // bumped by the writer on every push
    pub claim_epoch: AtomicU64,  // incremented on every successful claim
    _pad: [u8; 32],
}
```

Claim is `compare_exchange(0, 1, AcqRel, Acquire)`; failure is `EdgeAlreadyClaimed { owner_pid }`; release stores 0 with `Release`. A `Publisher` holds the `EdgeId` and observed `claim_epoch`; `Drop` releases. The liveness reaper is Phase 2; the fields exist now so the layout never changes.

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

One cacheline per slot. **Why `[AtomicU64; 7]` and not `[f64; 7]` behind `UnsafeCell`:** a classic seqlock reads data non-atomically and discards it on version mismatch — a data race, UB in the Rust and C++ models. `Relaxed` atomic loads make it sound at zero cost. Do not "optimize" this into a `memcpy`.

Stamps live in a **separate** `AtomicI64` array so binary search touches 8 stamps per cacheline and never pulls in pose data. The slot's seqlock protects the logical sample across both arrays.

### 6.2 Publish protocol (single writer)

The listing matches `SampleRing::push`. The odd flip is `slot.seq.load(Relaxed) | 1` (A5, `PHASE2.md` §1): a writer killed mid-write leaves a stale odd value the next writer heals idempotently. The heartbeat is a plain `store` because the ring is single-writer (D7).

```rust
// NORMATIVE ordering annotations.
fn push(&mut self, stamp: i64, iso: &Iso3) -> Result<(), PushError> {
    let h = self.rec.head.load(Ordering::Relaxed);   // single writer: Relaxed is correct
    if h > 0 {
        let last = self.stamps[((h - 1) & self.mask()) as usize].load(Ordering::Relaxed);
        if stamp < last { return Err(PushError::NonMonotonicStamp { edge: self.edge, last, got: stamp }); }
    }
    let idx = (h & self.mask()) as usize;
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

**NORMATIVE:** every ordering annotation above is deliberate. The `fence(Acquire)` before the second `seq` load keeps the data loads from reordering after it on weakly-ordered targets (no-op on x86-64, real barrier on aarch64). **Do not weaken any to `Relaxed` because a test passes on x86**; the loom tests in §10.2 catch exactly that.

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

The trailing revalidation makes the read wait-free in practice (4096 samples at 1 kHz is 4 s of slack). Return the error rather than loop; the caller knows whether a retry makes sense. Binary search runs over logical indices, masking on probe — searching the physical array is wrong once the ring has wrapped; a test exercises a buffer wrapped 3.5 times.

## 7. Plan compilation and evaluation

### 7.1 Step representation

```rust
pub const MAX_DEPTH: usize = 32;
pub const MAX_PATH_EDGES: usize = 64;

#[derive(Clone, Copy)]
pub enum Step { Static(Iso3), Dyn { edge: EdgeId, inverted: bool } }

pub struct Plan { generation: u64, steps: [Step; MAX_DEPTH], len: u8, domain: u8 }
```

**Two bounds price different slots** ([`0034`](./decisions/0034-the-depth-bound-priced-two-slots-the-same.md)). `MAX_DEPTH` bounds the *compiled* plan, counted **after** §7.2's folding: a slot is a `Step`, **64 bytes** ([`0042`](./decisions/0042-the-cacheline-the-arena-never-asked-for.md)), carried by value in every `Plan` and in a 16-slot thread-local cache. `MAX_PATH_EDGES` bounds the *raw walk*: a slot is a `u32` on `compile`'s stack.

Either overrun is `TreeTooDeep` (one variant; the C ABI status table is frozen). Its `depth` says which: `MAX_PATH_EDGES + 1` is the walk refusing; at or below `MAX_PATH_EDGES` it is the exact folded step count.

Basis: 91 real robot descriptions have a worst *graph diameter* (up to the LCA and back down) of **30 joints**; 32 is the next power of two, 64 is ~1.9× that plus a deployed prefix.

### 7.2 Compilation

Edge for child `c` stores `T_parent(c)_c`, and `T_target_source = (T_lca_target)⁻¹ · T_lca_source`.

`compile(target, source)` returns the empty plan when they are equal. Otherwise it lifts the deeper frame to equal `depth`, then steps both up until they meet, returning `Disconnected { cut_at }` if either reaches a `0` parent first. Target-side frames (`[target, .., child_of_lca]`) emit `Dyn { edge_of_child[f], inverted: true }` in that order; source-side frames emit `inverted: false` in **reverse** (`child_of_lca` first).

Verify the direction by hand on a three-frame example first; backwards gives a plausible transform that is wrong everywhere.

**Constant folding, after compilation:**

1. Replace any `Dyn` whose edge `kind == Static` with `Static(pose)` (pre-inverting if `inverted`).
2. Collapse every run of adjacent `Static` into one by composing them.

A test asserts the canonical URDF fixture folds from 6 steps to 3.

**Folding reads the walk's two `u32` buffers** (edge id plus an `inverted` flag, true iff the edge came from the target side), not an intermediate `[Step; MAX_DEPTH]` ([`0034`](./decisions/0034-the-depth-bound-priced-two-slots-the-same.md)), **and writes into the `Plan` being returned**: `Plan::identity` makes the buffer and `fold_into` fills it through `&mut Plan`, including `len`, `domain`, `dyn_count`, `first_dyn`. `Plan::identity` is the only constructor — a plan is complete the moment it exists (zero steps is the answer for `target == source`), so no arm can forget to finish one and answer `Iso3::IDENTITY` where a refusal belongs.

Load-bearing properties of the fold:

* **The source half is emitted in reverse of walk order**, so composition associates `((s[n-1] · s[n-2]) · …)`. `Iso3` composition is not associative under rounding and the tests are tolerance-based (`TOL = 1e-12`), so folding during the walk would change bits and pass every test. Verify a change here against **bits**.
* **The loop does not stop when the output array fills.** It skips the write, keeps counting and resolves every remaining edge, so `TreeTooDeep` reports the true folded length and `UnknownEdge` and `MixedTimeDomains` still win over it. `error_precedence_over_defect_kind_position_and_foldability` in `crates/tf_tree_core/src/tests.rs` pins it.

### 7.3 Evaluation

`Plan::at(&self, g: &Guard, t: Stamp)` compares the header's generation (one `Relaxed` load) to the plan's — mismatch is `TopologyChanged { plan, current }` — then folds `acc = Iso3::IDENTITY` over `steps[..len]`: `Static(m)` is `acc * m`; `Dyn { edge, inverted }` samples the edge (`g.sample(edge, t)?`) and applies `acc.mul_inv(&p)` if inverted, else `acc * p`.

`mul_inv(a, b) = a * b⁻¹` is computed directly and differential-tested against the naive form to 1e-14.

### 7.4 Batch sampling

```rust
pub fn at_many(&self, g: &Guard, stamps: &[Stamp], out: &mut [Iso3]) -> Result<(), LookupError>;
pub fn at_adaptive(&self, g: &Guard, span: (Stamp, Stamp), tol: ErrBound)
    -> Result<(&[Stamp], &[Iso3]), LookupError>;
```

`at_many` detects monotone input and switches to a galloping search resuming from the previous index. `at_adaptive` emits the minimum knot set such that linear interpolation stays within `tol`: evaluate the midpoint exactly, compare against the LERP of the endpoints, subdivide on excess. Recursion depth ≤ 16, knots ≤ 4096. It replaces the abandoned deskew helper — the consumer LERPs between knots wherever the points live. It may allocate only from a caller-provided scratch buffer.

## 8. Public API surface

**Edges are declared on the builder, before `build()`**: the arena is one fixed allocation whose pose region is `sum(per-edge capacity) × 64 B`, so a post-`build` declaration would have nowhere to put its ring. The only runtime topology change is [`Tree::reparent`], which reuses a declared edge. See [`docs/decisions/0004`](./decisions/0004-builder-time-edge-declaration.md).

```rust
let tree = TreeBuilder::new()
    .default_interp(InterpPolicy::ScLerp)
    .dynamic_edge("map", "odom", EdgeCfg::new(Capacity::history(50.0, 10.0)))
    .static_edge("base_link", "camera_mount", &iso)
    .frame_headroom(8)                        // only if names are interned later
    .build()?;                                // -> Tree (owns a HeapArena)

let mut w: EdgeWriter = tree.claim(base, odom)?;   // (child, parent); Drop releases
w.push(stamp, &iso)?;                              // wait-free, no alloc

let plan: Plan = tree.plan(cam, map)?;             // (target, source); compile once
let g: Guard = tree.guard();                       // pins generation + arena
let t = plan.at(&g, stamp)?;
let t = tree.lookup("camera_optical", "map", stamp)?;   // convenience: interned + plan-cached
```

`Tree: Send + Sync`. `Plan: Send + Sync + Copy`. `Publisher: Send + !Sync` (single writer is a type-level property). `Guard<'a>` borrows the tree.

`lookup` keeps a per-thread plan cache keyed by `(arena, FrameId, FrameId, generation)`, 16 entries, direct-mapped. The `arena` component is the arena's identity, not the handle's: the cache is shared by every `Tree` the thread touches, and two `Tree`s mapping one segment share an entry deliberately.

### Time

`Stamp<D: Domain = SystemDomain>(i64, PhantomData<D>)` holds nanoseconds; `Query` is `At(Stamp) | Latest | LatestCommon | Bracket(Stamp, i64)`.

Phase 1 implements `At`, `Latest`, `LatestCommon`. `LatestCommon` is the `min` over the plan's dynamic edges of their newest stamp — what tf2's `Time(0)` means; document it explicitly.

Domains are phantom types with a runtime `u8` tag on the edge; cross-domain lookup is `TimeDomainMismatch`. Alignment machinery is Phase 6.

## 9. Errors

```rust
#[derive(Clone, Copy, Debug, PartialEq)] #[non_exhaustive]
pub enum LookupError {
    UnknownFrame { hash: u64 }, Disconnected { target: FrameId, source: FrameId, cut_at: FrameId },
    TreeTooDeep { depth: u16 }, NoData { edge: EdgeId },
    Extrapolation { edge: EdgeId, requested: i64, oldest: i64, newest: i64 },
    SlotRecycled { edge: EdgeId }, SlotContended { edge: EdgeId },
    TopologyChanged { plan: u64, current: u64 }, TimeDomainMismatch { expected: u8, got: u8 },
}
```

`Copy`, no allocation, `no_std`. **Every variant that can name an edge does name one.** `Display` is implemented on `Described<'a>(LookupError, &'a Tree)`, which resolves IDs to names via the arena.

## 10. Test plan

### 10.1 Property tests (`proptest`)

Minimum set, each over ≥10⁴ cases with a fixed seed in CI:

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

`cargo xtask loom`, reduced buffer (capacity 4):

- One writer pushing 3 samples, one concurrent reader: a consistent sample or a documented error, never a torn one.
- Writer wrapping the ring mid-`read_slot`: `SlotRecycled` or a valid sample.
- Two threads racing `intern` on one name: same `FrameId`.
- Two threads racing `claim` on one edge: exactly one succeeds.
- Topology mutation concurrent with plan compilation: old or new topology, never a mix, and `at()` reports `TopologyChanged`.
- A reclaimer sweeping the participant table while a joiner registers: the state word is observed **before** the lock byte is probed, and no record a joiner published is erased. These are properties of the **caller** of `ParticipantTable::reclaim`; its CAS guard is pinned by `reclaim_fails_when_the_observed_word_has_changed`, and the two failing controls (reads reversed; observation weakened to `Relaxed`) are `#[should_panic]` tests in the same file (`0028` open question 6). `reclaim`'s own `AcqRel`/`Acquire` is required by the byte-as-authority protocol; nothing here can distinguish it because the byte probe is a syscall.

**Mutation test:** weaken each `Acquire`/`Release` in §6.2 and §6.3 to `Relaxed` one at a time and confirm the corresponding loom test fails. If none does, the ordering is unnecessary or coverage is insufficient — investigate which. Run as `RUSTFLAGS='--cfg loom' LOOM_MAX_PREEMPTIONS=3 cargo test -p tf_tree_core --tests --release` (`xtask/src/main.rs`'s settings). Result in `crates/tf_tree_core/src/buffer.rs`: deleting either `fence` or weakening `slot.seq`'s `Release` store or `Acquire` load fails `writer_three_pushes_reader_never_torn` (fences: both writer models); weakening `head.store(h + 1, Release)` passes every `sample`-shaped model, because `push`'s `fence(Release)` precedes that push's stamp store and observing `head == h` leaves `stamps[h-1]` (the `t_new` `sample` reads) unprotected, while a stale one leaves through the tolerated `Extrapolation` arm. `head_publishes_every_stamp_below_it` pins that ordering (fails under the `Relaxed` mutant); §7's `stamp_at` relies on it.

### 10.3 Miri

`cargo +nightly miri test -p tf_tree_arena -p tf_tree_core` with strict provenance. Must be clean.

### 10.4 Allocation

A `CountingAllocator` test asserts zero allocations across 10⁶ `push` and `at` calls after construction.

### 10.5 Differential against tf2

A harness drives `tf2::BufferCore` and `tf_tree` with an identical tree and sample stream and compares `lookupTransform` across 10⁵ random queries with `Interp::LerpSlerp`, within 1e-12. A failure is a release blocker.

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

**p99.9 is the number that matters**; a control loop cares about the tail.

**The `push` row's tail ships unmeasured.** [`0036`](./decisions/0036-the-receipt-time-the-format-already-reserved.md) step 1 put a clock-offset sampler on `EdgeWriter::push`: one push in `sample_every` reads a wall clock (38.4 ns on the development host). The *mean* is measured — `push` 4.85–5.0 ns → 5.87–6.1 ns, **+1.0–1.1 ns**, paired by `just push-sampler-cost` — on an edge declaring no rate (`sample_every` 1024). The rest is arithmetic from a ~1.06 ns rate-independent counter plus `38.4 / sample_every` ns of amortised clock:

| declared rate | `sample_every` | per push | clock's share | per second of publishing |
|---|---|---|---|---|
| none / 1 kHz | 1024 / 1000 | 1.10 ns | 3% | ~1.1 µs |
| 200 Hz | 200 | 1.25 ns | 15% | ~0.25 µs |
| 50 Hz | 50 | 1.83 ns | 42% | ~91 ns |
| 10 Hz | 10 | 4.90 ns | 78% | ~49 ns |
| ≤ 1 Hz | 1 | 39.5 ns | 97% | ~39 ns |

The last column is what a robot pays. The **p99.9** is arithmetic, not measured: for a 1 kHz publisher with `sample_every = 1000`, the 99.9th percentile *is* the sampled push, ~38 ns above its neighbours. Whether that reaches a consumer is `publish_to_visible`'s question, `unavailable` on this host (4 physical cores against the 17 it needs, no ROS 2), so it ships without one. This is the disclosure `0036` step 5 owes.

### 11.3 Gate

Proceed to Phase 2 if:

- Depth-3 hot lookup p50 under **300 ns with `ScLerp`**, under **220 ns with `LerpSlerp`**, *and* within **25 %** of the committed baseline per percentile.
- Zero allocations confirmed.
- **Read throughput scales at least 2.5× from 1 to 4 threads**, on ≥ 4 physical cores, *and* **tf_tree's 1→4 scaling factor is at least 5× tf2's** over the same sweep.

> The first and third criteria were re-cut by [`0013`](./decisions/0013-the-benchmark-gate-never-interpolated.md) (`ready`); its *Resolution* holds the arguments and this is the normative result. The 25 % clause is enforced by `bench_report`'s `lookup_latency` row (`LATENCY_SLACK`). Ceilings are per interpolator (off-grid ScLerp 192.7 ns, LerpSlerp 151.8 ns, ~1.12× above each observed maximum). The former 1→8 ≥ 6× criterion is undecidable on 4 physical cores and is retained as informational; 1→4 ≥ 2.5× is decidable (tf_tree 2.79× recorded stream, 3.09× fixture) and the ratio clause is 7.75× measured (tf2 anti-scales: 0.36× at 4 threads).

**Every latency row this gate bounds is measured with the fold *inlined* into its caller — NORMATIVE.** `benches/lookup.rs` does so. The depth-3 `LerpSlerp` fold costs 147.6 ns inlined and 200.3 ns behind `#[inline(never)]`; the call alone exceeds the ceilings' headroom. The non-inlined, out-of-crate cost is gated by `docs/PHASE5.md` §9.2's `embedding_cross_crate` row: **§11.3 gates the engine, §9.2 gates the boundary.**

**"Depth-3" means three *dynamic* steps after constant folding — NORMATIVE.** A static edge folds to a precomputed `Iso3` and costs one multiply; a static-heavy fixture would pass the gate without exercising the sampling path. **Every reported latency row must state its dynamic-step count**, not just its nominal depth.

The third criterion decides the project: `tf2::BufferCore` serializes every lookup on one mutex and anti-scales (0.36× at 4 threads, 0.31× at 8), so the gate is a *ratio* against tf2, not only tf_tree's own factor.

## 12. CLI (`tf_tree_cli`, built at the tail of Phase 1)

- `tf_tree tree` — live topology, per-edge rate, buffer occupancy, staleness, writer PID
- `tf_tree echo <target> <source> [--rate]` — continuous lookup
- `tf_tree doctor` — detects: cycles, unclaimed dynamic edges, multi-writer contention, buffers shorter than observed publish latency, frames published at inconsistent rates, unreachable frames, stamps arriving out of order
- `tf_tree bench --gate` — runs §11 and exits non-zero if the gate fails

## 13. Definition of done

- [x] All §10 tests pass, including loom and Miri, in CI on x86-64 **and aarch64** (landed 2026-09-09; first runs green, PR #307). §10.5's `tf2` arm stays x86-64 (container job). Loom remains the argument; aarch64 is corroboration, not a weak-memory proof.
- [ ] §11 benchmark suite runs via `cargo xtask bench-gate` and reports the full table — the runner reports §11.3's criteria, not §11.2's seven rows. §11.2's **cold-cache row is measured by nothing in this repository**, so the box cannot close until that row has an artifact or is withdrawn. `benches/lookup.rs`, `read_scaling.rs` and `push.rs` are executed by no recipe.
- [x] The gate in §11.3 is met, or a written explanation exists — [`0013`](./decisions/0013-the-benchmark-gate-never-interpolated.md)'s *Resolution* is the second arm. Its plan step 6 (re-baseline on a `Fitness`-passing host) is outstanding and is not this box.
- [x] `#![forbid(unsafe_code)]` holds on `tf_tree_math`, `tf_tree_cli`; `tf_tree` is `#![deny(unsafe_code)]` with exactly one `#[allow]` (`OwnedWriter`): `rg -c 'allow\(unsafe_code\)' crates/tf_tree/src` returns one line, in `tree.rs`. The scope is the crate ROOT, not the package ([`0048`](./decisions/0048-a-kind-is-not-a-crate-name.md)).
- [~] Every `unsafe` block has a `// SAFETY:` comment naming a §2 invariant — the comment half is gated (`clippy::undocumented_unsafe_blocks` is `deny`; `scripts/unsafe-budget.txt` is the per-file index) and checks placement, not content. The "naming a §2 invariant" half is not gated and cannot be as written: §2's invariants concern the **arena**, while most blocks are the OS, a foreign runtime, a foreign caller or the C ABI (`0007`'s kinds). Closing this box means restating it against those kinds.
- [x] `tf_tree doctor` detects all seven listed conditions — a **positive and a negative** test per check in `crates/tf_tree_cli/src/doctor.rs`. The negative arm is the load-bearing half.
- [x] Public API and the five §3.1 conventions documented (`crates/tf_tree_math/src/lib.rs`), with `#![deny(missing_docs)]` on the publishable roots (landed 2026-09-09). It binds this repository and its path dependents, not a crates.io consumer (`--cap-lints allow`).
- [x] A `PHASE2.md` lists every place a `MappedArena` must differ — its §1 carries amendments A1–A8 and §4 states the read-path claim.

## Appendix: suggested implementation order

1. `tf_tree_math` types, `exp`/`log`, the numerical test sweep from §3.3.
2. `tf_tree_math` interpolation, both `ScLerp` implementations, the invariance tests.
3. `tf_tree_arena` layout computation and `HeapArena`, with layout assertion tests.
4. `tf_tree_core` frame interning and the topology block, with loom tests.
5. `tf_tree_core` edge records, claims, `PoseSlot`, publish/read protocols, loom tests, the wrapped-ring test. Do not proceed past this step until its loom tests pass.
6. Plan compilation, static folding, evaluation.
7. Public API and the convenience path.
8. `at_many` and `at_adaptive`.
9. Benchmarks and the tf2 differential harness.
10. CLI.
