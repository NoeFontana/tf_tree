//! Property tests for `tf_tree_math` (`docs/PHASE1.md` §10.1 *Property tests*).
//!
//! Every test uses a fixed, deterministic RNG so CI is reproducible. Random
//! valid `Iso3`/`Quat` are built by `exp_se3` of a random twist, guaranteeing
//! unit quaternions by construction.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use proptest::prelude::*;
use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};
use tf_tree_math::{exp_se3, log_se3, reference, Interp, Iso3, LerpSlerp, Quat, ScLerp, Vec3};

/// A deterministic test runner with a fixed seed and the requested case count.
fn runner(cases: u32) -> TestRunner {
    TestRunner::new_with_rng(
        Config {
            cases,
            failure_persistence: None,
            ..Config::default()
        },
        // Fixed 32-byte seed → reproducible CI runs.
        TestRng::from_seed(RngAlgorithm::ChaCha, &[0x42; 32]),
    )
}

/// Strategy for a twist `[ω(3), v(3)]`: rotation up to ‖ω‖ ≈ 6.9 (covers past
/// `π`, exercising log-branch wrapping and near-π), translation in `±10`.
fn twist() -> impl Strategy<Value = [f64; 6]> {
    (
        -4.0f64..4.0,
        -4.0f64..4.0,
        -4.0f64..4.0,
        -10.0f64..10.0,
        -10.0f64..10.0,
        -10.0f64..10.0,
    )
        .prop_map(|(a, b, c, d, e, f)| [a, b, c, d, e, f])
}

/// Strategy for a random valid rigid transform.
fn iso() -> impl Strategy<Value = Iso3> {
    twist().prop_map(exp_se3)
}

/// A twist whose rotation is deliberately tiny (near identity), to force the
/// small-angle branches.
fn tiny_iso() -> impl Strategy<Value = Iso3> {
    (
        -1e-9f64..1e-9,
        -1e-9f64..1e-9,
        -1e-9f64..1e-9,
        -10.0f64..10.0,
        -10.0f64..10.0,
        -10.0f64..10.0,
    )
        .prop_map(|(a, b, c, d, e, f)| exp_se3([a, b, c, d, e, f]))
}

/// Scale-aware error between two transforms: absolute rotation error (radians)
/// plus translation error relative to the operands' scale. This matches the
/// spec's tolerances, which are calibrated for O(1) magnitudes; a raw geodesic
/// norm would fold in absolute translation error that grows with ‖t‖ (here up
/// to ~17) and so penalize large-but-correct transforms. Robust to the
/// quaternion double cover.
fn iso_err(a: &Iso3, b: &Iso3) -> f64 {
    let mut dq = a.q * b.q.conjugate();
    if dq.w < 0.0 {
        dq = dq.neg();
    }
    let nv = (dq.x * dq.x + dq.y * dq.y + dq.z * dq.z).sqrt();
    let rot = 2.0 * nv.atan2(dq.w.abs());
    let dt = (a.t.sub(b.t)).norm();
    let scale = 1.0 + a.t.norm().max(b.t.norm());
    rot + dt / scale
}

// #1 — associativity of composition.
#[test]
fn prop_assoc() {
    runner(10_000)
        .run(&(iso(), iso(), iso()), |(a, b, c)| {
            let lhs = (a * b) * c;
            let rhs = a * (b * c);
            prop_assert!(iso_err(&lhs, &rhs) < 1e-12);
            Ok(())
        })
        .unwrap();
}

// #2 — a · a⁻¹ ≈ identity.
#[test]
fn prop_inverse() {
    runner(10_000)
        .run(&iso(), |a| {
            let id = a * a.inverse();
            prop_assert!(iso_err(&id, &Iso3::IDENTITY) < 1e-12);
            Ok(())
        })
        .unwrap();
}

// #3 — exp_se3(log_se3(T)) ≈ T, including near-π and near-identity.
#[test]
fn prop_exp_log_roundtrip() {
    let strat = prop_oneof![iso(), tiny_iso()];
    runner(10_000)
        .run(&strat, |t| {
            let rt = exp_se3(log_se3(t));
            prop_assert!(iso_err(&rt, &t) < 1e-13, "err={:e}", iso_err(&rt, &t));
            Ok(())
        })
        .unwrap();
}

