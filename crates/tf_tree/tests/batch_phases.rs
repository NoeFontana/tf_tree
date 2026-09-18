//! The chunked batch fold against `Plan::at`, bit for bit.
//!
//! `docs/decisions/0060` Decision A replaced the batch entry points' per-stamp
//! loop with a chunked two-phase fold: per chunk and per plan step, phase 1
//! reads every lane's bracket and phase 2 interpolates every lane. Step 2 of
//! that record requires this file, and requires it to *"reproduce §8's engine
//! test in full: branch regions, isolated stamps in lanes 0 and 1, the
//! error-contract grid, counters, and the identity and all-static plans"* —
//! §8 being the prototype's own bit-identity test, which lived in a worktree
//! that no longer exists.
//!
//! # What makes this a test and not a tautology
//!
//! The property is **bit identity with the scalar `Plan::at`**, per stamp, by
//! `to_bits` rather than by tolerance. That is what makes batch a *layout*
//! rather than a second answer, and it is the only assertion that can see a
//! reassociated sum or a bracket built from the wrong pair.
//!
//! Two shapes of the fixture carry that weight, and both are there because the
//! prototype's campaign found them the hard way:
//!
//! * **Crafted branch regions.** Every arm of `slerp` and of `screw_parts` is
//!   reached on purpose — knots, an `s` that rounds to exactly `1.0`, identical
//!   rotations, the LERP fallback, either side of the 0.3 rad series threshold,
//!   a large arc, the far hemisphere, signed zeros and non-finite translations.
//!   A fixture of well-conditioned random poses reaches one arm and calls it a
//!   pass; §8 records two real mutants that survived exactly that.
//! * **Each stamp in a named lane.** A defect that depends on *where in the
//!   chunk* a stamp lands is invisible to a batch that always presents it in
//!   the same position. Every stamp is therefore also run alone, in lane 0, in
//!   lane 1, in the last lane of a chunk and in the first lane of the second
//!   chunk. §8's M3 and M4 were caught by nothing else.
//!
//! # The two constants this file mirrors
//!
//! `FOLD_LANES` (16) and `FOLD_MIN_BATCH` (3) are private to
//! `tf_tree_core::plan`, and `plan.rs` pins both with a `const` assertion that
//! names this file — so moving either one there breaks a build rather than
//! silently retargeting the lane shapes below.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::ns;

use tf_tree::{
    exp_so3, write_affine32, write_mat4, write_quat, Capacity, EdgeCfg, InterpPolicy, Iso3, Layout,
    LookupError, Plan, Quat, Stamp, SystemDomain, Tree, TreeBuilder, Vec3,
};

/// Lanes per chunk in `tf_tree_core::plan::FOLD_LANES`.
const LANES: usize = 16;
/// `tf_tree_core::plan::FOLD_MIN_BATCH`: below this a batch stays per-stamp.
const MIN_BATCH: usize = 3;

/// The bit pattern every output buffer is filled with before a call. Rows a
/// refused batch must not touch still hold it afterwards.
const SENTINEL: u64 = 0xDEAD_BEEF_DEAD_BEEF;

// ---------------------------------------------------------------------------
// Poses
// ---------------------------------------------------------------------------

/// A pose with a rotation of exactly `angle` radians about a fixed unit axis.
///
/// One axis for the whole fixture, so the relative rotation between two
/// consecutive samples is the difference of their angles and a region can name
/// the branch it is aiming at.
fn at_angle(angle: f64, t: [f64; 3]) -> Iso3 {
    // (1, 2, 2)/3 — a unit axis with no zero component, so no branch is reached
    // by accident through a zero in the rotation vector.
    let k = angle / 3.0;
    Iso3::new(
        exp_so3(Vec3::new(k, 2.0 * k, 2.0 * k)),
        Vec3::new(t[0], t[1], t[2]),
    )
}

/// A pose with an explicitly given quaternion, for the regions that need bit
/// patterns `exp_so3` will not produce.
fn raw(q: Quat, t: [f64; 3]) -> Iso3 {
    Iso3::new(q, Vec3::new(t[0], t[1], t[2]))
}

// ---------------------------------------------------------------------------
// The fixture's sample stream
// ---------------------------------------------------------------------------

/// One crafted numerical region: the consecutive samples it publishes and the
/// stamps it asks about, both as nanosecond offsets from the region's base.
struct Region {
    name: &'static str,
    offsets: Vec<i64>,
    poses: Vec<Iso3>,
    queries: Vec<i64>,
}

impl Region {
    fn new(name: &'static str, samples: Vec<(i64, Iso3)>, queries: Vec<i64>) -> Region {
        let (offsets, poses) = samples.into_iter().unzip();
        Region {
            name,
            offsets,
            poses,
            queries,
        }
    }
}

