# 0010: Naming the record-size refusal — `IngestError::RecordTooLarge`

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** landed 2026-08-29 — `IngestError::RecordTooLarge { declared,
ceiling }`, `IngestOptions::max_record_bytes` (default
`DEFAULT_MAX_RECORD_BYTES` = 256 MiB, unchanged), `--max-record-size` at the CLI,
and `crates/tf_tree_ingest/tests/record_ceiling.rs`. `source.rs`'s
`ChunkPolicy` became `ReadPolicy` in the same change: a top-level record is not a
chunk, so the old name covered two of its three fields.

## Context

`source.rs` refused an MCAP record body over a private 256 MiB ceiling with
`IngestError::Mcap` ("not a well-formed recording"), so a sound recording with very
large records read as corrupt.

## Decision

Add `IngestError::RecordTooLarge`, beside `AllChunksOverLimit`: a record declares
more bytes than this reader will allocate for. It is **not corruption**, so it is
not `IngestError::Mcap`. Both ceiling arms return it. The variant stays `Copy` and
`String`-free (`docs/PROJECT.md` §5). The ceiling is an `IngestOptions` knob
(question 1).

## Consequences

`IngestError` is `#[non_exhaustive]`, so the variant is not breaking.
`AllChunksOverLimit` carries a `skipped` count and cannot be reused.

## Implementation plan

1. `RecordTooLarge` in `crates/tf_tree_ingest/src/lib.rs`.
2. Return it from both arms in `source::read_tf`.
3. A test that a header declaring `MAX_RECORD_BYTES + 1` yields `RecordTooLarge`,
   not `Mcap`; the mutant returning `Mcap` must fail it.
4. The `MAX_RECORD_BYTES` doc comment cites the variant.

## Open questions

1. **RESOLVED 2026-08-29: `MAX_RECORD_BYTES` becomes an `IngestOptions` knob.**
   `IngestOptions::max_record_bytes` defaults to `DEFAULT_MAX_RECORD_BYTES`;
   `--max-record-size` is the CLI spelling, its `default_value_t` derived from the
   constant.

2. **MEASURED 2026-08-29: nothing challenges 256 MiB** (largest record 1.2 MiB
   across 41 recordings; `docs/benchmarks/EVIDENCE.md`). Question 1 makes the
   residual risk a flag.
