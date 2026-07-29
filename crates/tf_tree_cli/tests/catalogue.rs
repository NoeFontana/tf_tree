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
    // claim, and the tree would then be a *different* state — one where
    // `TFT017` legitimately fires on all four dynamic edges, which would make
    // this test assert nothing about the healthy case.
    let (writers, samples) = tf_tree_bench::fixture::spin_up(&tree).expect("populate history");

    let snap = Snapshot::capture(&tree);
    let obs = Observations::from_samples(samples);
    let stats = checks::collect_edge_stats(&tree, &snap);
    let clock = Clock::decide(&checks::newest_stamps(&snap), 1_700_000_000_000_000_000);
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
            "nothing should reach the id-less path any more (PHASE5 §6's amendment gave \
             the last two occupants TFT017/TFT018): {:?}",
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
/// still holds on the healthy fixture (nothing id-less fires — nothing produces
/// an [`Uncatalogued`] at all since §6's amendment), so the case is forced
/// explicitly with an injected id-less error, and *that* assertion fails. The
/// injection is the point: the `uncatalogued` array is still part of the stable
/// JSON schema, and this pins that a finding placed in it would still be counted
/// by the summary a CI job reads.
///
/// [`Uncatalogued`]: tf_tree_cli::catalogue::Uncatalogued
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

/// **Every occupancy row TFT015 reports must be capable of being non-zero.**
///
/// `TFT015` is a threshold check, so a row whose numerator is stuck at 0 does
/// not merely fail to fire — it reports `pass`, which is an active claim that
/// the table has room. `ArenaHeader::participant_count` is never incremented
/// anywhere in the workspace, so a `participants` row would read `0 / max` on
/// every arena that has ever existed and would report `pass` on a fleet that
/// had exhausted every slot and could not attach another node. The catalogue's
/// premise is that a check without evidence says so; the row is therefore
/// omitted and disclosed in `Meta.notes` instead.
///
/// The fixture is non-degenerate: it is a fully built tree with live
/// publishers, so `frames` and `edges` are both genuinely non-zero and the
/// assertion is about the *absent* row rather than about an empty arena.
///
/// Mutant: restore the `("participants", h.participant_count, h.max_participants)`
/// row in `occupancy_of`. Applied: `used = 0` for it and this fails with
/// "occupancy row \"participants\" reads 0 used".
#[test]
fn no_occupancy_row_is_permanently_zero() {
    let tree = tf_tree_bench::fixture::build_tree().expect("build fixture");
    let (writers, _samples) = tf_tree_bench::fixture::spin_up(&tree).expect("populate history");

    let rows = checks::occupancy_of(&tree);
    assert!(!rows.is_empty(), "TFT015 must measure something");
    for (what, used, cap) in rows {
        assert!(
            used > 0,
            "occupancy row {what:?} reads 0 used of {cap} on a fully populated arena, so \
             TFT015 can only ever report `pass` for it — omit the row and disclose the gap \
             in Meta.notes instead of passing silently"
        );
    }
    drop(writers);
}

