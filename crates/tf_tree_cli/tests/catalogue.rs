//! End-to-end: run the `docs/PHASE5.md` §6 catalogue against the real benchmark
//! fixture. `checks.rs` unit tests prove a check fires; this proves the
//! catalogue stays quiet on a correct, fully populated, live tree.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};

use tf_tree_cli::catalogue::{Severity, Status, Tft};
use tf_tree_cli::checks::{self, Clock, Inputs};
use tf_tree_cli::doctor::{Observations, Snapshot};

/// Run the catalogue over the fixture with its publishers still holding their claims.
fn run_on_fixture<R>(f: impl FnOnce(&tf_tree_cli::catalogue::Report, &Snapshot) -> R) -> R {
    let tree = tf_tree_bench::fixture::build_tree().expect("build fixture");
    // Writers are held for the whole run: dropping them releases the claims and
    // makes `TFT017` fire on all four dynamic edges.
    let (writers, samples) = tf_tree_bench::fixture::spin_up(&tree).expect("populate history");

    let snap = Snapshot::capture(&tree);
    let obs = Observations::from_samples(samples);
    let stats = checks::collect_edge_stats(&tree, &snap);
    let clock = Clock::decide(&checks::newest_stamps(&snap), 1_700_000_000_000_000_000);
    let inputs = Inputs {
        snap: &snap,
        obs: &obs,
        stats: &stats,
        // `None` on purpose: `TFT016` reads this host's `/sys` and `/proc`.
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

/// A healthy, fully published tree produces no finding except `TFT015` (the
/// fixture's arena is sized exactly to its topology, so 100% occupancy is true).
/// The claim owner word is `(epoch << 16) | (slot + 1)`, so a decode bug in
/// `Snapshot::capture` shows up here as a `TFT014` false positive.
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

        // Non-vacuity: the fixture is populated and claimed.
        assert_eq!(snap.frames.len(), 24);
        assert_eq!(snap.edges.len(), 23);
        assert!(
            snap.edges.iter().any(|e| e.head > 100 && e.claimed),
            "the fixture must be published-into and claimed, or this test is vacuous"
        );
    });
}

/// Every check the catalogue could not run states why, and none is missing from
/// the report.
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
        // Non-vacuity: this build cannot run several checks.
        assert!(skipped >= 5, "expected several skips, saw {skipped}");
    });
}

/// `--exit-code` has two tiers: `error` (in practice `TFT006`, `TFT012` on a
/// live arena) and `warn`, which is warn-and-above, so an arena with a cycle
/// must not pass `--exit-code warn`. `Report::is_healthy` is pinned against an
/// error-only report in `catalogue.rs`.
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

/// `--json` and `--exit-code` must agree: a finding in the `uncatalogued` array
/// (part of the stable JSON schema) is counted by the summary a CI job reads, so
/// one is injected explicitly. [`Uncatalogued`] is that type.
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

/// Every occupancy row `TFT015` reports must be capable of being non-zero: the
/// `participants` row is omitted and disclosed in `Meta.notes`, because
/// `ArenaHeader::participant_count` is never incremented. The participant
/// census itself is guarded by `attach.rs`'s
/// `the_two_participant_censuses_disagree_by_the_read_only_population`.
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

/// The whole `TFT007` path end to end: a topology file's `rate_hz` reaches the
/// arena, is read back, and judges a real publisher. The seam under test is
/// `Snapshot::capture` mapping the record's `0` sentinel to `None`. One edge
/// declares a rate and one does not, both published at the same wrong rate.
#[test]
fn a_topology_files_declared_rate_reaches_doctor_and_judges_the_publisher() {
    use tf_tree_bridge::TopologyConfig;

    // 20 Hz declared, 2 s of history -> a 64-slot ring; the sibling declares nothing.
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

    // The arena discloses that one of its two edges was never compared.
    let note = checks::rate_coverage_note(&snap, &obs, clock, checks::PushStream::RingsUnderWriter)
        .expect("a partial run must say so");
    assert!(note.contains("compared 1 of 2"), "{note}");

    drop(declared);
    drop(undeclared);
}

