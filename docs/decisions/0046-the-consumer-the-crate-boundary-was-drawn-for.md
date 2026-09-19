# 0046: the consumer the crate boundary was drawn for

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** (filled in as work lands)

## Context

`tf_tree_ingest` is a workspace member (`docs/PHASE5.md` §0.0 §3 row) because §4's
offline Python API needs it, but Python could not turn a recording into a `.tft`
without a Rust toolchain. Same pattern as
[`0044`](./0044-recovery-the-languages-a-robot-is-written-in-cannot-reach.md).

## Decision

**One new function, and no second way to spell it.**

```python
tf_tree.ingest_bag(
    path, /, *,
    static_topics=None, tf_topics=None, tf_prefix=None,
    max_memory_mb=None, max_record_bytes=None,
) -> Tree
```

It returns **an ordinary `Tree`** (as `open_file` does), so every method works
unchanged.

**The tree carries where it came from.** Read-only `Tree.source -> dict | None`:
`None` unless produced by `ingest_bag`; otherwise `path`, `digest` (BLAKE3 of the
recording, hex), `transforms`, `edges_without_samples`, `recording_start_ns`,
`recording_end_ns` (the recording's stamps, not `Tree.span(...)`).

**`Tree.freeze` writes that digest into `source_digest`** (zero otherwise).
**`Tree.source` is dropped inside `Tree.publisher()`**, the only mutation entry
point: a wrong digest is worse than an absent one.

**The dependency is unconditional**: no `ingest` Cargo feature, and no `shm`.
`just py-cross-check` passes no `--no-default-features`, so it already compiles
the ingest half for the three non-Linux targets.

## Rationale

**Not a `freeze_bag` binding**: `ingest_bag(p).freeze(out)` expresses it, and a
second spelling could differ silently in whether provenance is filled in.
**`max_record_bytes` is the one exposed `IngestOptions` guard**
([`0010`](./0010-naming-the-record-size-refusal.md)).

## Consequences

- `tf_tree_py` depends on a `publish = false` crate; `Tree.source` is state the
  Rust `Tree` does not carry.
- **Invariant:** every future mutation entry point on `PyTree` must drop `source`.
- §3's `freeze_from_arrays` is unaffected and still owed.

## Implementation plan

1. `tf_tree_py` takes the dependency; `digest_file` moves to the crate root.
2. `ingest_bag`, GIL released, `IngestError`s mapped to `OSError` as
   `offline::frozen_err` does. — `tests/python/test_ingest.py`.
3. `Tree.source` (digest equals the CLI's); `Tree.freeze` writes it and
   `Tree.publisher()` drops it — one test each.
4. `_core.pyi`, `python/tf_tree/__init__.py`, `docs/API.md` §6, `PHASE5.md` §0.0
   rows, README, CHANGELOG.
