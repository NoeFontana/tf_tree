# 0042: the cacheline the arena never asked for

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** #280

## Decision

**`Iso3` is `#[repr(C)]` with no padding: seven `f64` in `[qw qx qy qz tx ty tz]` order, 56 bytes, `align(8)`.** The arena never stored an `Iso3` (`PoseSlot` is its own atomics layout, reached through `Iso3::to_bits`/`from_bits`), so the old `align(64)` and `_pad` only cost in-memory use.

| | before | after |
|---|---|---|
| `Iso3` | 64 | **56** |
| `Step` | 128 | **64** |
| `Plan` | 4160 | **2064** |
| plan cache, per thread | 66.0 KiB | **32.6 KiB** |
| `(i64, Iso3)` | 128 | **64** |

No `FORMAT_VERSION` bump, no `layout_hash` change, no `PoseSlot` change, C ABI untouched. This is a footprint change, not a latency claim: `just bench-check` shows no regression.

## Consequences

- `Pod` still holds only while the fields total 56 bytes with no interior padding; a field added later must keep that true.
- `Iso3` is byte-identical to `Layout::Quat` (`crates/tf_tree_core/src/layout.rs`), which no longer aliases no consumer layout for `Quat`.
- `Iso3 { q, t }` construction and exhaustive destructuring are now permanently public, matching `Vec3` and `Quat`. Not `#[non_exhaustive]`: SE(3) is closed, D6 fixes the scalar, and [`0009`](./0009-descoping-phase-6.md) cut the one proposed field.
- `size_of::<Plan>()` doc figures state their value; `cache.rs`'s `Entry` assertion is written against `Key`'s size.

## Implementation plan

Pinned by `the_sizes_0042_halved_stay_halved` in `tf_tree_core`'s tests (`Iso3`, `Step`, `Plan` sizes).
