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

`crates/tf_tree_ingest/src/source.rs` bounded every MCAP record body at a private
`MAX_RECORD_BYTES = 256 MiB` and refused a larger (or, on a 32-bit host,
non-`usize`) declared length with `IngestError::Mcap`, documented as *"the file is
not a well-formed MCAP recording"* — what a JPEG produces. A sound recording with
very large records was indistinguishable from a non-recording. The crate had
already decided this the other way one layer down: `ChunkLimits` lives on
`IngestOptions` because *"the person who meets a limit is the person who cannot
patch the crate"*, and `AllChunksOverLimit` exists so a limit refusal is never
reported as a malformed recording.

## Decision

Add one variant to `IngestError`, beside `AllChunksOverLimit`:

```rust
/// A record declares more bytes than this reader will allocate for.
///
/// **Not corruption**, and that is the whole reason it is not
/// [`IngestError::Mcap`]. A recording written with very large records is
/// sound; this is the same class of refusal as
/// [`IngestError::AllChunksOverLimit`], named so the operator is not sent
/// hunting for damage in a file that has none.
#[error("a record declares {declared} bytes, past this reader's ceiling")]
RecordTooLarge {
    /// The length the record header declared.
    declared: u64,
},
```

Both ceiling arms return it. The variant stays `Copy` and `String`-free
(`docs/PROJECT.md` §5). **Superseded by question 1 below:** the ceiling is an
`IngestOptions` knob, not a private constant.

## Rationale

**Not leaving `Mcap`:** the two conditions have opposite remedies. **Not
`AllChunksOverLimit`:** it carries a `skipped` count and means nothing was read;
reusing it would make its message false.

## Consequences

- `IngestError` is `#[non_exhaustive]`, so the variant is not breaking. The CLI
  renders through `Display`, and the `Described` join in `lib.rs` falls through to
  `other => write!(f, "{other}")`.
- A *limit refusal* and a *malformed file* are different errors in this crate,
  consistently with chunks.

## Implementation plan

1. `RecordTooLarge` in `crates/tf_tree_ingest/src/lib.rs` — `cargo check -p
   tf_tree_ingest --all-targets`.
2. Return it from both arms in `source::read_tf` — `just lint`.
3. A test that a record header declaring `MAX_RECORD_BYTES + 1` yields
   `RecordTooLarge { declared }` rather than `Mcap`; the mutant returning `Mcap`
   must fail it — `cargo nextest run -p tf_tree_ingest`.
4. The `MAX_RECORD_BYTES` doc comment cites the variant.

## Open questions

1. **RESOLVED 2026-08-29 by the owner: `MAX_RECORD_BYTES` becomes an
   `IngestOptions` knob.** `IngestOptions::max_record_bytes` defaults to
   `DEFAULT_MAX_RECORD_BYTES` (256 MiB — the number does not move);
   `--max-record-size` is the CLI spelling, its `default_value_t` derived from the
   constant.

2. **MEASURED 2026-08-29: nothing challenges 256 MiB.** Corpus: the 41 recordings
   of [`DapengFeng/MCAP`](https://huggingface.co/datasets/DapengFeng/MCAP) (real
   SLAM datasets, 844 MiB–9.5 GiB, ~100 GiB total).
   * **Full framing walk of three recordings** (2.8 GiB, 27 974 top-level
     records): the largest record is **1.2 MiB in every one, always a `Chunk`** —
     0.47% of the ceiling, a 212× margin.
   * **Footer-and-summary survey of all 41** (`AttachmentIndex` lengths): **41 of
     41 carry zero attachments.**

   All 41 come from one collection and one conversion pipeline, so this is
   evidence 256 MiB is not *tight*, not proof no producer would meet it; question
   1 makes that residual risk a flag rather than a fork. The two scripts are not
   committed; both reproduce `foxglove/mcap`'s `OneAttachment` ground truth.