/// **The whole `TFT007` evidence path, end to end: a topology file's `rate_hz`
/// reaches the arena, is read back out of it, and judges a real publisher.**
///
/// The unit tests own the two halves — `config.rs` proves `rate_hz` lands in
/// `EdgeRecord::nominal_rate_mhz`, and `checks.rs` proves the comparison — and
/// neither can see the seam between them, which is `Snapshot::capture` mapping
/// the record's `0` sentinel to `None`. A capture that mapped it to `Some(0)`
/// passes every unit test in this repository (they build `EdgeInfo` directly)
/// and turns every undeclared edge on a live robot into a warn about deviating
/// from 0 Hz by infinity — the exact fabricated finding this id was blocked on
/// for one revision.
///
/// The arena is non-degenerate: two dynamic edges, one declaring a rate and one
/// not, both published into at the *same* wrong rate, so the difference between
/// them is the declaration and nothing else.
///
/// Mutant: in `Snapshot::capture`, map the field as `Some(rec.nominal_rate_mhz)`
/// unconditionally. Applied: `TFT007` fires on both edges and the "exactly the
/// declared edge" assertion fails. Mutant B: map it to `None` unconditionally.
/// Applied: the check skips instead of firing and the first assertion fails.
#[test]
fn a_topology_files_declared_rate_reaches_doctor_and_judges_the_publisher() {
    use tf_tree_bridge::TopologyConfig;

    // 20 Hz declared, 2 s of history -> a 64-slot ring; the sibling is sized by
    // slots and declares nothing.
    let text = "\
[[edge]]
parent = \"odom\"
child = \"base_footprint\"
kind = \"dynamic\"
rate_hz = 20.0
history_secs = 2.0

[[edge]]
parent = \"base_footprint\"
child = \"base_link\"
kind = \"dynamic\"
capacity = 64
";
    let config = TopologyConfig::parse(text).expect("the fixture config must parse");
    let tree = config.builder().build().expect("and must build");

    let odom = tree.frame("odom").unwrap();
    let foot = tree.frame("base_footprint").unwrap();
    let base = tree.frame("base_link").unwrap();
    let declared = tree.claim(foot, odom).expect("claim the declared edge");
    let undeclared = tree.claim(base, foot).expect("claim the sibling");
    // 10 Hz on both: half the declared rate on the one that declared.
    for k in 0..12 {
        let stamp = k * 100_000_000;
        declared.push(stamp, &tf_tree::Iso3::IDENTITY).unwrap();
        undeclared.push(stamp, &tf_tree::Iso3::IDENTITY).unwrap();
    }

    let snap = Snapshot::capture(&tree);
    let obs = Observations::from_arena(&tree, &snap);
    let stats = checks::collect_edge_stats(&tree, &snap);
    let clock = Clock::decide(&checks::newest_stamps(&snap), 1_700_000_000_000_000_000);
    let report = checks::run(
        &Inputs {
            snap: &snap,
            obs: &obs,
            stats: &stats,
            host: None,
            clock,
            arena_bytes: tree.arena_size_bytes() as u64,
            occupancy: checks::occupancy_of(&tree),
            live: true,
            counters: tf_tree::counters_compiled_in(),
        },
        &BTreeSet::new(),
    );

    let o = report
        .outcomes
        .iter()
        .find(|o| o.check == Tft::Tft007)
        .expect("TFT007 must be in the report");
    assert_eq!(
        o.status,
        Status::Fired,
        "a 10 Hz publisher on a 20 Hz declaration must be reported: {o:?}"
    );
    assert_eq!(o.findings.len(), 1, "{:?}", o.findings);
    assert!(
        o.findings[0].subject.contains("odom->base_footprint"),
        "the finding must name the declared edge, not its sibling: {}",
        o.findings[0].subject
    );
    assert!(
        o.findings[0].message.contains("20.00 Hz"),
        "the declared rate must survive the round trip through the arena: {}",
        o.findings[0].message
    );

    // And the arena discloses that one of its two edges was never compared.
    let note = checks::rate_coverage_note(&snap, &obs).expect("a partial run must say so");
    assert!(note.contains("compared 1 of 2"), "{note}");

    drop(declared);
    drop(undeclared);
}

/// **The CLI's `counters` feature must actually control the engine's.**
///
/// `TFT010`'s "built without counters" skip and `render_human`'s banner both
/// key off `tf_tree::counters_compiled_in`, which reports what the *engine* was
/// built with. The CLI's own `counters` feature is supposed to be what sets
/// that. Cargo will silently break the link: any workspace dependency declared
/// without `default-features = false` re-enables `tf_tree/counters` through its
/// own defaults, independently of what the CLI asked for. That is what
/// happened — `tf_tree_bench` (a dependency of this crate, carrying
/// `default = ["counters"]`) pinned the engine's counters on, so
/// `--no-default-features` produced a byte-identical report and the skip branch
/// was unreachable in every buildable configuration. Nothing warned, because
/// each half was individually truthful.
///
/// This assertion is the only place the two answers are compared, so it is the
/// only thing that can see them disagree.
///
/// Mutant: drop `default-features = false` from the `tf_tree_bench` line in the
/// workspace `Cargo.toml`. Applied, under
/// `cargo nextest run -p tf_tree_cli --no-default-features`: `left = false`,
/// `right = true`, and this fails. It passes on the default build with or
/// without the mutant, which is exactly why the mutant must be run with the
/// feature off — and why the defect survived the default gate.
#[test]
fn the_cli_counters_feature_switches_the_engine() {
    assert_eq!(
        cfg!(feature = "counters"),
        tf_tree::counters_compiled_in(),
        "the CLI asked for counters={} but the engine was built with counters={}; \
         some dependency is re-enabling tf_tree/counters through its own defaults",
        cfg!(feature = "counters"),
        tf_tree::counters_compiled_in()
    );
}