/// `TFT014` sees the wedge on a real arena through the real liveness predicate
/// (`Tree::participant_alive`). The `LIVE` record is injected with `register_at`
/// (staged); the real-death case is `crates/tf_tree/tests/rendezvous.rs`'s
/// `the_hangup_frees_a_joiners_slot_and_leaves_the_owners_live` (0028). The pid
/// is `u32::MAX`: it exceeds every `pid_max`, so it is deterministically gone.
/// The healthy half of the arena is asserted too.
#[test]
fn a_stale_live_participant_record_is_reported_on_a_real_arena() {
    const GONE: u32 = u32::MAX;

    let tree = tf_tree_bench::fixture::build_tree().expect("build fixture");
    let (writers, samples) = tf_tree_bench::fixture::spin_up(&tree).expect("populate history");

    // Slot 1 is the record a killed writer leaves: a complete registration
    // whose process then died.
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
    // `byte not probed`: an in-process arena with no rendezvous, so the verdict
    // rests on `/proc` alone and the subject says so.
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

/// The CLI's `counters` feature must control the engine's: a dependency without
/// `default-features = false` re-enables `tf_tree/counters`. Only meaningful
/// under `--no-default-features`.
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

/// A publisher's clock offset travels from `EdgeWriter::push` to a `TFT004`
/// finding on a real arena (sampler, `ClaimRecord` field, and `Snapshot::capture`
/// mapping the `0` sentinel to `None`). Both edges use wall-clock stamps, the
/// only configuration in which `TFT004` runs; one publisher is an hour behind.
#[test]
fn a_publishers_clock_offset_reaches_a_tft004_finding_on_a_real_arena() {
    use tf_tree::{Capacity, EdgeCfg, TreeBuilder};

    const HOUR_NS: i64 = 3_600 * 1_000_000_000;

    let tree = TreeBuilder::new()
        .dynamic_edge("map", "odom", EdgeCfg::new(Capacity::slots(64)))
        .dynamic_edge("odom", "base", EdgeCfg::new(Capacity::slots(64)))
        // Never claimed and never pushed: makes the `0 -> None` mapping load-bearing.
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
    // Broken: an hour behind.
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

/// A stamp in the future travels from `EdgeWriter::push` to a `TFT005` finding
/// on a real arena. The reference fixture stamps from zero (`Clock::NewestStamp`,
/// so `TFT005` skips), hence a second arena with wall-clock stamps. One publisher
/// stamps half a second ahead: past `FUTURE_TOLERANCE_NS`, under
/// `OFFSET_BEYOND_ANY_PIPELINE_NS`, so `TFT004` stays quiet.
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

    // `TFT004` must stay quiet, so this tests `TFT005` alone.
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

/// `doctor --json` is parsed as JSON and checked against the schema
/// `render_json` documents (`docs/PHASE5.md` §11, "schema-validated").
///
/// Asserted: it parses; the top-level key set equals the documented one
/// ([`documented_top_level_keys`]) and a literal; the schema string is pinned;
/// every catalogue id appears once, in id order, against a literal (a fold over
/// `Tft::ALL` would only assert membership) and to `Tft::ALL` as a set; `reason`
/// is a string iff `status` is `"skipped"`; `summary.error/warn/info` count
/// findings (`uncatalogued` included) while `passed/fired/not_run` count checks;
/// `uncatalogued` is present (§6). The fixture must produce a fired check and a
/// skip, or every rule holds over an empty document.
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

    // The keys `render_json` documents, and no others.
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

    // Every id, exactly once, in id order, against a literal (see the test doc).
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
    let mut catalogued: Vec<&str> = Tft::ALL.iter().map(|c| c.id()).collect();
    catalogued.sort_unstable();
    let mut on_the_wire = wire_order.to_vec();
    on_the_wire.sort_unstable();
    assert_eq!(
        catalogued, on_the_wire,
        "a catalogue id that --json never emits is invisible to every consumer"
    );

    let (mut pass, mut fired, mut skipped) = (0usize, 0usize, 0usize);
    // Findings by severity, all three.
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

    // Non-vacuity: without these every rule holds over an empty document.
    assert!(fired > 0, "the fixture must fire something:\n{stdout}");
    assert!(skipped > 0, "the fixture must skip something:\n{stdout}");
    assert!(
        checks
            .iter()
            .any(|c| !c["findings"].as_array().unwrap().is_empty()),
        "no finding in the whole report, so the finding shape was never checked"
    );
}

/// `TFT013`'s skip must not state something false about an arena whose rings
/// retain one sample (`PublishActivity::Unmeasurable`, not `NoPublisher`): the
/// grace period needs two retained samples for a median period. A second arena,
/// one push into a 512-slot ring (every `doctor --attach` at bringup), reaches
/// the same variant without the ring-size cause.
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
        // The fault `TFT013` names: declared dynamic, never published to.
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

    // Non-vacuity: the publisher has published and the arena yields no median period.
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

    // One fault is one id: no two checks report a finding about the same subject.
    let mut seen: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for oc in &report.outcomes {
        for f in &oc.findings {
            seen.entry(f.subject.as_str())
                .or_default()
                .push(oc.check.id());
        }
    }
    // An empty subject set is not a pass: the loop below would assert nothing.
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

    // The other arena reaching the same variant: 512 slots, one push.
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
    // Non-vacuity: the ring is large and the stream is what is short.
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

/// The top-level keys `catalogue::render_json`'s rustdoc schema block documents,
/// sorted. Parsed from source (the block is fenced `text`): a `///` line whose
/// content begins at exactly three spaces and a quote. Nested shapes are not
/// parsed and stay compared against literals in the test.
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
    // Non-vacuity: a parse that found nothing would compare against no keys.
    assert!(
        keys.len() > 5,
        "the schema block parse recovered {} key(s); the block's shape has \
         changed and this helper is reading nothing",
        keys.len()
    );
    keys.sort_unstable();
    keys
}
