# 0054: the conflicts the bridge counts and the arena cannot see

**Status:** draft
**Owner:** @noe
**Implementation:** (none yet)

## Context

`TFT002` (static edge republished with a different value) and `TFT003` (edge
kind changed) detect nothing in any `doctor` configuration. The bridge detects
both: `StaticVerdict::KindChanged` (`TFT003`) and
`StaticStore::conflicts` (`TFT002`), from `StaticStore::observe_static`. They are
**disjoint**: the `KindChanged` arm returns before the value-conflict increment.

A `StaticStore` lives in the bridge's heap and `doctor` is another process.
Publishing the counters **costs no `FORMAT_VERSION`**: `layout_hash()` folds region
strides, and `EdgeCounters` stays 128 bytes when named out of its `_pad`.

## Decision

**1. Two per-edge counters carved from `EdgeCounters::_pad`** (D11), both
`AtomicU64`: `static_value_conflicts` (`TFT002`) and `static_kind_changes`
(`TFT003`), each `0` on any arena no bridge filled. `_pad` shrinks
`[u8; 64]` → `[u8; 48]`; the two `size_of == 128` asserts check this stayed free.

**2. The bridge publishes** at `fill` in `crates/tf_tree_c/src/bridge.rs`, by
`Relaxed` stores.

**3. `TFT002` and `TFT003` read them through the existing accessor**, **fire on
non-zero and skip-with-a-reason on zero**, never pass. Skip reason: *"no bridge
reported a static conflict for this edge; an arena filled before `0054` does not
report them"*.

**`TFT003` additionally skips by source**: an edge-kind change aborts a bag
ingest (`IngestError::EdgeKindChanged`), so no `--from-bag` `.tft` can carry it.

**4. D22 holds unchanged.** The fields exist whether or not `counters` is
compiled in. A request past 128 bytes of `EdgeCounters` is a format break for
the ledger.

## Implementation plan

1. Carve the fields out of `_pad`. — `size_of::<EdgeCounters>() == 128`;
   `layout_hash` still `0x3D10_4195`.
2. Publish from the bridge's verdict consumer. — a test reading both back through
   `ArenaView`.
3. Rewrite `TFT002`/`TFT003`. — a `crates/tf_tree_cli/tests/catalogue.rs` case per
   rule, mutation-checked.
4. Correct `checks.rs`/`catalogue.rs` headers and `PHASE5.md` §6's rows.
5. Build §12 criterion 6's corpus; record it in `docs/benchmarks/EVIDENCE.md`.

## Open questions

This record stays `draft` until someone other than its author promotes it.

1. Does a per-participant *total* earn its 16 bytes? Deferred; nothing asks.