// #13 — mul_inv(a, b) ≈ a · b⁻¹.
#[test]
fn prop_mul_inv() {
    runner(10_000)
        .run(&(iso(), iso()), |(a, b)| {
            let direct = a.mul_inv(&b);
            let naive = a * b.inverse();
            prop_assert!(iso_err(&direct, &naive) < 1e-14);
            Ok(())
        })
        .unwrap();
}

// #6 — endpoints are EXACT for both interpolators.
#[test]
fn prop_endpoints_exact() {
    runner(10_000)
        .run(&(iso(), iso()), |(a, b)| {
            prop_assert_eq!(ScLerp::eval(&a, &b, 0.0), a);
            prop_assert_eq!(ScLerp::eval(&a, &b, 1.0), b);
            prop_assert_eq!(LerpSlerp::eval(&a, &b, 0.0), a);
            prop_assert_eq!(LerpSlerp::eval(&a, &b, 1.0), b);
            Ok(())
        })
        .unwrap();
}

// #14 — fast dual-quaternion sclerp ≈ reference log/exp sclerp (≥1e5 cases).
//
// A single tight tolerance holds across the whole domain, INCLUDING relative
// rotations near π. This used to require an adaptive bound: the mandated
// reference (`a·exp_se3(s·log_se3(a⁻¹·b))`) previously lost ~1e-11 near π because
// its naive `c3 = 1/θ² − (1+cosθ)/(2θ sinθ)` cancels as `1+cosθ → 0`. `v_coeffs`
// now uses the half-angle form `c3 = 1/θ² − cot(θ/2)/(2θ)`, which is
// well-conditioned up to π, so the reference and the fast screw method agree to
// ~1e-13 everywhere. `sclerp_fast_matches_high_precision_near_pi` below still
// pins the fast method independently against a 90-digit oracle. (This is the
// π-rotation precision trap `docs/PHASE1.md` §3.3 warns about — now closed on both
// paths.)
#[test]
fn prop_sclerp_fast_matches_reference() {
    let strat = prop_oneof![
        (iso(), iso(), 0.0f64..1.0),
        (tiny_iso(), tiny_iso(), 0.0f64..1.0),
    ];
    runner(100_000)
        .run(&strat, |(a, b, s)| {
            let fast = ScLerp::eval(&a, &b, s);
            let reference = reference::sclerp(&a, &b, s);
            let e = iso_err(&fast, &reference);
            prop_assert!(e < 1e-13, "s={} err={:e}", s, e);
            Ok(())
        })
        .unwrap();
}

// The fast screw method's accuracy where the reference cannot be trusted:
// near-π relative rotations, compared against a 90-significant-digit oracle
// (scratchpad/oracle_table.py, Python `decimal`). Each row is
// `(b_quat, b_translation, s, oracle_quat, oracle_translation)` with a = I, so
// the interpolated result is `b` raised to the power `s` along its screw.
#[test]
fn sclerp_fast_matches_high_precision_near_pi() {
    #[allow(clippy::type_complexity)]
    const NEAR_PI: [([f64; 4], [f64; 3], f64, [f64; 4], [f64; 3]); 5] = [
        (
            [
                4.999999997940337e-08,
                0.9407208683835961,
                0.18814417367671923,
                -0.2822162605150788,
            ],
            [0.5, -0.3, 0.8],
            0.4924,
            [
                0.7154976886601457,
                0.6572018032510643,
                0.13144036065021286,
                -0.19716054097531924,
            ],
            [0.21188329289479987, 0.3004496252887727, 0.5783107265085148],
        ),
        (
            [
                5.000000000073155e-06,
                -0.3698001308121969,
                0.9245003270304922,
                0.09245003270304923,
            ],
            [2.0, 1.5, -1.0],
            0.3,
            [
                0.891007205173115,
                -0.16788525195159495,
                0.41971312987898735,
                0.04197131298789874,
            ],
            [0.8230179464279189, 0.432105218133333, 0.7710196043783455],
        ),
        (
            [
                0.0004999999791666731,
                0.7035263827406415,
                -0.7035263827406415,
                0.10050376896294878,
            ],
            [-1.2, 0.4, 3.0],
            0.75,
            [
                0.38302986027423297,
                0.6498727006718726,
                -0.6498727006718726,
                0.09283895723883893,
            ],
            [-0.2041008556217491, 1.0698793347797846, 2.767861332810736],
        ),
        (
            [5.000000001311005e-07, 0.0, 0.0, 0.999999999999875],
            [1.0, 2.0, 3.0],
            0.5,
            [0.7071069579632208, 0.0, 0.0, 0.7071066044098301],
            [1.499999500000125, 0.5000002499999375, 1.5],
        ),
        (
            [
                4.9999999979333415e-05,
                0.5773502684679379,
                0.5773502684679379,
                0.5773502684679379,
            ],
            [-2.0, 0.0, 0.5],
            0.62,
            [
                0.5621090170798481,
                0.47750513188080923,
                0.47750513188080923,
                0.47750513188080923,
            ],
            [-1.470280143843953, 0.70300383391807, -0.16272369007411694],
        ),
    ];
    for (bq, bt, s, oq, ot) in NEAR_PI {
        let b = Iso3::new(
            Quat::new(bq[0], bq[1], bq[2], bq[3]),
            Vec3::new(bt[0], bt[1], bt[2]),
        );
        let oracle = Iso3::new(
            Quat::new(oq[0], oq[1], oq[2], oq[3]),
            Vec3::new(ot[0], ot[1], ot[2]),
        );
        let fast = ScLerp::eval(&Iso3::IDENTITY, &b, s);
        let e = iso_err(&fast, &oracle);
        assert!(e < 1e-13, "fast vs 90-digit oracle: s={s} err={e:e}");
    }
}

