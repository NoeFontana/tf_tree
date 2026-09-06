//! End-to-end: run the `docs/PHASE5.md` §6 catalogue against the real
//! benchmark fixture.
//!
//! The unit tests in `checks.rs` build each offending state by hand, which
//! proves a check *fires*. This file proves the opposite and harder property:
//! that on a **correct, fully populated, live** transform tree the catalogue
//! stays quiet. Every false positive found so far was found here and nowhere
//! else, because a hand-built fixture only contains what its author thought to
//! put in it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};

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
        clock_step: &checks::ClockStepEvidence::capture(&snap, &obs),
        stream: checks::PushStream::Observed,
        slots: checks::SlotTable::Current,
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
/// **`--exit-code` has two tiers, and the default one is unchanged.**
///
/// Six ids carry `Error`, and on a *live* arena four of them structurally skip —
/// so `--exit-code error` reduces to `TFT006` (impossible stamps) and `TFT012`
/// (cycle or disconnected subtree). Those are the right errors: both make every
/// lookup fail. But almost everything an operator is paged about is `Warn` — an
/// edge with no live writer, an undersized ring, rate collapse, gaps, clock
/// skew, a slot leak, an arena at capacity — and all of it exited 0.
///
/// **The capability existed and the exit code did not.** `doctor --json | jq -e
/// '.summary.warn == 0 and .summary.error == 0'` gates on exactly that today,
/// and `Report::is_healthy` was written and unit-tested for it with **no
/// caller**. This connects them.
///
/// The three assertions are the three things that had to stay true: a bare
/// `--exit-code` means what it always meant, `error` is that spelled out, and
/// `warn` is *warn-and-above* rather than warn-only — an arena with a cycle in
/// it must not pass `--exit-code warn` just because nothing warned.
///
/// **Mutant:** write the `warn` arm as `report.count_at(Severity::Warn) > 0`.
/// The first two assertions still hold and the tier stops being a ladder; this
/// test does not catch it, which is why the arm's comment argues it and the
/// unit test in `catalogue.rs` pins `is_healthy` against an error-only report.
#[test]
fn the_exit_code_gate_has_a_warn_tier_and_an_unchanged_default() {
    use std::process::Command;

    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_tf_tree"))
            .arg("doctor")
            .args(args)
            .output()
            .expect("run tf_tree doctor")
            .status
            .success()
    };

    // The fixture is healthy at error severity and not at warn — which is what
    // makes it a fixture that can tell the two tiers apart at all.
    assert!(run(&["--exit-code"]), "a bare --exit-code must still pass");
    assert!(
        run(&["--exit-code", "error"]),
        "`error` must mean what the bare flag always meant"
    );
    assert!(
        !run(&["--exit-code", "warn"]),
        "the fixture reports warn-severity findings, so the warn tier must gate \
         on them — if this passes, either the tier is not wired or the fixture \
         stopped being able to distinguish the two"
    );
}

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
            clock_step: &checks::ClockStepEvidence::capture(&snap, &obs),
            stream: checks::PushStream::RingsUnderWriter,
            slots: checks::SlotTable::Current,
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
    let note = checks::rate_coverage_note(&snap, &obs, clock, checks::PushStream::RingsUnderWriter)
        .expect("a partial run must say so");
    assert!(note.contains("compared 1 of 2"), "{note}");

    drop(declared);
    drop(undeclared);
}

