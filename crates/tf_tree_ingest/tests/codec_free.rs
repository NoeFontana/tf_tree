//! The `--no-default-features` build: what a reader with no codecs does.
//!
//! `compression` is on by default, so `--workspace` never compiles this
//! configuration; `just ingest-check` runs it with `--no-default-features`. The
//! assertions are false with codecs compiled in.
//!
//! A compressed chunk must be [`IngestError::CompressedChunk`] naming the codec,
//! never a skip, or a fully compressed recording would report `NoTransforms`.

#![cfg(not(feature = "compression"))]
#![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]

use tf_tree_ingest::fixture::{write_mcap_chunked, ChunkDamage, ChunkedSpec, FixtureMessage};
use tf_tree_ingest::{ChunkCodec, Frames, IngestError, IngestOptions, OnBadChunk};

/// Nine messages in three chunks, a survivor either side of the relabelled one.
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

/// Without the `compression` feature a zstd or lz4 chunk is `CompressedChunk`,
/// under either bad-chunk policy.
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

/// An uncompressed recording is unaffected.
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

/// `compression_compiled_in` reports the truth about this build; the default
/// build's counterpart is `ingest::the_predicate_reports_a_build_with_codecs`.
#[test]
fn the_predicate_reports_a_codec_free_build() {
    assert!(
        !tf_tree_ingest::compression_compiled_in(),
        "a --no-default-features build must report no codecs"
    );
}
