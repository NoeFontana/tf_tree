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

// ---------------------------------------------------------------------------
// Layout kernels (`docs/decisions/0005` Milestone B, `docs/PHASE3.md` §5.2)
// ---------------------------------------------------------------------------

/// **The kernels must agree with `at_many`, bit for bit.**
///
/// `at_many_into` exists to avoid an intermediate `Iso3` buffer, not to compute
/// anything different. Any divergence would be a second implementation of the
/// interpolation drifting from the first — so this compares the emitted
/// elements against the ones derived from `at_many`'s output, exactly.
#[test]
fn at_many_into_agrees_with_at_many_exactly() {
    use tf_tree::Layout;

    let c = Chain::new(64, 1000);
    let plan = c.tree.plan(c.base, c.map).unwrap();
    let g = c.tree.guard();
    let max_t = (c.n as i64 - 1) * c.dt;
    let stamps: Vec<Stamp> = (0..300).map(|k| ns((k as i64 * max_t) / 300)).collect();

    let mut reference = vec![Iso3::IDENTITY; stamps.len()];
    plan.at_many(&g, &stamps, &mut reference).unwrap();

    // Quat: the engine's own order, so equality is exact and unarguable.
    let mut quat = vec![0.0f64; stamps.len() * Layout::Quat.elems()];
    plan.at_many_into(&g, &stamps, Layout::Quat, &mut quat)
        .unwrap();
    for (i, iso) in reference.iter().enumerate() {
        let row = &quat[i * 7..(i + 1) * 7];
        assert_eq!(row[0].to_bits(), iso.q.w.to_bits(), "row {i} qw");
        assert_eq!(row[1].to_bits(), iso.q.x.to_bits(), "row {i} qx");
        assert_eq!(row[2].to_bits(), iso.q.y.to_bits(), "row {i} qy");
        assert_eq!(row[3].to_bits(), iso.q.z.to_bits(), "row {i} qz");
        assert_eq!(row[4].to_bits(), iso.t.x.to_bits(), "row {i} tx");
        assert_eq!(row[5].to_bits(), iso.t.y.to_bits(), "row {i} ty");
        assert_eq!(row[6].to_bits(), iso.t.z.to_bits(), "row {i} tz");
    }

    // Mat4: translation column is exact; the rotation block is checked by its
    // action in `tf_tree_core::layout`'s own tests.
    let mut mat = vec![0.0f64; stamps.len() * Layout::Mat4.elems()];
    plan.at_many_into(&g, &stamps, Layout::Mat4, &mut mat)
        .unwrap();
    for (i, iso) in reference.iter().enumerate() {
        let m = &mat[i * 16..(i + 1) * 16];
        assert_eq!(m[3].to_bits(), iso.t.x.to_bits(), "row {i} tx");
        assert_eq!(m[7].to_bits(), iso.t.y.to_bits(), "row {i} ty");
        assert_eq!(m[11].to_bits(), iso.t.z.to_bits(), "row {i} tz");
        assert_eq!(&m[12..16], &[0.0, 0.0, 0.0, 1.0], "row {i} bottom");
    }
}

/// The non-monotone fallback must produce the same answers as the cursor path.
///
/// Two loops, one shared kernel — but the *search* differs, and a cursor that
/// resumed wrongly on unsorted input would show up here and nowhere else.
#[test]
fn at_many_into_handles_unsorted_stamps() {
    use tf_tree::Layout;

    let c = Chain::new(32, 1000);
    let plan = c.tree.plan(c.base, c.map).unwrap();
    let g = c.tree.guard();
    let max_t = (c.n as i64 - 1) * c.dt;

    let sorted: Vec<Stamp> = (0..64).map(|k| ns((k as i64 * max_t) / 64)).collect();
    let mut shuffled = sorted.clone();
    shuffled.reverse();

    let mut a = vec![0.0f64; sorted.len() * 7];
    let mut b = vec![0.0f64; sorted.len() * 7];
    plan.at_many_into(&g, &sorted, Layout::Quat, &mut a)
        .unwrap();
    plan.at_many_into(&g, &shuffled, Layout::Quat, &mut b)
        .unwrap();

    for (i, _) in sorted.iter().enumerate() {
        let j = sorted.len() - 1 - i;
        assert_eq!(
            &a[i * 7..(i + 1) * 7],
            &b[j * 7..(j + 1) * 7],
            "stamp {i} disagreed between the monotone and fallback paths"
        );
    }
}

/// Validation happens before a single element is written (`PHASE3.md` §5.3).
///
/// A half-written output is worse than none, because it looks like data: the
/// caller sees plausible transforms for the first k samples and garbage after,
/// with nothing marking the boundary.
#[test]
fn a_rejected_call_leaves_the_buffer_untouched() {
    use tf_tree::{Layout, LookupError};

    let c = Chain::new(8, 1000);
    let plan = c.tree.plan(c.base, c.map).unwrap();
    let g = c.tree.guard();
    let stamps: Vec<Stamp> = (0..4).map(|k| ns(k * 1000)).collect();

    const SENTINEL: f64 = -12345.5;
    let mut out = vec![SENTINEL; 4 * 7 - 1]; // one element short

    let err = plan
        .at_many_into(&g, &stamps, Layout::Quat, &mut out)
        .unwrap_err();
    assert_eq!(err, LookupError::BufferTooSmall { need: 28, got: 27 });
    assert!(
        out.iter().all(|v| *v == SENTINEL),
        "the buffer was written before validation rejected the call"
    );

    // And the f64/f32 entry points refuse each other's layouts rather than
    // writing a differently-sized element into the caller's memory.
    let mut big = vec![SENTINEL; 4 * 12];
    assert_eq!(
        plan.at_many_into(&g, &stamps, Layout::Affine32, &mut big)
            .unwrap_err(),
        LookupError::WrongElementType
    );
    assert!(big.iter().all(|v| *v == SENTINEL));

    let mut f32s = vec![0.0f32; 4 * 7];
    assert_eq!(
        plan.at_many_into_f32(&g, &stamps, Layout::Quat, &mut f32s)
            .unwrap_err(),
        LookupError::WrongElementType
    );
}