const MS: i64 = 1_000_000;

/// Every crafted region, in publication order.
///
/// The angles are chosen against the thresholds `docs/decisions/0060` §9.1
/// pins: `slerp` takes its series arm while the relative rotation is at most
/// 0.3 rad (a half-angle of `THETA_SLERP_SMALL = 0.15`), its LERP fallback
/// below `SLERP_LERP_FALLBACK = 1e-6` of chord, and its closed form above.
fn crafted() -> Vec<Region> {
    let mut r = Vec::new();

    // Knots: three series-region samples, queried both *on* every stamp
    // (`t_i == t`, no interpolation at all) and between them.
    r.push(Region::new(
        "knots",
        vec![
            (0, at_angle(0.00, [1.0, 2.0, 3.0])),
            (MS, at_angle(0.05, [1.5, 2.5, 3.5])),
            (2 * MS, at_angle(0.10, [2.0, 3.0, 4.0])),
        ],
        vec![0, MS / 2, MS, 3 * MS / 2, 2 * MS],
    ));

    // `s` rounds to exactly 1.0. Over a segment of 2^55 ns the ratio
    // (2^55 - 1)/2^55 is a tie at the last representable step below one and
    // rounds to 1.0, so the query lands on the *upper* endpoint by rounding
    // rather than by equality — the one input that reaches an endpoint through
    // `s` instead of through the exact-hit branch.
    const WIDE: i64 = 1 << 55;
    r.push(Region::new(
        "wide_span",
        vec![
            (0, at_angle(0.00, [0.0, 0.0, 0.0])),
            (WIDE, at_angle(0.08, [1.0, -1.0, 0.5])),
        ],
        vec![1, WIDE / 2, WIDE - 1],
    ));

    // Identical rotations: `h == 0` for LerpSlerp, and the degenerate screw for
    // ScLerp. The translation still moves, so a fold that returned either
    // endpoint outright would be caught.
    let q_fixed = exp_so3(Vec3::new(0.4 / 3.0, 0.8 / 3.0, 0.8 / 3.0));
    r.push(Region::new(
        "stationary",
        vec![
            (0, raw(q_fixed, [0.0, 0.0, 0.0])),
            (MS, raw(q_fixed, [1.0, 2.0, -3.0])),
            (2 * MS, raw(q_fixed, [2.0, 4.0, -6.0])),
        ],
        vec![0, MS / 3, MS, 5 * MS / 3],
    ));

    // The LERP fallback band: 1e-7 rad between consecutive samples.
    r.push(Region::new(
        "lerp_fallback",
        vec![
            (0, at_angle(0.2, [0.0, 0.0, 0.0])),
            (MS, at_angle(0.2 + 1e-7, [0.1, 0.0, 0.0])),
        ],
        vec![MS / 4, MS / 2],
    ));

    // Either side of the series threshold, 2e-4 rad apart.
    r.push(Region::new(
        "below_series_threshold",
        vec![
            (0, at_angle(0.0, [0.0, 0.0, 0.0])),
            (MS, at_angle(0.2998, [0.3, 0.2, 0.1])),
        ],
        vec![MS / 2],
    ));
    r.push(Region::new(
        "above_series_threshold",
        vec![
            (0, at_angle(0.0, [0.0, 0.0, 0.0])),
            (MS, at_angle(0.3002, [0.3, 0.2, 0.1])),
        ],
        vec![MS / 2],
    ));

    // A large arc, and one past the hemisphere boundary where `slerp` must flip
    // the sign of the second quaternion and take the short way round.
    r.push(Region::new(
        "large_arc",
        vec![
            (0, at_angle(0.0, [0.0, 0.0, 0.0])),
            (MS, at_angle(2.5, [1.0, 1.0, 1.0])),
        ],
        vec![MS / 8, MS / 2],
    ));
    r.push(Region::new(
        "far_hemisphere",
        vec![
            (0, at_angle(0.0, [0.0, 0.0, 0.0])),
            (MS, at_angle(3.5, [1.0, 1.0, 1.0])),
        ],
        vec![MS / 2, 7 * MS / 8],
    ));

    // Exact identity at both endpoints: every difference is zero and every
    // series denominator is at its own limit.
    r.push(Region::new(
        "identity",
        vec![(0, Iso3::IDENTITY), (MS, Iso3::IDENTITY)],
        vec![0, MS / 2],
    ));

    // Signed zeros. `-0.0 == 0.0` compares true and `to_bits` does not, which
    // is the whole reason this file compares bits: a fold that reached an
    // endpoint by arithmetic rather than by copy loses the sign here.
    r.push(Region::new(
        "signed_zeros",
        vec![
            (0, raw(Quat::IDENTITY, [-0.0, -0.0, -0.0])),
            (MS, raw(Quat::IDENTITY, [0.0, -0.0, 0.0])),
        ],
        vec![0, MS / 2, MS],
    ));

    // Non-finite translations. The engine has no opinion about them — `push`
    // stores what it is given — so what this pins is that the batch fold
    // propagates exactly the bits the scalar fold propagates, NaN payload
    // included.
    r.push(Region::new(
        "non_finite",
        vec![
            (0, raw(Quat::IDENTITY, [f64::NAN, 1.0, 2.0])),
            (MS, raw(Quat::IDENTITY, [1.0, f64::INFINITY, 2.0])),
            (2 * MS, raw(Quat::IDENTITY, [1.0, 2.0, f64::NEG_INFINITY])),
        ],
        vec![0, MS / 2, MS, 3 * MS / 2],
    ));

    r
}

