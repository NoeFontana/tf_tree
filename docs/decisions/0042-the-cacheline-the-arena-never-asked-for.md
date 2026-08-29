# 0042: the cacheline the arena never asked for

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** (filled in as work lands)

## Context

`Iso3` was `#[repr(C, align(64))]` with an 8-byte `_pad`, and its doc gave the
reason:

> Laid out as exactly one 64-byte cacheline (`align(64)`) with an 8-byte pad so
> the Phase 2 shared-memory arena can store slots without re-deriving layout.

**The arena re-derived the layout anyway, and never stored an `Iso3`.**
`tf_tree_core::buffer::PoseSlot` is its own `#[repr(C, align(64))]` of
`{ AtomicU32, u32, [AtomicU64; 7] }` with its own compile-time size assertion —
it has to be, because the payload must be atomics for the seqlock to be sound in
the Rust memory model. An `Iso3` reaches it through `Iso3::to_bits` and comes
back through `Iso3::from_bits`. No arena structure has an `Iso3` field, and
nothing outside `tf_tree_math`'s own tests asserts its size or casts bytes to it.

So the alignment bought the arena nothing, and every **in-memory** use of the
type paid for it. Measured before the change:

| | bytes |
|---|---|
| `Iso3` | 64 (56 of data) |
| `Step` | 128 — the discriminant forced a second cacheline |
| `Plan` (`[Step; MAX_DEPTH]` + fields) | 4160 |
| facade's 16-slot thread-local plan cache | **66.0 KiB per thread** |
| `(i64, Iso3)` | 128 |

**A second consumer had already paid to route around it.**
`tf_tree_ingest::ingest`'s `SAMPLE_BYTES` buffers a bare `[f64; 7]` beside its
stamp, and says why: *"`Iso3` is `align(64)`, so a `(i64, Iso3)` pair occupies
128 bytes and would double the memory this module is trying to bound."* When a
module works around a type's layout to meet its own budget, the layout is a
liability rather than an asset.

## Decision

**`Iso3` becomes `#[repr(C)]` with no padding: seven `f64` in canonical order,
56 bytes, `align(8)`.**

Measured after:

| | before | after |
|---|---|---|
| `Iso3` | 64 | **56** |
| `Step` | 128 | **64** |
| `Plan` | 4160 | **2064** |
| plan cache, per thread | 66.0 KiB | **32.6 KiB** |
| `(i64, Iso3)` | 128 | **64** |

Nothing about the arena moves: no `FORMAT_VERSION` bump, no `layout_hash`
change, no `PoseSlot` change. The C ABI is untouched — it takes `&Iso3` and
writes into caller layouts, and exposes the type's own shape nowhere.

**`Pod` still holds and is the thing to watch.** Four plus three `f64` is 56
bytes with no interior padding at `align(8)`, so the derive is as valid as it was
at 64 — but a field added later must keep that true, where previously the pad
absorbed anything.

## Rationale

**Why this is not sold on latency.** `docs/design/fast-path.md` §15 already
measured the alignment's effect on the fold at zero, and the audit that surfaced
this said plainly not to claim otherwise. `just bench-check` passes and
`just control-loop` reads p50 500 ns against 531 before — but that host is
unpinned and its own maximum moved from 3.6 ms to 15.2 ms between runs, so the
honest statement is **no regression visible**, not an improvement. What this buys
is footprint.

**Why footprint is worth a decision here.** The plan cache is per *thread*. A
perception node with eight worker threads held 528 KiB of plan cache and now
holds 261; on a Jetson-class part whose L2 is a couple of megabytes shared across
a cluster, that is the difference between the cache being background and being a
tenant. `Plan` itself halving matters for the same reason one step down: a
control loop holds one and touches it every cycle.

**Why the alignment might still have been wanted, and why it is not.** A 64-byte
`Iso3` never straddles a cacheline; a 56-byte one in an array sometimes does. But
the array is half as large, so a depth-*d* walk touches fewer lines in total, and
the fold reads each `Step` once in order — the access pattern straddling hurts
least. The gate that would catch it being wrong is `just bench-check`, and it
passes.

