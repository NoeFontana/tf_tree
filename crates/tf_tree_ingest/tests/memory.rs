//! What `--max-memory` actually bounds — `docs/PHASE5.md` §3.1.
//!
//! §3.1's motivating user has "a 4-hour recording and will not accept an OOM",
//! so the honest scope of the cap is a correctness property of the *tool*, not
//! documentation trivia. `ingest::fill`'s doc comment states that the cap bounds
//! the sort buffers and **not** the arena, and gives numbers. This file is where
//! those numbers come from; without it the doc is an unchecked claim, and an
//! earlier revision of it said "peak memory is the cap either way", which was
//! false by more than the amount the cap was saving.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use tf_tree_ingest::fixture::{write_mcap, FixtureMessage};
use tf_tree_ingest::{Frames, IngestOptions};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let dir =
            std::env::temp_dir().join(format!("tf_tree_ingest_mem_{tag}_{}", std::process::id()));
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

/// Three edges of 30 000 samples each. Equal sizes on purpose: they divide 2–1
/// against a cap, which is close to the *worst* case for the grouping strategy,
/// so the saving measured here is a floor rather than a flattering best case.
const PER_EDGE: i64 = 30_000;

fn big_recording() -> Vec<FixtureMessage> {
    let mut msgs = Vec::with_capacity(PER_EDGE as usize * 3);
    for i in 0..PER_EDGE {
        let t = 1_000_000_000 + i * 1_000_000;
        for (k, (a, b)) in [("odom", "base_link"), ("map", "odom"), ("base_link", "arm")]
            .iter()
            .enumerate()
        {
            // The stamps are offset per edge so the three streams are not
            // identical — a shared stamp would make this fixture degenerate for
            // anything that looks at per-edge time.
            msgs.push(FixtureMessage::dynamic(
                a,
                b,
                t + k as i64,
                [1.0, 0.0, 0.0, 0.0, i as f64, k as f64, 0.0],
            ));
        }
    }
    msgs
}

/// **`--max-memory` bounds the sort buffers and not the arena**, and the arena
/// is the larger of the two.
///
/// This is the assertion an earlier doc comment contradicted. It is deliberately
/// stated as "the cap is exceeded overall" rather than as a tidy inequality: a
/// user who reads `--max-memory 512M` as a promise about the process will be
/// OOM-killed by the arena, and the tool has to be honest about which number it
/// controls.
///
/// Mutant: change `plan_groups`'s flush to never split (return one group) —
/// applied, and the `passes == 2` assertion failed at 1, taking the
/// `peak_capped < peak_uncapped` assertion with it. Mutant 2: make the cap also
/// bound the arena — not applicable, there is no such code; that is the finding.
#[test]
fn the_cap_bounds_the_buffers_not_the_arena() {
    let dir = Scratch::new("cap");
    let path = dir.0.join("big.mcap");
    write_mcap(&path, &big_recording()).unwrap();

    let cap = 4 * 1024 * 1024;
    let mut f1 = Frames::default();
    let capped = tf_tree_ingest::run(
        &path,
        &IngestOptions {
            max_memory_bytes: cap,
            ..IngestOptions::default()
        },
        &mut f1,
    )
    .unwrap();
    let mut f2 = Frames::default();
    let uncapped = tf_tree_ingest::run(&path, &IngestOptions::default(), &mut f2).unwrap();

    let n = capped.report.samples_pushed;
    assert_eq!(n, PER_EDGE as u64 * 3, "the fixture did not survive ingest");
    assert_eq!(uncapped.report.samples_pushed, n);

    // The cap did its job on the half it covers.
    assert_eq!(
        capped.report.fill.passes, 2,
        "the cap did not split anything"
    );
    assert_eq!(uncapped.report.fill.passes, 1);
    assert!(capped.report.fill.peak_buffer_bytes <= cap);
    assert!(
        capped.report.fill.peak_buffer_bytes < uncapped.report.fill.peak_buffer_bytes,
        "capped {} vs uncapped {}",
        capped.report.fill.peak_buffer_bytes,
        uncapped.report.fill.peak_buffer_bytes
    );

    // And **not** on the half it does not: the arena is identical under both,
    // is larger than the cap, and is larger than the buffers it saved.
    let arena = capped.tree.arena_size_bytes() as u64;
    assert_eq!(
        arena,
        uncapped.tree.arena_size_bytes() as u64,
        "the cap must not change the output"
    );
    assert!(
        arena > cap,
        "fixture too small to show the point: arena {arena} <= cap {cap}"
    );
    assert!(
        arena > uncapped.report.fill.peak_buffer_bytes,
        "the unbounded allocation ({arena} B) is the larger one; \
         that is what makes --max-memory's scope worth documenting"
    );

    // The numbers `ingest::fill`'s doc comment quotes, pinned loosely enough to
    // survive a ring-capacity change and tightly enough to catch a doubling.
    let arena_per_sample = arena / n;
    assert!(
        (70..=90).contains(&arena_per_sample),
        "the doc says ~78 B/sample of arena; measured {arena_per_sample}"
    );
    let peak_uncapped = (arena + uncapped.report.fill.peak_buffer_bytes) / n;
    let peak_capped = (arena + capped.report.fill.peak_buffer_bytes) / n;
    assert!(
        peak_capped < peak_uncapped,
        "capping must lower the peak: {peak_capped} vs {peak_uncapped}"
    );
}