/// **`TFT014` sees the wedge, on a real arena, through the real liveness
/// predicate.**
///
/// The unit tests build a `ParticipantInfo` and hand it to the check, which
/// proves the reporting and nothing about how `alive` was reached. This one
/// puts a `LIVE` record into an actual participant table and lets
/// `Snapshot::capture` ask `Tree::participant_alive` about it — the seam issue
/// #184 fell through, where a killed writer's record is left `LIVE` and
/// `ParticipantTable::identity` keeps answering with its dead pid.
///
/// **What is staged and what is real.** The record is injected with
/// `register_at`, which no ordinary code path produces, so this proves the
/// *predicate and the report* on a real arena and not that any deployment
/// reaches this state. The shape that does is a whole process dying, and it is
/// `crates/tf_tree/tests/rendezvous.rs`'s
/// `the_hangup_frees_a_joiners_slot_and_leaves_the_owners_live`: a real owner
/// `SIGKILL`ed, leaving a real `LIVE` record over a free byte, which is
/// `docs/decisions/0028`'s candidate-B hole 3. It lives there rather than here
/// because that arena has no owner left to attach to — `doctor --attach` is
/// refused `ArenaHeldButUnreachable`, so the only observer is a process that
/// joined first.
///
/// **The pid is `u32::MAX` because that is the deterministic form of "gone".**
/// It exceeds every `pid_max`, so `/proc/<pid>` cannot exist and the predicate
/// takes its `NoSuchProcess` branch — the same branch a killed and reaped
/// writer's pid takes, without racing pid reuse to get there. On a
/// rendezvous-opened tree the same slot is answered by `F_OFD_GETLK` on its
/// lock byte instead; both are `Tree::participant_alive`, which is the point of
/// routing through it rather than spelling a third predicate here.
///
/// Non-vacuity is the whole risk in a test like this, so three things are
/// asserted about the *healthy* half of the same arena: the table is fully
/// captured, this process's own slot reads alive, and the live writers'
/// claims produce no edge finding.
///
/// Mutant: in `tft014`, drop the participant loop. Applied: `left: Pass,
/// right: Fired` — which is exactly the state `tf_tree doctor` was in before
/// this, on the arena the check is named for.
/// Mutant B: in `Snapshot::capture`, set `alive: state == SlotState::Live`.
/// Applied: the "must read dead through the real predicate" assertion fires
/// first, printing `alive: true` for pid 4294967295. That mutant is
/// `docs/PHASE2.md` §5.1's forbidden inference written out, and the assertion
/// that catches it is deliberately upstream of the check, so the failure names
/// the predicate rather than the report.
#[test]
fn a_stale_live_participant_record_is_reported_on_a_real_arena() {
    const GONE: u32 = u32::MAX;

    let tree = tf_tree_bench::fixture::build_tree().expect("build fixture");
    let (writers, samples) = tf_tree_bench::fixture::spin_up(&tree).expect("populate history");

    // Slot 0 is this process. Slot 1 is the record a killed writer leaves: the
    // publication protocol is the same one `register` runs, because the state
    // being simulated is a *complete* registration whose process then died.
    tree.arena_view()
        .participants()
        .register_at(1, GONE, 1, 0)
        .expect("slot 1 of a 64-slot table is free");

    let snap = Snapshot::capture(&tree);
    let obs = Observations::from_samples(samples);
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
            clock_step: &checks::ClockStepEvidence::capture(&snap, &obs),
            stream: checks::PushStream::Observed,
            slots: checks::SlotTable::Current,
            counters: tf_tree::counters_compiled_in(),
        },
        &BTreeSet::new(),
    );

    assert_eq!(
        snap.participants.len(),
        64,
        "the whole table must be captured, or `1 of N` in the finding is a guess"
    );
    let own = snap.participant(0).expect("this process holds slot 0");
    assert!(
        own.alive,
        "the running process that built the arena must not read as leaked: {own:?}"
    );
    let stale = snap.participant(1).expect("slot 1 was just registered");
    assert!(
        !stale.alive && stale.pid == GONE,
        "the injected record must read dead through the real predicate: {stale:?}"
    );

    let o = report
        .outcomes
        .iter()
        .find(|o| o.check == Tft::Tft014)
        .expect("TFT014 must be in the report");
    assert_eq!(o.status, Status::Fired, "{o:?}");
    assert_eq!(
        o.findings.len(),
        1,
        "one leaked slot, and no edge: the fixture's writers are alive and holding \
         their claims: {:?}",
        o.findings
    );
    // `byte not probed`: this is an in-process arena with no rendezvous, so
    // `Snapshot::probe_lock_facts` was never called and the verdict rests on
    // `/proc` alone. The subject says which evidence the run had rather than
    // letting a `--from-bag` finding read like an `--attach` one.
    assert_eq!(
        o.findings[0].subject,
        format!("slot 1 pid {GONE}, byte not probed")
    );
    assert!(
        o.findings[0].message.contains("1 of 64"),
        "the operator's budget is 64 slots and the finding must say so: {}",
        o.findings[0].message
    );
    assert_eq!(
        Tft::Tft014.severity(),
        Severity::Warn,
        "PHASE5 §6's row says warn; detection is not reclamation"
    );

    drop(writers);
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

/// **A publisher's clock offset travels from `EdgeWriter::push` to a `TFT004`
/// finding, on a real arena.**
///
/// The unit tests in `checks.rs` set `EdgeInfo::clock_offset_nanos` by hand,
/// which proves the check reads its input and proves nothing about whether
/// anything writes one. Three seams sit between the two and none of them is
/// visible from either side: `EdgeWriter`'s sampler, `ClaimRecord`'s field, and
/// `Snapshot::capture` mapping the arena's `0` sentinel to `None`. This is the
/// test that fails if any of them stops.
///
/// Both edges publish with **wall-clock** stamps, because that is the only
/// configuration in which this check runs at all — the benchmark fixture stamps
/// from zero, so `Clock::decide` puts it in the `NewestStamp` arm and `TFT004`
/// skips there for `TFT005`'s reason.
///
/// One publisher is healthy. The other stamps an hour into the past, which is a
/// machine whose NTP never came up — the failure the check exists to name, and
/// the one an operator cannot attribute from the symptom.
///
/// Mutants, run:
///
/// * in `Tree::claim`, drop `claim_rec.clock_offset_nanos.store(0, ..)` and the
///   sampler's store — the check skips with *"no edge has recorded a clock
///   offset yet"* and the assertion on `Fired` fails.
/// * in `Snapshot::capture`, map the field as `Some(raw)` rather than matching
///   the `0` sentinel — the healthy publisher's unsampled sibling arrives as
///   `Some(0)` and the note reports a fleet member that was never measured.
#[test]
fn a_publishers_clock_offset_reaches_a_tft004_finding_on_a_real_arena() {
    use tf_tree::{Capacity, EdgeCfg, TreeBuilder};

    const HOUR_NS: i64 = 3_600 * 1_000_000_000;

    let tree = TreeBuilder::new()
        .dynamic_edge("map", "odom", EdgeCfg::new(Capacity::slots(64)))
        .dynamic_edge("odom", "base", EdgeCfg::new(Capacity::slots(64)))
        // **Never claimed and never pushed**, so its `clock_offset_nanos` stays
        // at the arena's zero. It is here to make the `0 -> None` mapping in
        // `Snapshot::capture` load-bearing: without a never-sampled edge in the
        // snapshot, a capture that passed the raw value through would be
        // indistinguishable from one that maps the sentinel.
        .dynamic_edge("base", "sensor", EdgeCfg::new(Capacity::slots(64)))
        .build()
        .expect("build");

    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    )
    .unwrap();

    let map = tree.frame("map").unwrap();
    let odom = tree.frame("odom").unwrap();
    let base = tree.frame("base").unwrap();

    // Healthy: stamps track this host's clock.
    let healthy = tree.claim(odom, map).expect("claim map->odom");
    // Broken: an hour behind, which no publish pipeline accounts for.
    let broken = tree.claim(base, odom).expect("claim odom->base");
    for k in 0..4i64 {
        healthy
            .push(now + k * 1_000, &tf_tree::Iso3::IDENTITY)
            .unwrap();
        broken
            .push(now - HOUR_NS + k * 1_000, &tf_tree::Iso3::IDENTITY)
            .unwrap();
    }

    let snap = Snapshot::capture(&tree);
    let obs = Observations::from_arena(&tree, &snap);
    let stats = checks::collect_edge_stats(&tree, &snap);
    let clock = Clock::decide(&checks::newest_stamps(&snap), now);
    assert!(
        matches!(clock, Clock::Wall(_)),
        "these stamps are wall-clock time; if Clock::decide disagrees this test \
         is asserting nothing: {clock:?}"
    );
    let report = checks::run(
        &Inputs {
            snap: &snap,
            obs: &obs,
            stats: &stats,
            host: None,
            clock,
            arena_bytes: tree.arena_size_bytes() as u64,
            occupancy: checks::occupancy_of(&tree),
            clock_step: &checks::ClockStepEvidence::capture(&snap, &obs),
            stream: checks::PushStream::RingsUnderWriter,
            slots: checks::SlotTable::Current,
            counters: tf_tree::counters_compiled_in(),
        },
        &BTreeSet::new(),
    );

    let o = report
        .outcomes
        .iter()
        .find(|o| o.check == Tft::Tft004)
        .expect("TFT004 must be in the report");
    assert_eq!(
        o.status,
        Status::Fired,
        "an hour-wrong publisher on a live arena went unreported: {o:?}"
    );
    assert_eq!(
        o.findings.len(),
        1,
        "only one edge is wrong: {:?}",
        o.findings
    );
    assert!(
        o.findings[0].subject.contains("odom->base"),
        "the finding names the healthy edge instead of the broken one: {}",
        o.findings[0].subject
    );
    assert!(
        o.findings[0].message.contains("behind"),
        "a publisher stamping an hour in the past must be described as behind: {}",
        o.findings[0].message
    );

    let note = checks::clock_offset_note(&snap, checks::PushStream::RingsUnderWriter, clock)
        .expect("two publishers have recorded offsets");
    assert!(
        note.contains("2 publisher clock offset(s)"),
        "the spread should cover the two publishers that pushed and not the \
         third edge, which has never been claimed and whose offset is the \
         arena's zero — a fleet member reported as perfectly synchronised \
         because nobody has ever measured it: {note}"
    );
}

