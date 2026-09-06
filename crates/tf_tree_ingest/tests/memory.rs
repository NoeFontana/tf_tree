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
/// applied, and the `passes == 3` assertion failed at 1, taking the
/// `peak_capped < peak_uncapped` assertion with it. Mutant 2: make the cap also
/// bound the arena — not applicable, there is no such code; that is the finding.
///
/// # What this test cannot see, stated because it read as though it could
///
/// `assert!(capped.report.fill.peak_buffer_bytes <= cap)` asserts the number the
/// code computed, not the number the process used. Until 2026-09-06 those were
/// different: the stable sort's own scratch was in neither the reported peak nor
/// the budget, so this assertion passed at a **1.95×** overrun (measured with a
/// counting allocator on this very fixture: 3 840 000 reported against
/// 5 762 113 B used at a 4 MiB cap). `plan_groups` reserves for the scratch now
/// and `fill` reports it, so the two agree again — but the instrument here is
/// still the report. Measuring the allocator needs an `unsafe impl GlobalAlloc`
/// and a row in `scripts/unsafe-budget.txt`; what pins the arithmetic instead is
/// `ingest::tests::groups_respect_the_cap` and `spill::tests::budget_fits_the_cap`.
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
    //
    // **Three passes, not two.** Three equal 1 920 000 B buffers used to pack
    // two to a group under a 4 194 304 B cap; the peak of such a group is
    // `sum + max` = 5 760 000, because sorting the first buffer allocates a full
    // copy of it while the second is still held. One per group is what fits.
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

    // And **not** on the half it does not: the arena is identical under both and
    // is larger than the cap, so under the cap the unbounded allocation is the
    // larger one — which is what makes `--max-memory`'s scope worth documenting.
    // That last clause is not asserted separately: `capped.peak <= cap` above and
    // `arena > cap` here give `arena > capped.peak` by transitivity, so an
    // assertion of it could not fail without one of these two failing first.
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
    // **And on this fixture the *uncapped* run is the other way round**, which
    // is new since the sort's scratch entered the reported peak and is asserted
    // rather than left to be rediscovered. Three equal edges means the scratch
    // is a third of the buffers' total, so one pass costs 85.3 B/sample of
    // buffer against 78.9 B/sample of arena. The doc's "the arena is the larger
    // of the two" is a statement about the per-sample *rates* (78 against 64);
    // the scratch is a per-group term on top of that, largest exactly when the
    // edges are few and equal — which is what this fixture was built to be.
    assert!(
        uncapped.report.fill.peak_buffer_bytes > arena,
        "three equal edges in one pass should cost more buffer than arena: \
         {} vs {arena}",
        uncapped.report.fill.peak_buffer_bytes
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
