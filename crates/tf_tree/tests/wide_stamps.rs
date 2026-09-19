//! Stamps far from the origin: an all-static path is answerable at any stamp, so
//! `i64::MIN`/`i64::MAX` are ordinary arguments and must not overflow.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::ns;
use tf_tree::{Capacity, EdgeCfg, ErrBound, Iso3, Quat, TreeBuilder, Vec3};

/// A pose that is neither the identity nor symmetric, so a dropped or inverted
/// term shows up rather than cancelling.
fn pose(x: f64, y: f64, z: f64) -> Iso3 {
    Iso3::new(
        Quat::new(0.9394, 0.1017, -0.1695, 0.2746).normalize(),
        Vec3::new(x, y, z),
    )
}

/// An all-static plan is bit-identical at every stamp, pinned at the extremes.
#[test]
fn an_all_static_plan_is_bit_identical_at_every_stamp() {
    let tree = TreeBuilder::new()
        .static_edge("a", "b", &pose(1.5, -2.25, 0.75))
        .static_edge("b", "c", &pose(-0.5, 3.0, -1.25))
        .static_edge("c", "d", &pose(2.0, 0.125, 4.5))
        .build()
        .unwrap();
    let (a, d) = (tree.frame("a").unwrap(), tree.frame("d").unwrap());
    let plan = tree.plan(d, a).unwrap();
    let g = tree.guard();

    assert_eq!(plan.len(), 1, "three static edges must fold to one step");
    assert_eq!(
        plan.span(&g).unwrap(),
        None,
        "an all-static plan has no span"
    );

    let reference = plan.at(&g, ns(0)).unwrap();
    for t in [i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX - 1, i64::MAX] {
        let got = plan.at(&g, ns(t)).unwrap();
        assert_eq!(
            bits(&got),
            bits(&reference),
            "all-static plan answered differently at t = {t}"
        );
    }
}

fn bits(i: &Iso3) -> [u64; 7] {
    [
        i.q.w.to_bits(),
        i.q.x.to_bits(),
        i.q.y.to_bits(),
        i.q.z.to_bits(),
        i.t.x.to_bits(),
        i.t.y.to_bits(),
        i.t.z.to_bits(),
    ]
}

/// `at_adaptive` over the whole `i64` range, on an all-static plan.
#[test]
fn at_adaptive_spans_the_full_i64_range_on_a_static_plan() {
    let tree = TreeBuilder::new()
        .static_edge("a", "b", &pose(1.0, 2.0, 3.0))
        .build()
        .unwrap();
    let (a, b) = (tree.frame("a").unwrap(), tree.frame("b").unwrap());
    let plan = tree.plan(a, b).unwrap();
    let g = tree.guard();
    let mut scratch = Default::default();

    let (stamps, poses) = plan
        .at_adaptive(
            &g,
            (ns(i64::MIN), ns(i64::MAX)),
            ErrBound::new(1e-6, 1e-6),
            &mut scratch,
        )
        .expect("full-range adaptive on a static plan");

    assert_eq!(stamps.len(), 2, "a constant needs exactly two knots");
    assert_eq!(poses.len(), 2);
    assert_eq!(bits(&poses[0]), bits(&poses[1]), "a constant is constant");
}

/// A span wider than `i64::MAX` over a curving path must still subdivide, not
/// wrap to a negative width and return two knots.
#[test]
fn at_adaptive_subdivides_a_span_wider_than_i64_max() {
    let cfg = EdgeCfg::new(Capacity::slots(8));
    let tree = TreeBuilder::new()
        .dynamic_edge("a", "b", cfg)
        .build()
        .unwrap();
    let (a, b) = (tree.frame("a").unwrap(), tree.frame("b").unwrap());

    const LO: i64 = -(1i64 << 62) - 1;
    const HI: i64 = 1i64 << 62;
    {
        let w = tree.claim(b, a).unwrap();
        w.push(LO, &pose(0.0, 0.0, 0.0)).unwrap();
        // Off the LO..HI line, so a two-knot LERP cannot be within tolerance.
        w.push(0, &pose(-20.0, 5.0, -3.0)).unwrap();
        w.push(HI, &pose(1.0, 0.0, 0.0)).unwrap();
    }

    let plan = tree.plan(a, b).unwrap();
    let g = tree.guard();
    let mut scratch = Default::default();

    let (stamps, poses) = plan
        .at_adaptive(
            &g,
            (ns(LO), ns(HI)),
            ErrBound::new(1e-6, 1e-6),
            &mut scratch,
        )
        .expect("adaptive over a span wider than i64::MAX");

    assert!(
        stamps.len() > 2,
        "a curved path over a >i64::MAX span must subdivide; got {} knots — \
         this is the release-build wrap, not a panic",
        stamps.len()
    );
    // The knots must bracket the excursion: some pose comes near the middle sample.
    let closest = poses
        .iter()
        .map(|p| (p.t.x - -20.0).abs())
        .fold(f64::INFINITY, f64::min);
    assert!(
        closest < 1.0,
        "no knot came near the middle sample (closest t.x error {closest}); \
         the subdivision is not following the path"
    );
}

/// `sample.rs`'s interpolation parameter for two bracketing samples more than
/// `i64::MAX` apart must stay inside the bracket, not run backwards.
#[test]
fn interpolating_across_a_span_wider_than_i64_max_stays_inside_the_bracket() {
    let cfg = EdgeCfg::new(Capacity::slots(4));
    let tree = TreeBuilder::new()
        .dynamic_edge("a", "b", cfg)
        .build()
        .unwrap();
    let (a, b) = (tree.frame("a").unwrap(), tree.frame("b").unwrap());

    {
        let w = tree.claim(b, a).unwrap();
        w.push(i64::MIN + 1, &pose(0.0, 0.0, 0.0)).unwrap();
        w.push(i64::MAX, &pose(10.0, 0.0, 0.0)).unwrap();
    }

    let plan = tree.plan(a, b).unwrap();
    let g = tree.guard();

    let got = plan.at(&g, ns(0)).unwrap();
    assert!(
        (0.0..=10.0).contains(&got.t.x),
        "interpolated outside the bracket: t.x = {}, expected within [0, 10] \
         (≈5 at the midpoint)",
        got.t.x
    );
    assert!(
        (got.t.x - 5.0).abs() < 0.01,
        "t = 0 is the midpoint of [i64::MIN+1, i64::MAX]; expected t.x ≈ 5, got {}",
        got.t.x
    );
}