/// **A stamp in the future travels from `EdgeWriter::push` to a `TFT005`
/// finding, on a real arena.**
///
/// The unit test in `checks.rs` sets `EdgeInfo::newest_stamp` by hand, which
/// proves the rule and proves nothing about the seam between the ring and the
/// check: `Snapshot::capture` reads the newest retained stamp out of
/// `SampleRing`, and `Clock::decide` has to put a wall-clock arena in its `Wall`
/// arm before the check runs at all. Neither is visible from a hand-built
/// `EdgeInfo`.
///
/// **The reference fixture cannot host this test, which is why there is a second
/// arena here rather than an addition to `run_on_fixture`.** `fixture::spin_up`
/// stamps from zero, so `Clock::decide` lands on `Clock::NewestStamp` and
/// `TFT005` skips there — the same reason `TFT004`'s end-to-end test builds its
/// own tree. Re-basing the fixture's stamps onto the wall clock would move
/// `NOW_NS`/`QUERY_NS`, which every latency and history benchmark keys on.
///
/// One publisher is healthy. The other stamps half a second ahead of this host's
/// clock, which is ten times `FUTURE_TOLERANCE_NS` — comfortably past the band
/// the constant exists to leave for a publisher that stamps just before it
/// pushes, and comfortably inside `OFFSET_BEYOND_ANY_PIPELINE_NS`, so `TFT004`
/// stays quiet and this is a statement about `TFT005` alone.
///
/// Mutant: in `Snapshot::capture`, drop the newest-stamp read and leave
/// `newest_stamp: None`. Applied: `TFT005` reports `Pass` and the `Fired`
/// assertion fails. Mutant B: return `Clock::NewestStamp` unconditionally from
/// `Clock::decide`. Applied: the `matches!(clock, Clock::Wall(_))` assertion
/// fails first, which is the point of asserting it.
#[test]
fn a_stamp_in_the_future_reaches_a_tft005_finding_on_a_real_arena() {
    use tf_tree::{Capacity, EdgeCfg, TreeBuilder};

    const AHEAD_NS: i64 = 500_000_000;

    let tree = TreeBuilder::new()
        .dynamic_edge("map", "odom", EdgeCfg::new(Capacity::slots(64)))
        .dynamic_edge("odom", "base", EdgeCfg::new(Capacity::slots(64)))
        .build()
        .expect("build");

    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    )
    .unwrap();

    let map = tree.frame("map").unwrap();
    let odom = tree.frame("odom").unwrap();
    let base = tree.frame("base").unwrap();

    let healthy = tree.claim(odom, map).expect("claim map->odom");
    let future = tree.claim(base, odom).expect("claim odom->base");
    for k in 0..12i64 {
        healthy
            .push(now + k * 1_000, &tf_tree::Iso3::IDENTITY)
            .unwrap();
        future
            .push(now + AHEAD_NS + k * 1_000, &tf_tree::Iso3::IDENTITY)
            .unwrap();
    }

    let snap = Snapshot::capture(&tree);
    let obs = Observations::from_arena(&tree, &snap);
    let stats = checks::collect_edge_stats(&tree, &snap);
    let clock = Clock::decide(&checks::newest_stamps(&snap), now);
    assert!(
        matches!(clock, Clock::Wall(_)),
        "these stamps are wall-clock time; if Clock::decide disagrees TFT005 skips \
         and this test asserts nothing: {clock:?}"
    );
    let report = checks::run(
        &Inputs {
            snap: &snap,
            obs: &obs,
            stats: &stats,
            host: None,
            clock,
            arena_bytes: tree.arena_size_bytes() as u64,
            occupancy: checks::occupancy_of(&tree),
            clock_step: &checks::ClockStepEvidence::capture(&snap, &obs),
            stream: checks::PushStream::RingsUnderWriter,
            slots: checks::SlotTable::Current,
            counters: tf_tree::counters_compiled_in(),
        },
        &BTreeSet::new(),
    );

    let o = report
        .outcomes
        .iter()
        .find(|o| o.check == Tft::Tft005)
        .expect("TFT005 must be in the report");
    assert_eq!(
        o.status,
        Status::Fired,
        "a publisher stamping half a second ahead of this host went unreported: {o:?}"
    );
    assert_eq!(
        o.findings.len(),
        1,
        "only one of the two publishers is ahead: {:?}",
        o.findings
    );
    assert!(
        o.findings[0].subject.contains("odom->base"),
        "the finding names the healthy edge instead of the future one: {}",
        o.findings[0].subject
    );
    assert!(
        o.findings[0]
            .message
            .contains("500 ms ahead of the wall clock"),
        "the distance the operator acts on must survive the round trip: {}",
        o.findings[0].message
    );

    // `TFT004` reads the same arena from the other side — `wall clock - stamp`,
    // recorded by the writer — and half a second is an ordinary stamp-to-push
    // latency there. Asserted so this fixture cannot quietly become a test of
    // two checks at once, which is what §11's "exactly that check and no other"
    // is about.
    let clocks = report
        .outcomes
        .iter()
        .find(|o| o.check == Tft::Tft004)
        .expect("TFT004 must be in the report");
    assert_ne!(
        clocks.status,
        Status::Fired,
        "500 ms is inside OFFSET_BEYOND_ANY_PIPELINE_NS, so TFT004 must stay quiet \
         and leave this fixture to TFT005: {clocks:?}"
    );

    drop(healthy);
    drop(future);
}

