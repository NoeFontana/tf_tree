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

1. ~~**Should `MAX_RECORD_BYTES` become an `IngestOptions` knob at the same
   time?**~~ **RESOLVED 2026-08-29 by the owner: yes — the split is overturned.**
   The *Decision* section above still reads as written and is superseded by this
   line rather than rewritten, because what it argued was a *sequencing* claim —
   "take it when somebody has a recording that needs it" — and the counter-argument
   it acknowledged is the one that won: `ChunkLimits` made the opposite call for
   the same class of limit, and the code's own doc comment called the difference
   "a gap rather than a decision" rather than defending it.

   `IngestOptions::max_record_bytes` defaults to `DEFAULT_MAX_RECORD_BYTES`,
   which is 256 MiB — **the number does not move**, so no existing caller's
   behaviour changes; what changes is that the person who meets the ceiling can
   raise it without forking the crate. `--max-record-size` is the CLI spelling,
   with its `default_value_t` derived from the constant so the two cannot drift.

2. ~~**Is 256 MiB still the right number?**~~ **MEASURED 2026-08-29. Nothing in
   reach challenges it, and the attachment hypothesis this question rests on did
   not reproduce at all.**

   Corpus: the 41 recordings of
   [`DapengFeng/MCAP`](https://huggingface.co/datasets/DapengFeng/MCAP) — real
   published SLAM datasets (FAST-LIVO, R3LIVE, MARS-LVIG; HKU/HKUST campus, HK
   airport and island), 844 MiB to 9.5 GiB each, ~100 GiB in total.

   **Two measurements, because they answer different halves.**

   * **Full framing walk of three recordings** (2.8 GiB, 27 974 top-level
     records): the largest single record is **1.2 MiB, in every one of them, and
     it is always a `Chunk`** — 1.198, 1.204 and 1.208 MiB. That is **0.47% of
     the 256 MiB ceiling**, a 212× margin, and it confirms the "a chunk is
     typically 1–8 MiB" premise the number was chosen against, at the low end of
     that range.
   * **Footer-and-summary survey of all 41**, which is O(KB) per file rather than
     O(GB): read the footer for `summary_start`, then walk the summary for
     `AttachmentIndex` records, each of which carries its attachment's length.
     **41 of 41 carry zero attachments.**

   So the specific worry — "a recording with large attachments, which are also
   records" — **is not observable in this corpus at all**, and the ceiling is two
   orders of magnitude above anything measured.

   **What this does not establish, stated because the corpus has one
   provenance.** All 41 files come from one dataset collection and were, on the
   evidence of their identical ~1.2 MiB chunk targets, written by one conversion
   pipeline. A converter that does not emit attachments produces zero of them
   whatever the source contained, so this measures one writer's output rather
   than "robotics recordings" in general. It is evidence that 256 MiB is not
   *tight*, not proof that no producer exists that would meet it — and question 1
   is what makes that residual risk a flag rather than a fork.

   The tooling is two short scripts, not committed: a streaming framing walker
   and the summary survey, both validated against the `OneAttachment` case in
   `foxglove/mcap`'s conformance corpus, whose published ground truth they
   reproduce exactly. An earlier revision of the survey also read
   `Statistics.attachment_count` at a guessed offset and printed `0` where that
   ground truth says `1`; the guess was deleted rather than reported, and
   `AttachmentIndex` — which is self-describing — is what the numbers above come
   from.
