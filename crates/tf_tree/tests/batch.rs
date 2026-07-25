//! Batch-sampling tests: `at_many` (monotone galloping + non-monotone fallback)
//! and `at_adaptive` (knot-count bound + reconstruction within tolerance).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{max_err, ns, Chain};

use tf_tree::{AdaptiveScratch, ErrBound, Iso3, LerpSlerp, Stamp, SystemDomain, MAX_KNOTS};

/// `at_many` over a monotone stamp sweep equals calling `at` per stamp, and the
/// long sweep exercises the galloping (resume-from-cursor) path.
#[test]
fn at_many_monotone_matches_per_stamp() {
    let c = Chain::new(64, 1000);
    let plan = c.tree.plan(c.base, c.map).unwrap();
    let g = c.tree.guard();
    let max_t = (c.n as i64 - 1) * c.dt;

    // ~700 interpolated stamps, strictly increasing — far more than log(n), so the
    // galloping resume is the dominant path.
    let stamps: Vec<Stamp> = (0..700).map(|k| ns((k as i64 * max_t) / 700)).collect();

    let mut out = vec![Iso3::IDENTITY; stamps.len()];
    plan.at_many(&g, &stamps, &mut out).unwrap();

    for (s, got) in stamps.iter().zip(out.iter()) {
        let want = plan.at(&g, *s).unwrap();
        assert_eq!(
            got.to_bits(),
            want.to_bits(),
            "galloping vs binary at {s:?}"
        );
    }
}

/// Non-monotone input falls back to an independent search per stamp, still
/// matching `at`.
#[test]
fn at_many_nonmonotone_matches_per_stamp() {
    let c = Chain::new(64, 1000);
    let plan = c.tree.plan(c.base, c.map).unwrap();
    let g = c.tree.guard();
    let max_t = (c.n as i64 - 1) * c.dt;

    // Deterministic non-monotone order.
    let stamps: Vec<Stamp> = [0.37, 0.9, 0.1, 0.55, 0.05, 0.99, 0.42, 0.7, 0.2, 0.8]
        .iter()
        .map(|f| ns((f * max_t as f64) as i64))
        .collect();

    let mut out = vec![Iso3::IDENTITY; stamps.len()];
    plan.at_many(&g, &stamps, &mut out).unwrap();
    for (s, got) in stamps.iter().zip(out.iter()) {
        let want = plan.at(&g, *s).unwrap();
        assert_eq!(got.to_bits(), want.to_bits(), "fallback vs binary at {s:?}");
    }
}

/// `at_adaptive` emits a bounded knot set whose LerpSlerp reconstruction stays
/// within tolerance across a curved trajectory.
#[test]
fn at_adaptive_bounded_and_within_tol() {
    let c = Chain::new(64, 1000);
    let plan = c.tree.plan(c.base, c.map).unwrap();
    let g = c.tree.guard();
    let max_t = (c.n as i64 - 1) * c.dt;

    let tol = ErrBound {
        rot_rad: 1e-3,
        trans: 1e-3,
    };
    let mut scratch = AdaptiveScratch::<SystemDomain>::new();
    let (stamps, poses) = plan
        .at_adaptive(&g, (ns(0), ns(max_t)), tol, &mut scratch)
        .unwrap();

    assert!(stamps.len() >= 2, "expected at least two knots");
    assert!(
        stamps.len() <= MAX_KNOTS,
        "knot count {} exceeds cap {MAX_KNOTS}",
        stamps.len()
    );
    // Knots are strictly increasing.
    for w in stamps.windows(2) {
        assert!(w[0].nanos() < w[1].nanos(), "knots not increasing");
    }

    // Reconstruct at 400 probe stamps by LerpSlerp between bracketing knots and
    // compare to the exact plan evaluation. The bisection bounds midpoint error;
    // allow a small factor for off-midpoint probes.
    let tol_check = tol.rot_rad.max(tol.trans) * 2.0;
    for k in 0..=400 {
        let q = (k as i64 * max_t) / 400;
        // Find bracket [i, j=i+1] with stamps[i] <= q < stamps[j] (or the last).
        let mut i = 0usize;
        while i + 1 < stamps.len() && stamps[i + 1].nanos() <= q {
            i += 1;
        }
        let j = (i + 1).min(stamps.len() - 1);
        let a_s = stamps[i].nanos();
        let b_s = stamps[j].nanos();
        let s = if b_s > a_s {
            (q - a_s) as f64 / (b_s - a_s) as f64
        } else {
            0.0
        };
        let approx = <LerpSlerp as tf_tree::Interp>::eval(&poses[i], &poses[j], s);
        let exact = plan.at(&g, ns(q)).unwrap();
        let e = max_err(approx, exact);
        assert!(
            e <= tol_check,
            "reconstruction err {e:e} > {tol_check:e} at q={q}"
        );
    }
}

/// A zero tolerance forces maximal subdivision but the knot count stays capped.
#[test]
fn at_adaptive_zero_tol_hits_cap() {
    let c = Chain::new(64, 1000);
    let plan = c.tree.plan(c.base, c.map).unwrap();
    let g = c.tree.guard();
    let max_t = (c.n as i64 - 1) * c.dt;

    let tol = ErrBound {
        rot_rad: 0.0,
        trans: 0.0,
    };
    let mut scratch = AdaptiveScratch::<SystemDomain>::new();
    let (stamps, _poses) = plan
        .at_adaptive(&g, (ns(0), ns(max_t)), tol, &mut scratch)
        .unwrap();
    assert!(
        stamps.len() <= MAX_KNOTS,
        "knot count {} must stay within cap {MAX_KNOTS}",
        stamps.len()
    );
}
