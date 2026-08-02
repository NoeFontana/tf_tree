//! Batch-sampling tests: `at_many` (monotone galloping + non-monotone fallback)
//! and `at_adaptive` (knot-count bound + reconstruction within tolerance).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{max_err, ns, Chain};

use tf_tree::{AdaptiveScratch, ErrBound, Iso3, LerpSlerp, Stamp, SystemDomain, MAX_KNOTS};

/// `at_many` over a monotone stamp sweep equals calling `at` per stamp, and the
/// long sweep exercises the galloping (resume-from-cursor) path.
///
/// **The reference side takes a fresh `Guard` per stamp, and that is now
/// load-bearing.** `Guard` carries a per-step bracket-search cursor, so
/// `plan.at` on a *reused* guard resumes from the previous answer exactly as
/// `at_many` does — comparing the two on one guard would compare galloping
/// against galloping and assert nothing about the binary search. A guard built
/// per stamp starts every cursor cold, which is the independent search this test
/// means by "binary".
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
        let want = plan.at(&c.tree.guard(), *s).unwrap();
        assert_eq!(
            got.to_bits(),
            want.to_bits(),
            "galloping vs binary at {s:?}"
        );
    }
}

/// A warm cursor never changes an answer.
///
/// `Guard` caches a per-step bracket-search hint so a scalar `Plan::at` resumes
/// beside the previous answer instead of restarting at the window midpoint —
/// worth ~9% at depth 3 (`docs/design/fast-path.md` §16). The entire safety
/// argument for that cache is that
/// [`SampleRing::sample_from`] returns exactly what `sample` returns and only
/// the *search path* differs, so a stale, wrong or absent hint costs time and
/// never accuracy. Nothing else in the type system enforces that, so this does.
///
/// Three shapes, because the cursor is in a different state in each:
///
/// * **monotone forward** — the case the cache is for, and the one where the
///   hint is always warm and always close;
/// * **non-monotone** — the hint points past the answer, so the gallop must walk
///   *backwards* and still land exactly;
/// * **two plans interleaved on one guard** — step `k` alternates between two
///   different edges, so the tag check is exercised on every call.
///
/// **Mutants, all three applied and run** — two of them survive, and saying so
/// is the point:
///
/// * `let i = lo` instead of `self.bracket(lo, hi, t)` in `sample_from` — the
///   gallop's lower bound used as the answer. **Caught**, and this test is the
///   only one of the eight that catches it.
/// * dropping the tag check in `Guard::sample_hinted`, so one plan's cursor is
///   used as another's hint. **Survives**, correctly: the gallop corrects a
///   wrong hint, so the tag is a *performance* guard and no correctness test can
///   or should kill it.
/// * dropping `clamp(lo_logical, newest)` on the hint in `sample_from`.
///   **Survives** — a cursor is only ever written after a successful sample, so
///   it is already inside the window and the clamp is defence against a state
///   this path cannot reach.
#[test]
fn a_warm_cursor_never_changes_an_answer() {
    let c = Chain::new(64, 1000);
    let max_t = (c.n as i64 - 1) * c.dt;
    let base_map = c.tree.plan(c.base, c.map).unwrap();
    let odom_map = c.tree.plan(c.odom, c.map).unwrap();

    // Cold reference: a fresh guard per lookup, so every cursor starts at 0 and
    // every search is an independent binary search.
    let cold = |plan: &tf_tree::Plan, s: Stamp| plan.at(&c.tree.guard(), s).unwrap();

    let monotone: Vec<Stamp> = (0..500).map(|k| ns((k as i64 * max_t) / 500)).collect();
    // Deterministic jumps around the window: forwards, backwards, and repeats.
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

    // Two plans on one guard: `base->map` has two dynamic steps and `odom->map`
    // has one, so step 0 alternates between two different edges and the tag
    // check decides on every call whether the hint is usable.
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

    // Mat4: translation column is exact; the rotation block is checked by its
    // action in `tf_tree_core::layout`'s own tests.
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

/// **`Layout::QuatTwist` is `at_with_derivatives` in a buffer, not a second
/// implementation of it** — `docs/API.md` §3.3, `docs/PHASE5.md` §4.4.
///
/// The layout exists so derivatives reach a batch caller without a fourth
/// method. That is only true if the thirteen `f64` it writes are the *same
/// bits* the scalar call produces: the moment the batch path derives a twist its
/// own way — a finite difference, a re-composed adjoint chain — two bindings can
/// disagree about a velocity, and neither one is obviously wrong to a user.
/// Compared with `to_bits`, not a tolerance, for exactly that reason.
///
/// The first seven elements are also checked against `Layout::Quat` on the same
/// stamps, which is the other half of the promise: a consumer that already
/// parses a `(N, 7)` row can read a `(N, 13)` one by ignoring the tail.
///
/// **The stamps ascend, so the batch takes the monotone cursor branch while the
/// scalar reference does not** — each `at_with_derivatives` below is on a fresh
/// guard and restarts every bracket search at the window midpoint. So this is
/// also the plan-level assertion that resuming a search cannot move a bit of a
/// twist, which is what makes the cursor safe to pick from the *stamps* rather
/// than from anything the caller asked for.
///
/// Mutant: emit `v` before `ω` in `write_quat_twist` ⇒ the tail assertions fail
/// while the pose ones still pass. Mutant B: route `Layout::QuatTwist` through
/// `fold_batch(.., write_quat, ..)` and zero the tail ⇒ the pose half still
/// agrees and only the twist assertions catch it.
#[test]
fn quat_twist_rows_are_bit_identical_to_at_with_derivatives() {
    use tf_tree::Layout;

    let c = Chain::new(64, 1000);
    let plan = c.tree.plan(c.base, c.map).unwrap();
    let g = c.tree.guard();
    let max_t = (c.n as i64 - 1) * c.dt;
    // Off-grid stamps, so the interpolant and its derivative both actually run.
    let stamps: Vec<Stamp> = (0..97).map(|k| ns((k as i64 * max_t) / 97 + 37)).collect();

    let mut rows = vec![0.0f64; stamps.len() * Layout::QuatTwist.elems()];
    plan.at_many_into::<SystemDomain>(&g, &nanos(&stamps), Layout::QuatTwist, &mut rows)
        .unwrap();

    // The pose half, against the layout it claims to extend.
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

    // Non-vacuity: a fixture whose twist is zero everywhere would pass every
    // assertion above against a layout that wrote six zeros.
    assert!(
        moving > 90,
        "the fixture is not moving; only {moving} of {} rows had a live twist",
        stamps.len()
    );
}

/// The 13-element buffer is sized and rejected like every other layout.
///
/// `elems()` is the single place the stride comes from, so the interesting
/// failure is not "13 is wrong" but "the check ran against a different number
/// than the write did" — which is why the error's `need` is asserted and not
/// merely that it failed.
///
/// Mutant: `Layout::QuatTwist => 7` in `elems()` ⇒ `need` becomes 28 here, and
/// `quat_twist_rows_are_bit_identical_to_at_with_derivatives` reads its rows at
/// the wrong stride.
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

    // And it is an `f64` layout: the `f32` entry point must refuse it rather
    // than writing thirteen 4-byte elements where thirteen 8-byte ones go.
    let mut f32s = vec![0.0f32; 4 * 13];
    assert_eq!(
        plan.at_many_into_f32::<SystemDomain>(&g, &nanos(&stamps), Layout::QuatTwist, &mut f32s)
            .unwrap_err(),
        LookupError::WrongElementType
    );
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

/// The twist layout's two batch loops must agree, exactly as the pose layouts'
/// do.
///
/// `Layout::QuatTwist` gained the monotone cursor branch, so it now has the
/// same shape as `fold_batch`: ascending stamps gallop from a resumable cursor,
/// anything else restarts each search. Feeding the same stamps forward and
/// reversed puts one call down each branch, and the rows must come back
/// element-for-element identical after un-reversing.
///
/// **Reversed is not a second gallop direction.** An earlier revision of this
/// comment said the reversed order was what made `bracket_from`'s *downward*
/// arm run; that is false, and was checked rather than reasoned about. The
/// reversed call is non-monotone, so it takes the fallback arm, which calls
/// `bracket` and never `bracket_from` — no gallop runs in either direction.
/// Reversed is simply the cheapest input that is guaranteed non-monotone while
/// still being a permutation of the forward one, which is what lets the rows be
/// compared element for element. Injecting `panic!()` into `bracket_from`'s
/// downward arm leaves this test **passing** and fails `tf_tree_core`'s
/// `sample_from_agrees_with_sample_from_every_cursor` and
/// `sample_with_twist_from_agrees_with_sample_with_twist_from_every_cursor` —
/// that arm's coverage is there, in the `start in 0..21` sweep, and deleting it
/// would leave the arm untested whatever this test says.
///
/// What this *does* pin is the upward arm the monotone batch really uses, and
/// it pins it against an independent answer rather than against itself.
/// Mutant, run: in `bracket_from`'s upward arm, hand `bracket` a lower bound of
/// `hint + step` instead of `hint + step / 2` ⇒ fails, "stamp 0 element 0
/// disagreed between the cursor and fallback loops", `13824777323826317557`
/// against `...562`.
///
/// Mutant B, run: in `fold_batch_with_twist`, declare the `cursors` array
/// *inside* the loop so every stamp restarts cold ⇒ still passes, because a
/// cold cursor is a valid cursor. That is the shape of the limit here: the
/// cursor is a hint, so no assertion about the *values* can see whether it
/// advanced. The advance is pinned in `tf_tree_core`'s
/// `sample_with_twist_from_agrees_with_sample_with_twist_from_every_cursor`.
#[test]
fn quat_twist_agrees_between_the_cursor_and_fallback_batch_loops() {
    use tf_tree::Layout;

    let c = Chain::new(32, 1000);
    let plan = c.tree.plan(c.base, c.map).unwrap();
    let g = c.tree.guard();
    let max_t = (c.n as i64 - 1) * c.dt;

    // Off-grid, so the interpolant and its derivative both actually run.
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
    // Non-vacuity: the twist tail must be live, or this compares zeros.
    assert!(
        a.chunks_exact(n)
            .any(|r| r[7..].iter().any(|v| v.abs() > 1e-9)),
        "the fixture's twist is zero everywhere"
    );
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
        .at_many_into::<SystemDomain>(&g, &nanos(&stamps), Layout::Quat, &mut out)
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
