//! Batch-sampling tests: `at_many` (monotone galloping + non-monotone fallback)
//! and `at_adaptive` (knot-count bound + reconstruction within tolerance).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{max_err, ns, Chain};

use tf_tree::{AdaptiveScratch, ErrBound, Iso3, LerpSlerp, Stamp, SystemDomain, MAX_KNOTS};

/// `at_many` over a monotone sweep equals `at` per stamp. The reference takes a
/// fresh `Guard` per stamp so its cursors start cold (an independent search).
#[test]
fn at_many_monotone_matches_per_stamp() {
    let c = Chain::new(64, 1000);
    let plan = c.tree.plan(c.base, c.map).unwrap();
    let g = c.tree.guard();
    let max_t = (c.n as i64 - 1) * c.dt;

    // ~700 stamps, so the galloping resume dominates.
    let stamps: Vec<Stamp> = (0..700).map(|k| ns((k as i64 * max_t) / 700)).collect();

    let mut out = vec![Iso3::IDENTITY; stamps.len()];
    plan.at_many(&g, &stamps, &mut out).unwrap();

    for (s, got) in stamps.iter().zip(out.iter()) {
        let want = plan.at(&c.tree.guard(), *s).unwrap();
        assert_eq!(
            got.to_bits(),
            want.to_bits(),
            "galloping vs binary at {s:?}"
        );
    }
}

