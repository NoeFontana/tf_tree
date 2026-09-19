# 0009: Descoping Phase 6 — covariance and CoW branches are cut, URDF leaves the engine

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** (filled in as work lands)

## Context

[`0006`](./0006-the-eight-phase-roadmap.md) gave Phase 6 the title *"remaining
engine features"* with four items: covariance with adjoint transport,
copy-on-write branches, cumulative B-spline interpolation with analytic
derivatives, and URDF parsing with typed-frame codegen. Phase 6 is the only phase
not organised by [`docs/PROJECT.md`](../PROJECT.md) §4's principle *"ordered by
what constrains what"*.

`PHASE5.md` §1 pre-reserved Phase 6's layout in four header fields:

| field | offset |
|---|---|
| `covariance_region_off: u32` | 160 |
| `covariance_stride: u32` | 164 |
| `spline_region_off: u32` | 168 |
| `spline_degree: u8` | 172 |

Removing one is cheap today (`layout_hash()` hashes region strides, not header
fields; ≥ 64 bytes stay reserved; covariance is written as `0` in one place,
`crates/tf_tree_arena/src/heap.rs`, and read nowhere). After Phase 6 populates it,
it is a `FORMAT_VERSION = 4` and a synchronised restart of every participant.

## Decision

**Phase 6 is renamed *continuous-time interpolation* and reduced to one item:
cumulative B-spline interpolation with analytic derivatives.**

### 1. Covariance is descoped entirely — not deferred

`covariance_region_off` and `covariance_stride` are removed from `ArenaHeader`;
their twelve bytes return to `_reserved_v3`. `PROJECT.md` §1's promise that
"uncertainty, when it arrives in Phase 6, will be a marginal" is replaced by a
statement that `tf_tree` **does not carry uncertainty at all**, and D2's
"Uncertainty is a marginal" clause goes with it. A reader must not infer that
covariance is coming later.

### 2. Copy-on-write branches are cut

No arena change (CoW reserved no layout space). `PROJECT.md` §4 and the
"out of scope" tables in `PHASE4.md` and `PHASE5.md` name it as rejected, citing D2.

### 3. URDF leaves the engine and becomes a converter

URDF parsing is **not** an engine feature and **not built in this repository as
part of any phase**. It may be a separate optional tool that reads a URDF and
emits the topology config Phase 4 shipped (`tf_tree topology --config`), never a
dependency of an engine crate. Whoever builds it uses
[`urdf-rs`](https://crates.io/crates/urdf-rs) (Apache-2.0, already on
`deny.toml`'s allow-list).

### 4. B-splines stay

`spline_region_off` and `spline_degree` keep their fields and offsets. Phase 6's
thesis is the §2 row *"no derivatives, no continuous-time model → cannot serve as
a VIO/SLAM trajectory backbone"*.

## Rationale

**Covariance.** It has no row in `PROJECT.md` §2, and the tree "cannot represent
cross-correlation between sibling branches", so a composed covariance is valid only
for independent edges; `map → odom` and `odom → base_link` come from one estimator
and are correlated. Composing marginals as independent is **optimistic in the
direction that causes harm**. **We cannot ship a correct number, so we ship none**;
joint uncertainty needs a factor graph (D2).

**CoW branches** serve the use case D2 rejects and contradict fixed capacity, one
writer per edge, and append-only ids: a second storage model.

**URDF.** Not every user needs it, `urdf-rs` already exists, and the dependency
budget forbids an XML parser in `tf_tree_core`/`tf_tree_arena`. A `tf_tree urdf`
subcommand would make URDF something the project owes.

**B-splines** answer a §2 row and extend an axis that exists.

## Consequences

- Phase 6 becomes a single-thesis phase, and much smaller.
- **`FORMAT_VERSION` stays 3.** **The pinned offsets of the remaining fields must
  not move**: `spline_region_off` and `spline_degree` keep 168 and 172; 160..168
  becomes reserved in place.
- `tf_tree` states plainly that it carries no uncertainty; D2 is amended.
- **The `tf_treed --config <file.toml|urdf>` surface in `PHASE2.md` §9 loses its
  `urdf` half.**

  > [`0019`](./0019-one-binary-and-topology-you-can-wait-for.md) supersedes
  > `PHASE2.md` §9: there is no `tf_treed`; the capability is
  > `tf_tree serve --config <topology.toml>`, which takes the topology format
  > only, so the `urdf` half stays retracted.
- A future contributor may add URDF against the topology config format without a
  decision record.

## Implementation plan

1. This record and the `README.md` state table.
2. **`PROJECT.md`**: §1 non-goal paragraph, §4 Phase 6 sentence, D2's uncertainty
   clause, D22's first-consumer note.
3. **`crates/tf_tree_arena/src/header.rs`** and **`heap.rs`**: remove the two fields
   and their zeroing writes, extend the reserved block over 160..168 — spline
   `offset_of!` assertions unchanged, `size_of::<ArenaHeader>() == 320`,
   `_reserved_v3 >= 64`. With **`crates/tf_tree_cli/src/lib.rs`**'s
   `doctor --explain-version` text; one PR.
4. **`PHASE5.md`** §1.2, **`PHASE4.md`** §0 and `Sample::accel`, **`PHASE2.md`** §9.
5. `just lint`, `just test`, `just shm-check`.

## Open questions

None. `PHASE1.md` §3.1's note that the twist convention "is what `docs/PHASE1.md`
§3.1 fixes for covariance" stays: the right-perturbation convention is
load-bearing for B-spline derivatives too.
