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

    let ok = tree.lookup("odom", "map", Stamp::<SensorDomain>::from_nanos(500));
    assert!(ok.is_ok(), "sensor-domain lookup should succeed: {ok:?}");
}

/// A path whose dynamic edges live in different clock domains is rejected at
/// compile time (D9).
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
    // Walking up from `base` visits the sensor-domain edge first.
    let msg = tree.describe(err).to_string();
    assert!(
        msg.contains("time domains") && msg.contains("map->odom"),
        "msg: {msg}"
    );
}

/// A single-domain path still compiles and evaluates.
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

/// Claiming a static edge is refused rather than reaching `ring()`.
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

/// A rejected `reparent` leaves the topology byte-identical and plans valid.
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

    assert!(tree.reparent(odom, base).is_err());

    assert!(
        plan.at(&tree.guard(), ns(0)).is_ok(),
        "an aborted topology mutation must not invalidate compiled plans"
    );
}

#[test]
fn topology_change_invalidates_a_plan() {
    let tree = TreeBuilder::new()
        .static_edge("map", "odom", &Iso3::IDENTITY)
        .static_edge("odom", "extra", &Iso3::IDENTITY)
        .build()
        .unwrap();
    let map = tree.frame("map").unwrap();
    let odom = tree.frame("odom").unwrap();
    let extra = tree.frame("extra").unwrap();

    let plan = tree.plan(odom, map).unwrap();
    assert!(plan.at(&tree.guard(), ns(0)).is_ok());

    // A runtime re-parent bumps the topology generation.
    tree.reparent(extra, map).unwrap();

    let err = plan.at(&tree.guard(), ns(0)).unwrap_err();
    assert!(
        matches!(err, LookupError::TopologyChanged { .. }),
        "{err:?}"
    );
}

/// `Latest` samples each edge at its own newest stamp; `LatestCommon` uses the
/// min of the newest stamps (tf2 `Time(0)`).
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

    let common = plan.latest_common(&g).unwrap();
    let want_common = (mo[5] * ob[5]).inverse(); // T_base_map at t=5000
    assert_close(common, want_common, TOL, "latest_common");

    let latest = plan.latest(&g).unwrap();
    let want_latest = (mo[10] * ob[5]).inverse();
    assert_close(latest, want_latest, TOL, "latest");

    assert!(
        common.t.sub(latest.t).norm() > 1e-6,
        "latest and latest_common should differ"
    );
}

/// A8's rescue path is wired through the public API: `Tree` supplies an
/// interning identity and a liveness source. Gated on `unstable` because both
/// are `ArenaView` methods.
#[test]
#[cfg(feature = "unstable")]
fn a_tree_can_rescue_a_wedged_intern() {
    let tree = TreeBuilder::new()
        .dynamic_edge("map", "odom", EdgeCfg::new(Capacity::slots(16)))
        .build()
        .unwrap();
    let view = tree.arena_view();

    assert!(
        view.interning_identity().is_some(),
        "Tree interns anonymously: it can wait on a stalled claimant but never \
         take the entry over, so A8's recovery is inert"
    );
    assert!(
        view.has_liveness_source(),
        "Tree has no liveness source: every claimant is believed alive, so A8's \
         takeover can never fire"
    );
}

/// Three dynamic edges with staggered windows, the intersection bounded by two
/// different non-first edges: `[0, 30_000]`, `[19_000, 25_000]` (lapped ring),
/// `[0, 22_000]`, so `[19_000, 22_000]`.
fn staggered_tree() -> tf_tree::Tree {
    let big = EdgeCfg::new(Capacity::slots(64));
    let small = EdgeCfg::new(Capacity::slots(8));
    let tree = TreeBuilder::new()
        .dynamic_edge("map", "odom", big)
        .dynamic_edge("odom", "base", small)
        .dynamic_edge("base", "lidar", big)
        .build()
        .unwrap();
    let odom = tree.frame("odom").unwrap();
    let map = tree.frame("map").unwrap();
    let base = tree.frame("base").unwrap();
    let lidar = tree.frame("lidar").unwrap();
    {
        let w = tree.claim(odom, map).unwrap();
        for i in 0..=30i64 {
            w.push(i * 1000, &pose(i as u64 + 1)).unwrap();
        }
        // 25 pushes into an 8-slot ring: window is the last seven stamps.
        let w = tree.claim(base, odom).unwrap();
        for i in 0..25i64 {
            w.push(1000 + i * 1000, &pose(i as u64 + 100)).unwrap();
        }
        let w = tree.claim(lidar, base).unwrap();
        for i in 0..=22i64 {
            w.push(i * 1000, &pose(i as u64 + 200)).unwrap();
        }
    }
    tree
}