/// A deterministic 64-bit LCG — the fixture must be the same on every host and
/// in every run, so `rand` is neither wanted nor in the dependency budget.
struct Lcg(u64);

impl Lcg {
    fn next_f64(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 11) as f64) / ((1u64 << 53) as f64)
    }
}

/// The whole stream: the crafted regions four times over, then 200 random
/// series steps.
///
/// Four repeats because a region that is only ever the *first* thing a chunk
/// sees is a region tested in one lane. Repeating shifts every region across
/// the 16-lane grid, and the random tail — whose stamps are irregular — shifts
/// it again.
fn stream() -> (Vec<(i64, Iso3)>, Vec<i64>) {
    let mut samples: Vec<(i64, Iso3)> = Vec::new();
    let mut queries: Vec<i64> = Vec::new();
    let mut base: i64 = 1_000 * MS;

    for _ in 0..4 {
        for region in crafted() {
            let span = *region.offsets.last().unwrap();
            for (off, pose) in region.offsets.iter().zip(region.poses.iter()) {
                samples.push((base + off, *pose));
            }
            for q in &region.queries {
                assert!(
                    *q >= 0 && *q <= span,
                    "{}: query {q} is outside its own region",
                    region.name
                );
                queries.push(base + q);
            }
            // A gap no query falls in, so every bracket belongs to one region.
            base += span + 10 * MS;
        }
    }

    // 200 random series steps: rotations well inside the series arm, stamps
    // irregular so the queries are neither on a grid nor evenly spread.
    let mut rng = Lcg(0x5EED_0060);
    let mut angle = 0.0f64;
    let mut prev = base;
    for i in 0..200 {
        angle += 0.01 + 0.09 * rng.next_f64();
        let t = [rng.next_f64(), rng.next_f64(), rng.next_f64()];
        let step = MS / 2 + (rng.next_f64() * (MS as f64)) as i64;
        let stamp = prev + step;
        samples.push((stamp, at_angle(angle, t)));
        if i > 0 {
            queries.push(prev + (rng.next_f64() * (step - 1) as f64) as i64 + 1);
        }
        prev = stamp;
    }
    // `t == t_new`: the very last published stamp, which is the one arm that
    // needs the newest sample and no interpolation.
    queries.push(prev);

    for w in samples.windows(2) {
        assert!(w[0].0 < w[1].0, "sample stamps must be strictly increasing");
    }
    for w in queries.windows(2) {
        assert!(w[0] <= w[1], "query stamps must be non-decreasing");
    }
    (samples, queries)
}

// ---------------------------------------------------------------------------
// Trees
// ---------------------------------------------------------------------------

/// A built tree plus the plan under test and the stamps to sweep.
struct Case {
    name: &'static str,
    tree: Tree,
    plan: Plan,
    stamps: Vec<i64>,
}

fn cfg(cap: u32, interp: InterpPolicy) -> EdgeCfg {
    EdgeCfg::new(Capacity::slots(cap)).interp(interp)
}

