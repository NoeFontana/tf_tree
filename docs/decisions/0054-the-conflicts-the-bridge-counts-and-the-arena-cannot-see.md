# 0054: the conflicts the bridge counts and the arena cannot see

**Status:** draft
**Owner:** @noe
**Implementation:** (none yet)

## Context

`TFT002` (a static edge republished with a different value) and `TFT003` (an
edge's kind changed) are the two of nineteen catalogue rules that detect nothing
in any configuration `doctor` builds (`crates/tf_tree_cli/src/catalogue.rs`,
`checks.rs`). Both conditions are detected in `tf_tree_bridge`:
`StaticVerdict::KindChanged` (`TFT003`) and `StaticStore::conflicts` (`TFT002`)
from `StaticStore::observe_static` (`crates/tf_tree_bridge/src/statics.rs`). The
two are **disjoint**: the `KindChanged` arm returns before the value-conflict
increment, so two counters are needed.

The gap is locality: a `StaticStore` lives in the bridge's heap and `doctor` is
another process. `checks.rs` concludes that surfacing them needs arena space
`PHASE5.md` §1.2 does not reserve, which `CLAUDE.md`'s "do not add arena fields
opportunistically" routes to `PROJECT.md` §5.1's scheduled-break ledger. That
conclusion is wrong, and `PHASE5.md` §12 criterion 6 cannot be met while two
rules cannot fire. **Publishing these two counters does not cost a
`FORMAT_VERSION`:**

- `layout_hash()` (`crates/tf_tree_arena/src/layout.rs`) folds
  `size_of::<ArenaHeader>()`, `align_of` and the **region strides**, not fields
  inside a record.
- `EdgeCounters` and `ParticipantCounters` are exactly 128 bytes, pinned by
  compile-time asserts in `crates/tf_tree_core/src/counters.rs`, with
  `_pad: [u8; 64]` and `_pad: [u8; 60]`. Naming bytes out of the padding leaves
  the stride at 128, so the hash is unchanged.

## Decision

**1. Two counters, per edge, carved from `EdgeCounters::_pad`.**

```rust
/// A `/tf_static` message republished this edge with a different value
/// (`TFT002`). Written by the ingest bridge; `0` on any arena no bridge filled.
pub static_value_conflicts: AtomicU64,
/// This edge's declared kind changed between declaration and a message
/// (`TFT003`). Disjoint from the above — the `KindChanged` arm returns before
/// the value-conflict path runs.
pub static_kind_changes: AtomicU64,
```

`_pad` shrinks `[u8; 64]` → `[u8; 48]`; the two `size_of == 128` asserts are the
check that this stayed free. **Per edge, not per participant, because of D11**
(every error names the offending edge).

**2. The bridge publishes into the arena it already fills**, at `fill` in
`crates/tf_tree_c/src/bridge.rs`, which holds both the `Action` and the writer
map and already has the raw names in `scratch` (no new plumbing, no hot-path
allocation). The counters are `Relaxed` stores: diagnostics, nothing branches on
them.

**3. `TFT002` and `TFT003` read them through the existing accessor** and
**fire on non-zero and skip-with-a-reason on zero**, never pass (zero is
indistinguishable from no bridge having run). Skip reason: *"no bridge reported a
static conflict for this edge; an arena filled before `0054` does not report
them"*.

**`TFT003` additionally skips by source**: an edge-kind change **aborts** a bag
ingest (`IngestError::EdgeKindChanged` at three sites in
`crates/tf_tree_ingest/src/ingest.rs`), so no `.tft` reached through
`--from-bag` can carry it, and the skip says *"an edge-kind change aborts a bag
ingest before an arena exists, so this source cannot carry the condition"*. The
live bridge drops the message and keeps running, so only a bridge-filled arena
can. `TFT002` is unaffected: a value conflict aborts neither path.

**4. D22 holds unchanged.** The fields exist whether or not `counters` is
compiled in; only the writing code disappears.

## Consequences

- A pre-`0054` arena reads as zeros; the skip arm refuses a clean bill.
- **`EdgeCounters::_pad` drops to 48 bytes.** A request past 128 bytes is a format
  break for the ledger.
- §12 criterion 6 becomes satisfiable, and `checks.rs`'s and `catalogue.rs`'s headers
  must be corrected. The counters are `Relaxed`: no crash-matrix row (D15).

## Implementation plan

1. **Carve the two fields out of `EdgeCounters::_pad`**
   (`crates/tf_tree_core/src/counters.rs`), update `Default`. — the
   `size_of::<EdgeCounters>() == 128` assert and `layout_hash` still reading
   `0x3D10_4195` (`cargo nextest run -p tf_tree_arena -- layout_hash`).
2. **Publish from the bridge's verdict consumer**: `static_value_conflicts` on the
   value-conflict arm, `static_kind_changes` on `StaticVerdict::KindChanged`. — a
   test ingesting a conflicting `/tf_static` pair and reading both back through
   `ArenaView`.
3. **Rewrite `TFT002` and `TFT003`** to fire-on-non-zero / skip-with-a-reason,
   naming the edge. — a `crates/tf_tree_cli/tests/catalogue.rs` case per rule,
   mutation-checked (zero the counter and the rule must stop firing).
4. **Correct the two headers** and `PHASE5.md` §6's rows. — `just lint`.
5. **Build §12 criterion 6's corpus** and record the reading in
   `docs/benchmarks/EVIDENCE.md`. — `just evidence-audit`.

## Open questions

This record stays `draft` until someone other than its author promotes it.

1. **Does a per-participant *total* earn its 16 bytes as well?** Deferred, not
   blocking. `ParticipantCounters` has 60 bytes of padding, with `last_err_edge`
   as precedent. Nothing has asked for it.