/// A warm `Guard` cursor never changes an answer: `SampleRing::sample_from`
/// returns what `sample` returns, only the search path differs. Shapes: monotone
/// forward, non-monotone, and two plans interleaved on one guard.
#[test]
fn a_warm_cursor_never_changes_an_answer() {
    let c = Chain::new(64, 1000);
    let max_t = (c.n as i64 - 1) * c.dt;
    let base_map = c.tree.plan(c.base, c.map).unwrap();
    let odom_map = c.tree.plan(c.odom, c.map).unwrap();

    // Cold reference: a fresh guard per lookup.
    let cold = |plan: &tf_tree::Plan, s: Stamp| plan.at(&c.tree.guard(), s).unwrap();

    let monotone: Vec<Stamp> = (0..500).map(|k| ns((k as i64 * max_t) / 500)).collect();
    let scattered: Vec<Stamp> = (0..500)
        .map(|k: i64| ns(((k * 7919) % (max_t / 1000)) * 1000))
        .collect();

    for (label, stamps) in [("monotone", &monotone), ("scattered", &scattered)] {
        let warm = c.tree.guard();
        for s in stamps {
            assert_eq!(
                base_map.at(&warm, *s).unwrap().to_bits(),
                cold(&base_map, *s).to_bits(),
                "{label}: warm cursor disagreed with a cold search at {s:?}"
            );
        }
    }

    // Two plans on one guard, so step 0 alternates between edges.
    let shared = c.tree.guard();
    for (i, s) in monotone.iter().enumerate() {
        let plan = if i % 2 == 0 { &base_map } else { &odom_map };
        assert_eq!(
            plan.at(&shared, *s).unwrap().to_bits(),
            cold(plan, *s).to_bits(),
            "interleaved plans: warm cursor disagreed at {s:?}"
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
/// within tolerance. `RECON_TOL` is a literal so a loose request is judged.
#[test]
fn at_adaptive_bounded_and_within_tol() {
    let c = Chain::new(64, 1000);
    let plan = c.tree.plan(c.base, c.map).unwrap();
    let g = c.tree.guard();
    let max_t = (c.n as i64 - 1) * c.dt;

    let tol = ErrBound::new(1e-3, 1e-3);
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
    for w in stamps.windows(2) {
        assert!(w[0].nanos() < w[1].nanos(), "knots not increasing");
    }

    // Reconstruct at 400 probes between knots; `2 x` the tolerance allows for
    // off-midpoint probes.
    const RECON_TOL: f64 = 2e-3;
    for k in 0..=400 {
        let q = (k as i64 * max_t) / 400;
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
            e <= RECON_TOL,
            "reconstruction err {e:e} > {RECON_TOL:e} at q={q}"
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

    let tol = ErrBound::new(0.0, 0.0);
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

/// The kernels agree with `at_many` bit for bit.
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

    let mut quat = vec![0.0f64; stamps.len() * Layout::Quat.elems()];
    plan.at_many_into::<SystemDomain>(&g, &nanos(&stamps), Layout::Quat, &mut quat)
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

    // Mat4: translation exact; the rotation block is checked in `tf_tree_core::layout`.
    let mut mat = vec![0.0f64; stamps.len() * Layout::Mat4.elems()];
    plan.at_many_into::<SystemDomain>(&g, &nanos(&stamps), Layout::Mat4, &mut mat)
        .unwrap();
    for (i, iso) in reference.iter().enumerate() {
        let m = &mat[i * 16..(i + 1) * 16];
        assert_eq!(m[3].to_bits(), iso.t.x.to_bits(), "row {i} tx");
        assert_eq!(m[7].to_bits(), iso.t.y.to_bits(), "row {i} ty");
        assert_eq!(m[11].to_bits(), iso.t.z.to_bits(), "row {i} tz");
        assert_eq!(&m[12..16], &[0.0, 0.0, 0.0, 1.0], "row {i} bottom");
    }
}

/// `Layout::QuatTwist` writes the same bits as `at_with_derivatives`
/// (`docs/API.md` §3.3, `docs/PHASE5.md` §4.4), and its first seven elements
/// match `Layout::Quat`. Compared with `to_bits`.
#[test]
fn quat_twist_rows_are_bit_identical_to_at_with_derivatives() {
    use tf_tree::Layout;

    let c = Chain::new(64, 1000);
    let plan = c.tree.plan(c.base, c.map).unwrap();
    let g = c.tree.guard();
    let max_t = (c.n as i64 - 1) * c.dt;
    // Off-grid stamps.
    let stamps: Vec<Stamp> = (0..97).map(|k| ns((k as i64 * max_t) / 97 + 37)).collect();

    let mut rows = vec![0.0f64; stamps.len() * Layout::QuatTwist.elems()];
    plan.at_many_into::<SystemDomain>(&g, &nanos(&stamps), Layout::QuatTwist, &mut rows)
        .unwrap();

    let mut quat = vec![0.0f64; stamps.len() * Layout::Quat.elems()];
    plan.at_many_into::<SystemDomain>(&g, &nanos(&stamps), Layout::Quat, &mut quat)
        .unwrap();

    let mut moving = 0usize;
    for (i, s) in stamps.iter().enumerate() {
        let row = &rows[i * 13..(i + 1) * 13];
        assert_eq!(
            &row[..7],
            &quat[i * 7..(i + 1) * 7],
            "row {i}: the pose half is not the Quat layout"
        );

        let want = plan.at_with_derivatives(&c.tree.guard(), *s).unwrap();
        for (k, bits) in [
            want.pose.q.w,
            want.pose.q.x,
            want.pose.q.y,
            want.pose.q.z,
            want.pose.t.x,
            want.pose.t.y,
            want.pose.t.z,
            want.twist.omega.x,
            want.twist.omega.y,
            want.twist.omega.z,
            want.twist.v.x,
            want.twist.v.y,
            want.twist.v.z,
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(
                row[k].to_bits(),
                bits.to_bits(),
                "row {i} element {k}: the batch layout and the scalar call disagree"
            );
        }

        if want.twist.omega.norm() > 1e-6 && want.twist.v.norm() > 1e-6 {
            moving += 1;
        }
    }

    // Non-vacuity: the twist is non-zero.
    assert!(
        moving > 90,
        "the fixture is not moving; only {moving} of {} rows had a live twist",
        stamps.len()
    );
}

/// The 13-element buffer is sized and rejected like every other layout; the
/// error's `need` is asserted.
#[test]
fn a_short_quat_twist_buffer_is_refused_before_anything_is_written() {
    use tf_tree::{Layout, LookupError};

    let c = Chain::new(8, 1000);
    let plan = c.tree.plan(c.base, c.map).unwrap();
    let g = c.tree.guard();
    let stamps: Vec<Stamp> = (0..4).map(|k| ns(k * 1000)).collect();

    const SENTINEL: f64 = -12345.5;
    let mut out = vec![SENTINEL; 4 * 13 - 1];
    assert_eq!(
        plan.at_many_into::<SystemDomain>(&g, &nanos(&stamps), Layout::QuatTwist, &mut out)
            .unwrap_err(),
        LookupError::BufferTooSmall { need: 52, got: 51 }
    );
    assert!(
        out.iter().all(|v| *v == SENTINEL),
        "the buffer was written before validation rejected the call"
    );

    // The `f32` entry point refuses an `f64` layout.
    let mut f32s = vec![0.0f32; 4 * 13];
    assert_eq!(
        plan.at_many_into_f32::<SystemDomain>(&g, &nanos(&stamps), Layout::QuatTwist, &mut f32s)
            .unwrap_err(),
        LookupError::WrongElementType
    );
}

/// The non-monotone fallback gives the same answers as the cursor path.
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
    plan.at_many_into::<SystemDomain>(&g, &nanos(&sorted), Layout::Quat, &mut a)
        .unwrap();
    plan.at_many_into::<SystemDomain>(&g, &nanos(&shuffled), Layout::Quat, &mut b)
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

/// The twist layout's monotone and fallback loops agree: forward and reversed
/// stamps go down each branch, rows compared after un-reversing. Cursor
/// advance is pinned in `tf_tree_core`'s
/// `sample_with_twist_from_agrees_with_sample_with_twist_from_every_cursor`.
#[test]
fn quat_twist_agrees_between_the_cursor_and_fallback_batch_loops() {
    use tf_tree::Layout;

    let c = Chain::new(32, 1000);
    let plan = c.tree.plan(c.base, c.map).unwrap();
    let g = c.tree.guard();
    let max_t = (c.n as i64 - 1) * c.dt;

    // Off-grid stamps.
    let sorted: Vec<Stamp> = (0..64).map(|k| ns((k as i64 * max_t) / 64 + 37)).collect();
    let mut reversed = sorted.clone();
    reversed.reverse();

    let n = Layout::QuatTwist.elems();
    let mut a = vec![0.0f64; sorted.len() * n];
    let mut b = vec![0.0f64; sorted.len() * n];
    plan.at_many_into::<SystemDomain>(&g, &nanos(&sorted), Layout::QuatTwist, &mut a)
        .unwrap();
    plan.at_many_into::<SystemDomain>(&g, &nanos(&reversed), Layout::QuatTwist, &mut b)
        .unwrap();

    for i in 0..sorted.len() {
        let j = sorted.len() - 1 - i;
        for k in 0..n {
            assert_eq!(
                a[i * n + k].to_bits(),
                b[j * n + k].to_bits(),
                "stamp {i} element {k} disagreed between the cursor and fallback loops"
            );
        }
    }
    assert!(
        a.chunks_exact(n)
            .any(|r| r[7..].iter().any(|v| v.abs() > 1e-9)),
        "the fixture's twist is zero everywhere"
    );
}

/// `at_many` refuses a short buffer with a `Copy` error instead of panicking
/// (`docs/API.md` R5).
#[test]
fn at_many_refuses_a_short_buffer_rather_than_panicking() {
    use tf_tree::LookupError;

    let c = Chain::new(8, 1000);
    let plan = c.tree.plan(c.base, c.map).unwrap();
    let g = c.tree.guard();
    let stamps: Vec<Stamp> = (0..4).map(|k| ns(k * 1000)).collect();

    let mut out = vec![Iso3::IDENTITY; 3]; // one short of the four stamps
    let err = plan.at_many(&g, &stamps, &mut out).unwrap_err();
    assert_eq!(err, LookupError::BufferTooSmall { need: 4, got: 3 });
    assert!(
        out.iter().all(|p| p.to_bits() == Iso3::IDENTITY.to_bits()),
        "the buffer was written before validation rejected the call"
    );

    // The exact-fit case still works.
    let mut exact = vec![Iso3::IDENTITY; 4];
    plan.at_many(&g, &stamps, &mut exact).unwrap();
    assert!(exact
        .iter()
        .any(|p| p.to_bits() != Iso3::IDENTITY.to_bits()));
}

/// Validation happens before a single element is written (`PHASE3.md` §5.3).
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
        .at_many_into::<SystemDomain>(&g, &nanos(&stamps), Layout::Quat, &mut out)
        .unwrap_err();
    assert_eq!(err, LookupError::BufferTooSmall { need: 28, got: 27 });
    assert!(
        out.iter().all(|v| *v == SENTINEL),
        "the buffer was written before validation rejected the call"
    );

    // The f64/f32 entry points refuse each other's layouts.
    let mut big = vec![SENTINEL; 4 * 12];
    assert_eq!(
        plan.at_many_into::<SystemDomain>(&g, &nanos(&stamps), Layout::Affine32, &mut big)
            .unwrap_err(),
        LookupError::WrongElementType
    );
    assert!(big.iter().all(|v| *v == SENTINEL));

    let mut f32s = vec![0.0f32; 4 * 7];
    assert_eq!(
        plan.at_many_into_f32::<SystemDomain>(&g, &nanos(&stamps), Layout::Quat, &mut f32s)
            .unwrap_err(),
        LookupError::WrongElementType
    );
}

/// Raw nanoseconds from typed stamps, for the `*_into` entry points.
fn nanos(stamps: &[Stamp]) -> Vec<i64> {
    stamps.iter().map(|s| s.nanos()).collect()
}