/// Every plan shape §8 names: one dynamic step, and dyn/static/dyn folded in
/// both directions so both `inverted` flags run.
fn cases() -> Vec<Case> {
    let (samples, queries) = stream();
    let cap = u32::try_from(samples.len()).unwrap() + 1;
    let mut out = Vec::new();

    for (name, interp_a) in [
        ("one_dyn/sclerp", InterpPolicy::ScLerp),
        ("one_dyn/lerpslerp", InterpPolicy::LerpSlerp),
    ] {
        let tree = TreeBuilder::new()
            .dynamic_edge("a", "b", cfg(cap, interp_a))
            .build()
            .unwrap();
        let (a, b) = (tree.frame("a").unwrap(), tree.frame("b").unwrap());
        {
            let w = tree.claim(b, a).unwrap();
            for (s, p) in &samples {
                w.push(*s, p).unwrap();
            }
        }
        let plan = tree.plan(b, a).unwrap();
        out.push(Case {
            name,
            tree,
            plan,
            stamps: queries.clone(),
        });
    }

    for (name, forward, interp_a, interp_b) in [
        (
            "dyn_static_dyn/fwd/sclerp",
            true,
            InterpPolicy::ScLerp,
            InterpPolicy::ScLerp,
        ),
        (
            "dyn_static_dyn/rev/lerpslerp",
            false,
            InterpPolicy::LerpSlerp,
            InterpPolicy::LerpSlerp,
        ),
        (
            "dyn_static_dyn/fwd/mixed",
            true,
            InterpPolicy::ScLerp,
            InterpPolicy::LerpSlerp,
        ),
        (
            "dyn_static_dyn/rev/mixed",
            false,
            InterpPolicy::LerpSlerp,
            InterpPolicy::ScLerp,
        ),
    ] {
        let tree = TreeBuilder::new()
            .dynamic_edge("a", "b", cfg(cap, interp_a))
            .static_edge("b", "c", &at_angle(0.37, [0.25, -0.5, 0.75]))
            .dynamic_edge("c", "d", cfg(cap, interp_b))
            .build()
            .unwrap();
        let (a, b) = (tree.frame("a").unwrap(), tree.frame("b").unwrap());
        let (c, d) = (tree.frame("c").unwrap(), tree.frame("d").unwrap());
        {
            let w_ab = tree.claim(b, a).unwrap();
            let w_cd = tree.claim(d, c).unwrap();
            for (i, (s, p)) in samples.iter().enumerate() {
                w_ab.push(*s, p).unwrap();
                // A different stream on the second edge, so a fold that used
                // one edge's bracket for the other is visible.
                w_cd.push(*s, &at_angle(0.013 * i as f64, [p.t.z, p.t.x, p.t.y]))
                    .unwrap();
            }
        }
        let plan = if forward {
            tree.plan(d, a).unwrap()
        } else {
            tree.plan(a, d).unwrap()
        };
        out.push(Case {
            name,
            tree,
            plan,
            stamps: queries.clone(),
        });
    }

    out
}

// ---------------------------------------------------------------------------
// The comparison
// ---------------------------------------------------------------------------

/// The scalar answer for every stamp, each on its own fresh `Guard`.
///
/// A fresh guard per stamp is deliberate: the guard carries the cursor hints,
/// and the reference must not be able to inherit a hint from the query before
/// it. The batch calls below get a fresh guard each too.
fn scalar(tree: &Tree, plan: &Plan, stamps: &[i64]) -> Vec<Result<Iso3, LookupError>> {
    stamps
        .iter()
        .map(|t| {
            let g = tree.guard();
            plan.at(&g, ns(*t))
        })
        .collect()
}

/// The `f64` rows `layout` would write for one pose.
fn rows(iso: &Iso3, layout: Layout) -> Vec<f64> {
    let mut v = vec![0.0; elems(layout)];
    match layout {
        Layout::Mat4 => write_mat4(iso, &mut v),
        Layout::Quat => write_quat(iso, &mut v),
        _ => unreachable!("rows is for the f64 layouts"),
    }
    v
}

fn elems(layout: Layout) -> usize {
    match layout {
        Layout::Mat4 => 16,
        Layout::Quat => 7,
        Layout::Affine32 => 12,
        _ => unreachable!("no other layout is tested here"),
    }
}

/// The index of the first stamp in `expected` that fails, if any.
fn first_err(expected: &[Result<Iso3, LookupError>]) -> Option<(usize, LookupError)> {
    expected
        .iter()
        .enumerate()
        .find_map(|(i, r)| r.as_ref().err().map(|e| (i, *e)))
}

