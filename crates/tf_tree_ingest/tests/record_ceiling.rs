//! `IngestOptions::max_record_bytes` — `docs/decisions/0010` question 1.
//!
//! The knob moves, not the file: the same file must be refused at a low
//! ceiling and accepted at the default.

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

    // 32 bytes is under any record MCAP can legally write.
    let low = IngestOptions {
        max_record_bytes: 32,
        ..Default::default()
    };
    let mut frames = Frames::default();
    let refused = tf_tree_ingest::survey(&path, &low, &mut frames);
    // The variant, not `Mcap`: `0010` gives the refusal a name and the number.
    let Err(IngestError::RecordTooLarge { declared, ceiling }) = refused else {
        panic!("a 32-byte ceiling did not report RecordTooLarge: {refused:?}");
    };
    assert_eq!(ceiling, 32, "the error names a ceiling nobody set");
    assert!(
        declared > ceiling,
        "a record was refused for being {declared} bytes against a {ceiling}-byte ceiling"
    );

    // The identical file at the default must go through.
    let mut frames = Frames::default();
    let accepted = tf_tree_ingest::survey(&path, &IngestOptions::default(), &mut frames);
    assert!(
        accepted.is_ok(),
        "the same file was refused at the default ceiling: {accepted:?}"
    );
}

/// The default is the library constant, and the constant is the documented one.
#[test]
fn the_default_ceiling_is_the_published_constant() {
    assert_eq!(
        IngestOptions::default().max_record_bytes,
        DEFAULT_MAX_RECORD_BYTES
    );
    assert_eq!(DEFAULT_MAX_RECORD_BYTES, 256 * 1024 * 1024);
}

/// A ceiling above `usize::MAX` on a 32-bit target must not wrap into a tiny
/// one. The comparison is in `u64`; on 64-bit hosts this covers only the
/// ordinary path.
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
// The ceiling bounds what this reader allocates, so the opcode is consulted
// before the length decides: an unneeded oversized record is skipped, a needed
// one refuses, and a skip's landing point and report are checked (arms 1-5).
// ---------------------------------------------------------------------------

/// MCAP top-level record opcodes, from the specification rather than from
/// `mcap::records::op`.
const OP_HEADER: u8 = 0x01;
const OP_CHUNK: u8 = 0x06;
const OP_ATTACHMENT: u8 = 0x09;

/// Admits every record `write_mcap` produces (chunks are 4 KiB) and refuses the
/// spliced one below.
const CEILING: u64 = 16 * 1024;

/// Four times the ceiling.
const OVERSIZED: usize = 64 * 1024;

/// Splice a top-level record with `opcode` and a `len`-byte filler body
/// directly after the recording's `Header`. The body is never parsed; the
/// splice invalidates summary offsets, which the linear `read_tf` walk ignores.
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

/// Arm 1: an oversized attachment is stepped over, counted, and costs the
/// recording nothing.
#[test]
fn an_oversized_record_the_reader_does_not_need_is_skipped() {
    let dir = Scratch::new("attach");
    let plain = fixture(&dir.0);
    let opts = IngestOptions {
        max_record_bytes: CEILING,
        ..Default::default()
    };

    // Control: the ceiling admits every other record in this file.
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

/// Arm 2: an oversized record the reader does need still refuses (skipping a
/// `Chunk` would silently lose its transforms).
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
/// truncation, not a clean end (`TruncatedBeforeAnyChunk`, not `NoTransforms`).
#[test]
fn a_skipped_record_cut_short_reports_truncation_not_a_clean_end() {
    let dir = Scratch::new("cut");
    let plain = fixture(&dir.0);
    let spliced = splice_record(&std::fs::read(&plain).unwrap(), OP_ATTACHMENT, OVERSIZED);
    // Cut inside the attachment's body.
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

/// MCAP's `Metadata` opcode: a real record kind this reader does not read.
const OP_METADATA: u8 = 0x0C;

/// A recording long enough to hold several chunks; callers assert they found
/// enough offsets to aim between.
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
/// everything between, and the report must not claim completeness.
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
    // Nearest later chunk boundary further away than the ceiling.
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

    // The summary must not claim completeness.
    let summary = out.report.summary();
    assert!(
        !summary.contains("no transform was lost"),
        "the report claimed a completeness it cannot check: {summary}"
    );
    assert!(
        summary.contains("--max-record-size"),
        "the flag that would have read the span must be named: {summary}"
    );
    // The JSON carries the same fact as a count.
    let json = out.report.to_json();
    assert!(
        json.contains("\"oversized_records_skipped\":1"),
        "the skip is not in the JSON at all: {json}"
    );
}

/// Arm 5: a skip that lands off a boundary refuses. Extra `0xAB` filler makes
/// the landing a deterministic non-header.
#[test]
fn a_skip_that_lands_off_a_boundary_refuses_rather_than_resyncing() {
    let dir = Scratch::new("resync");
    let plain = fixture(&dir.0);
    let bytes = std::fs::read(&plain).unwrap();
    // Sixteen extra filler bytes: the skip stops sixteen short of the next record.
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

// ---------------------------------------------------------------------------
// The file bounds the allocation too: the record buffer is clamped by
// `file_len`.
// ---------------------------------------------------------------------------

/// Magic, then a record header and nothing else; written by hand because the
/// writer would never emit it.
fn magic_and_one_header(opcode: u8, declared: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(17);
    out.extend_from_slice(b"\x89MCAP0\r\n");
    out.push(opcode);
    out.extend_from_slice(&declared.to_le_bytes());
    out
}

/// A record header declaring more than `isize::MAX` bytes must not reach the
/// allocator (`Vec::reserve_exact` would panic).
#[test]
fn a_declared_length_past_the_address_space_is_truncation_not_a_panic() {
    let dir = Scratch::new("hostile_wide");
    let path = dir.0.join("tiny.mcap");
    std::fs::write(&path, magic_and_one_header(OP_CHUNK, 1u64 << 63)).unwrap();

    // The ceiling that admits it; the default would refuse first.
    let wide = IngestOptions {
        max_record_bytes: u64::MAX,
        ..Default::default()
    };
    let mut frames = Frames::default();
    assert_eq!(
        tf_tree_ingest::survey(&path, &wide, &mut frames).unwrap_err(),
        IngestError::TruncatedBeforeAnyChunk,
        "a record body that is not in the file is a truncated recording"
    );
}

/// The same file at the default ceiling. It passes against the defect too: it
/// holds that the diagnosis stays truncation; the arm above fails without the
/// clamp.
#[test]
fn a_declared_length_larger_than_the_file_is_truncation() {
    let dir = Scratch::new("hostile_default");
    let path = dir.0.join("tiny.mcap");
    std::fs::write(
        &path,
        magic_and_one_header(OP_CHUNK, DEFAULT_MAX_RECORD_BYTES),
    )
    .unwrap();

    let mut frames = Frames::default();
    assert_eq!(
        tf_tree_ingest::survey(&path, &IngestOptions::default(), &mut frames).unwrap_err(),
        IngestError::TruncatedBeforeAnyChunk
    );
}
