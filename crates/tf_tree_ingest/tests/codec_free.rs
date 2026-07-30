//! The `--no-default-features` build: what a reader with no codecs does.
//!
//! # Why this is a file of its own
//!
//! `compression` is **on by default**, so `cargo nextest run --workspace` compiles
//! exactly one configuration and the codec-free build would be compiled by nothing.
//! That is the shape of four defects this repository has already shipped — a
//! default-off feature is invisible to `--workspace`, and a file nobody compiles is
//! not a checked file — so `just ingest-check` runs
//! `cargo nextest run -p tf_tree_ingest --no-default-features`, and this is what it
//! finds there.
//!
//! Everything here is `#![cfg(not(feature = "compression"))]`, which means it
//! compiles to nothing in the ordinary build. That is deliberate: the assertions
//! below are *false* with codecs compiled in, because a zstd chunk then ingests.
//!
//! What must hold without them is narrow and specific. A compressed chunk is
//! [`IngestError::CompressedChunk`] naming the codec — **not** a bad chunk, not
//! truncation, and not a skip — because every chunk in a recording uses the same
//! codec, so skipping them all yields "no transforms" about a file that is
//! perfectly intact and needs one `mcap compress` command.

#![cfg(not(feature = "compression"))]
#![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]

use tf_tree_ingest::fixture::{write_mcap_chunked, ChunkDamage, ChunkedSpec, FixtureMessage};
use tf_tree_ingest::{ChunkCodec, Frames, IngestError, IngestOptions, OnBadChunk};

/// Nine messages in three chunks, so there is a survivor either side of the chunk
/// the fixture relabels.
fn corpus() -> Vec<FixtureMessage> {
    (0..9)
        .map(|i| {
            let k = f64::from(i) + 1.0;
            FixtureMessage::dynamic(
                "base_link",
                "sensor",
                1_000_000_000 + i64::from(i) * 10_000_000,
                [k.cos(), k.sin(), 0.0, 0.0, k, k * 2.0, k * 3.0],
            )
        })
        .collect()
}

/// **Without the `compression` feature a zstd chunk is `CompressedChunk`, under
/// either bad-chunk policy.**
///
/// The policy is asserted both ways because the interesting failure is the *skip*:
/// a build that treated a missing decoder as damage would, on a real recording
/// where every chunk is compressed, skip all of them and report
/// `IngestError::NoTransforms` — a diagnosis with no relation to the cause and no
/// mention of the remedy.
///
/// lz4 is checked alongside zstd because they are compiled out by one feature and
/// a `cfg` that covered only one of them would be invisible everywhere else.
///
/// Mutant: make `ChunkCodec::is_built_in` return `true` for `Zstd`/`Lz4`
/// unconditionally, i.e. drop its `#[cfg]` — applied, and **all 83 tests still
/// passed**, this one included. `decompress_into`'s fallback arm returns
/// `ChunkFault::Unsupported` for any codec it has no decoder for, so the answer is
/// unchanged; the property is **structurally guarded** by that arm, which exists
/// precisely so a `cfg` mistake cannot become a wrong answer.
///
/// Mutant 2, which does kill it: turn `chunk_records`'s
/// `return Err(ChunkFault::Unsupported(head.codec))` into
/// `ChunkFault::Bad(BadChunkKind::Decompress { codec })` — applied, and this failed
/// with `a zstd chunk must not be skipped under Skip; got 6 of 9 transforms and 1
/// bad chunk(s)`. That is the hazard in one line: a missing decoder reported as
/// damage is skippable, so on a real recording — every chunk compressed — the answer
/// would be `NoTransforms` about an intact file, with no mention of compression.
///
/// It killed three other tests with it, in both feature configurations:
/// `decompress::tests::a_codec_free_build_reports_both_codecs_unsupported`,
/// `fixture::tests::each_damage_variant_produces_its_documented_fault` and
/// `ingest::an_unknown_codec_in_a_chunk_is_a_hard_error_not_a_skip`.
#[test]
fn a_compressed_chunk_is_refused_by_name_in_a_codec_free_build() {
    let dir =
        std::env::temp_dir().join(format!("tf_tree_ingest_codec_free_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    for (name, want) in [("zstd", ChunkCodec::Zstd), ("lz4", ChunkCodec::Lz4)] {
        let path = dir.join(format!("{name}.mcap"));
        write_mcap_chunked(
            &path,
            &corpus(),
            ChunkedSpec::new(3).damaged(ChunkDamage::Relabelled(name)),
        )
        .unwrap();

        for policy in [OnBadChunk::Skip, OnBadChunk::Halt] {
            let opts = IngestOptions {
                on_bad_chunk: policy,
                ..IngestOptions::default()
            };
            let mut frames = Frames::default();
            let err = match tf_tree_ingest::survey(&path, &opts, &mut frames) {
                Err(e) => e,
                Ok(s) => panic!(
                    "a {name} chunk must not be skipped under {policy:?}; got {} of 9 \
                     transforms and {} bad chunk(s)",
                    s.transforms_read, s.anomalies.bad_chunks
                ),
            };
            assert_eq!(
                err,
                IngestError::CompressedChunk { codec: want },
                "{name} under {policy:?}"
            );
            assert!(
                err.to_string().contains("cannot read"),
                "the message must name the build's limitation: {err}"
            );
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// **An uncompressed recording is unaffected**, which is the property that makes
/// the codec-free build a build and not a broken one.
///
/// Mutant: `ChunkCodec::parse("")` → `Self::Other` — applied, and this failed with
/// `the recording uses an unrecognised codec-compressed chunks, which this build
/// cannot read` (39 of this configuration's 83 tests died with it). That mutant is
/// the reason this test is here rather than only in the default configuration: the
/// uncompressed path is now one arm of a codec `match`, and this is the build where
/// nothing else can reach the other arms at all.
#[test]
fn an_uncompressed_recording_still_ingests_in_a_codec_free_build() {
    let dir = std::env::temp_dir().join(format!(
        "tf_tree_ingest_codec_free_plain_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("plain.mcap");
    write_mcap_chunked(&path, &corpus(), ChunkedSpec::new(3)).unwrap();

    let mut frames = Frames::default();
    let out = tf_tree_ingest::run(&path, &IngestOptions::default(), &mut frames)
        .unwrap_or_else(|e| panic!("{}", tf_tree_ingest::describe(e, &frames)));
    assert_eq!(out.report.samples_pushed, 9);
    assert_eq!(out.report.anomalies.bad_chunks, 0);
    assert!(!out.report.anomalies.truncated);

    let _ = std::fs::remove_dir_all(&dir);
}
