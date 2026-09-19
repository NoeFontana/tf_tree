# 0042: the cacheline the arena never asked for

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** #280

## Decision

**`Iso3` is `#[repr(C)]` with no padding: seven `f64` in `[qw qx qy qz tx ty tz]` order, 56 bytes, `align(8)`.** The arena never stored an `Iso3`. `Step` is 64 B and `Plan` 2064 B; no `FORMAT_VERSION` or `layout_hash` change.

## Consequences

- `Pod` holds only while the fields total 56 bytes with no padding.
- `Iso3 { q, t }` construction and destructuring are permanently public; not `#[non_exhaustive]` (D6; [`0009`](./0009-descoping-phase-6.md)).

## Implementation plan

Pinned by `the_sizes_0042_halved_stay_halved` in `tf_tree_core`'s tests.
