//! What `--max-memory` bounds — `docs/PHASE5.md` §3.1: the sort buffers, not
//! the arena. `ingest::fill`'s doc quotes the numbers pinned here.

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

/// Three equal edges: they divide 2–1 against a cap, near the worst case for
/// the grouping strategy.
const PER_EDGE: i64 = 30_000;

fn big_recording() -> Vec<FixtureMessage> {
    let mut msgs = Vec::with_capacity(PER_EDGE as usize * 3);
    for i in 0..PER_EDGE {
        let t = 1_000_000_000 + i * 1_000_000;
        for (k, (a, b)) in [("odom", "base_link"), ("map", "odom"), ("base_link", "arm")]
            .iter()
            .enumerate()
        {
            // Per-edge stamp offset keeps the streams distinct.
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

/// `--max-memory` bounds the sort buffers and not the arena, and the arena is
/// the larger of the two.
///
/// The reported peak is the number the code computed, not allocator use;
/// `ingest::tests::groups_respect_the_cap` and
/// `spill::tests::budget_fits_the_cap` pin the arithmetic.
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

    // Three passes, not two: a group's peak is `sum + max` (sort copies a
    // buffer while the next is held), so one buffer per group fits the cap.
    assert_eq!(
        capped.report.fill.passes, 3,
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

    // The arena is identical under both and larger than the cap.
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
    // With three equal edges the sort scratch makes one uncapped pass cost
    // more buffer than arena (85.3 vs 78.9 B/sample).
    assert!(
        uncapped.report.fill.peak_buffer_bytes > arena,
        "three equal edges in one pass should cost more buffer than arena: \
         {} vs {arena}",
        uncapped.report.fill.peak_buffer_bytes
    );

    // Pinned loosely enough to survive a ring-capacity change, tightly enough
    // to catch a doubling.
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