/// Run every batch entry point over `stamps` and check each against `expected`,
/// bit for bit, including where the batch stops and what it leaves untouched.
fn assert_batch(
    tree: &Tree,
    plan: &Plan,
    stamps: &[i64],
    expected: &[Result<Iso3, LookupError>],
    ctx: &str,
) {
    assert_eq!(stamps.len(), expected.len());
    let stop = first_err(expected);
    let n_ok = stop.map_or(stamps.len(), |(i, _)| i);

    // `at_many` — `&mut [Iso3]`.
    {
        let sentinel = Iso3::from_bits(&[SENTINEL; 7]);
        let mut out = vec![sentinel; stamps.len()];
        let g = tree.guard();
        let typed: Vec<Stamp<SystemDomain>> = stamps.iter().map(|t| ns(*t)).collect();
        let got = plan.at_many(&g, &typed, &mut out);
        match stop {
            None => assert!(got.is_ok(), "{ctx}: at_many refused: {got:?}"),
            Some((_, e)) => assert_eq!(got, Err(e), "{ctx}: at_many returned the wrong error"),
        }
        for (i, o) in out.iter().enumerate() {
            if i < n_ok {
                assert_eq!(
                    o.to_bits(),
                    expected[i].as_ref().unwrap().to_bits(),
                    "{ctx}: at_many row {i} (stamp {}) is not bit-identical to Plan::at",
                    stamps[i]
                );
            } else {
                assert_eq!(
                    o.to_bits(),
                    [SENTINEL; 7],
                    "{ctx}: at_many wrote row {i}, at or past the refusal"
                );
            }
        }
    }

    // `at_many_into` — the two `f64` pose layouts.
    for layout in [Layout::Quat, Layout::Mat4] {
        let n = elems(layout);
        let mut out = vec![f64::from_bits(SENTINEL); stamps.len() * n];
        let g = tree.guard();
        let got = plan.at_many_into::<SystemDomain>(&g, stamps, layout, &mut out);
        match stop {
            None => assert!(
                got.is_ok(),
                "{ctx}: at_many_into {layout:?} refused: {got:?}"
            ),
            Some((_, e)) => assert_eq!(
                got,
                Err(e),
                "{ctx}: at_many_into {layout:?} returned the wrong error"
            ),
        }
        for i in 0..stamps.len() {
            let row = &out[i * n..(i + 1) * n];
            if i < n_ok {
                let want = rows(expected[i].as_ref().unwrap(), layout);
                for (k, (gv, wv)) in row.iter().zip(want.iter()).enumerate() {
                    assert_eq!(
                        gv.to_bits(),
                        wv.to_bits(),
                        "{ctx}: at_many_into {layout:?} row {i} element {k} (stamp {})",
                        stamps[i]
                    );
                }
            } else {
                assert!(
                    row.iter().all(|v| v.to_bits() == SENTINEL),
                    "{ctx}: at_many_into {layout:?} wrote row {i}, at or past the refusal"
                );
            }
        }
    }

    // `at_many_into_f32` — `Affine32`.
    {
        let n = elems(Layout::Affine32);
        let sent = f32::from_bits(0xDEAD_BEEF);
        let mut out = vec![sent; stamps.len() * n];
        let g = tree.guard();
        let got = plan.at_many_into_f32::<SystemDomain>(&g, stamps, Layout::Affine32, &mut out);
        match stop {
            None => assert!(got.is_ok(), "{ctx}: at_many_into_f32 refused: {got:?}"),
            Some((_, e)) => assert_eq!(got, Err(e), "{ctx}: at_many_into_f32 wrong error"),
        }
        let mut want = vec![0.0f32; n];
        for i in 0..stamps.len() {
            let row = &out[i * n..(i + 1) * n];
            if i < n_ok {
                write_affine32(expected[i].as_ref().unwrap(), &mut want);
                for (k, (gv, wv)) in row.iter().zip(want.iter()).enumerate() {
                    assert_eq!(
                        gv.to_bits(),
                        wv.to_bits(),
                        "{ctx}: at_many_into_f32 row {i} element {k} (stamp {})",
                        stamps[i]
                    );
                }
            } else {
                assert!(
                    row.iter().all(|v| v.to_bits() == 0xDEAD_BEEF),
                    "{ctx}: at_many_into_f32 wrote row {i}, at or past the refusal"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The fixture reaches the branches its regions are named after.
///
/// **This is the anti-vacuity check, and it is not decoration.** Every
/// assertion in this file is "the batch agrees with the scalar fold", which is
/// satisfied trivially by a fixture that only ever reaches one arm — and
/// §8 of `docs/decisions/0060` records two real mutants that survived exactly
/// that fixture. What is asserted here is the *input*: that the relative
/// rotation between the two samples of each crafted region is the one the
/// region's name claims, against the thresholds §9.1 pins.
#[test]
fn the_fixture_reaches_the_branches_it_names() {
    /// The rotation angle between two poses, in radians.
    fn arc(a: &Iso3, b: &Iso3) -> f64 {
        tf_tree::log_so3(a.q.conjugate() * b.q).norm()
    }

    let by_name: std::collections::HashMap<&str, Region> =
        crafted().into_iter().map(|r| (r.name, r)).collect();
    let get = |n: &str| by_name.get(n).unwrap_or_else(|| panic!("region {n}"));

    // The series arm runs while the arc is at most 0.3 rad; either side of it,
    // by 2e-4.
    let below = get("below_series_threshold");
    assert!(
        (arc(&below.poses[0], &below.poses[1]) - 0.2998).abs() < 1e-9,
        "the below-threshold region is not below the threshold"
    );
    let above = get("above_series_threshold");
    let a = arc(&above.poses[0], &above.poses[1]);
    assert!(
        a > 0.3 && a < 0.3005,
        "the above-threshold region is at {a} rad"
    );

    // The LERP fallback wants a chord under 1e-6; 1e-7 rad is well inside it.
    let fb = get("lerp_fallback");
    let f = arc(&fb.poses[0], &fb.poses[1]);
    assert!(f > 0.0 && f < 1e-6, "the fallback region is at {f} rad");

    // A large arc, and one the short-way-round flip has to handle.
    assert!(arc(&get("large_arc").poses[0], &get("large_arc").poses[1]) > 2.0);
    let far = get("far_hemisphere");
    assert!(
        far.poses[0].q.dot(far.poses[1].q) < 0.0,
        "the far-hemisphere region does not cross the hemisphere"
    );

    // Identical rotations, bit for bit — `h == 0` and the degenerate screw.
    let st = get("stationary");
    let qbits = |q: Quat| (q.w.to_bits(), q.x.to_bits(), q.y.to_bits(), q.z.to_bits());
    assert_eq!(
        qbits(st.poses[0].q),
        qbits(st.poses[1].q),
        "the stationary region's rotations are not identical"
    );
    assert_ne!(
        st.poses[0].t.x.to_bits(),
        st.poses[1].t.x.to_bits(),
        "the stationary region does not move at all, so it cannot see a wrong endpoint"
    );

    // Signed zeros really are negative zeros, which `== 0.0` cannot see.
    let sz = get("signed_zeros");
    assert_eq!(sz.poses[0].t.x.to_bits(), (-0.0f64).to_bits());

    // Non-finite translations.
    assert!(get("non_finite").poses[0].t.x.is_nan());
    assert!(get("non_finite").poses[1].t.y.is_infinite());

    // And the sweep is big enough to cross several chunks, with an exact hit on
    // the newest published stamp at the end of it.
    let (samples, queries) = stream();
    assert!(
        queries.len() > 8 * LANES,
        "the sweep is {} stamps, too short to cross chunks",
        queries.len()
    );
    assert_eq!(
        *queries.last().unwrap(),
        samples.last().unwrap().0,
        "the sweep must end on `t == t_new`"
    );
}

/// The whole sweep, and every slice length either side of a chunk boundary, at
/// three offsets.
#[test]
fn every_batch_entry_point_is_bit_identical_to_at() {
    for case in cases() {
        let expected = scalar(&case.tree, &case.plan, &case.stamps);
        assert!(
            expected.iter().all(Result::is_ok),
            "{}: the sweep fixture must not contain a refusal",
            case.name
        );
        assert_batch(
            &case.tree,
            &case.plan,
            &case.stamps,
            &expected,
            &format!("{}/full", case.name),
        );

        for len in [1usize, 2, MIN_BATCH, 63, LANES, LANES + 1, 65, 129] {
            for off in [0usize, 1, 7] {
                if off + len > case.stamps.len() {
                    continue;
                }
                assert_batch(
                    &case.tree,
                    &case.plan,
                    &case.stamps[off..off + len],
                    &expected[off..off + len],
                    &format!("{}/len{len}@{off}", case.name),
                );
            }
        }
    }
}

/// Every stamp alone, and every stamp in a named lane of a chunk.
///
/// The five shapes are the ones that can disagree. Alone and paired take the
/// sub-[`MIN_BATCH`] bypass, which is a different fold; the three windowed
/// shapes put the stamp in lane 0, lane 1 and the last lane of the first chunk
/// of a 17-stamp batch, which always crosses into a second chunk.
///
/// **This is the shape §8 of the record says caught what nothing else did.** A
/// defect that depends on a stamp's position in the chunk — or that is masked
/// when some other lane of the same chunk takes a different branch — is
/// invisible to a fixture that only ever presents a stamp in the same place.
#[test]
fn every_stamp_holds_in_every_lane() {
    for case in cases() {
        let expected = scalar(&case.tree, &case.plan, &case.stamps);
        let n = case.stamps.len();
        for i in 0..n {
            assert_batch(
                &case.tree,
                &case.plan,
                &case.stamps[i..i + 1],
                &expected[i..i + 1],
                &format!("{}/alone@{i}", case.name),
            );
            if i + 2 <= n {
                assert_batch(
                    &case.tree,
                    &case.plan,
                    &case.stamps[i..i + 2],
                    &expected[i..i + 2],
                    &format!("{}/pair@{i}", case.name),
                );
            }
            for lane in [0usize, 1, LANES - 1] {
                let start = i.saturating_sub(lane);
                let end = (start + LANES + 1).min(n);
                assert_batch(
                    &case.tree,
                    &case.plan,
                    &case.stamps[start..end],
                    &expected[start..end],
                    &format!("{}/lane{lane}@{i}", case.name),
                );
            }
        }
    }
}

/// The identity plan and two all-static plans, which fold no dynamic step.
///
/// They reach the chunked loop's `Step::Static` arm with `live` never lowered,
/// and the identity plan reaches it with no steps at all — the one input where
/// a chunk writes its accumulator exactly as it initialised it.
#[test]
fn identity_and_static_only_plans_match_at() {
    let tree = TreeBuilder::new()
        .dynamic_edge("a", "b", cfg(256, InterpPolicy::ScLerp))
        .static_edge("b", "c", &at_angle(0.37, [0.25, -0.5, 0.75]))
        .static_edge("c", "e", &at_angle(-0.2, [1.5, 0.0, -0.25]))
        .build()
        .unwrap();
    let b = tree.frame("b").unwrap();
    let c = tree.frame("c").unwrap();
    let e = tree.frame("e").unwrap();
    {
        let w = tree.claim(b, tree.frame("a").unwrap()).unwrap();
        for i in 0..64i64 {
            w.push(i * MS, &at_angle(0.01 * i as f64, [i as f64, 0.0, 0.0]))
                .unwrap();
        }
    }
    let stamps: Vec<i64> = (0..70).map(|i| i * MS / 2).collect();

    for (name, plan) in [
        ("identity", tree.plan(c, c).unwrap()),
        ("static_only", tree.plan(e, b).unwrap()),
        ("static_only_rev", tree.plan(b, e).unwrap()),
    ] {
        let expected = scalar(&tree, &plan, &stamps);
        assert!(expected.iter().all(Result::is_ok), "{name}: refused");
        assert_batch(&tree, &plan, &stamps, &expected, name);
        for len in [1usize, 2, 3, LANES + 1] {
            assert_batch(&tree, &plan, &stamps[..len], &expected[..len], name);
        }
    }
}

// ---------------------------------------------------------------------------
// The error contract
// ---------------------------------------------------------------------------

/// A tree whose two dynamic edges retain **different** windows, so a stamp can
/// fail on the second step of a plan while the first step answers it.
///
/// `a -> b` retains `[0, 63] ms`; `c -> d` retains `[10, 53] ms`. The plan
/// crosses both.
struct ErrFixture {
    tree: Tree,
    plan: Plan,
}

impl ErrFixture {
    fn new() -> ErrFixture {
        let tree = TreeBuilder::new()
            .dynamic_edge("a", "b", cfg(128, InterpPolicy::ScLerp))
            .static_edge("b", "c", &at_angle(0.37, [0.25, -0.5, 0.75]))
            .dynamic_edge("c", "d", cfg(128, InterpPolicy::LerpSlerp))
            .build()
            .unwrap();
        let (a, b) = (tree.frame("a").unwrap(), tree.frame("b").unwrap());
        let (c, d) = (tree.frame("c").unwrap(), tree.frame("d").unwrap());
        {
            let w_ab = tree.claim(b, a).unwrap();
            for i in 0..=63i64 {
                w_ab.push(i * MS, &at_angle(0.01 * i as f64, [i as f64, 1.0, 2.0]))
                    .unwrap();
            }
            let w_cd = tree.claim(d, c).unwrap();
            for i in 10..=53i64 {
                w_cd.push(i * MS, &at_angle(-0.02 * i as f64, [0.0, i as f64, 0.0]))
                    .unwrap();
            }
        }
        let plan = tree.plan(d, a).unwrap();
        ErrFixture { tree, plan }
    }

    /// A stamp every step answers.
    fn good(i: i64) -> i64 {
        20 * MS + i * (MS / 4)
    }
}

/// The error contract, on a grid: five batch lengths x six positions x three
/// failure kinds.
///
/// What each cell asserts is everything a caller can observe: the error the
/// call returns, that it is the *scalar* error for that stamp, that every row
/// before it is bit-identical to `Plan::at`, and that every row from it on
/// still holds the sentinel the buffer was filled with.
///
/// **The three kinds are three different ways to stop a chunk**, not three
/// spellings of one:
///
/// * `before` — older than the first edge's oldest retained sample, so the
///   refusal comes from step 0 and the lane's accumulator never gets a pose;
/// * `after` — newer than the first edge's newest, which is the other arm of
///   the same branch and the one that also moves the cursor;
/// * `second_step` — inside the first edge's window and outside the second's,
///   so phase 1 of an *earlier* step has already read every lane of the chunk
///   before the refusal happens. That is the cell mutant M5 lives in.
///
/// **Mutant M5, run** (`docs/decisions/0060` §8): in `Plan::fold_batch`, do not
/// lower `live` when phase 1 refuses — `let _ = read;` in place of
/// `live = read;`. It is the defect that writes rows past the refusal, and it
/// is what the sentinel assertion exists for.
#[test]
fn a_refused_batch_stops_where_the_scalar_fold_stops() {
    let f = ErrFixture::new();
    let kinds: [(&str, i64); 3] = [
        ("before", -5 * MS),
        ("after", 500 * MS),
        ("second_step", 60 * MS),
    ];

    for (kind, bad) in kinds {
        // Every kind must really refuse, and for its own reason: an assertion
        // grid whose "failing" stamp answers would pass vacuously.
        let g = f.tree.guard();
        let solo = f.plan.at(&g, ns(bad));
        assert!(solo.is_err(), "{kind}: the failing stamp answered");
        drop(g);

        // Lengths and positions either side of both chunk boundaries: a
        // refusal in the *second* chunk has a whole completed chunk behind it,
        // which is the case where `live` has to shrink without disturbing rows
        // an earlier chunk already wrote.
        for len in [1usize, 2, 3, LANES, LANES + 1, 2 * LANES + 3] {
            for pos in [
                0usize,
                1,
                2,
                LANES - 1,
                LANES,
                LANES + 1,
                2 * LANES,
                2 * LANES + 2,
            ] {
                if pos >= len {
                    continue;
                }
                let stamps: Vec<i64> = (0..len)
                    .map(|i| {
                        if i == pos {
                            bad
                        } else {
                            ErrFixture::good(i as i64)
                        }
                    })
                    .collect();
                // The grid only means anything on a batch the fold accepts as
                // monotone; a `before` stamp in the middle is not.
                if stamps.windows(2).any(|w| w[0] > w[1]) {
                    continue;
                }
                let expected = scalar(&f.tree, &f.plan, &stamps);
                assert_eq!(
                    first_err(&expected).map(|(i, _)| i),
                    Some(pos),
                    "{kind}: len {len} pos {pos} did not fail where it was placed"
                );
                assert_batch(
                    &f.tree,
                    &f.plan,
                    &stamps,
                    &expected,
                    &format!("err/{kind}/len{len}@{pos}"),
                );
            }
        }
    }
}

/// The per-edge counters a refused batch leaves behind: one `lookups_ok` per
/// row written, and nothing for the rows that were never reached.
///
/// **A one-dynamic-edge plan, deliberately.** `Plan::first_dynamic_edge`
/// returns the "credit no edge" sentinel for a plan that crosses several, so a
/// multi-edge plan's successes are attributed to nobody by design and this
/// question cannot be asked of one.
///
/// Gated on `unstable` for the reason `tests/counters.rs` is gated whole: an
/// edge counter is only observable through `Tree::arena_view`, and there is no
/// stable-tier spelling of the question. The count is flushed when the guard
/// drops, not per lookup, so every case reads it after the guard is gone.
#[cfg(all(feature = "unstable", feature = "counters"))]
#[test]
fn a_refused_batch_counts_one_lookup_per_row_it_wrote() {
    use std::sync::atomic::Ordering::Relaxed;

    // `a -> b`, retaining [0, 63] ms.
    let build = || {
        let tree = TreeBuilder::new()
            .dynamic_edge("a", "b", cfg(128, InterpPolicy::ScLerp))
            .build()
            .unwrap();
        let (a, b) = (tree.frame("a").unwrap(), tree.frame("b").unwrap());
        {
            let w = tree.claim(b, a).unwrap();
            for i in 0..=63i64 {
                w.push(i * MS, &at_angle(0.01 * i as f64, [i as f64, 1.0, 2.0]))
                    .unwrap();
            }
        }
        let plan = tree.plan(b, a).unwrap();
        let edge = {
            let view = tree.arena_view();
            let (_p, _d, e, _g) = view.topology().read_frame(b).expect("b is in the topology");
            tf_tree::EdgeId(e)
        };
        (tree, plan, edge)
    };

    for (kind, bad, len, pos) in [
        ("before", -5 * MS, 1usize, 0usize),
        ("after", 500 * MS, LANES + 1, 5),
        ("clean", 0, LANES + 1, usize::MAX),
    ] {
        let (tree, plan, edge) = build();
        let stamps: Vec<i64> = (0..len)
            .map(|i| {
                if i == pos {
                    bad
                } else {
                    ErrFixture::good(i as i64)
                }
            })
            .collect();
        let want_ok = pos.min(len) as u64;

        let mut out = vec![Iso3::IDENTITY; stamps.len()];
        let typed: Vec<Stamp<SystemDomain>> = stamps.iter().map(|t| ns(*t)).collect();
        {
            let g = tree.guard();
            let got = plan.at_many(&g, &typed, &mut out);
            assert_eq!(got.is_err(), pos < len, "{kind}: wrong verdict");
        }

        let view = tree.arena_view();
        let c = view.edge_counters(edge).expect("edge counters");
        assert_eq!(
            c.lookups_ok.load(Relaxed),
            want_ok,
            "{kind}: one counted lookup per row written"
        );
    }
}
