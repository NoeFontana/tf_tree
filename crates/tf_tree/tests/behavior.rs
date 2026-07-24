//! Time domains, `Latest`/`LatestCommon`, topology-change detection, and the
//! `Described` error display.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{assert_close, ns, pose};

use tf_tree::{
    Capacity, ClaimApiError, Domain, EdgeCfg, Iso3, LookupError, SensorDomain, Stamp, SystemDomain,
    TreeBuilder,
};

const TOL: f64 = 1e-12;

#[test]
fn public_trait_bounds_hold() {
    fn send_sync<T: Send + Sync>() {}
    fn copy_send_sync<T: Copy + Send + Sync>() {}
    send_sync::<tf_tree::Tree>();
    copy_send_sync::<tf_tree::Plan>();
}

#[test]
fn unknown_frame_is_reported() {
    let tree = TreeBuilder::new().frame("map").build().unwrap();
    let _map = tree.frame("map").unwrap();
    let err = tree.lookup("map", "ghost", ns(0)).unwrap_err();
    assert!(matches!(err, LookupError::UnknownFrame { .. }), "{err:?}");
}

#[test]
fn disconnected_frames_are_reported_and_described() {
    let tree = TreeBuilder::new()
        .frame("island_a")
        .frame("island_b")
        .build()
        .unwrap();
    let a = tree.frame("island_a").unwrap();
    let b = tree.frame("island_b").unwrap();
    // Two interned but unattached roots.
    let plan = tree.plan(a, b).unwrap_err();
    assert!(matches!(plan, LookupError::Disconnected { .. }), "{plan:?}");
    let msg = tree.describe(plan).to_string();
    assert!(
        msg.contains("island_a") && msg.contains("island_b"),
        "msg: {msg}"
    );
}

#[test]
fn no_data_is_reported_and_described() {
    let tree = TreeBuilder::new()
        .dynamic_edge("map", "odom", EdgeCfg::new(Capacity::slots(16)))
        .build()
        .unwrap();
    let _map = tree.frame("map").unwrap();
    let _odom = tree.frame("odom").unwrap();
    // No samples published.
    let err = tree.lookup("odom", "map", ns(0)).unwrap_err();
    assert!(matches!(err, LookupError::NoData { .. }), "{err:?}");
    let msg = tree.describe(err).to_string();
    assert!(msg.contains("map->odom"), "msg: {msg}");
}

#[test]
fn cross_domain_lookup_is_rejected_but_matching_domain_succeeds() {
    let tree = TreeBuilder::new()
        .dynamic_edge(
            "map",
            "odom",
            EdgeCfg::new(Capacity::slots(16)).domain(SensorDomain::TAG),
        )
        .build()
        .unwrap();
    let map = tree.frame("map").unwrap();
    let odom = tree.frame("odom").unwrap();
    {
        let w = tree.claim(odom, map).unwrap();
        w.push(0, &pose(1)).unwrap();
        w.push(1000, &pose(2)).unwrap();
    }

    // System-domain stamp against a sensor-domain edge: mismatch.
    let err = tree
        .lookup::<SystemDomain>("odom", "map", ns(500))
        .unwrap_err();
    assert!(
        matches!(
            err,
            LookupError::TimeDomainMismatch { expected, got } if expected == SensorDomain::TAG && got == SystemDomain::TAG
        ),
        "{err:?}"
    );

    // Matching sensor-domain stamp: succeeds.
    let ok = tree.lookup("odom", "map", Stamp::<SensorDomain>::from_nanos(500));
    assert!(ok.is_ok(), "sensor-domain lookup should succeed: {ok:?}");
}

/// A path whose dynamic edges live in different clock domains cannot be
/// evaluated at any single stamp, so compilation rejects it. Recording only the
/// last edge's domain (as `fold` used to) let such a plan pass `check_domain` and
/// then sample the other edge with the wrong clock — the silent misread D9 exists
/// to prevent.
#[test]
fn a_path_crossing_time_domains_is_rejected_at_compile_time() {
    let tree = TreeBuilder::new()
        .dynamic_edge(
            "map",
            "odom",
            EdgeCfg::new(Capacity::slots(16)).domain(SystemDomain::TAG),
        )
        .dynamic_edge(
            "odom",
            "base",
            EdgeCfg::new(Capacity::slots(16)).domain(SensorDomain::TAG),
        )
        .build()
        .unwrap();
    let map = tree.frame("map").unwrap();
    let base = tree.frame("base").unwrap();

    let err = tree.plan(base, map).unwrap_err();
    assert!(
        matches!(
            err,
            LookupError::MixedTimeDomains { expected, got, .. }
                if expected != got
        ),
        "{err:?}"
    );
    // The message names the offending edge (D11). Walking up from `base` visits
    // the sensor-domain edge first, so `map->odom` is the one that disagrees.
    let msg = tree.describe(err).to_string();
    assert!(
        msg.contains("time domains") && msg.contains("map->odom"),
        "msg: {msg}"
    );
}