/// **`doctor --json` is parsed as JSON and checked against the schema
/// `render_json` documents — `docs/PHASE5.md` §11's *"schema-validated"*.**
///
/// Until this test the schema was a rustdoc code block and nothing compared it
/// to the bytes. What existed was positional: `tests/doctor_recording.rs` finds
/// the `"id"` line and then indexes `lines[at + 3]` for the status, with an
/// assertion that the field order has not moved — which is an explicit
/// acknowledgement that it is reading a layout rather than a document. Nothing
/// would have caught an unbalanced brace, a trailing comma, an unescaped byte
/// outside the one hostile-name unit test, a missing id, or a `reason` on a
/// check that ran.
///
/// **It runs the real binary**, so it covers the whole path a CI consumer uses:
/// `clap`, the fixture source, `checks::run`, `Meta`, `render_json`, stdout.
///
/// # What is asserted, and why each one
///
/// * **It parses.** The document is hand-written with `writeln!`, so this is the
///   one property no unit test on a `String` had.
/// * **The top-level key set is exactly the documented one**, in both
///   directions. A key present in the bytes and absent from `render_json`'s
///   schema block is an undocumented field a consumer will come to depend on;
///   a key in the block and absent from the bytes is a promise. §6 says adding a
///   field is compatible by construction — it stays compatible, and it now
///   requires editing the schema block and this list in the same commit, which
///   is the point.
///
///   **That last clause was false of the block until 2026-09-06 and it is the
///   defect worth recording**, because the assertion message named a comparison
///   nobody made. The wire document has **three** spellings — the block, the
///   `writeln!` emitter under it, and `expected_keys` here — and only the last
///   two were compared. Measured: emitting `"host_arch"` and adding it to
///   `expected_keys`, with the block untouched, left `cargo nextest run -p
///   tf_tree_cli` green, and `cargo test --doc` never sees the block because it
///   is fenced ```` ```text ````. [`documented_top_level_keys`] parses it now
///   and this list is held to it first, so the literal keeps its value (it is
///   what caught the `Tft::ALL` reorder mutant below) while the block stops
///   being a spelling nothing reads. Its **top level only** — see that
///   function's own doc for which lists that leaves uncoupled.
/// * **The schema string is pinned to a literal**, so `JSON_SCHEMA` moving is a
///   deliberate edit and not a typo. §6's rule is *bump only for an incompatible
///   change*, and nothing else in the tree reads that constant.
/// * **Every catalogue id appears exactly once, in id order.** `checks` always
///   carrying every id is what lets a consumer tell "did not fire" from "this
///   build does not have this check". The expected sequence is a literal here.
///   It was a fold over `Tft::ALL` until a review swapped two entries of that
///   array and watched the wire document reorder with this test still green:
///   `render_json`'s order *is* `Tft::ALL`'s, so comparing against it asserted
///   only membership, which the `[Tft; 19]` type already gives. The literal is
///   held to be ascending on its own terms, and to `Tft::ALL` as a set, so
///   neither an id missing from the wire nor a mis-typed literal passes.
/// * **`reason` is a string iff `status` is `"skipped"`.** A `Skipped` with no
///   reason is the specific dishonesty `Status::Skipped(String)` exists to
///   prevent, and a `reason` on a check that *ran* would read as a caveat on its
///   verdict.
/// * **The summary agrees with the arrays it summarises.** `summary.error`,
///   `warn` and `info` count findings — `uncatalogued` ones included, which is
///   what makes them agree with the exit status — and `passed`/`fired`/`not_run`
///   count checks: two different denominators in one object, which is how a
///   consumer comes to read one as the other. All three severities are compared;
///   the first version of this test compared `warn` alone, so the other two
///   agreed with nothing. The reference fixture produces no **error**-severity
///   finding, so that arm holds `0 == 0` against the findings arrays and catches
///   only an emitter that invents a count — the mutant below is what it is
///   worth.
/// * **`uncatalogued` is present** even with no producer: §6's amendment keeps
///   it in the schema deliberately.
///
/// Non-vacuity is asserted rather than assumed: the fixture must produce at
/// least one fired check with a finding and at least one skip, or every rule
/// above holds over an empty document.
///
/// # Mutants, all run
///
/// * Append a `,` after the `"uncatalogued"` array's closing bracket in
///   `render_json`. Applied: `serde_json::from_str` fails and the parse
///   assertion reports "trailing comma".
/// * Emit `TFT018` as `TFT019`'s id, which leaves the document *valid* — the
///   obvious mutant, dropping the last outcome, is caught by the parse instead,
///   because the comma logic keys on the array's last index and the id rule is
///   then never reached. Applied: the id sequence assertion fails on a list
///   ending `"TFT018", "TFT018"`.
/// * Emit `"reason": ""` for a check that passed. Applied: the reason/status
///   agreement fails on `TFT006`.
/// * Count *checks* at info severity into `summary.info` instead of findings.
///   Applied: the info arm fails at `2` against `1`.
/// * Emit the fired-check count as `summary.error`. Applied: the error arm
///   fails at `2` against `0`.
/// * `JSON_SCHEMA = "tf_tree.doctor/2"`. Applied: the pin fails. (The
///   `contains` check in `catalogue.rs`'s own unit tests does not: it reads the
///   constant, so it agrees with any value.)
/// * Swap `Tft::Tft018` and `Tft::Tft019` in `Tft::ALL`. Applied: the sequence
///   assertion fails on a list ending `"TFT019", "TFT018"`. Against the earlier
///   `Tft::ALL`-derived expectation this mutant was green, which is why the
///   literal exists.
/// * Add a `Tft020` variant to `Tft::ALL` and `continue` past it in
///   `checks::run`, so the catalogue carries an id the wire never emits — the
///   one shape the sequence assertion cannot see, since the literal and the
///   emitter would agree on the other nineteen. Applied: the set comparison
///   against `Tft::ALL` fails.
#[test]
fn the_json_report_parses_and_matches_its_documented_schema() {
    use serde_json::Value;

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_tf_tree"))
        .args(["doctor", "--json"])
        .output()
        .expect("run tf_tree doctor --json");
    let stdout = String::from_utf8(out.stdout).expect("the report must be UTF-8");
    let doc: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("doctor --json is not JSON ({e}):\n{stdout}"));

    // The keys `render_json`'s doc comment documents, and no others.
    let expected_keys = [
        "arena",
        "checks",
        "clock_source",
        "counters_compiled_in",
        "generated_unix_nanos",
        "notes",
        "now_nanos",
        "runtime_dir",
        "schema",
        "source",
        "summary",
        "tool_version",
        "uncatalogued",
    ];
    // **The literal is held to the block, and only then to the bytes.** The
    // sentence above claimed the emitted document was compared against
    // `render_json`'s schema block; nothing read that block, so what was
    // compared was the bytes against this literal, and a key added to the
    // emitter and to the literal in one commit left the block silently behind.
    // Measured: emitting one extra key and adding it here left the crate green,
    // and `cargo test --doc` never runs the block because it is fenced ```text.
    assert_eq!(
        documented_top_level_keys(),
        expected_keys.to_vec(),
        "the schema block and this list disagree"
    );

    let obj = doc.as_object().expect("the report is an object");
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys, expected_keys,
        "the emitted document and this list disagree"
    );

    assert_eq!(
        doc["schema"], "tf_tree.doctor/1",
        "the schema identifier is a wire contract; a bump is a deliberate edit"
    );
    assert_eq!(
        doc["schema"],
        Value::from(tf_tree_cli::catalogue::JSON_SCHEMA),
        "the constant and the emitted string must be the same string"
    );
    assert!(doc["now_nanos"].is_i64(), "{}", doc["now_nanos"]);
    assert!(doc["generated_unix_nanos"].is_i64());
    assert!(doc["counters_compiled_in"].is_boolean());
    assert!(doc["clock_source"].is_string() && doc["source"].is_string());
    assert!(doc["tool_version"].is_string());
    assert!(doc["runtime_dir"].is_null() || doc["runtime_dir"].is_string());
    for n in doc["notes"].as_array().expect("notes is an array") {
        assert!(n.is_string(), "a note must be a string: {n}");
    }
    assert!(
        doc["uncatalogued"].is_array(),
        "the uncatalogued key stays in the schema with no producer"
    );

    let arena = doc["arena"].as_object().expect("arena is an object");
    let mut arena_keys: Vec<&str> = arena.keys().map(String::as_str).collect();
    arena_keys.sort_unstable();
    assert_eq!(
        arena_keys,
        [
            "edges",
            "format_version",
            "frames",
            "instance",
            "layout_hash",
            "rings"
        ]
    );
    let rings = arena["rings"].as_object().expect("rings is an object");
    let mut ring_keys: Vec<&str> = rings.keys().map(String::as_str).collect();
    ring_keys.sort_unstable();
    assert_eq!(
        ring_keys,
        [
            "bytes_per_slot",
            "declared_bytes",
            "declared_slots",
            "edges",
            "rounding_slack_bytes_max",
            "rounding_slack_slots_max",
            "used_bytes",
            "used_slots",
        ]
    );

    // Every id, exactly once, in id order. The expected sequence is written
    // out here rather than folded from `Tft::ALL`, because `render_json`
    // iterates `Tft::ALL` too: a comparison against it pins whatever order that
    // array happens to be in, and swapping two of its entries reorders the wire
    // document with the assertion still green. The literal is the wire
    // contract's own source; the sorted check below is what keeps the literal
    // itself from encoding a wrong order.
    let checks = doc["checks"].as_array().expect("checks is an array");
    let ids: Vec<&str> = checks
        .iter()
        .map(|c| c["id"].as_str().expect("an id is a string"))
        .collect();
    let wire_order = [
        "TFT001", "TFT002", "TFT003", "TFT004", "TFT005", "TFT006", "TFT007", "TFT008", "TFT009",
        "TFT010", "TFT011", "TFT012", "TFT013", "TFT014", "TFT015", "TFT016", "TFT017", "TFT018",
        "TFT019",
    ];
    assert!(
        wire_order.windows(2).all(|w| w[0] < w[1]),
        "the literal above is the order assertion's only independent source, so it \
         has to be strictly ascending on its own terms"
    );
    assert_eq!(
        ids, wire_order,
        "the checks array must carry every catalogue id, once, in id order"
    );
    // And the literal is held to the catalogue from the other side: an id added
    // to `Tft::ALL` that never reaches the wire would satisfy the sequence
    // assertion above, since both the emitter and a stale literal would omit it.
    let mut catalogued: Vec<&str> = Tft::ALL.iter().map(|c| c.id()).collect();
    catalogued.sort_unstable();
    let mut on_the_wire = wire_order.to_vec();
    on_the_wire.sort_unstable();
    assert_eq!(
        catalogued, on_the_wire,
        "a catalogue id that --json never emits is invisible to every consumer"
    );

    let (mut pass, mut fired, mut skipped) = (0usize, 0usize, 0usize);
    // Findings by severity — all three, because `summary.error`, `warn` and
    // `info` are one denominator and `passed`/`fired`/`not_run` are another,
    // and a test that checks only `warn` leaves the other two agreeing with
    // nothing. (It checked only `warn` until a review said so.)
    let mut findings_at: BTreeMap<&str, usize> = BTreeMap::new();
    for c in checks {
        let id = c["id"].as_str().unwrap();
        let mut ck: Vec<&str> = c
            .as_object()
            .unwrap_or_else(|| panic!("{id} is not an object"))
            .keys()
            .map(String::as_str)
            .collect();
        ck.sort_unstable();
        assert_eq!(
            ck,
            [
                "findings",
                "id",
                "reason",
                "severity",
                "status",
                "suppressed",
                "title"
            ],
            "{id}'s field set disagrees with the schema block"
        );
        assert!(c["title"].is_string() && c["suppressed"].is_boolean());
        let severity = c["severity"].as_str().expect("a severity is a string");
        assert!(
            ["error", "warn", "info"].contains(&severity),
            "{id} has severity {severity:?}"
        );
        let status = c["status"].as_str().expect("a status is a string");
        match status {
            "pass" => pass += 1,
            "fired" => fired += 1,
            "skipped" => skipped += 1,
            other => panic!("{id} has an undocumented status {other:?}"),
        }
        assert_eq!(
            c["reason"].is_string(),
            status == "skipped",
            "{id} is {status:?} and its reason is {}: a reason is exactly what a skip \
             carries and what a verdict does not",
            c["reason"]
        );
        if let Some(why) = c["reason"].as_str() {
            assert!(
                why.len() > 20,
                "{id} skipped without a usable reason: {why:?}"
            );
        }
        let fs = c["findings"].as_array().expect("findings is an array");
        for f in fs {
            let mut fk: Vec<&str> = f.as_object().unwrap().keys().map(String::as_str).collect();
            fk.sort_unstable();
            assert_eq!(fk, ["edge", "message", "subject"], "{id}'s finding shape");
            assert!(f["edge"].is_null() || f["edge"].is_u64());
            assert!(f["subject"].is_string() && f["message"].is_string());
        }
        if !c["suppressed"].as_bool().unwrap() {
            *findings_at.entry(severity).or_default() += fs.len();
        }
    }

    // Two different denominators in one object: checks, and findings.
    let s = &doc["summary"];
    let mut sk: Vec<&str> = s.as_object().unwrap().keys().map(String::as_str).collect();
    sk.sort_unstable();
    assert_eq!(
        sk,
        [
            "error",
            "fired",
            "info",
            "not_run",
            "passed",
            "suppressed",
            "warn"
        ]
    );
    assert_eq!(s["passed"], pass, "summary.passed disagrees with the array");
    assert_eq!(s["fired"], fired, "summary.fired disagrees with the array");
    assert_eq!(s["not_run"], skipped, "summary.not_run disagrees");
    // `summary.error`/`warn`/`info` count `uncatalogued` findings too, which is
    // what makes them agree with the process exit status, so they go into the
    // same buckets.
    for f in doc["uncatalogued"]
        .as_array()
        .expect("uncatalogued is an array")
    {
        let sev = f["severity"].as_str().expect("a severity is a string");
        *findings_at.entry(sev).or_default() += 1;
    }
    for sev in ["error", "warn", "info"] {
        assert_eq!(
            s[sev],
            findings_at.get(sev).copied().unwrap_or(0),
            "summary.{sev} counts *findings* at that severity, not checks: {s}"
        );
    }

    // Non-vacuity: without these every rule above holds over an empty document.
    assert!(fired > 0, "the fixture must fire something:\n{stdout}");
    assert!(skipped > 0, "the fixture must skip something:\n{stdout}");
    assert!(
        checks
            .iter()
            .any(|c| !c["findings"].as_array().unwrap().is_empty()),
        "no finding in the whole report, so the finding shape was never checked"
    );
}