/// The intersection is a `max` of lower ends and a `min` of upper ends.
#[test]
fn span_is_the_intersection_of_every_dynamic_window() {
    let tree = staggered_tree();
    let map = tree.frame("map").unwrap();
    let lidar = tree.frame("lidar").unwrap();
    let plan = tree.plan(lidar, map).unwrap();

    assert_eq!(plan.span(&tree.guard()).unwrap(), Some((19_000, 22_000)));
}

/// The interval is answerable inside it and refused outside it
/// (`docs/PHASE5.md` §4.2), checked against the sampler.
#[test]
fn span_answers_exactly_at_the_ends_it_reports() {
    let tree = staggered_tree();
    let map = tree.frame("map").unwrap();
    let lidar = tree.frame("lidar").unwrap();
    let plan = tree.plan(lidar, map).unwrap();
    let g = tree.guard();
    let (t0, t1) = plan.span(&g).unwrap().unwrap();

    assert!(plan.at(&g, ns(t0)).is_ok(), "span's lower end must answer");
    assert!(plan.at(&g, ns(t1)).is_ok(), "span's upper end must answer");
    assert!(matches!(
        plan.at(&g, ns(t0 - 1)),
        Err(LookupError::Extrapolation { .. })
    ));
    assert!(matches!(
        plan.at(&g, ns(t1 + 1)),
        Err(LookupError::Extrapolation { .. })
    ));

    // The upper end is `latest_common`'s stamp.
    assert_close(
        plan.at(&g, ns(t1)).unwrap(),
        plan.latest_common(&g).unwrap(),
        TOL,
        "span's upper end is latest_common",
    );
}

/// A static step constrains nothing in time and is skipped. No Python-reachable
/// plan contains one, so coverage lives here.
#[test]
fn span_skips_static_steps_and_is_bounded_by_the_dynamic_one() {
    let tree = TreeBuilder::new()
        .static_edge("map", "odom", &pose(7))
        .dynamic_edge("odom", "base", EdgeCfg::new(Capacity::slots(64)))
        .static_edge("base", "lidar", &pose(9))
        .build()
        .unwrap();
    let map = tree.frame("map").unwrap();
    let odom = tree.frame("odom").unwrap();
    let base = tree.frame("base").unwrap();
    let lidar = tree.frame("lidar").unwrap();
    {
        let w = tree.claim(base, odom).unwrap();
        for i in 0..=3i64 {
            w.push(i * 1000, &pose(i as u64 + 1)).unwrap();
        }
    }

    let plan = tree.plan(lidar, map).unwrap();
    // Guards against constant folding no longer emitting a `Step::Static`.
    assert!(
        plan.steps()
            .iter()
            .any(|s| matches!(s, tf_tree::Step::Static(_))),
        "fixture must contain a static step: {:?}",
        plan.steps()
    );
    assert!(
        plan.steps()
            .iter()
            .any(|s| matches!(s, tf_tree::Step::Dyn { .. })),
        "fixture must contain a dynamic step too, or the assertion below is the \
         all-static case in disguise"
    );

    assert_eq!(plan.span(&tree.guard()).unwrap(), Some((0, 3_000)));
}

/// `None` means unbounded; an all-static path is the case it is for.
#[test]
fn span_of_an_all_static_path_is_none() {
    let tree = TreeBuilder::new()
        .static_edge("map", "odom", &pose(3))
        .static_edge("odom", "base", &pose(4))
        .build()
        .unwrap();
    let map = tree.frame("map").unwrap();
    let base = tree.frame("base").unwrap();
    let plan = tree.plan(base, map).unwrap();
    assert!(!plan.is_empty(), "an all-static path is not the empty plan");
    assert_eq!(plan.span(&tree.guard()).unwrap(), None);
}

/// The empty `lookup(x, x)` plan is unbounded too, via an empty `steps()`.
#[test]
fn span_of_an_empty_plan_is_none() {
    let tree = staggered_tree();
    let map = tree.frame("map").unwrap();
    let plan = tree.plan(map, map).unwrap();
    assert!(plan.is_empty());
    assert_eq!(plan.span(&tree.guard()).unwrap(), None);
}