/// A single-domain path still compiles and evaluates — the mixed-domain rejection
/// must not catch the ordinary case.
#[test]
fn a_single_domain_path_still_compiles() {
    let cfg = EdgeCfg::new(Capacity::slots(16)).domain(SensorDomain::TAG);
    let tree = TreeBuilder::new()
        .dynamic_edge("map", "odom", cfg)
        .dynamic_edge("odom", "base", cfg)
        .build()
        .unwrap();
    let map = tree.frame("map").unwrap();
    let odom = tree.frame("odom").unwrap();
    let base = tree.frame("base").unwrap();
    {
        let a = tree.claim(odom, map).unwrap();
        let b = tree.claim(base, odom).unwrap();
        for i in 0..4i64 {
            a.push(i * 1000, &pose(i as u64 + 1)).unwrap();
            b.push(i * 1000, &pose(i as u64 + 50)).unwrap();
        }
    }
    let plan = tree.plan(base, map).unwrap();
    assert_eq!(plan.domain(), SensorDomain::TAG);
    assert!(plan
        .at(&tree.guard(), Stamp::<SensorDomain>::from_nanos(1500))
        .is_ok());
}

/// Claiming a static edge used to reach `ring()` unconditionally: a debug build
/// tripped the `is_power_of_two` assertion and a release build built a ring with
/// `mask == u64::MAX` over an empty slot slice, panicking on the first push.
#[test]
fn claiming_a_static_edge_is_a_typed_error() {
    let tree = TreeBuilder::new()
        .static_edge("a", "b", &Iso3::IDENTITY)
        .build()
        .unwrap();
    let a = tree.frame("a").unwrap();
    let b = tree.frame("b").unwrap();
    // `Publisher` is deliberately not `Debug`, so `map(drop)` before asserting.
    let err = tree.claim(b, a).map(drop).unwrap_err();
    assert!(matches!(err, ClaimApiError::NotDynamic { .. }), "{err:?}");
}

/// A rejected `reparent` leaves the published topology byte-identical, so it must
/// not invalidate compiled plans.
#[test]
fn a_rejected_reparent_does_not_invalidate_plans() {
    let tree = TreeBuilder::new()
        .static_edge("map", "odom", &Iso3::IDENTITY)
        .static_edge("odom", "base", &Iso3::IDENTITY)
        .build()
        .unwrap();
    let map = tree.frame("map").unwrap();
    let odom = tree.frame("odom").unwrap();
    let base = tree.frame("base").unwrap();

    let plan = tree.plan(base, map).unwrap();
    assert!(plan.at(&tree.guard(), ns(0)).is_ok());

    // Moving `odom` under its own descendant would close a cycle: rejected.
    assert!(tree.reparent(odom, base).is_err());

    assert!(
        plan.at(&tree.guard(), ns(0)).is_ok(),
        "an aborted topology mutation must not invalidate compiled plans"
    );
}

#[test]
fn topology_change_invalidates_a_plan() {
    // `extra` is declared under `odom` so it can be re-parented at runtime.
    let tree = TreeBuilder::new()
        .static_edge("map", "odom", &Iso3::IDENTITY)
        .static_edge("odom", "extra", &Iso3::IDENTITY)
        .build()
        .unwrap();
    let map = tree.frame("map").unwrap();
    let odom = tree.frame("odom").unwrap();
    let extra = tree.frame("extra").unwrap();

    let plan = tree.plan(odom, map).unwrap();
    // Works before the change.
    assert!(plan.at(&tree.guard(), ns(0)).is_ok());

    // A runtime re-parent (off the odom<-map path) bumps the topology generation.
    tree.reparent(extra, map).unwrap();

    let err = plan.at(&tree.guard(), ns(0)).unwrap_err();
    assert!(
        matches!(err, LookupError::TopologyChanged { .. }),
        "{err:?}"
    );
}

/// `Latest` samples each edge at its own newest stamp; `LatestCommon` uses the
/// min of the newest stamps (tf2 `Time(0)`). They differ when the edges have
/// different newest stamps.
#[test]
fn latest_and_latest_common_differ_when_edges_are_uneven() {
    let cfg = EdgeCfg::new(Capacity::slots(64));
    let tree = TreeBuilder::new()
        .dynamic_edge("map", "odom", cfg)
        .dynamic_edge("odom", "base", cfg)
        .build()
        .unwrap();
    let map = tree.frame("map").unwrap();
    let odom = tree.frame("odom").unwrap();
    let base = tree.frame("base").unwrap();

    // map->odom: 11 samples up to t=10000. odom->base: 6 samples up to t=5000.
    let mo: Vec<Iso3> = (0..=10).map(|i| pose(i as u64 + 1)).collect();
    let ob: Vec<Iso3> = (0..=5).map(|i| pose(i as u64 + 100)).collect();
    {
        let w_mo = tree.claim(odom, map).unwrap();
        for (i, p) in mo.iter().enumerate() {
            w_mo.push(i as i64 * 1000, p).unwrap();
        }
        let w_ob = tree.claim(base, odom).unwrap();
        for (i, p) in ob.iter().enumerate() {
            w_ob.push(i as i64 * 1000, p).unwrap();
        }
    }

    let plan = tree.plan(base, map).unwrap();
    let g = tree.guard();

    // LatestCommon: min newest = 5000. Both edges at 5000.
    let common = plan.latest_common(&g).unwrap();
    let want_common = (mo[5] * ob[5]).inverse(); // T_base_map at t=5000
    assert_close(common, want_common, TOL, "latest_common");

    // Latest: map->odom at 10000, odom->base at 5000.
    let latest = plan.latest(&g).unwrap();
    let want_latest = (mo[10] * ob[5]).inverse();
    assert_close(latest, want_latest, TOL, "latest");

    // They genuinely differ (the map->odom pose moved between t=5000 and 10000).
    assert!(
        common.t.sub(latest.t).norm() > 1e-6,
        "latest and latest_common should differ"
    );
}
