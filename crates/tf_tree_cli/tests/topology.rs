//! End-to-end for `docs/PHASE4.md` §5.8's resolution, against a **real** robot.
//!
//! The unit tests in `tf_tree_bridge` build each property from a hand-written
//! stream, which proves each piece works on input its author chose. This drives
//! the whole chain — discover, emit, reparse, build, ingest — over
//! `testdata/tfstream/indoor_atelier.tfstream`: 4 static edges, 5 dynamic,
//! 1066 samples from a ROSBOT PLUS indoor run (provenance and CC BY 4.0
//! attribution in that directory's `ATTRIBUTION.md`).
//!
//! What a real recording has that a fixture does not: irregular periods,
//! duplicated stamps, quaternions that are only approximately normalised, and a
//! topology nobody chose to make convenient. Every one of those is a way the
//! chain could be wrong while every unit test passes.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use tf_tree_bridge::{Action, Ingest, Publisher, Sample, Topic, TopologyConfig};
use tf_tree_cli::topology::discover_from_tfstream;

fn stream_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/tfstream/indoor_atelier.tfstream")
}

/// **The whole chain, on a real robot's `/tf`.**
///
/// Discover the topology, print it, parse the printed file back, build the
/// arena, and check the arena's edges are the ones the recording contains — and
/// that every dynamic edge is *claimable*, which is precisely what §5.8's
/// amendment says an `edge_headroom` slot is not.
///
/// The counts are asserted against the recording's own header (4 static, 5
/// dynamic): a test that only checked "some edges were found" would pass on a
/// discover that silently dropped half the stream.
///
/// Mutant: feed the `/tf` samples before the statics in
/// `discover_from_tfstream` ⇒ nothing changes here (the two sets are disjoint
/// in this recording), so the mutant this actually kills is the one that
/// matters: emit the discovered rate as `Slots(count)` rather than
/// `History { rate, secs }` and `left_front_link`'s ring becomes 256 slots for
/// a 47-second recording at 4.79 Hz, failing the capacity assertion below.
#[test]
fn a_real_recording_discovers_a_topology_that_builds_and_is_claimable() {
    let d = discover_from_tfstream(&stream_path(), 10.0, None, None).expect("discover");
    assert!(
        d.multi_parent().is_empty(),
        "this recording has a single-parent tree: {:?}",
        d.multi_parent()
    );
    assert!(d.kind_clashes().is_empty(), "{:?}", d.kind_clashes());
    assert_eq!(d.dropped_bad_name(), 0);

    let config = d.to_config();
    let text = config.to_toml();
    let reparsed = TopologyConfig::parse(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
    assert_eq!(
        config, reparsed,
        "the printed file is the discovered config"
    );

    // The recording's own header says 4 static edges and 5 dynamic ones.
    let statics = config
        .edges
        .iter()
        .filter(|e| matches!(e.shape, tf_tree_bridge::EdgeShape::Static { .. }))
        .count();
    assert_eq!(statics, 4);
    assert_eq!(config.edges.len() - statics, 5);

    // 4.79 Hz for 10 s is 48 samples, which `Capacity` rounds to 64. A ring
    // sized from the sample *count* instead would be 256 — four times the
    // memory for history nobody asked for.
    let wheel = config
        .edge("base_link", "left_front_link")
        .expect("the recording's fastest edge");
    match wheel.shape {
        tf_tree_bridge::EdgeShape::Dynamic { ring } => {
            assert_eq!(ring.capacity().get(), 64, "10 s at the measured 4.79 Hz");
        }
        ref other => panic!("{other:?}"),
    }

    let tree = config.builder().build().expect("build the declared arena");
    for e in &config.edges {
        let parent = tree.frame(&e.parent).expect(&e.parent);
        let child = tree.frame(&e.child).expect(&e.child);
        let claim = tree.claim(child, parent);
        match e.shape {
            // §5.8's amendment in one assertion: a declared dynamic edge is
            // claimable, which is the thing `edge_headroom`'s zero-capacity
            // slots can never be.
            tf_tree_bridge::EdgeShape::Dynamic { .. } => {
                assert!(
                    claim.is_ok(),
                    "{} -> {}: {:?}",
                    e.parent,
                    e.child,
                    claim.err()
                );
            }
            // …and a static edge is not, because it has no ring. The file has
            // to say which, and this is why.
            tf_tree_bridge::EdgeShape::Static { .. } => {
                assert!(claim.is_err(), "{} -> {}", e.parent, e.child);
            }
        }
    }
}

/// **The discovered config accepts the stream it was discovered from** — every
/// transform, with nothing undeclared and nothing unaccounted for.
///
/// This is the property that makes `--discover` an operator workflow rather
/// than a report: run it, ship the file, and the bridge takes the same traffic.
/// If discovery and ingest disagreed about *any* key — a stripped slash, a
/// normalized name, a kind — this fails with a non-zero `dropped_undeclared`,
/// and no unit test on either side would notice, because each is self-consistent.
///
/// Mutant: drop the `NameNormalizer` from `Discovery::observe` ⇒ still passes
/// here (this recording's names carry no leading slash), so the mutant that
/// kills it is one that changes the key on *one* side: make `Discovery` key on
/// `(child, parent)` and every one of the 1070 transforms is reported
/// undeclared.
#[test]
fn the_discovered_config_accepts_the_stream_it_came_from() {
    let d = discover_from_tfstream(&stream_path(), 10.0, None, None).expect("discover");
    let config = d.to_config();

    let stream = tf_tree_bench::replay::TfStream::load(&stream_path()).expect("load");
    let mut ingest = Ingest::new(&config);
    let publisher = Publisher::Node("/robot_state_publisher".to_string());

    // The latched statics, delivered twice — which is what a transient-local
    // subscription actually sees when a second late joiner appears. Both
    // deliveries must be silently verified against the declared constant, not
    // reported as conflicts (§5.7), and this is the case §5.8 re-aimed that
    // machinery at.
    let mut verified = 0u64;
    for _ in 0..2 {
        for (parent, child, iso) in &stream.static_edges {
            let s = Sample {
                frame_id: parent.clone(),
                child_frame_id: child.clone(),
                stamp_nanos: 0,
                pose: [
                    iso.q.w, iso.q.x, iso.q.y, iso.q.z, iso.t.x, iso.t.y, iso.t.z,
                ],
            };
            match ingest.offer(Topic::TfStatic, &s, &publisher) {
                Action::StaticVerified { .. } => verified += 1,
                other => panic!("{parent} -> {child}: {other:?}"),
            }
        }
    }
    assert_eq!(verified, 8, "4 static edges, delivered twice");

    let mut published = 0u64;
    for s in &stream.samples {
        let (parent, child) = &stream.dynamic_edges[s.edge];
        let sample = Sample {
            frame_id: parent.clone(),
            child_frame_id: child.clone(),
            stamp_nanos: s.stamp_ns,
            pose: [
                s.pose.q.w, s.pose.q.x, s.pose.q.y, s.pose.q.z, s.pose.t.x, s.pose.t.y, s.pose.t.z,
            ],
        };
        match ingest.offer(Topic::Tf, &sample, &publisher) {
            Action::Publish { .. } => published += 1,
            // A real recording interleaves five edges, so a sample can arrive
            // with a stamp behind one already seen on another edge. §5.5's
            // clock guard is global by design (it tracks the *bridge's* notion
            // of now), so this is expected and is not a discovery defect —
            // what matters is that nothing is undeclared.
            Action::Drop {
                reason: tf_tree_bridge::DropReason::NonMonotonic { .. },
            } => {}
            other => panic!("{parent} -> {child} @ {}: {other:?}", s.stamp_ns),
        }
    }

    let stats = ingest.stats();
    assert_eq!(
        stats.dropped_undeclared,
        0,
        "discovery and ingest must agree on every key; undeclared: {:?}",
        ingest.undeclared()
    );
    assert_eq!(stats.static_conflicts, 0);
    assert_eq!(stats.dropped_bad_name, 0);
    assert_eq!(stats.dropped_kind_change, 0);
    assert!(stats.balanced(), "{stats:?}");
    assert!(
        published > 900,
        "the bulk of a 1066-sample recording must be published, got {published}"
    );
}

/// **A config that names an edge the robot does not publish is fine; one that
/// omits an edge the robot does publish is not, and the counter says so.**
///
/// The asymmetry is the point. An over-declared edge costs ring slots and
/// answers `NoData`; an under-declared one silently swallows traffic, and the
/// only downstream symptom is a lookup returning `NoPath`. §5.8 requires the
/// second to be counted and diagnosed once per edge naming both frames.
///
/// Mutant: count an undeclared transform as `dropped_bad_name` instead ⇒ the
/// operator is told their frame names are wrong when their config is short.
#[test]
fn an_edge_missing_from_the_config_is_counted_and_named_once() {
    let d = discover_from_tfstream(&stream_path(), 10.0, None, None).expect("discover");
    let mut config = d.to_config();
    // Remove the odometry edge — the single most consequential one to forget,
    // and the one whose absence a `map -> base_link` lookup reports as `NoPath`.
    let before = config.edges.len();
    config
        .edges
        .retain(|e| e.child != "base_footprint" || e.parent != "odom_combined");
    assert_eq!(
        config.edges.len(),
        before - 1,
        "the fixture must remove one"
    );

    let stream = tf_tree_bench::replay::TfStream::load(&stream_path()).expect("load");
    let mut ingest = Ingest::new(&config);
    let publisher = Publisher::Node("/ekf".to_string());
    let mut first_times = 0u64;
    for s in &stream.samples {
        let (parent, child) = &stream.dynamic_edges[s.edge];
        let sample = Sample {
            frame_id: parent.clone(),
            child_frame_id: child.clone(),
            stamp_nanos: s.stamp_ns,
            pose: [
                s.pose.q.w, s.pose.q.x, s.pose.q.y, s.pose.q.z, s.pose.t.x, s.pose.t.y, s.pose.t.z,
            ],
        };
        if let Action::UndeclaredEdge {
            parent,
            child,
            first_time,
        } = ingest.offer(Topic::Tf, &sample, &publisher)
        {
            assert_eq!(
                (parent.as_str(), child.as_str()),
                ("odom_combined", "base_footprint"),
                "the diagnostic names both frames"
            );
            first_times += u64::from(first_time);
        }
    }
    assert_eq!(first_times, 1, "diagnosed once, not once per message");
    let stats = ingest.stats();
    assert_eq!(
        stats.dropped_undeclared, 254,
        "the recording's odom samples"
    );
    assert_eq!(stats.dropped_bad_name, 0);
    assert!(stats.balanced(), "{stats:?}");
    assert_eq!(
        ingest.undeclared(),
        [("odom_combined", "base_footprint", 254)]
    );
}
