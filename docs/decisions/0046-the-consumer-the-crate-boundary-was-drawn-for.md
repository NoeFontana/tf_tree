# 0046: the consumer the crate boundary was drawn for

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** (filled in as work lands)

## Context

`docs/PHASE5.md` §0.0's §3 row says `tf_tree_ingest` is a workspace member rather
than a CLI module **because §4's offline Python API needs the same logic and
cannot depend on a binary crate**. That consumer was never built:
`grep -c tf_tree_ingest crates/tf_tree_py/Cargo.toml` was `0`. Python could
`open_file` (the way in to an index somebody else built) and `Tree.freeze`, but
could not turn a recording into a `.tft`; the commands that do
(`tf_tree ingest --bag`, `tf_tree freeze --from-bag`) needed a Rust toolchain.
This is the fourth instance of the pattern
[`0044`](./0044-recovery-the-languages-a-robot-is-written-in-cannot-reach.md)
names (after `0038`, `0039`, `0044`): a capability implemented, tested,
documented and callable by nobody who needs it.

## Decision

**One new function, and no second way to spell it.**

```python
tf_tree.ingest_bag(
    path, /, *,
    static_topics=None, tf_topics=None, tf_prefix=None,
    max_memory_mb=None, max_record_bytes=None,
) -> Tree
```

It returns **an ordinary `Tree`** — the type `open_file` returns — so `plan`,
`at`, `span`, `frames`, `edges` and `freeze` work unchanged (§4.1's *"no parallel
offline API"* holds structurally).

**The tree carries where it came from.** A read-only property
`Tree.source -> dict | None`: `None` for every tree not produced by `ingest_bag`;
otherwise a `dict` with `path`, `digest` (BLAKE3 of the recording's bytes, hex),
`transforms`, `edges_without_samples`, `recording_start_ns`, `recording_end_ns`.

**`Tree.freeze` writes that digest into the container's `source_digest`** (it
passed all-zero unconditionally; zero is right for a hand-assembled tree).

**`Tree.source` is dropped the moment the tree can be written to**, i.e. inside
`Tree.publisher()`.

**The dependency is unconditional**: no `ingest` Cargo feature.

**`recording_*`, not `span_*`**: the stamps describe the *recording*, not the
interval the tree can answer (a ring retains what fits). `Tree.span(...)` is the
queryable interval; querying the report's upper stamp gave `ExtrapolationError`.

## Rationale

**Not a `freeze_bag` binding.** `tf_tree_ingest::tft::freeze_bag` streams the
*digest*, not the tree; `Tree::freeze_to` already takes `source_digest`, so the
composition `ingest_bag(p).freeze(out)` expresses it and the gap was only
`tf_tree_py`'s hardcoded zero. A top-level `freeze_bag` would be **a second
spelling** differing silently in whether provenance is filled in.

**Provenance on the tree, not a `freeze` argument.** It matters least when
writing and most six months later, so the spelling that requires the caller to
remember it produces unattributed indexes.

**Dropped in `publisher()`**, the only mutation entry point: ingest, add a
calibration edge, freeze would otherwise carry a digest asserting it is that
recording. **A wrong digest is worse than an absent one**: §2.3's field answers
*"was this index built from that file"*; a zero is the documented "no recording".

**`max_record_bytes` is exposed and other `IngestOptions` fields are not.**
`tf_prefix`, the two topic lists and `max_memory_mb` are what a real recording
needs; `on_clock_reset`, `on_bad_chunk` and the chunk-bomb ceilings are a single
supported value or a hostile-file guard. `max_record_bytes` is the guard
[`0010`](./0010-naming-the-record-size-refusal.md) added so that "the person who
meets it can raise it without forking the crate".

**The wheel does not split**: 548 129 → 726 396 bytes (+32.5 %) against `numpy`'s
29.3 MiB install; splitting would double a seven-row `wheels.yml` matrix.

**No `ingest` feature.** `just py-cross-check` passes no `--no-default-features`,
so it already compiles the ingest half for the three non-Linux targets — the only
proof `mcap`, `ruzstd` and `lz4_flex` cross-compile to macOS and Windows. A
feature no gate exercises in its off position is an unchecked configuration, and
nothing ships with it off. The `features = ["shm"]` went with it (it served
`tft::freeze_bag`, which this design does not call).

## Consequences

- `tf_tree_py` depends on a `publish = false` crate (as `tf_tree_cli` does).
- **`Tree.source` is state the Rust `Tree` does not carry**, and the C ABI gains none;
  a third binding should prompt asking whether `tf_tree::Tree` carries provenance.
- **Invariant:** every future mutation entry point on `PyTree` must drop `source`;
  the test asserts the *property*.
- **§3's `freeze_from_arrays` is unaffected and still owed.**
- `just py-cross-check` now cross-compiles `mcap` and the codecs.

## Implementation plan

1. **`tf_tree_py` takes the dependency**, no `shm`. `digest_file` moves from
   `tf_tree_ingest::tft` to the crate root; its `blake3` dependency stops being
   optional (`tf_tree_core` requires it, D14). — `cargo check -p tf_tree_ingest
   --no-default-features`, `just py-cross-check`.
2. **`ingest_bag`**, GIL released, errno-bearing `IngestError`s mapped to `OSError`
   as `offline::frozen_err` does. — `tests/python/test_ingest.py`.
3. **`Tree.source`**, populated only by `ingest_bag`; digest equals the CLI's own
   output.
4. **`Tree.freeze` writes the digest; `Tree.publisher()` drops it.** Two tests (freeze
   shows the digest; ingest → `publisher()` → freeze shows zeros), both failing at
   step 3's commit.
5. **`_core.pyi`, `python/tf_tree/__init__.py`, `docs/API.md` §6, `PHASE5.md` §0.0
   §3/§4 rows, README, CHANGELOG.** — `just py-lint`, `just py-test`,
   `just py-test-freethreaded`, `just artifact-versions`.

## Open questions

None.