// #7 — ScLerp left-invariance: interp(G·a, G·b, s) == G·interp(a, b, s).
#[test]
fn prop_sclerp_left_invariant() {
    runner(10_000)
        .run(&(iso(), iso(), iso(), 0.0f64..1.0), |(g, a, b, s)| {
            let lhs = ScLerp::eval(&(g * a), &(g * b), s);
            let rhs = g * ScLerp::eval(&a, &b, s);
            prop_assert!(iso_err(&lhs, &rhs) < 1e-13);
            Ok(())
        })
        .unwrap();
}

// #8 — ScLerp right-invariance: interp(a·H, b·H, s) == interp(a, b, s)·H.
#[test]
fn prop_sclerp_right_invariant() {
    runner(10_000)
        .run(&(iso(), iso(), iso(), 0.0f64..1.0), |(a, b, h, s)| {
            let lhs = ScLerp::eval(&(a * h), &(b * h), s);
            let rhs = ScLerp::eval(&a, &b, s) * h;
            prop_assert!(iso_err(&lhs, &rhs) < 1e-13);
            Ok(())
        })
        .unwrap();
}

// #9 — LerpSlerp is NOT right-invariant, and this MUST be true.
//
// This is a positive test that `LerpSlerp` breaks the property `ScLerp`
// preserves. It is exactly why `ScLerp` is the default. If someone "fixes"
// `LerpSlerp` so this passes, they have silently changed its tf2-compatible
// semantics — do NOT weaken or delete this assertion (`docs/PHASE1.md` §3.4).
#[test]
fn prop_lerpslerp_not_right_invariant() {
    // A fixed, seeded set built from a deterministic xorshift PRNG.
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        // map to a broad twist component in ±3
        ((state >> 11) as f64 / (1u64 << 53) as f64) * 6.0 - 3.0
    };
    let mut sample = || exp_se3([next(), next(), next(), next(), next(), next()]);

    let mut max_err = 0.0f64;
    for _ in 0..4_000 {
        let a = sample();
        let b = sample();
        let h = sample();
        let s = 0.5; // a generic interior point
        let lhs = LerpSlerp::eval(&(a * h), &(b * h), s);
        let rhs = LerpSlerp::eval(&a, &b, s) * h;
        max_err = max_err.max(iso_err(&lhs, &rhs));
    }
    assert!(
        max_err > 1e-6,
        "LerpSlerp unexpectedly looked right-invariant (max_err={max_err:e}); \
         this test is supposed to demonstrate it is NOT — do not 'fix' it"
    );
}

// ---------------------------------------------------------------------------
// `quat_from_rot3` — the publish direction of the C ABI's matrix layouts
// ---------------------------------------------------------------------------

/// The rotation matrix of `q`, row-major. The forward direction, written out
/// here rather than imported so the round-trip below is not `f(f⁻¹(x))` through
/// one shared expression.
fn rot3_of(q: Quat) -> [f64; 9] {
    let (w, x, y, z) = (q.w, q.x, q.y, q.z);
    let (xx, yy, zz) = (x * x, y * y, z * z);
    let (xy, xz, yz) = (x * y, x * z, y * z);
    let (wx, wy, wz) = (w * x, w * y, w * z);
    [
        1.0 - 2.0 * (yy + zz),
        2.0 * (xy - wz),
        2.0 * (xz + wy),
        2.0 * (xy + wz),
        1.0 - 2.0 * (xx + zz),
        2.0 * (yz - wx),
        2.0 * (xz - wy),
        2.0 * (yz + wx),
        1.0 - 2.0 * (xx + yy),
    ]
}