/// **`TFT013`'s grace evidence is unobtainable on an arena whose rings retain
/// one sample, and the skip it printed there stated something false about the
/// arena.**
///
/// The grace period is `(head - 1) x median period`, and `doctor::median_period`
/// needs two retained samples. `Capacity::history` is
/// `next_pow2(ceil(rate_hz * secs))` and `SampleRing::retained` is
/// `capacity - 1`, so an edge declared `rate_hz * secs <= 2` retains **one**
/// sample however long its publisher runs — the median is `None`, the whole
/// check skips, and it skipped saying *nothing in this arena has published a
/// measurable stream … an edge with head == 0 is what every dynamic edge of a
/// correct arena reads as at bringup. TFT017 is the id for an edge whose writer
/// is gone*. The publisher below has accepted 3 600 pushes.
///
/// It is an integration test rather than a unit one because the claim under test
/// is **reachability**: a hand-built `Snapshot` can be given `head = 3600` and
/// one observation by fiat, which proves the reason and not that any arena is
/// ever in that state. This builds the ring through the shipped `Capacity` and
/// lets `Observations::from_arena` decide how many samples come back.
///
/// **The second arena below is the same defect one level down.**
/// `Unmeasurable` is reached whenever *no* dynamic edge yields two
/// observations, and a ring size is only one of the two ways to get there: a
/// publisher that has pushed once into a 512-slot ring — every `doctor
/// --attach` at bringup — is in it with 511 slots free. A skip that answered
/// *make the ring bigger* there would be the ring-size sentence stating
/// something false about the arena it printed on, which is what the first
/// arena's assertions exist to stop.
///
/// Mutant, run: fold `PublishActivity::Unmeasurable` back into `NoPublisher`.
/// The status is unchanged and the reason assertion fails — which is the whole
/// finding, since the defect was never a wrong verdict.
/// **Mutant, run:** delete the `retained_capacity < 2` branch and print the
/// ring-size clause unconditionally. The first arena still passes and the
/// second one fails on `!why.contains("four slots")`.
#[test]
fn tft013_skips_with_the_ring_size_reason_on_an_arena_whose_publisher_it_cannot_measure() {
    use tf_tree::{Capacity, EdgeCfg, TreeBuilder};

    // Two slots, one retained sample, for the life of the arena.
    let slow = Capacity::history(1.0, 2.0);
    assert_eq!(
        slow.get(),
        2,
        "if this stops rounding to two the fixture no longer reaches the state"
    );

    let tree = TreeBuilder::new()
        .dynamic_edge("odom", "base_footprint", EdgeCfg::new(slow))
        // The fault `TFT013` exists to name, present and unreportable: declared
        // dynamic and never published to.
        .dynamic_edge(
            "base_footprint",
            "base_link",
            EdgeCfg::new(Capacity::slots(64)),
        )
        .build()
        .expect("build");

    let odom = tree.frame("odom").unwrap();
    let foot = tree.frame("base_footprint").unwrap();
    let w = tree.claim(foot, odom).expect("claim odom->base_footprint");
    for k in 0..3_600i64 {
        w.push(k * 1_000_000_000, &tf_tree::Iso3::IDENTITY).unwrap();
    }

    let snap = Snapshot::capture(&tree);
    let obs = Observations::from_arena(&tree, &snap);
    let stats = checks::collect_edge_stats(&tree, &snap);

    // Non-vacuity, both halves: the publisher really has published, and the
    // arena really cannot yield it a median period.
    assert_eq!(
        snap.edges.iter().map(|e| e.head).max(),
        Some(3_600),
        "the busiest edge must have published, or the reason under test is right"
    );
    assert_eq!(
        obs.by_edge().values().map(Vec::len).max(),
        Some(1),
        "a two-slot ring must retain one sample, or this arena is not the one \
         the finding is about"
    );

    let report = checks::run(
        &Inputs {
            snap: &snap,
            obs: &obs,
            stats: &stats,
            host: None,
            clock: Clock::Wall(3_600_000_000_000),
            arena_bytes: tree.arena_size_bytes() as u64,
            occupancy: checks::occupancy_of(&tree),
            clock_step: &checks::ClockStepEvidence::capture(&snap, &obs),
            stream: checks::PushStream::RingsUnderWriter,
            slots: checks::SlotTable::Current,
            counters: tf_tree::counters_compiled_in(),
        },
        &BTreeSet::new(),
    );

    let o = report
        .outcomes
        .iter()
        .find(|o| o.check == Tft::Tft013)
        .expect("TFT013 must be in the report");
    match &o.status {
        Status::Skipped(why) => {
            assert!(
                why.contains("has publishers") && why.contains("3600 push(es)"),
                "the reason must name the publishers this arena has: {why}"
            );
            assert!(
                !why.contains("nothing in this arena has published"),
                "the arena has published 3 600 times; this sentence is false of it: {why}"
            );
            assert!(
                why.contains("four slots") && why.contains("no ring in it can hold two"),
                "on THIS arena the obstacle is the ring size and the reason has to say so: \
                 {why}"
            );
        }
        other => panic!("TFT013 reported {other:?} on an arena it cannot measure a grace on"),
    }

    // The catalogue's rule that one fault is one id: no two checks may report a
    // finding about the same subject. `TFT017` owns the unpublished edge here.
    let mut seen: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for oc in &report.outcomes {
        for f in &oc.findings {
            seen.entry(f.subject.as_str())
                .or_default()
                .push(oc.check.id());
        }
    }
    // An empty subject set is not a pass: if nothing fired at all the loop
    // below runs no comparison and this section asserts nothing.
    assert!(
        seen.values().flatten().any(|id| *id == "TFT017"),
        "the never-published edge must reach TFT017, or the duplicate check \
         below has no subjects: {seen:?}"
    );
    for (subject, ids) in &seen {
        let unique: BTreeSet<&&str> = ids.iter().collect();
        assert_eq!(
            unique.len(),
            1,
            "one fault, two warn ids on {subject}: {ids:?}"
        );
    }

    // The other arena that reaches the same variant: a 512-slot ring with one
    // push in it, which is every `doctor --attach` issued at bringup. The
    // obstacle here is the stream, not the ring, and a ring-size remedy printed
    // over it is the sentence this whole test exists to keep out of the report.
    let tree = TreeBuilder::new()
        .dynamic_edge("odom", "base_footprint", EdgeCfg::new(Capacity::slots(512)))
        .dynamic_edge(
            "base_footprint",
            "base_link",
            EdgeCfg::new(Capacity::slots(64)),
        )
        .build()
        .expect("build");
    let odom = tree.frame("odom").unwrap();
    let foot = tree.frame("base_footprint").unwrap();
    let w = tree.claim(foot, odom).expect("claim odom->base_footprint");
    w.push(0, &tf_tree::Iso3::IDENTITY).unwrap();

    let snap = Snapshot::capture(&tree);
    let obs = Observations::from_arena(&tree, &snap);
    let stats = checks::collect_edge_stats(&tree, &snap);
    // Non-vacuity: the ring is large and the stream is what is short, or this
    // arena is not the one the second half of the finding is about.
    assert_eq!(
        snap.edges.iter().map(|e| e.head).max(),
        Some(1),
        "one push, or the arena under test is a different one"
    );
    assert_eq!(
        obs.by_edge().values().map(Vec::len).max(),
        Some(1),
        "one retained sample out of 511 slots, or the arena is a different one"
    );

    let report = checks::run(
        &Inputs {
            snap: &snap,
            obs: &obs,
            stats: &stats,
            host: None,
            clock: Clock::Wall(3_600_000_000_000),
            arena_bytes: tree.arena_size_bytes() as u64,
            occupancy: checks::occupancy_of(&tree),
            clock_step: &checks::ClockStepEvidence::capture(&snap, &obs),
            stream: checks::PushStream::RingsUnderWriter,
            slots: checks::SlotTable::Current,
            counters: tf_tree::counters_compiled_in(),
        },
        &BTreeSet::new(),
    );
    let o = report
        .outcomes
        .iter()
        .find(|o| o.check == Tft::Tft013)
        .expect("TFT013 must be in the report");
    match &o.status {
        Status::Skipped(why) => {
            assert!(
                why.contains("1 push(es)") && why.contains("rings are large enough"),
                "the reason must name the obstacle this arena actually has: {why}"
            );
            assert!(
                !why.contains("four slots") && !why.contains("RingSize::History"),
                "511 slots are free; a ring-size remedy is false about this arena: {why}"
            );
        }
        other => panic!("TFT013 reported {other:?} on an arena it cannot measure a grace on"),
    }
}

