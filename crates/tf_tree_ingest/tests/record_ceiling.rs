//! `IngestOptions::max_record_bytes` — `docs/decisions/0010` question 1.
//!
//! The ceiling on one top-level MCAP record was a private constant until
//! 2026-08-29, and `source.rs`'s own doc comment called that "**a gap rather
//! than a decision**". `0010` asked whether it should become a knob; it now is
//! one, and this file is what makes that a fact rather than a field nobody
//! reads.
//!
//! **The knob moves, not the file.** The alternative — writing a recording with
//! a genuinely oversized record — would need a multi-hundred-megabyte fixture to
//! exercise the default, which is a slow test that measures the disk. Lowering
//! the ceiling under a fixture the reader otherwise accepts isolates exactly the
//! comparison the knob controls, and the pair of assertions below is what makes
//! it non-vacuous: the *same file* must be refused at a low ceiling and accepted
//! at the default. A test with only the first half would pass against a reader
//! that refused everything.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use tf_tree_ingest::fixture::{write_mcap, FixtureMessage};
use tf_tree_ingest::{Frames, IngestError, IngestOptions, DEFAULT_MAX_RECORD_BYTES};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let dir =
            std::env::temp_dir().join(format!("tf_tree_ingest_ceil_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A small, entirely ordinary recording: two frames, a handful of samples.
fn fixture(dir: &Path) -> PathBuf {
    let path = dir.join("ceiling.mcap");
    let msgs: Vec<FixtureMessage> = (0..8)
        .map(|i| {
            FixtureMessage::dynamic(
                "map",
                "base",
                1_000_000 * i64::from(i + 1),
                [f64::from(i) * 0.01, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0],
            )
        })
        .collect();
    write_mcap(&path, &msgs).expect("write the fixture");
    path
}

/// The knob governs, in both directions, on one file.
#[test]
fn the_record_ceiling_is_a_knob_and_the_same_file_turns_on_it() {
    let dir = Scratch::new("knob");
    let path = fixture(&dir.0);

    // A ceiling below the file's own magic-and-header framing refuses it. 32
    // bytes is under any record MCAP can legally write, so this is the reader's
    // bound firing and not a property of the fixture's contents.
    let low = IngestOptions {
        max_record_bytes: 32,
        ..Default::default()
    };
    let mut frames = Frames::default();
    let refused = tf_tree_ingest::survey(&path, &low, &mut frames);
    // **The variant, not `Mcap`** — `0010`'s Decision is that the refusal gets a
    // name and carries the number to raise. Asserting on `Mcap` here would have
    // passed against the reader as it stood before this change.
    let Err(IngestError::RecordTooLarge { declared, ceiling }) = refused else {
        panic!("a 32-byte ceiling did not report RecordTooLarge: {refused:?}");
    };
    assert_eq!(ceiling, 32, "the error names a ceiling nobody set");
    assert!(
        declared > ceiling,
        "a record was refused for being {declared} bytes against a {ceiling}-byte ceiling"
    );

    // **The half that makes the first one mean something.** The identical file,
    // at the shipped default, must go through — otherwise the assertion above
    // would hold against a reader that refused every recording.
    let mut frames = Frames::default();
    let accepted = tf_tree_ingest::survey(&path, &IngestOptions::default(), &mut frames);
    assert!(
        accepted.is_ok(),
        "the same file was refused at the default ceiling: {accepted:?}"
    );
}

/// The default is the library constant, and the constant is the documented one.
///
/// Cheap, and it is the assertion that would have caught the CLI's `--max-record-size`
/// default drifting from `DEFAULT_MAX_RECORD_BYTES` — the drift `--max-chunk-size`
/// avoids by deriving its `default_value_t` from the constant rather than writing
/// the number twice.
#[test]
fn the_default_ceiling_is_the_published_constant() {
    assert_eq!(
        IngestOptions::default().max_record_bytes,
        DEFAULT_MAX_RECORD_BYTES
    );
    assert_eq!(DEFAULT_MAX_RECORD_BYTES, 256 * 1024 * 1024);
}

/// A ceiling above `usize::MAX` on a 32-bit target must not wrap into a tiny one.
///
/// The comparison is made in `u64`, before the `usize` narrowing, precisely so
/// that a caller who sets a very large ceiling gets a very large ceiling. On a
/// 64-bit host this asserts the ordinary path still admits the fixture; the
/// narrowing it guards is only reachable on a 32-bit target, which no CI row
/// builds, so the test states what it does and does not cover rather than
/// implying more.
#[test]
fn a_very_large_ceiling_does_not_wrap() {
    let dir = Scratch::new("wide");
    let path = fixture(&dir.0);
    let wide = IngestOptions {
        max_record_bytes: u64::MAX,
        ..Default::default()
    };
    let mut frames = Frames::default();
    assert!(
        tf_tree_ingest::survey(&path, &wide, &mut frames).is_ok(),
        "u64::MAX as a ceiling refused an ordinary recording, which means it \
         wrapped rather than widened"
    );
}

// ---------------------------------------------------------------------------
// The ceiling is a bound on what this reader **allocates**, so the opcode has to
// be consulted before the length decides.
//
// Until 2026-09-05 it was not: `read_tf` compared `declared` against the ceiling
// and returned before it had looked at `header[0]`, so a record the reader never
// reads — an attachment, a metadata block — aborted an ingest whose transforms
// were all intact. `DEFAULT_MAX_RECORD_BYTES`' own doc comment named the defect
// ("the reader has no opcode-based skip") and `docs/decisions/0010`'s second open
// question was measured against a corpus in which 41 of 41 recordings carry zero
// attachments — so the case was known, unreproduced, and unhandled.
//
// Each arm below is red-tested on its own mutant, because a single "an attachment
// is skipped" test passes against a reader that skips every oversized record
// including a `Chunk` — and because a skip is a seek onto an offset nothing
// validated, which is a second question with two answers of its own: where the
// length lands, and what the report may claim about the span it stepped over.
// ---------------------------------------------------------------------------

/// MCAP top-level record opcodes, from the specification rather than from
/// `mcap::records::op` — `mcap` is not a dev-dependency here, and a test that
/// asserts against the same constant the reader matches on is asserting nothing
/// about the format.
const OP_HEADER: u8 = 0x01;
const OP_CHUNK: u8 = 0x06;
const OP_ATTACHMENT: u8 = 0x09;

/// A ceiling that admits every record `write_mcap` produces and refuses the
/// spliced one below. The fixture writer chunks at `FIXTURE_CHUNK_SIZE` (4 KiB),
/// so 16 KiB clears every chunk with room to spare — and
/// `an_oversized_record_the_reader_does_not_need_is_skipped` asserts the
/// unspliced file at this very ceiling, so "the ceiling admits the rest" is a
/// measured control rather than an assumption.
const CEILING: u64 = 16 * 1024;

/// Four times the ceiling: unambiguously over, and small enough that the fixture
/// stays a few tens of kilobytes.
const OVERSIZED: usize = 64 * 1024;

/// Splice a top-level record with `opcode` and a `len`-byte filler body in
/// directly after the recording's `Header`.
///
/// **The body is filler and is never parsed**, which is the point: the reader
/// steps over a record it does not need without handing the bytes to
/// `mcap::parse_record`. Raising the ceiling above `len` in a future test would
/// therefore make this file malformed, and deliberately so — the skip is what is
/// under test.
///
/// The splice invalidates the summary section's byte offsets. That is harmless
/// here and worth saying out loud: `read_tf` is a linear record walk and reads no
/// index, so a `ChunkIndex` pointing at the wrong offset is parsed and dropped
/// like every other record this reader does not need.
fn splice_record(bytes: &[u8], opcode: u8, len: usize) -> Vec<u8> {
    // magic(8), then every record as opcode(1) + len(8, LE) + body.
    assert_eq!(
        bytes[8], OP_HEADER,
        "the first record after the file magic must be Header"
    );
    let header_len = u64::from_le_bytes(bytes[9..17].try_into().unwrap()) as usize;
    let at = 17 + header_len;
    let mut out = Vec::with_capacity(bytes.len() + 9 + len);
    out.extend_from_slice(&bytes[..at]);
    out.push(opcode);
    out.extend_from_slice(&u64::try_from(len).unwrap().to_le_bytes());
    out.resize(out.len() + len, 0xAB);
    out.extend_from_slice(&bytes[at..]);
    out
}

/// Arm 1: an oversized **attachment** is stepped over, counted, and costs the
/// recording nothing.
///
/// Mutant: restore the old ordering by moving the `reader_needs(opcode)` test
/// after the ceiling comparison — applied as `if !fits { return Err(...) }`, and
/// this failed with `RecordTooLarge { declared: 65536, ceiling: 16384 }` on a
/// recording whose every transform was intact.
#[test]
fn an_oversized_record_the_reader_does_not_need_is_skipped() {
    let dir = Scratch::new("attach");
    let plain = fixture(&dir.0);
    let opts = IngestOptions {
        max_record_bytes: CEILING,
        ..Default::default()
    };

    // **The control.** Without it the assertion below would hold against a reader
    // that skipped everything, and "the ceiling admits every other record in this
    // file" would be an assumption rather than a measurement.
    let mut frames = Frames::default();
    let clean = tf_tree_ingest::run(&plain, &opts, &mut frames).expect("the unspliced file");
    assert_eq!(clean.report.anomalies.oversized_records_skipped, 0);

    let spliced = splice_record(&std::fs::read(&plain).unwrap(), OP_ATTACHMENT, OVERSIZED);
    let path = dir.0.join("attachment.mcap");
    std::fs::write(&path, &spliced).unwrap();

    let mut frames = Frames::default();
    let out = tf_tree_ingest::run(&path, &opts, &mut frames)
        .expect("an attachment this reader never reads must not abort the ingest");
    assert_eq!(
        out.report.anomalies.oversized_records_skipped, 1,
        "the skip is counted, because it is data this run declined to look at"
    );
    // Nothing about the transform stream moved.
    assert_eq!(
        out.report.transforms_read, clean.report.transforms_read,
        "the spliced file's transforms must be the unspliced file's"
    );
    assert_eq!(out.report.samples_pushed, clean.report.samples_pushed);
    assert!(
        !out.report.anomalies.truncated,
        "a complete file with a skipped record is not truncated"
    );
    assert!(
        out.report.summary().contains("--max-record-size"),
        "the flag that would have read it must be named: {}",
        out.report.summary()
    );
}

/// Arm 2: an oversized record the reader **does** need still refuses.
///
/// This is the arm a single "attachments are skipped" test would have hidden.
/// Skipping a `Chunk` loses every transform inside it and reads, downstream,
/// exactly like a recording that never had them — which is worse than a named
/// error carrying the number to raise.
///
/// Mutant: widen the skip to every opcode (`if !fits { skip }` with no
/// `reader_needs` test) — applied, and this failed: the ingest returned
/// `Ok` with the spliced chunk silently gone.
#[test]
fn an_oversized_record_the_reader_needs_still_refuses() {
    let dir = Scratch::new("chunk");
    let plain = fixture(&dir.0);
    let spliced = splice_record(&std::fs::read(&plain).unwrap(), OP_CHUNK, OVERSIZED);
    let path = dir.0.join("chunk.mcap");
    std::fs::write(&path, &spliced).unwrap();

    let opts = IngestOptions {
        max_record_bytes: CEILING,
        ..Default::default()
    };
    let mut frames = Frames::default();
    let refused = tf_tree_ingest::survey(&path, &opts, &mut frames);
    let Err(IngestError::RecordTooLarge { declared, ceiling }) = refused else {
        panic!("an oversized chunk must be refused, not skipped: {refused:?}");
    };
    assert_eq!(
        (declared, ceiling),
        (OVERSIZED as u64, CEILING),
        "the error carries the number to pass to --max-record-size"
    );
}

/// Arm 3: a skipped record whose body runs past the end of the file is
/// **truncation**, not a clean end.
///
/// Seeking past the end of a file succeeds, so a skip that only seeks would leave
/// the cursor past the end, the next header read would return zero bytes, and the
/// walk would take that for the ordinary end of a recording. The file's length is
/// read once at open for exactly this comparison.
///
/// The two outcomes are different error variants, which is what makes the
/// assertion sharp: `TruncatedBeforeAnyChunk` means "the file stops early",
/// `NoTransforms` means "the file is whole and has no TF in it", and those send an
/// operator to opposite places.
///
/// Mutant: drop the `file_len` comparison from `skip_body` and always report the
/// body complete — applied, and this failed with
/// `IngestError::NoTransforms`, reporting an amputated recording as an intact one.
#[test]
fn a_skipped_record_cut_short_reports_truncation_not_a_clean_end() {
    let dir = Scratch::new("cut");
    let plain = fixture(&dir.0);
    let spliced = splice_record(&std::fs::read(&plain).unwrap(), OP_ATTACHMENT, OVERSIZED);
    // Cut inside the attachment's body: the record header is whole and declares
    // `OVERSIZED` bytes, and only a hundred of them exist.
    let header_len = u64::from_le_bytes(spliced[9..17].try_into().unwrap()) as usize;
    let cut = 17 + header_len + 9 + 100;
    let path = dir.0.join("cut.mcap");
    std::fs::write(&path, &spliced[..cut]).unwrap();

    let opts = IngestOptions {
        max_record_bytes: CEILING,
        ..Default::default()
    };
    let mut frames = Frames::default();
    assert_eq!(
        tf_tree_ingest::survey(&path, &opts, &mut frames).unwrap_err(),
        IngestError::TruncatedBeforeAnyChunk,
        "a file that stops inside a skipped record has stopped early"
    );
}

/// MCAP's `Metadata` opcode. A real record kind this reader does not read, which
/// is what makes the probe below a *plausible* corrupt header rather than a
/// contrived one.
const OP_METADATA: u8 = 0x0C;

/// A recording long enough to hold several chunks, so that a skip can be aimed to
/// swallow some of them.
///
/// `write_mcap` chunks at `FIXTURE_CHUNK_SIZE`, so how many chunks this produces
/// is the fixture writer's business and is deliberately not written down here:
/// the caller walks the file for the real offsets and asserts only that it found
/// enough of them to aim between, which is what would fail — loudly, and before
/// the probe — if the writer's chunking ever changed.
fn chunky_fixture(dir: &Path) -> PathBuf {
    let path = dir.join("chunky.mcap");
    let msgs: Vec<FixtureMessage> = (0..512)
        .map(|i| {
            FixtureMessage::dynamic(
                "map",
                "base",
                1_000_000 * i64::from(i + 1),
                [f64::from(i) * 0.01, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0],
            )
        })
        .collect();
    write_mcap(&path, &msgs).expect("write the fixture");
    path
}

/// Byte offsets of the top-level records with this opcode, walking the framing
/// the way the reader does: magic(8), then opcode(1) + len(8, LE) + body.
fn record_offsets(bytes: &[u8], opcode: u8) -> Vec<usize> {
    let mut out = Vec::new();
    let mut at = 8;
    while at + 9 <= bytes.len() {
        let len = u64::from_le_bytes(bytes[at + 1..at + 9].try_into().unwrap()) as usize;
        let Some(end) = at.checked_add(9).and_then(|h| h.checked_add(len)) else {
            break;
        };
        if end > bytes.len() {
            break;
        }
        if bytes[at] == opcode {
            out.push(at);
        }
        at = end;
    }
    out
}

/// Splice only a 9-byte record header at `at`, declaring `declared` body bytes it
/// does not own — the shape a corrupt length has on disk.
fn splice_header_at(bytes: &[u8], at: usize, opcode: u8, declared: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 9);
    out.extend_from_slice(&bytes[..at]);
    out.push(opcode);
    out.extend_from_slice(&declared.to_le_bytes());
    out.extend_from_slice(&bytes[at..]);
    out
}

/// Arm 4: a skip whose declared length lands on a later record boundary swallows
/// everything between, and the report must not say otherwise.
///
/// **This arm exists because the shipped summary row said "no transform was
/// lost", and here transforms are.** A record body is stepped over on the length
/// written in its own header; nothing validates that length; and a corrupt one
/// aimed at a later record's first byte resyncs perfectly. `resyncs_here` cannot
/// see it — the position it checks *is* a record boundary — so what this arm pins
/// is the honesty of the report rather than the absence of the loss, which a
/// linear walk cannot deliver.
///
/// Mutant: restore the withdrawn wording (append "; no transform was lost" to
/// `report.rs`'s row) — applied, and this failed on the summary assertion.
#[test]
fn a_skip_that_lands_on_a_later_boundary_loses_transforms_and_says_so() {
    let dir = Scratch::new("boundary");
    let plain = chunky_fixture(&dir.0);
    let bytes = std::fs::read(&plain).unwrap();
    let chunks = record_offsets(&bytes, OP_CHUNK);
    assert!(
        chunks.len() >= 4,
        "the probe needs chunks to aim between, found {}",
        chunks.len()
    );
    // The nearest later chunk boundary that is further away than the ceiling —
    // the splice has to be *over* the ceiling to be skipped at all, and aiming at
    // a boundary rather than a byte count is what makes the resync check pass.
    let land = chunks
        .iter()
        .position(|&c| c > chunks[1] && (c - chunks[1]) as u64 > CEILING)
        .expect("no pair of chunk offsets is further apart than the ceiling");
    let declared = (chunks[land] - chunks[1]) as u64;
    let spliced = splice_header_at(&bytes, chunks[1], OP_METADATA, declared);
    let path = dir.0.join("boundary.mcap");
    std::fs::write(&path, &spliced).unwrap();

    let opts = IngestOptions {
        max_record_bytes: CEILING,
        ..Default::default()
    };
    let mut frames = Frames::default();
    let clean = tf_tree_ingest::run(&plain, &opts, &mut frames).expect("the unspliced file");
    let mut frames = Frames::default();
    let out = tf_tree_ingest::run(&path, &opts, &mut frames)
        .expect("the skip resyncs on a real boundary, so the walk carries on");

    // The loss is real, and the file reads as clean apart from the skip row.
    assert!(
        out.report.transforms_read < clean.report.transforms_read,
        "the probe is meant to swallow transforms; it swallowed none, so it is \
         testing nothing ({} of {})",
        out.report.transforms_read,
        clean.report.transforms_read
    );
    assert_eq!(out.report.anomalies.oversized_records_skipped, 1);
    assert!(
        !out.report.anomalies.truncated,
        "nothing about this file is short — that is what makes the loss quiet"
    );

    // So the one thing the operator is shown must not claim completeness.
    let summary = out.report.summary();
    assert!(
        !summary.contains("no transform was lost"),
        "the report claimed a completeness it cannot check: {summary}"
    );
    assert!(
        summary.contains("--max-record-size"),
        "the flag that would have read the span must be named: {summary}"
    );
    // The machine-readable path carries the same fact as a count. (Asserting the
    // *absence* of the withdrawn sentence here would be a check that cannot fail
    // — `to_json` renders no prose.)
    let json = out.report.to_json();
    assert!(
        json.contains("\"oversized_records_skipped\":1"),
        "the skip is not in the JSON at all: {json}"
    );
}

/// Arm 5: a skip that lands anywhere else refuses, as the ceiling did before the
/// opcode was consulted at all.
///
/// The record is spliced with more filler than it declares, so the position the
/// skip resyncs at is a run of `0xAB` — a deterministic non-header, rather than
/// whatever compressed bytes happened to be at an offset. Without the resync
/// check the walk reads `0xAB` as an opcode with a sixteen-exabyte length, skips
/// again, runs off the end and reports the file as truncated: a corrupt file
/// diagnosed as a short one, pointing the operator at the recorder instead of the
/// bytes.
///
/// Mutant: delete the `resyncs_here` call from the skip branch — applied, and
/// this failed with `TruncatedBeforeAnyChunk` in place of the refusal.
#[test]
fn a_skip_that_lands_off_a_boundary_refuses_rather_than_resyncing() {
    let dir = Scratch::new("resync");
    let plain = fixture(&dir.0);
    let bytes = std::fs::read(&plain).unwrap();
    // `splice_record` writes `len` filler bytes and declares `len`; sixteen of
    // those are then extra, so the skip stops sixteen bytes short of the record
    // that follows.
    let mut spliced = splice_record(&bytes, OP_METADATA, OVERSIZED);
    let header_len = u64::from_le_bytes(spliced[9..17].try_into().unwrap()) as usize;
    let filler_end = 17 + header_len + 9 + OVERSIZED;
    spliced.splice(filler_end..filler_end, std::iter::repeat_n(0xABu8, 16));
    let path = dir.0.join("resync.mcap");
    std::fs::write(&path, &spliced).unwrap();

    let opts = IngestOptions {
        max_record_bytes: CEILING,
        ..Default::default()
    };
    let mut frames = Frames::default();
    let refused = tf_tree_ingest::survey(&path, &opts, &mut frames);
    let Err(IngestError::RecordTooLarge { declared, ceiling }) = refused else {
        panic!("a skip that does not resync must refuse: {refused:?}");
    };
    assert_eq!(
        (declared, ceiling),
        (OVERSIZED as u64, CEILING),
        "the refusal names the record that was stepped over, not the garbage it \
         landed in"
    );
}