/// `q` and `−q` are the same rotation, so compare on the canonical hemisphere.
fn quat_err(a: Quat, b: Quat) -> f64 {
    let b = if a.dot(b) < 0.0 {
        Quat::new(-b.w, -b.x, -b.y, -b.z)
    } else {
        b
    };
    (a.w - b.w)
        .abs()
        .max((a.x - b.x).abs())
        .max((a.y - b.y).abs())
        .max((a.z - b.z).abs())
}

/// **`quat_from_rot3` inverts `rot3` over the whole rotation group.**
///
/// The twist strategy reaches ‖ω‖ ≈ 6.9, so the sample covers rotations past π
/// as well as the near-identity ones — all four of Shepperd's branches are
/// taken.
#[test]
fn prop_quat_from_rot3_inverts_rot3() {
    // `Cell`, because proptest's closure is `Fn` and not `FnMut`.
    let branches: [core::cell::Cell<usize>; 4] = Default::default();
    runner(2048)
        .run(&twist(), |xi| {
            let q = exp_se3(xi).q;
            let r = rot3_of(q);
            let got = tf_tree_math::quat_from_rot3(&r);
            // Record which branch ran, so the assertion below is not vacuous.
            let (r00, r11, r22) = (r[0], r[4], r[8]);
            let b = &branches[if r00 + r11 + r22 > 0.0 {
                0
            } else if r00 > r11 && r00 > r22 {
                1
            } else if r11 > r22 {
                2
            } else {
                3
            }];
            b.set(b.get() + 1);
            prop_assert!(
                quat_err(q, got) < 1e-14,
                "err {} for {q:?} -> {got:?}",
                quat_err(q, got)
            );
            // The result is a unit quaternion without any normalization step.
            prop_assert!((got.norm() - 1.0).abs() < 1e-14);
            Ok(())
        })
        .unwrap();
    assert!(
        branches.iter().all(|n| n.get() > 0),
        "not every Shepperd branch was exercised: {branches:?} — the test is \
         weaker than it looks"
    );
}

/// **Near θ = π, which is the entire reason the four branches exist.**
///
/// The naive `w = √(1 + tr R)/2` form divides the vector part by `4w`, and
/// `w → 0` as `θ → π`. A robot's `map → odom` yaw passes through π routinely, so
/// this is not a corner case, it is Tuesday.
///
/// The oracle is **Rodrigues**, built from the axis-angle vector directly, so it
/// shares no code with the quaternion algebra on either side.
///
/// Mutant: delete the three non-trace branches and always take the trace one ⇒
/// the error at `θ = π − 1e−7` blows up from ~1e−16 to ~1e−9 and this fails,
/// while `prop_quat_from_rot3_inverts_rot3` above still passes at its 1e−14
/// tolerance for most cases.
#[test]
fn quat_from_rot3_is_accurate_at_a_half_turn() {
    let axis = Vec3::new(0.6, -0.8, 0.0); // unit
    for eps in [0.0f64, 1e-12, 1e-7, 1e-3] {
        let theta = core::f64::consts::PI - eps;
        let w = Vec3::new(axis.x * theta, axis.y * theta, axis.z * theta);

        // Rodrigues, independent of any quaternion code.
        let (s, c) = (theta.sin(), theta.cos());
        let k = [
            [0.0, -axis.z, axis.y],
            [axis.z, 0.0, -axis.x],
            [-axis.y, axis.x, 0.0],
        ];
        let mut r = [0.0f64; 9];
        for i in 0..3 {
            for j in 0..3 {
                let kk: f64 = (0..3).map(|m| k[i][m] * k[m][j]).sum();
                r[i * 3 + j] = f64::from(u8::from(i == j)) + s * k[i][j] + (1.0 - c) * kk;
            }
        }

        let want = tf_tree_math::exp_so3(w);
        let got = tf_tree_math::quat_from_rot3(&r);
        let err = quat_err(want, got);
        assert!(
            err < 1e-14,
            "θ = π − {eps}: error {err} (want {want:?}, got {got:?})"
        );
    }
}
