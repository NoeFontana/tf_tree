//! End-to-end for `docs/PHASE4.md` §5.8's resolution against a real recording:
//! `testdata/tfstream/indoor_atelier.tfstream` (4 static edges, 5 dynamic, 1066
//! samples; provenance in that directory's `ATTRIBUTION.md`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use tf_tree::InterpPolicy;
use tf_tree_bridge::{Action, Ingest, Publisher, Sample, Topic, TopologyConfig};
use tf_tree_cli::topology::discover_from_tfstream;

fn stream_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/tfstream/indoor_atelier.tfstream")
}

/// One recorded dynamic sample, shaped for [`Ingest::offer`].
///
/// `received` stays `SteadyNanos::UNKNOWN`: a `.tfstream` records no arrival
/// time, and passing the stamp would zero every offset. So `clock_resets == 0`
/// is structural and `dropped_non_monotonic == 0` is the real claim.
fn dynamic_sample(
    parent: &str,
    child: &str,
    s: &tf_tree_bench::replay::Sample,
) -> tf_tree_bridge::Sample {
    let mut sample = Sample::identity(parent, child, s.stamp_ns);
    sample.pose = [
        s.pose.q.w, s.pose.q.x, s.pose.q.y, s.pose.q.z, s.pose.t.x, s.pose.t.y, s.pose.t.z,
    ];
    sample
}

/// The whole chain on a real `/tf`: discover, print, reparse, build; the
/// arena's edges match the recording (4 static, 5 dynamic) and every dynamic
/// edge is claimable (§5.8 amendment).
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

    // The recording's header: 4 static edges, 5 dynamic.
    let statics = config
        .edges
        .iter()
        .filter(|e| matches!(e.shape, tf_tree_bridge::EdgeShape::Static { .. }))
        .count();
    assert_eq!(statics, 4);
    assert_eq!(config.edges.len() - statics, 5);

    // 4.79 Hz for 10 s is 48 samples, rounded to 64; sized from count it would be 256.
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
            // A declared dynamic edge is claimable, unlike an `edge_headroom` slot.
            tf_tree_bridge::EdgeShape::Dynamic { .. } => {
                assert!(
                    claim.is_ok(),
                    "{} -> {}: {:?}",
                    e.parent,
                    e.child,
                    claim.err()
                );
            }
            // A static edge is not: it has no ring.
            tf_tree_bridge::EdgeShape::Static { .. } => {
                assert!(claim.is_err(), "{} -> {}", e.parent, e.child);
            }
        }
    }
}

/// The discovered config accepts the stream it came from: every transform,
/// nothing undeclared or unaccounted for.
#[test]
fn the_discovered_config_accepts_the_stream_it_came_from() {
    let d = discover_from_tfstream(&stream_path(), 10.0, None, None).expect("discover");
    let config = d.to_config();

    let stream = tf_tree_bench::replay::TfStream::load(&stream_path()).expect("load");
    let mut ingest = Ingest::new(&config);
    let publisher = Publisher::named(
        &tf_tree_bridge::gid_for_name("/robot_state_publisher"),
        "/robot_state_publisher",
    );

    // Latched statics delivered twice must verify silently against the declared
    // constant, not conflict (§5.7, §5.8).
    let mut verified = 0u64;
    for _ in 0..2 {
        for (parent, child, iso) in &stream.static_edges {
            let mut s = Sample::identity(parent, child, 0);
            s.pose = [
                iso.q.w, iso.q.x, iso.q.y, iso.q.z, iso.t.x, iso.t.y, iso.t.z,
            ];
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
        let sample = dynamic_sample(parent, child, s);
        match ingest.offer(Topic::Tf, &sample, &publisher) {
            Action::Publish { .. } => published += 1,
            // Every other outcome fails, `NonMonotonic` included: the guard is
            // per edge (`docs/decisions/0011`).
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
    // All of it: per-edge scoping drops nothing on this corpus.
    assert_eq!(
        published, 1066,
        "every sample of a 1066-sample recording must be published"
    );
    assert_eq!(stats.dropped_non_monotonic, 0);
    assert_eq!(stats.clock_resets, 0);
}

/// An over-declared edge is fine; an under-declared one is counted and
/// diagnosed once per edge (§5.8).
#[test]
fn an_edge_missing_from_the_config_is_counted_and_named_once() {
    let d = discover_from_tfstream(&stream_path(), 10.0, None, None).expect("discover");
    let mut config = d.to_config();
    // Remove the odometry edge, whose absence surfaces as `NoPath`.
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
    let publisher = Publisher::named(&tf_tree_bridge::gid_for_name("/ekf"), "/ekf");
    let mut first_times = 0u64;
    for s in &stream.samples {
        let (parent, child) = &stream.dynamic_edges[s.edge];
        let sample = dynamic_sample(parent, child, s);
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

/// `--tf-prefix` reaches the collector and prefixes every frame, compared
/// against the same recording discovered bare.
#[test]
fn a_tf_prefix_reaches_the_discovered_config() {
    let bare = discover_from_tfstream(&stream_path(), 10.0, None, None)
        .expect("discover")
        .to_config();
    let prefixed = discover_from_tfstream(&stream_path(), 10.0, Some("robot1"), None)
        .expect("discover")
        .to_config();

    assert_eq!(bare.edges.len(), prefixed.edges.len());
    assert_ne!(bare.edges, prefixed.edges, "the prefix changed something");
    for (b, p) in bare.edges.iter().zip(&prefixed.edges) {
        assert_eq!(p.parent, format!("robot1/{}", b.parent));
        assert_eq!(p.child, format!("robot1/{}", b.child));
    }
    // It still reparses.
    TopologyConfig::parse(&prefixed.to_toml()).expect("prefixed config reparses");
}

/// `--interp` reaches the collector.
#[test]
fn an_interp_override_reaches_the_discovered_config() {
    let d = discover_from_tfstream(&stream_path(), 10.0, None, Some(InterpPolicy::LerpSlerp))
        .expect("discover");
    let config = d.to_config();
    assert_eq!(config.default_interp, InterpPolicy::LerpSlerp);
    // Survives the round trip.
    let reparsed = TopologyConfig::parse(&config.to_toml()).expect("reparses");
    assert_eq!(reparsed.default_interp, InterpPolicy::LerpSlerp);
}

/// The recording's topology passes §5.5's domain check against its declared
/// domain and fails against another.
#[test]
fn the_discovered_config_passes_its_own_domain_and_fails_another() {
    let config = discover_from_tfstream(&stream_path(), 10.0, None, None)
        .expect("discover")
        .to_config();
    assert_eq!(config.default_domain, 0, "discovery declares domain 0");
    assert_eq!(config.check_domain(0), Ok(()));

    let e = config
        .check_domain(1)
        .expect_err("a domain-1 bridge must be refused");
    assert_eq!(e.declared, 0);
    assert_eq!(e.bridge, 1);
    assert!(
        !e.child.is_empty(),
        "the refusal names the offending edge, not just the domains"
    );
}
