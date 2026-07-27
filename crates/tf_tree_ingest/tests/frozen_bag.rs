//! Recording → `.tft` → query, and the provenance the container carries —
//! `docs/PHASE5.md` §2 meeting §3.
//!
//! Needs `--features shm` because the frozen backend is Linux-only and behind
//! that flag (it reuses the shared-memory mapping code). `just shm-check` runs
//! this file; a plain `cargo nextest run --workspace` cannot.
//!
//! The recording is synthetic — `tf_tree_ingest::fixture` says so at length.

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

/// A `.tft` built from a recording answers **bit-identically** to the in-memory
/// tree the same ingest produced — which is §2.1's claim, reached from §3's
/// source rather than from a live arena.
///
/// The comparison sweeps 200 stamps across the span and asserts `Ok`/`Err`
/// agreement as well as value equality, so a frozen tree that answered
/// *nothing* could not pass by having no answers to disagree about.
///
/// Mutant: in `ingest::fill`, halve every ring —
/// `Capacity::slots(clamp_u32(e.samples) / 2)` — applied, and this test failed
/// on the `answered > 150` guard, because half the span had been lapped away.
/// The heap tree and the frozen file still agreed with each other, which is
/// exactly why that guard is here and not just the equality assertion.
/// Mutant 2: in `tft::freeze_bag`, pass `[0u8; 32]` instead of the computed
/// digest — applied, and the `source_digest` assertion failed.
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

    // §2.3: the digest is BLAKE3 of the *recording*, so it is reproducible from
    // the file alone and is not all-zero the way `--from-live` leaves it.
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

/// The `.tft` is read-only, permanently — §2.4. Freezing a bag must not hand
/// back something a caller can publish into.
///
/// Mutant: none applied; this is a restatement of a property `Tree::open_frozen`
/// already owns and tests (`crates/tf_tree/tests/frozen.rs`). It is here because
/// the *bag* path is a second way to produce one, and a future change that gave
/// bag-frozen files a writable backing would not be caught by that file.
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
