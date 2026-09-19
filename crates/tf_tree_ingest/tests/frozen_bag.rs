//! Recording → `.tft` → query, and the provenance the container carries —
//! `docs/PHASE5.md` §2 meeting §3.
//!
//! Needs `--features shm` (`just shm-check`).

#![cfg(all(feature = "shm", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use tf_tree::{Stamp, SystemDomain, Tree};
use tf_tree_ingest::fixture::{small_recording, write_mcap};
use tf_tree_ingest::{Frames, IngestOptions};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let p =
            std::env::temp_dir().join(format!("tf_tree_frozen_bag-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Scratch(p)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A `.tft` built from a recording answers bit-identically to the in-memory
/// tree the same ingest produced (§2.1), including `Ok`/`Err` agreement, so a
/// frozen tree that answered nothing cannot pass.
#[test]
fn a_frozen_bag_answers_like_the_tree_it_came_from() {
    let dir = Scratch::new("roundtrip");
    let bag = dir.0.join("run.mcap");
    let tft = dir.0.join("run.tft");
    write_mcap(&bag, &small_recording()).unwrap();

    let opts = IngestOptions::default();
    let mut frames = Frames::default();
    let (ingested, header) = tf_tree_ingest::tft::freeze_bag(&bag, &tft, &opts, &mut frames)
        .unwrap_or_else(|e| panic!("{}", tf_tree_ingest::describe(e, &frames)));

    // §2.3: the digest is BLAKE3 of the recording, not all-zero.
    let expect = blake3::hash(&std::fs::read(&bag).unwrap());
    assert_eq!(&header.source_digest, expect.as_bytes());
    assert_ne!(header.source_digest, [0u8; 32]);

    let frozen = Tree::open_frozen(&tft).unwrap();
    let mut answered = 0;
    for i in 0..200 {
        let t = Stamp::<SystemDomain>::from_nanos(1_000_000_000 + i * 5_000_000);
        let a = ingested.tree.lookup("map", "laser", t);
        let b = frozen.lookup("map", "laser", t);
        assert_eq!(a, b, "at {t:?}");
        if a.is_ok() {
            answered += 1;
        }
    }
    assert!(
        answered > 150,
        "only {answered}/200 stamps answered; the sweep is not exercising the data"
    );
}

/// The `.tft` is read-only, permanently (§2.4); the bag path is a second way to
/// produce one, beyond `crates/tf_tree/tests/frozen.rs`.
#[test]
fn a_frozen_bag_is_not_writable() {
    let dir = Scratch::new("readonly");
    let bag = dir.0.join("run.mcap");
    let tft = dir.0.join("run.tft");
    write_mcap(&bag, &small_recording()).unwrap();

    let opts = IngestOptions::default();
    let mut frames = Frames::default();
    tf_tree_ingest::tft::freeze_bag(&bag, &tft, &opts, &mut frames).unwrap();

    let frozen = Tree::open_frozen(&tft).unwrap();
    assert!(!frozen.is_writable());
}