**Why not keep the pad and drop only the alignment.** The pad exists *for* the
alignment: eight bytes to round 56 up to 64. With `align(8)` it would be eight
bytes of nothing, and `Pod` would then require it be initialised anyway.

## Consequences

- A consequence worth stating because it makes an existing rationale partly
  false: `Iso3` is now 56 bytes in exactly `[qw qx qy qz tx ty tz]` order, which
  is **byte-identical to `Layout::Quat`**. `crates/tf_tree_core/src/layout.rs`
  argued that `&mut [Iso3]` aliases *no* layout a consumer wants; that now holds
  for `Mat4` and `Affine32` and not for `Quat`. Nothing changes behaviourally —
  the kernels still fold into the destination — but the `Quat` kernel becomes a
  candidate for a straight copy, which is an optimisation with its own
  measurement and is deliberately not taken here.
- `size_of::<Plan>()` appears in several doc comments and in `PHASE1.md`, all of
  them measured figures. Every one is updated, and each says what it was, because
  a number that changes silently is how the stale-figure defects in this
  repository's history started.
- `tf_tree::cache`'s `Entry` assertion was written as
  `size_of::<Plan>() + align_of::<Plan>()`, which was correct only while `Plan`
  was `align(64)` and `Key` was 24 bytes — the key hid entirely inside the
  padding, so the padding *was* its cost. It is rewritten against `Key`'s size.
- The wasted eight bytes are gone from every `Iso3` in memory, which includes
  every `Step`, every buffered ingest sample, and every pose a consumer holds.
- **The public surface widened, and that is accepted rather than overlooked.**
  `_pad` was private, and it was the only thing stopping a downstream crate
  writing `Iso3 { q, t }` or destructuring the struct exhaustively. Both are now
  permanently supported, so adding a field later is breaking for a second reason
  on top of `Pod`'s no-padding rule.

  Accepted, and not closed with `#[non_exhaustive]`, for two reasons. `Vec3` and
  `Quat` are plain `#[repr(C)]` structs with public fields and no marker, so
  `Iso3` now *matches its siblings* — the encapsulation was a side effect of the
  alignment rather than a design choice anybody made. And `Iso3` is
  mathematically closed: SE(3) is a rotation and a translation, D6 fixes the
  scalar at `f64`, and [`0009`](./0009-descoping-phase-6.md) cut the one field
  anybody proposed adding. A marker that costs every downstream caller the
  literal, to protect against a field this type should not gain, is the wrong
  trade — but it is a trade, so it is written down.

## Implementation plan

1. Drop `align(64)` and `_pad`; update `Iso3`'s own size/align assertions and its
   `Pod` round-trip byte length. Verified by `cargo test -p tf_tree_math`.
2. Rewrite `cache::tests::a_refusal_is_free_to_cache`'s `Entry` assertion against
   `size_of::<Key>()` rounded to `align_of::<Plan>()`. Verified by that test,
   which fails on the old formula at the new alignment.
3. Pin the sizes so they cannot drift silently again: a test asserting
   `size_of::<Iso3>()`, `size_of::<Step>()` and `size_of::<Plan>()`, with the
   before/after in its message. Verified by the test.
4. Update every doc figure — `MAX_DEPTH`'s slot price, `MAX_PATH_EDGES`'s
   comparison, `cache.rs`'s two blocks, `plan.rs`'s copy table, `PHASE1.md` §7 —
   each stating its previous value. Verified by `just lint` and by grep for the
   old figures.
5. Correct the two *arguments* the change falsifies: `layout.rs`'s aliasing
   rationale and `ingest.rs`'s `SAMPLE_BYTES` comment. Verified by reading, and
   by the `Quat`-coincidence being demonstrated rather than asserted.
6. `just bench-check` passes against the committed baseline, and the reading is
   reported as "no regression visible" rather than as an improvement.

## Open questions

None. One was resolved by measurement: whether a 56-byte `Iso3` straddling
cachelines inside `[Step; MAX_DEPTH]` costs more than the halved footprint saves.
`bench-check` holds and the control-loop reading does not regress, so it does
not — and if a future host says otherwise, that gate is where it will say so.
