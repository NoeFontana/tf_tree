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
