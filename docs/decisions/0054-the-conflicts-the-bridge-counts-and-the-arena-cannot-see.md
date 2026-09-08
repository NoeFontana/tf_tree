# 0054: the conflicts the bridge counts and the arena cannot see

**Status:** draft
**Owner:** @noe
**Implementation:** (none yet)

## Context

`TFT002` (a static edge republished with a different value) and `TFT003` (an
edge's kind changed) are **the two of nineteen catalogue rules that detect
nothing in any configuration `doctor` builds**. `crates/tf_tree_cli/src/catalogue.rs`
says so against itself — *"some ids have no detection here: `TFT002` and
`TFT003` cannot detect anything in any configuration `doctor` currently
builds"* — and `crates/tf_tree_cli/src/checks.rs`'s header says why.

This is not a gap in the *detection*. Both conditions are detected today, in
`tf_tree_bridge`:

- `StaticVerdict::KindChanged` is produced by `StaticStore::observe_static`
  (`crates/tf_tree_bridge/src/statics.rs`), which is `TFT003`'s condition
  exactly.
- `StaticStore::conflicts` is incremented on the value-conflict path of the same
  function, which is `TFT002`'s. Note the two are **disjoint**: the
  `KindChanged` arm returns before the increment, so a kind change is not
  counted as a value conflict and two counters are needed, not one.

It is a **locality** gap, and `checks.rs` names it precisely: a `StaticStore`
lives in the bridge process's heap, the arena stores neither a static edge's
publication history nor its declared kind over time, and `doctor` is a different
process. That header then states the disposition this record exists to revisit:

> Surfacing them needs the bridge to publish its counters into the arena, which
> `docs/PHASE5.md` §1.2 does not reserve space for.

**That sentence is true about §1.2 and wrong about the conclusion**, which is
what forces this record now rather than later. `PHASE5.md` §12 **criterion 6**
requires the catalogue to fire against a corpus, and these two rules are its
standing residual; the criterion cannot be met while two of its rules are
structurally incapable of firing. Meanwhile `CLAUDE.md`'s standing instruction —
**do not add arena fields opportunistically** — routes any request for arena
space to `PROJECT.md` §5.1's scheduled-break ledger, and a ledger entry is a
wait of indefinite length. So the question that decides this is narrow: **does
publishing these two counters cost a `FORMAT_VERSION`?**

It does not, and that is measurable rather than arguable.

- `layout_hash()` (`crates/tf_tree_arena/src/layout.rs`) folds
  `size_of::<ArenaHeader>()`, `align_of::<ArenaHeader>()` and the **region
  strides**. It does not hash the meaning, name, or count of fields *inside* a
  record.
- `EdgeCounters` and `ParticipantCounters` are both exactly 128 bytes, and both
  sizes are pinned by a **compile-time** assert (`crates/tf_tree_core/src/counters.rs`,
  `assert!(core::mem::size_of::<EdgeCounters>() == 128)` and the same for
  `ParticipantCounters`).
- `EdgeCounters` carries `_pad: [u8; 64]` and `ParticipantCounters` carries
  `_pad: [u8; 60]`. **Naming bytes out of that padding leaves the stride at 128**,
  so every input to `layout_hash` is unchanged, so the hash is unchanged, so two
  correctly-built participants of adjacent versions still attach.

The space is therefore already reserved and already sized. §1.2 does not need to
reserve it. This is not the region break `0032` scheduled, it does not join
§5.1's ledger, and it costs no fleet restart.

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

`_pad` shrinks `[u8; 64]` → `[u8; 48]`. The two `size_of == 128` asserts are
unchanged and are the check that this stayed free.

**Per edge, not per participant, because of D11.** *Every error names the
offending edge.* A per-participant total answers "this bridge saw four
conflicts" and cannot say which edge, which is precisely the `tf2` behaviour
`PROJECT.md` §2 exists to fix. `ParticipantCounters` has the padding too and is
the wrong home for the same reason `last_err_edge` had to be added to it: the
participant names the process, the edge names the fault.

**2. The bridge publishes into the arena it already fills.** `tf_tree_bridge`
is `no_std`-shaped and holds no arena handle itself; the publishing happens
where the bridge's verdicts are already consumed and an `EdgeId` is already in
hand. The counters are `Relaxed` stores — they are diagnostics, nothing branches
on them, and no ordering claim is made about them.

**3. `TFT002` and `TFT003` read them through the existing accessor** and stop
being unconditional skips. They **fire on non-zero and skip-with-a-reason on
zero**, never pass:

- non-zero → the rule fires and names the edge.
- zero → skipped, reason *"no bridge reported a static conflict for this edge;
  an arena filled before `0054` does not report them"*.

**`TFT003` additionally skips by source, for a reason `TFT002` does not share**
(open question 3, measured): an edge-kind change **aborts** a bag ingest —
`tf_tree_ingest` returns `IngestError::EdgeKindChanged` rather than continuing —
so no arena reached through `--from-bag` can carry the condition, and the skip
must say that rather than reporting an absence. Only the live bridge, which
drops the message and keeps running, produces an arena that can.

A conflict detector has no honest "OK" state — zero conflicts is
indistinguishable from no bridge having run — and `PHASE5.md` §6's rule is that
a skip states its reason. This also keeps the pre-`0054`-writer case honest
rather than silently reporting a clean bill.

**4. D22 holds unchanged.** The fields exist whether or not the `counters`
feature is compiled in; only the code that writes them disappears. That is what
D22 requires and is already how both counter regions behave.

## Rationale

**A new region** — the shape §1.2's absence implies — costs a `FORMAT_VERSION`,
joins §5.1's ledger behind Phase 6's spline region, and forces a fleet-wide
restart on upgrade. It buys nothing the padding does not already provide. The
ledger exists so that a request for arena *space* has somewhere to be argued;
this request needs no space.

**Leaving them skipped** is the status quo and its cost is now legible: §12
criterion 6 is unmeetable by construction, and the catalogue advertises
nineteen rules of which two can never fire. `PHASE5.md` §6's promise that a skip
states its reason is honoured, but the reason is *"this is architecturally
impossible here"*, which is a permanent answer to a solvable problem.

**Having `doctor` read the bridge's heap** is the locality problem `checks.rs`
names and is not available across a process boundary. Rejected on arrival.

**Per-participant instead of per-edge** loses the edge name (D11) and is
rejected above. It remains the right home for a *total*, which is left open
below rather than decided here.

## Consequences

- **A pre-`0054` arena reads as zeros**, which the skip-with-a-reason arm
  handles by refusing to report a clean bill rather than by inventing a
  provenance bit. No participant flag is introduced.
- **`EdgeCounters::_pad` drops to 48 bytes.** The remaining padding is smaller,
  and the next request against it should read this record first: the argument
  here is that *naming padding is free*, not that padding is a general-purpose
  extension point. A request that would push the record past 128 bytes is a
  format break and joins the ledger.
- **§12 criterion 6 becomes satisfiable on this host**, which is the point. The
  fixture already exists in spirit: `statics.rs`'s own tests drive
  `StaticStore` to a non-zero `conflicts()`, so the corpus is a bridge ingest
  with conflicting `/tf_static` declarations, frozen, then run through `doctor`.
- **`checks.rs`'s and `catalogue.rs`'s headers must both be corrected**, and in
  this repository's style — they currently state the impossibility as settled,
  and that is the claim this record retires.
- Two more counters is two more things a `counters`-off build compiles out and a
  `counters`-on build must keep correct; they are `Relaxed` and unordered, so
  they add no concurrency obligation and no crash-matrix row (D15 is about
  mutation protocols; a monotone diagnostic counter is not one).

## Implementation plan

1. **Carve the two fields out of `EdgeCounters::_pad`** (`crates/tf_tree_core/src/counters.rs`),
   `_pad` 64 → 48, and update the hand-written `Default`. — verified by the
   existing `size_of::<EdgeCounters>() == 128` compile-time assert and by
   `layout_hash_is_deterministic_and_stable` still reading `0x3D10_4195`
   (`cargo nextest run -p tf_tree_arena -- layout_hash`).
2. **Publish from the bridge's verdict consumer**, incrementing
   `static_value_conflicts` on the value-conflict arm and `static_kind_changes`
   on `StaticVerdict::KindChanged`. — verified by a test that ingests a
   conflicting `/tf_static` pair and reads the two counters back through
   `ArenaView`.
3. **Rewrite `TFT002` and `TFT003`** from unconditional skips to the
   fire-on-non-zero / skip-with-a-reason shape, naming the edge. — verified by a
   `crates/tf_tree_cli/tests/catalogue.rs` case per rule, each **mutation-checked**:
   zero the counter and the rule must stop firing.
4. **Correct the two headers** (`checks.rs`, `catalogue.rs`) and `PHASE5.md`
   §6's rows, recording what they used to claim. — verified by `just lint` and
   by `catalogue.rs`'s own detecting-count instrument.
5. **Build §12 criterion 6's corpus** and record the reading in
   `docs/benchmarks/EVIDENCE.md`. — verified by `just evidence-audit`.

## Open questions

Resolved before this moves from `draft` to `ready`. **Questions 2 and 3 were
answered by measurement while drafting and are recorded here rather than
deleted, because each changed the Decision above.** What remains is
ratification, not investigation — and this record deliberately stays `draft`
until someone other than its author promotes it, which is the standard this
repository failed to hold for `0047`/`0048`/`0049`.

1. **Does a per-participant *total* earn its 16 bytes as well?** Deferred, not
   blocking. The per-edge counters answer *which edge*; an operator triaging a
   fleet may want *which process*, and `ParticipantCounters` has 60 bytes of
   padding with `last_err_edge` as the precedent for closing that loop. Nothing
   has asked for it, and an unused counter is a maintained one — so this is
   listed as a known future request with somewhere to be argued, not as an
   omission.

2. **Where does the bridge get its `EdgeId`?** — **ANSWERED.** The site is
   `fill` in `crates/tf_tree_c/src/bridge.rs`, the only consumer that holds both
   the `Action` and the writer map. The obstacle this question was raised
   against does not bite: `Action::Drop` carries only a reason and deliberately
   does not carry the pair, but `fill` **already has the raw names in
   `scratch`** and puts them there for exactly this class of reason — §5.4
   requires the authority diagnostic to name the edge, and that comment records
   that asking `tf_tree_bridge` to attach names instead *"would put two
   `String`s on the hot path of every dropped 1 kHz sample"*. So the counter
   costs no new plumbing and no hot-path allocation. It also lands at the right
   granularity by that code's own argument: `KIND_CHANGE` is *"bounded by the
   declared topology — genuinely once per edge"*, so the edge is declared and
   has an arena `EdgeId` whenever this fires.

3. **Does `TFT003`'s condition survive ingest?** — **ANSWERED, and it splits by
   source rather than splitting the record.** Measured on both paths:

   - **Bag ingest aborts.** `crates/tf_tree_ingest/src/ingest.rs` turns
     `StaticVerdict::KindChanged` into `return Err(IngestError::EdgeKindChanged)`
     at three sites. The ingest fails, no arena is produced, and **no frozen
     `.tft` reached through `--from-bag` can ever carry a non-zero
     `static_kind_changes`.**
   - **The live bridge does not abort.** `crates/tf_tree_bridge/src/ingest.rs`
     increments `stats.dropped_kind_change` and returns
     `Action::Drop { reason: DropReason::KindChange }`; the node keeps running,
     so a bridge-filled arena carries the condition.

   So `TFT003` fires only on a bridge-filled arena and must skip **with the
   source named** on `--from-bag` — *"an edge-kind change aborts a bag ingest
   before an arena exists, so this source cannot carry the condition"* — which
   is a materially different reason from `TFT002`'s. This is also the concrete
   form of the prior finding that the two rules' stated skip reasons are wrong
   on the `--from-bag` source. `TFT002` is unaffected: a value conflict does not
   abort either path.