/// The **top-level** keys `catalogue::render_json`'s rustdoc schema block
/// documents, sorted.
///
/// Source-text parsing, because the block is fenced ```` ```text ```` — rustdoc
/// never runs it and `#[doc = ...]` takes no `const`, so there is no way to
/// interpolate one list into both. What this buys is that the block stops being
/// a spelling nothing compares: the literal in
/// [`the_json_report_parses_and_matches_its_documented_schema`] is held to it,
/// and the bytes are held to the literal, so all three move together.
///
/// # What it does not recover
///
/// **Only the top level.** The block spells nested shapes inline
/// (`"rings": { "edges": u32, … }`), so `arena`, `arena.rings`, the per-check
/// object, `summary` and the per-finding object are still compared against
/// literals in that test and against nothing else. Writing a parser for the
/// nested shapes would be a second, partial JSON grammar in a test file, which
/// is a worse trade than naming the lists that stay uncoupled — the sentence
/// above enumerates them, which is what a count of them would replace.
///
/// The rule is "a `///` line whose content begins at exactly three spaces and a
/// quote", which is what makes the top level separable from a continuation line
/// — every nested key in the block is indented past its opening brace.
///
/// **Mutant, run:** emit and document one extra top-level key, then delete it
/// from the block alone. The assertion fails with the 13 documented keys on the
/// left and 14 on the right.
fn documented_top_level_keys() -> Vec<&'static str> {
    let src = include_str!("../src/catalogue.rs");
    let (_, after) = src
        .split_once("/// # Schema — stable")
        .expect("render_json's schema block must be findable by its heading");
    let (_, body) = after
        .split_once("/// ```text")
        .expect("the schema block opens with a text fence");
    let (block, _) = body
        .split_once("/// ```")
        .expect("the schema block is closed");

    let mut keys: Vec<&str> = block
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix("///"))
        .filter_map(|l| l.strip_prefix("   \""))
        .filter_map(|l| l.split_once('"'))
        .map(|(key, _)| key)
        .collect();
    // Non-vacuity: a parse that found nothing would make the comparison above
    // an assertion that the emitter documents no keys at all, which is the
    // empty-subject-set pass this repository keeps finding.
    assert!(
        keys.len() > 5,
        "the schema block parse recovered {} key(s); the block's shape has \
         changed and this helper is reading nothing",
        keys.len()
    );
    keys.sort_unstable();
    keys
}
