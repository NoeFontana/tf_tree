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

/// A8's rescue path must be **wired**, not merely present.
///
/// The core tests inject a liveness predicate and name a participant slot by
/// hand. That proves the algorithm; it does not prove `Tree` supplies either —
/// and at first it supplied neither. `Tree::view` built an anonymous view with
/// no liveness source, so a rescuer had no identity to publish into `claiming`
/// and believed every claimant alive. A8 was correct, loom-tested, and totally
/// inert through the public API.
///
/// An earlier version of this test interned a fresh name and asserted it
/// terminated. That passed with the wiring deliberately removed — interning an
/// unclaimed name never reaches the rescue path, so it asserted nothing. This
/// checks the two properties that were actually missing.
#[test]
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

// ---------------------------------------------------------------------------
// `Plan::span` — `docs/PHASE5.md` §4.2
// ---------------------------------------------------------------------------

/// Three dynamic edges whose retained windows are staggered *and* whose
/// intersection is bounded by two **different** edges, neither of them the first.
///
/// `map->odom` is deliberately the widest window, so a `span` that returned the
/// first step's own window (or stopped after it) reports `(0, 30_000)` and every
/// assertion below fails. `odom->base`'s ring is smaller than its push count, so
/// its lower end is a genuinely *lapped* window rather than "the first stamp ever
/// pushed" — a fixture where every ring still held its whole history could not
/// tell `oldest_stamp` from `stamps[0]`.
///
/// Windows: `map->odom = [0, 30_000]`, `odom->base = [19_000, 25_000]`,
/// `base->lidar = [0, 22_000]`. Intersection `[19_000, 22_000]`: the low end
/// comes from the second edge, the high end from the third.
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
        // 25 pushes into an 8-slot ring: `retained() == 7`, so the readable
        // window is the last seven stamps, `[19_000, 25_000]`.
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
///
/// Mutant: `lo.max(oldest)` -> `lo.min(oldest)` in `Plan::span` yields
/// `(0, 22_000)`. Mutant: `hi.min(newest)` -> `hi.max(newest)` yields
/// `(19_000, 30_000)`. Mutant: `break` after the first step yields
/// `(0, 30_000)`.
#[test]
fn span_is_the_intersection_of_every_dynamic_window() {
    let tree = staggered_tree();
    let map = tree.frame("map").unwrap();
    let lidar = tree.frame("lidar").unwrap();
    let plan = tree.plan(lidar, map).unwrap();

    assert_eq!(plan.span(&tree.guard()).unwrap(), Some((19_000, 22_000)));
}

/// The interval means what it says: answerable inside it, refused outside it.
///
/// This is what makes the numbers above more than arithmetic — it is the same
/// claim `docs/PHASE5.md` §4.2 makes for the query, checked against the sampler
/// rather than against a second copy of `span`'s own `max`/`min`.
///
/// Mutant: widen either end by one nanosecond (`hi.min(newest) + 1`, or
/// `lo.max(oldest) - 1`) and the matching `at` past that end stops raising.
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

    // The upper end *is* `latest_common`'s stamp, which is the agreement
    // `Plan::span`'s doc comment claims the two keep without sharing a helper.
    assert_close(
        plan.at(&g, ns(t1)).unwrap(),
        plan.latest_common(&g).unwrap(),
        TOL,
        "span's upper end is latest_common",
    );
}

/// A static step constrains nothing in time and must be skipped, not folded in.
///
/// **This is the branch no Python-reachable plan can contain** — `tf_tree.build`
/// declares only dynamic edges — which is why the coverage lives here, in a crate
/// `just test` runs.
///
/// Mutant: replace the `continue` in `Plan::span`'s `else` arm with
/// `return Ok(None)`. The path then reports itself answerable at every stamp
/// while its one dynamic edge bounds it to `[0, 3_000]`.
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
    // Without these the test would still pass if constant folding ever stopped
    // emitting a `Step::Static` here, and would then assert nothing about the
    // branch it is named for.
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

/// `None` means *unbounded*, and an all-static path is the case it is for.
///
/// Distinguished from the empty `lookup(x, x)` plan below on purpose: a test that
/// queries `span(x, x)` takes the `len == 0` path and never reaches the
/// `Step::Static` arm at all.
///
/// Mutant: return `Some((0, 0))` for a plan with no dynamic step. A caller's
/// `t0 <= t <= t1` is then false everywhere for a path that answers everywhere.
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

/// The empty `lookup(x, x)` plan is unbounded for the same reason but by a
/// different code path: `steps()` is empty, so the loop body never runs.
#[test]
fn span_of_an_empty_plan_is_none() {
    let tree = staggered_tree();
    let map = tree.frame("map").unwrap();
    let plan = tree.plan(map, map).unwrap();
    assert!(plan.is_empty());
    assert_eq!(plan.span(&tree.guard()).unwrap(), None);
}

/// An empty intersection is a real answer — not an error, and not `None`.
///
/// Two edges with genuinely disjoint histories: the caller's `t0 <= t <= t1` is
/// correctly false everywhere, which is a different fact from "this path is
/// answerable at any stamp".
///
/// Mutant: collapse `t0 > t1` to `None` in `Plan::span`. The `Some` assertion
/// fails — and the two cases become indistinguishable to a caller.
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
        // Starts long after the first edge stopped: no overlap at all.
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

/// An edge that has never published is `NoData` naming that edge — not an empty
/// interval, because the caller acts differently on the two.
///
/// Mutant: `continue` past an empty ring in `Plan::span` instead of propagating
/// `Guard::window`'s error. `span` then reports the *other* edges' intersection,
/// a window over which the path is not answerable at all, and this gets `Ok`.
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
    // `odom->base` was declared and never pushed to.
    let plan = tree.plan(base, map).unwrap();
    let err = plan.span(&tree.guard()).unwrap_err();
    // It must name the *silent* edge, not merely fail: a `span` that reported
    // whichever edge it looked at first would satisfy a bare `matches!`.
    let view = tree.arena_view();
    let named = match err {
        LookupError::NoData { edge } => view.edge(edge).map(|r| (r.parent, r.child)),
        _ => None,
    };
    assert_eq!(
        named,
        Some((odom.get(), base.get())),
        "expected NoData naming odom -> base, got {err:?}"
    );
}

/// A plan compiled against an older topology must not be answered from, and
/// `span` is not an exception.
///
/// This is the property the binding-side implementation could not have: it read
/// `ArenaView` directly and never consulted a `Guard`, so it answered where
/// `Plan::at` refuses — including from a fork-poisoned child, whose guard reports
/// `ChildDetached` through the same `check_generation`.
///
/// Mutant: drop the `check_generation(g)?` from `Plan::span`. The stale plan then
/// returns `Ok` and this fails.
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
