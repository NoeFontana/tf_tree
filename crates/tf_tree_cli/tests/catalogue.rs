//! End-to-end: run the `docs/PHASE5.md` §6 catalogue against the real
//! benchmark fixture.
//!
//! The unit tests in `checks.rs` build each offending state by hand, which
//! proves a check *fires*. This file proves the opposite and harder property:
//! that on a **correct, fully populated, live** transform tree the catalogue
//! stays quiet. Every false positive found so far was found here and nowhere
//! else, because a hand-built fixture only contains what its author thought to
//! put in it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;

use tf_tree_cli::catalogue::{Severity, Status, Tft};
use tf_tree_cli::checks::{self, Clock, Inputs};
use tf_tree_cli::doctor::{Observations, Snapshot};

/// Run the catalogue over the fixture with its publishers still holding their
/// claims, which is the state a healthy robot is in.
fn run_on_fixture<R>(f: impl FnOnce(&tf_tree_cli::catalogue::Report, &Snapshot) -> R) -> R {
    let tree = tf_tree_bench::fixture::build_tree().expect("build fixture");
    // The writers are held for the whole run: dropping them releases every
    // claim, and the tree would then be a *different* state — one where the
    // id-less `unclaimed-dynamic` check legitimately fires on all four dynamic
    // edges, which would make this test assert nothing about the healthy case.
    let (writers, samples) = tf_tree_bench::fixture::spin_up(&tree).expect("populate history");

    let snap = Snapshot::capture(&tree);
    let obs = Observations::from_samples(samples);
    let stats = checks::collect_edge_stats(&tree, &snap);
    let clock = Clock::decide(checks::newest_stamp(&snap), 1_700_000_000_000_000_000);
    let inputs = Inputs {
        snap: &snap,
        obs: &obs,
        stats: &stats,
        // `None` on purpose: `TFT016` reads this host's `/sys` and `/proc`, and
        // a test whose result depends on the CI runner's huge-page setting is a
        // test that fails for a reason nobody can act on.
        host: None,
        clock,
        arena_bytes: tree.arena_size_bytes() as u64,
        occupancy: checks::occupancy_of(&tree),
        live: false,
        counters: tf_tree::counters_compiled_in(),
    };
    let report = checks::run(&inputs, &BTreeSet::new());
    let out = f(&report, &snap);
    drop(writers);
    out
}

/// **A healthy, fully published tree produces no finding except the one that is
/// true of it.**
///
/// The fixture's arena is sized exactly to its topology, so `TFT015` (occupancy
/// above 80%) is a correct report at 100%, not a false positive. Everything
/// else must be silent — a diagnostic that fires on a correct robot is one that
/// gets piped to `/dev/null` inside a week, and this is the assertion that
/// makes each new check earn its place.
///
/// Mutant: in `Snapshot::capture`, decode the claim owner word by hand as
/// `u32::try_from(owner_word - 1).ok()` instead of calling
/// `tf_tree_core::edge::slot_of`. Applied: every live writer resolves to pid 0
/// and `TFT014` fires on all four claimed edges, which this test fails with
/// listed. That is the false positive this test exists for — the word is
/// `(epoch << 16) | (slot + 1)`, and no unit test on a hand-built `Snapshot`
/// can see a decode bug, because a hand-built one never encodes anything.
#[test]
fn the_healthy_fixture_fires_only_the_check_that_is_true_of_it() {
    run_on_fixture(|report, snap| {
        let fired: Vec<&str> = report
            .outcomes
            .iter()
            .filter(|o| o.status == Status::Fired)
            .map(|o| o.check.id())
            .collect();
        assert_eq!(
            fired,
            vec!["TFT015"],
            "unexpected findings on a healthy tree: {:#?}",
            report
                .outcomes
                .iter()
                .filter(|o| o.status == Status::Fired)
                .collect::<Vec<_>>()
        );
        assert!(
            report.uncatalogued.is_empty(),
            "id-less checks fired on a healthy tree: {:?}",
            report.uncatalogued
        );
        assert!(
            !report.has_error(),
            "`doctor --exit-code` must pass on a healthy tree"
        );

        // Non-vacuity: the fixture really is populated and claimed, so the
        // checks above had something to look at rather than an empty tree.
        assert_eq!(snap.frames.len(), 24);
        assert_eq!(snap.edges.len(), 23);
        assert!(
            snap.edges.iter().any(|e| e.head > 100 && e.claimed),
            "the fixture must be published-into and claimed, or this test is vacuous"
        );
    });
}

/// **Every check the catalogue could not run states why, and no check is
/// missing from the report.**
///
/// A skipped check with an empty reason is indistinguishable in the output from
/// one that passed, which is the specific dishonesty `Status::Skipped` carries a
/// mandatory `String` to prevent.
///
/// Mutant: change `TFT007`'s reason to `""`. Applied: the non-empty-reason
/// assertion fails, naming `TFT007`.
#[test]
fn every_id_is_reported_and_every_skip_states_a_reason() {
    run_on_fixture(|report, _| {
        let ids: Vec<&str> = report.outcomes.iter().map(|o| o.check.id()).collect();
        let expected: Vec<&str> = Tft::ALL.iter().map(|c| c.id()).collect();
        assert_eq!(ids, expected, "the report must carry every id, in id order");

        let mut skipped = 0;
        for o in &report.outcomes {
            if let Status::Skipped(why) = &o.status {
                skipped += 1;
                assert!(
                    why.len() > 20,
                    "{} was skipped without a usable reason: {why:?}",
                    o.check.id()
                );
            }
        }
        // Non-vacuity: this build genuinely cannot run several checks, so the
        // loop above examined something.
        assert!(skipped >= 5, "expected several skips, saw {skipped}");
    });
}

/// **The JSON carries every catalogue id, and its summary agrees with the exit
/// status.**
///
/// `--json` is what a CI job consumes and `--exit-code` is what it gates on. If
/// the two disagree, a job either fails while its report says everything is
/// fine, or passes while the report lists errors — and both are worse than
/// having neither.
///
/// Mutant: make `Report::count_at` in the JSON summary use `at()` alone,
/// dropping the `uncatalogued` term. Applied: the summary/gate agreement below
/// still holds on the healthy fixture (nothing id-less fires), so the case is
/// forced explicitly with an injected id-less error, and *that* assertion
/// fails.
#[test]
fn the_json_summary_agrees_with_the_exit_status() {
    run_on_fixture(|report, _| {
        let meta = tf_tree_cli::catalogue::Meta::default();
        let json = tf_tree_cli::catalogue::render_json(report, &meta);
        for c in Tft::ALL {
            assert!(
                json.contains(&format!("\"id\": \"{}\"", c.id())),
                "{} missing from the JSON: {json}",
                c.id()
            );
        }
        assert!(json.contains("\"error\": 0"), "{json}");
        assert!(!report.has_error());
    });

    // Forced: an id-less error must appear in the summary count, or a CI job
    // reading the JSON disagrees with the process it just ran.
    let mut report = tf_tree_cli::catalogue::Report::default();
    report
        .uncatalogued
        .push(tf_tree_cli::catalogue::Uncatalogued {
            check: "out-of-order",
            severity: Severity::Error,
            subject: "tree".to_owned(),
            message: "stamps went backwards".to_owned(),
        });
    let json =
        tf_tree_cli::catalogue::render_json(&report, &tf_tree_cli::catalogue::Meta::default());
    assert!(report.has_error());
    assert!(
        json.contains("\"error\": 1"),
        "the summary must count id-less findings: {json}"
    );
}