/// An empty intersection is a real answer, not an error and not `None`.
#[test]
fn span_reports_a_disjoint_intersection_rather_than_none() {
    let cfg = EdgeCfg::new(Capacity::slots(64));
    let tree = TreeBuilder::new()
        .dynamic_edge("map", "odom", cfg)
        .dynamic_edge("odom", "base", cfg)
        .build()
        .unwrap();
    let map = tree.frame("map").unwrap();
    let odom = tree.frame("odom").unwrap();
    let base = tree.frame("base").unwrap();
    {
        let w = tree.claim(odom, map).unwrap();
        for i in 0..=5i64 {
            w.push(i * 1000, &pose(i as u64 + 1)).unwrap();
        }
        let w = tree.claim(base, odom).unwrap();
        for i in 0..=5i64 {
            w.push(100_000 + i * 1000, &pose(i as u64 + 50)).unwrap();
        }
    }

    let plan = tree.plan(base, map).unwrap();
    let (t0, t1) = plan.span(&tree.guard()).unwrap().unwrap();
    assert!(t0 > t1, "expected an empty interval, got [{t0}, {t1}]");
    assert_eq!((t0, t1), (100_000, 5_000));
}

/// An edge that has never published is `NoData` naming that edge.
#[test]
fn span_names_the_edge_that_has_never_published() {
    let cfg = EdgeCfg::new(Capacity::slots(64));
    let tree = TreeBuilder::new()
        .dynamic_edge("map", "odom", cfg)
        .dynamic_edge("odom", "base", cfg)
        .build()
        .unwrap();
    let map = tree.frame("map").unwrap();
    let odom = tree.frame("odom").unwrap();
    let base = tree.frame("base").unwrap();
    {
        let w = tree.claim(odom, map).unwrap();
        for i in 0..=5i64 {
            w.push(i * 1000, &pose(i as u64 + 1)).unwrap();
        }
    }
    let plan = tree.plan(base, map).unwrap();
    let err = plan.span(&tree.guard()).unwrap_err();
    // It must name the silent edge; resolved through the stable `Tree::edges`
    // (`EdgeId` order, index `id - 1`).
    let edges = tree.edges().unwrap();
    assert_eq!(edges.len(), 2, "both declared edges are listed: {edges:?}");
    let named = match err {
        LookupError::NoData { edge } => edges
            .get(edge.get().wrapping_sub(1) as usize)
            .map(|(p, c)| (p.as_str(), c.as_str())),
        _ => None,
    };
    assert_eq!(
        named,
        Some(("odom", "base")),
        "expected NoData naming odom -> base, got {err:?}"
    );
}

/// A plan compiled against an older topology is refused by `span` too
/// (`check_generation`).
#[test]
fn span_refuses_a_plan_from_an_older_topology() {
    let cfg = EdgeCfg::new(Capacity::slots(64));
    let tree = TreeBuilder::new()
        .dynamic_edge("map", "odom", cfg)
        .dynamic_edge("odom", "base", cfg)
        .dynamic_edge("odom", "extra", cfg)
        .build()
        .unwrap();
    let map = tree.frame("map").unwrap();
    let odom = tree.frame("odom").unwrap();
    let base = tree.frame("base").unwrap();
    let extra = tree.frame("extra").unwrap();
    {
        let w = tree.claim(odom, map).unwrap();
        w.push(0, &pose(1)).unwrap();
        let w = tree.claim(base, odom).unwrap();
        w.push(0, &pose(2)).unwrap();
    }

    let plan = tree.plan(base, map).unwrap();
    assert!(plan.span(&tree.guard()).unwrap().is_some());

    tree.reparent(extra, base).unwrap();
    let err = plan.span(&tree.guard()).unwrap_err();
    assert!(
        matches!(err, LookupError::TopologyChanged { .. }),
        "{err:?}"
    );
}

/// A contended topology lock never names a participant slot it cannot see
/// (`docs/API.md` R5): the unnamed case renders without a number.
#[test]
fn a_contended_topology_lock_never_renders_a_sentinel_slot() {
    let named = tf_tree::ReparentError::LockContended {
        owner_slot: Some(3),
    }
    .to_string();
    assert!(
        named.contains('3'),
        "a holder the observation *could* name must reach the reader: {named}"
    );

    let unnamed = tf_tree::ReparentError::LockContended { owner_slot: None }.to_string();
    assert!(
        !unnamed.chars().any(|c| c.is_ascii_digit()),
        "an unnamed holder rendered a number, which is a slot claim this error \
         cannot support: {unnamed}"
    );
    assert!(
        !unnamed.contains("4294967295"),
        "the u32::MAX sentinel reached a user-facing message: {unnamed}"
    );
}
