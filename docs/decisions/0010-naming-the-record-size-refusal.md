# 0010: Naming the record-size refusal — `IngestError::RecordTooLarge`

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** (filled in as work lands)

## Context

`crates/tf_tree_ingest/src/source.rs` bounds every MCAP record body it will
allocate for at a private constant, `MAX_RECORD_BYTES = 256 MiB`. A record whose
declared length exceeds it — or whose `u64` length does not fit a `usize` on a
32-bit host — is refused with `IngestError::Mcap`:

```rust
let Ok(want) = usize::try_from(declared) else {
    return Err(IngestError::Mcap);
};
if want > MAX_RECORD_BYTES {
    return Err(IngestError::Mcap);
}
```

`IngestError::Mcap` is documented as *"the file is not a well-formed MCAP
recording"*, and it is what a JPEG handed to `tf_tree ingest` produces. So the
two are indistinguishable to a caller: a sound recording written with very large
chunks, and a file that is not a recording at all.

**This crate has already decided this question once, one layer down, and decided
it the other way.** `ChunkLimits`' own doc says the two chunk ceilings live on
`IngestOptions` *"for one reason: the person who meets a limit is the person who
cannot patch the crate"*, and `IngestError::AllChunksOverLimit` exists precisely
so that a limit refusal is never reported as an empty or malformed recording —
its doc spends a paragraph on why a refusal with no knob named "sends the
operator to look for a publisher that was running the whole time". The record
ceiling is the same class of refusal and gets neither treatment.

**What forces the decision now rather than later.** Nothing has met the ceiling:
256 MiB is an order of magnitude above any real record, since recorders chunk at
1–8 MiB. That is exactly why it is cheap to fix now and why it is worth doing
before someone does meet it — at which point the diagnosis they get is "your file
is not an MCAP", about a file that is.

This came out of a five-lens audit of `tf_tree_ingest`; every other finding in
that pass was applied directly, and this is the one that needs a decision because
it adds a variant to a public error enum.

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

Both arms above return it. The variant stays `Copy` and `String`-free, as
`docs/PROJECT.md` §5 requires of every error in this workspace.

**`MAX_RECORD_BYTES` stays a private constant and does not become an
`IngestOptions` knob.** The variant names the refusal; it does not yet make it
adjustable. Promoting it to an option is a separate decision, and it should be
taken when somebody has a recording that needs it — at which point the number in
this error is the evidence.

## Rationale

Three alternatives were considered.

**Leave it as `IngestError::Mcap`.** This is the status quo, and it is what the
crate rejected one layer down for `AllChunksOverLimit`. The two conditions have
opposite remedies — one is "this is not the file you think it is", the other is
"this reader will not allocate that much" — and a shared variant cannot express
that.

**Make `MAX_RECORD_BYTES` an `IngestOptions` knob in the same change.** This is
the fuller fix and it is what `ChunkLimits` did. It is not taken here because it
is a bigger public surface (a new option field, a new CLI flag, a default to
defend) for a limit nobody has met. Naming the refusal is the half that costs
nothing and is a prerequisite for the other half anyway.

**Reuse `AllChunksOverLimit`.** It carries a `skipped` count and means "every
chunk was refused, so nothing was read". A single oversized record is neither a
count nor necessarily fatal to the rest of the file. Reusing it would make its
message false.

## Consequences

- `IngestError` is `#[non_exhaustive]`, so adding a variant is not a breaking
  change for downstream matchers.
- `tf_tree_cli` renders `IngestError` through `Display`; the new variant gets a
  message with a number in it and needs no CLI change.
- The `Described` join in `lib.rs` does not need a new arm — the variant names no
  frame, so it falls through to `other => write!(f, "{other}")`.
- One more error variant to keep honest. The two arms it replaces are the only
  places that construct it.
- It commits us to the position that a *limit refusal* and a *malformed file* are
  different errors in this crate. That position is already taken for chunks; this
  makes it consistent rather than new.

## Implementation plan

1. Add the `RecordTooLarge { declared: u64 }` variant to `IngestError` in
   `crates/tf_tree_ingest/src/lib.rs`, beside `AllChunksOverLimit` — verified by
   `cargo check -p tf_tree_ingest --all-targets`.
2. Return it from both arms in `source::read_tf` — verified by `just lint`
   (the `Described` match is non-exhaustive-safe, and clippy is `-D warnings`).
3. Add `tests/ingest.rs::a_record_past_the_ceiling_is_named_not_called_corruption`,
   which hand-rolls a record header declaring `MAX_RECORD_BYTES + 1` and asserts
   `IngestError::RecordTooLarge { declared }` rather than `IngestError::Mcap` —
   verified by `cargo nextest run -p tf_tree_ingest`, and by the mutant of
   returning `Mcap` from the ceiling arm, which must fail it.
4. Update the `MAX_RECORD_BYTES` doc comment to cite the new variant instead of
   recording the gap — verified by `cargo test --doc -p tf_tree_ingest`.

## Open questions

1. **Should `MAX_RECORD_BYTES` become an `IngestOptions` knob at the same time?**
   The *Decision* above says no and gives the reason, but the counter-argument is
   real: `ChunkLimits` made exactly the opposite call for the same class of
   limit, and a user who meets this ceiling still cannot get past it. Resolving
   this either confirms the split or folds step 1 into a larger change.
2. **Is 256 MiB still the right number?** It was chosen against "a chunk is
   typically 1–8 MiB". Nothing has re-measured it against a recording with large
   attachments, which are also records.
