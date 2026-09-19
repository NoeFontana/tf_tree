# 0009: Descoping Phase 6 — covariance and CoW branches are cut, URDF leaves the engine

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** (filled in as work lands)

## Context

[`0006`](./0006-the-eight-phase-roadmap.md) gave Phase 6 four items: covariance,
copy-on-write branches, cumulative B-splines, URDF parsing. `PHASE5.md` §1
reserved four header fields for it (`covariance_region_off` 160,
`covariance_stride` 164, `spline_region_off` 168, `spline_degree` 172); removing
one after Phase 6 populates it would be a `FORMAT_VERSION = 4`.

## Decision

**Phase 6 is renamed *continuous-time interpolation* and reduced to one item:
cumulative B-spline interpolation with analytic derivatives.**

### 1. Covariance is descoped entirely — not deferred

`covariance_region_off` and `covariance_stride` are removed from `ArenaHeader`;
their bytes return to `_reserved_v3`; `PROJECT.md` states that `tf_tree` **does
not carry uncertainty at all**.

### 2. Copy-on-write branches are cut

No arena change; the out-of-scope tables in `PHASE4.md` and `PHASE5.md` cite D2.

### 3. URDF leaves the engine and becomes a converter

Not built here as part of any phase. It may be a separate tool that emits the
topology config (`tf_tree topology --config`), never an engine dependency.

### 4. B-splines stay

`spline_region_off` and `spline_degree` keep their fields and offsets.

## Rationale

**Covariance.** The tree cannot represent cross-correlation between sibling
branches, so a composed covariance is optimistic in the direction that causes
harm: we cannot ship a correct number, so we ship none (D2). **CoW branches**
contradict fixed capacity, one writer per edge, and append-only ids. **URDF**:
the dependency budget forbids an XML parser in the core crates. **B-splines**
answer a `PROJECT.md` §2 row (no derivatives, no continuous-time model).

## Consequences

- **`FORMAT_VERSION` stays 3**; 160..168 becomes reserved in place.
- **The `tf_treed --config <file.toml|urdf>` surface in `PHASE2.md` §9 loses its
  `urdf` half.**

  > [`0019`](./0019-one-binary-and-topology-you-can-wait-for.md) supersedes
  > `PHASE2.md` §9: the capability is `tf_tree serve --config <topology.toml>`,
  > topology format only, so the `urdf` half stays retracted.

## Implementation plan

1. This record, the `README.md` table, and `PROJECT.md` §1, §4, D2, D22.
2. `header.rs`, `heap.rs`, `doctor --explain-version`: remove the two fields,
   reserve 160..168 (`size_of::<ArenaHeader>() == 320`, `_reserved_v3 >= 64`).
3. `PHASE5.md` §1.2, `PHASE4.md` §0 and `Sample::accel`, `PHASE2.md` §9.
