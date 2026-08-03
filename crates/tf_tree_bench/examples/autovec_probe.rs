//! Does autovectorisation reach the batch interpolation loop for free?
//!
//! `docs/decisions/0016` open question 2, and its implementation-plan step 3:
//! *"Try autovectorisation first: shape the batch interpolation loop over
//! `[f64; 4]` and read the asm. If it vectorises, stop."* This harness is the
//! measurement half of that; the asm half is recorded in `0016`'s *Amendment*.
//!
//! # What it isolates, and why it is not `interp_cost`
//!
//! `interp_cost` times **one** `Interp::eval` at a time. The question here is
//! about a *loop over stamps* — whether LLVM can run several stamps' worth of
//! interpolation in one set of SIMD registers — so the unit of measurement has
//! to be the loop, not the call.
//!
//! Each variant computes the same interpolation over the same 1024 pose pairs
//! and differs only in the **shape** it hands the vectoriser:
//!
//! | | shape |
//! |---|---|
//! | **A** | `Interp::eval` in a loop — the engine's arithmetic, idealised: no atomics, no bracket search, no `?` |
//! | **B** | as A with the endpoint shortcuts and the large-arc branch removed, so the loop body is straight-line |
//! | **C** | structure-of-arrays: seven planar `f64` lanes in, four out |
//! | **D** | `0016` step 3 literally — `[f64; 4]` blocks over a `chunks`-style walk |
//!
//! A is the ceiling any *engine* change could reach without restructuring the
//! fold, because the engine's real loop body is strictly A plus a seqlock, a
//! galloping search and a `?`. B/C/D are what a restructured fold could reach.
//!
//! # Run it twice
//!
//! ```text
//! taskset -c 2 cargo run --release -p tf_tree_bench --example autovec_probe
//! RUSTFLAGS="-C no-vectorize-slp -C no-vectorize-loops" \
//!   taskset -c 2 cargo run --release -p tf_tree_bench --example autovec_probe
//! ```
//!
//! The difference between the two runs is what the compiler's vectoriser is
//! worth on each shape. A shape whose two runs agree was never vectorised.
//!
//! Unpinned runs migrate cores and swing by >30%.
#![allow(clippy::unwrap_used, clippy::print_stdout)]

use std::hint::black_box;
use std::time::Instant;

use tf_tree::{Interp, Iso3, LerpSlerp, Quat, ScLerp, Vec3};

/// Stamps per timed round — `docs/API.md` §3.3's batch size.
const N: usize = 1024;
/// Timed rounds per repeat.
const ROUNDS: usize = 20_000;
/// Repeats; the **best** is reported, not the median. A cold round is noise in
/// one direction only.
const REPEATS: usize = 7;

/// Pose pairs one 200 Hz tick apart on a body rotating at a brisk 180 °/s, plus
/// an interior `s`. Both endpoints are arbitrary rigid transforms, and the arc
/// sits inside `THETA_SLERP_SMALL`, so every variant takes the series path —
/// which is the branch `interp.rs`'s own rate table says dominates.
fn data() -> (Vec<Iso3>, Vec<Iso3>, Vec<f64>) {
    let mut a = Vec::with_capacity(N);
    let mut b = Vec::with_capacity(N);
    let mut s = Vec::with_capacity(N);
    for i in 0..N {
        let u = i as f64 * 0.000_613;
        let base = Iso3::new(
            axis_angle(0.3 + u, 0.267_261, 0.534_522, 0.801_784),
            Vec3::new(1.5 + u, -0.7 + 2.0 * u, 3.1 - u),
        );
        let (sx, sy, sz) = ((u * 7.0).cos(), (u * 5.0).sin(), (u * 3.0 + 1.0).cos());
        let n = (sx * sx + sy * sy + sz * sz).sqrt();
        let rel = Iso3::new(
            axis_angle(1.6e-2, sx / n, sy / n, sz / n),
            Vec3::new(0.01 * u, -0.02 + u, 0.03 * u),
        );
        a.push(base);
        b.push(base * rel);
        // Interior, and never 0 or 1: the endpoint shortcuts would otherwise
        // skip the work being measured.
        s.push(0.05 + 0.9 * ((i % 97) as f64 / 97.0));
    }
    (a, b, s)
}

fn axis_angle(theta: f64, x: f64, y: f64, z: f64) -> Quat {
    let half = 0.5 * theta;
    let sn = half.sin();
    Quat::new(half.cos(), sn * x, sn * y, sn * z)
}

/// **A** — the shipped `Interp::eval`, in the most vectoriser-friendly loop the
/// engine's arithmetic can be put in.
#[inline(never)]
fn variant_a<I: Interp>(a: &[Iso3], b: &[Iso3], s: &[f64], out: &mut [Iso3]) {
    for (((a, b), s), o) in a.iter().zip(b).zip(s).zip(out) {
        *o = I::eval(a, b, *s);
    }
}

/// **B** — as A, with the two endpoint shortcuts, the degenerate-input guard and
/// the large-arc `acos`/`sin` branch removed, leaving a straight-line body.
#[inline(never)]
fn variant_b(a: &[Iso3], b: &[Iso3], s: &[f64], out: &mut [Iso3]) {
    for (((a, b), s), o) in a.iter().zip(b).zip(s).zip(out) {
        let s = *s;
        let t = a.t.scale(1.0 - s).add(b.t.scale(s));
        let (qa, qb) = (a.q, b.q);
        let qb = if qa.dot(qb) < 0.0 { qb.neg() } else { qb };
        let u = theta_sq(0.5 * qa.sub(qb).norm_squared());
        *o = Iso3::new(qa.scale(weight(1.0 - s, u)).add(qb.scale(weight(s, u))), t);
    }
}

/// **C** — structure-of-arrays. Every slice is re-sliced to the common length
/// first, so the loop has one exit rather than one per index.
#[inline(never)]
fn variant_c(qa: &[[f64; 4]], qb: &[[f64; 4]], s: &[f64], ow: &mut [[f64; 4]]) {
    let n = s.len();
    let (qa, qb, ow) = (&qa[..n], &qb[..n], &mut ow[..n]);
    for i in 0..n {
        let (a, b) = (qa[i], qb[i]);
        let dot = a[0] * b[0] + a[1] * b[1] + a[2] * b[2] + a[3] * b[3];
        let sg = if dot < 0.0 { -1.0 } else { 1.0 };
        let bq = [sg * b[0], sg * b[1], sg * b[2], sg * b[3]];
        let d = [a[0] - bq[0], a[1] - bq[1], a[2] - bq[2], a[3] - bq[3]];
        let h = 0.5 * (d[0] * d[0] + d[1] * d[1] + d[2] * d[2] + d[3] * d[3]);
        let u = theta_sq(h);
        let (wa, wb) = (weight(1.0 - s[i], u), weight(s[i], u));
        ow[i] = [
            a[0] * wa + bq[0] * wb,
            a[1] * wa + bq[1] * wb,
            a[2] * wa + bq[2] * wb,
            a[3] * wa + bq[3] * wb,
        ];
    }
}

/// **D** — `0016`'s implementation-plan step 3, taken literally: `[f64; 4]`
/// blocks with a compile-time-constant inner trip count.
#[inline(never)]
fn variant_d(qa: &[[f64; 4]], qb: &[[f64; 4]], s: &[f64], ow: &mut [[f64; 4]]) {
    let n = s.len() / 4 * 4;
    let mut i = 0;
    while i < n {
        let mut wa = [0.0f64; 4];
        let mut wb = [0.0f64; 4];
        let mut sgn = [0.0f64; 4];
        for (l, ((wa, wb), sgn)) in wa.iter_mut().zip(&mut wb).zip(&mut sgn).enumerate() {
            let (a, b) = (qa[i + l], qb[i + l]);
            let dot = a[0] * b[0] + a[1] * b[1] + a[2] * b[2] + a[3] * b[3];
            let sg = if dot < 0.0 { -1.0 } else { 1.0 };
            let d = [
                a[0] - sg * b[0],
                a[1] - sg * b[1],
                a[2] - sg * b[2],
                a[3] - sg * b[3],
            ];
            let h = 0.5 * (d[0] * d[0] + d[1] * d[1] + d[2] * d[2] + d[3] * d[3]);
            let u = theta_sq(h);
            *sgn = sg;
            *wa = weight(1.0 - s[i + l], u);
            *wb = weight(s[i + l], u);
        }
        for l in 0..4 {
            let (a, b) = (qa[i + l], qb[i + l]);
            let (ka, kb) = (wa[l], sgn[l] * wb[l]);
            ow[i + l] = [
                a[0] * ka + b[0] * kb,
                a[1] * ka + b[1] * kb,
                a[2] * ka + b[2] * kb,
                a[3] * ka + b[3] * kb,
            ];
        }
        i += 4;
    }
}

/// `θ²` from `h = 1 − |cos θ|` — a local copy of `interp.rs`'s
/// `theta_sq_from_chord`, which is `pub(crate)`. `variants_agree_with_eval`
/// below is what keeps this copy honest.
#[inline]
fn theta_sq(h: f64) -> f64 {
    const C: [f64; 8] = [
        1.0,
        1.0 / 6.0,
        2.0 / 45.0,
        1.0 / 70.0,
        8.0 / 1575.0,
        4.0 / 2079.0,
        16.0 / 21021.0,
        2.0 / 6435.0,
    ];
    let mut acc = C[7];
    for &c in C[..7].iter().rev() {
        acc = c + h * acc;
    }
    2.0 * h * acc
}

/// `sin(a·θ)/sin(θ)` as a series in `u = θ²` — a local copy of `interp.rs`'s
/// `slerp_weight`, for the same reason.
#[inline]
fn weight(a: f64, u: f64) -> f64 {
    let x = a * a;
    let k = 1.0 - x;
    let c1 = 1.0 / 6.0;
    let c2 = (7.0 - 3.0 * x) / 360.0;
    let c3 = (31.0 + x * (-18.0 + 3.0 * x)) / 15120.0;
    let c4 = (381.0 + x * (-239.0 + x * (55.0 - 5.0 * x))) / 1_814_400.0;
    let c5 = (2555.0 + x * (-1636.0 + x * (410.0 + x * (-52.0 + 3.0 * x)))) / 119_750_400.0;
    a * (1.0 + k * u * (c1 + u * (c2 + u * (c3 + u * (c4 + u * c5)))))
}

/// Best-of-`REPEATS` nanoseconds per element.
fn best<F: FnMut()>(label: &str, mut f: F) -> f64 {
    for _ in 0..3 {
        f();
    }
    let mut b = f64::INFINITY;
    for _ in 0..REPEATS {
        let t0 = Instant::now();
        for _ in 0..ROUNDS {
            f();
        }
        let ns = t0.elapsed().as_nanos() as f64 / (ROUNDS * N) as f64;
        b = b.min(ns);
    }
    println!("{label:<44} {b:7.3}");
    b
}

/// The comparison is only worth anything if every variant computes the same
/// number. B, C and D drop branches that are unreachable *for this data*; if a
/// future edit makes one of them reachable this is what says so, before the
/// timings are read as a speedup.
fn variants_agree_with_eval(a: &[Iso3], b: &[Iso3], s: &[f64]) -> f64 {
    let (qa, qb) = (quats(a), quats(b));
    let mut ob = vec![Iso3::IDENTITY; N];
    let mut oc = vec![[0.0f64; 4]; N];
    let mut od = vec![[0.0f64; 4]; N];
    variant_b(a, b, s, &mut ob);
    variant_c(&qa, &qb, s, &mut oc);
    variant_d(&qa, &qb, s, &mut od);
    let mut worst = 0.0f64;
    for i in 0..N {
        let want = <LerpSlerp as Interp>::eval(&a[i], &b[i], s[i]);
        let w = [want.q.w, want.q.x, want.q.y, want.q.z];
        for k in 0..4 {
            let bq = [ob[i].q.w, ob[i].q.x, ob[i].q.y, ob[i].q.z];
            worst = worst
                .max((bq[k] - w[k]).abs())
                .max((oc[i][k] - w[k]).abs())
                .max((od[i][k] - w[k]).abs());
        }
    }
    worst
}

fn quats(v: &[Iso3]) -> Vec<[f64; 4]> {
    v.iter().map(|p| [p.q.w, p.q.x, p.q.y, p.q.z]).collect()
}

fn main() {
    let (a, b, s) = data();
    let (qa, qb) = (quats(&a), quats(&b));
    let mut out = vec![Iso3::IDENTITY; N];
    let mut oq = vec![[0.0f64; 4]; N];

    let worst = variants_agree_with_eval(&a, &b, &s);
    println!("autovectorisation probe — `docs/decisions/0016` open question 2");
    println!("===============================================================");
    println!("{N} stamps/round, best of {REPEATS} × {ROUNDS} rounds");
    println!("variants agree with LerpSlerp::eval to {worst:.3e} absolute\n");
    assert!(
        worst < 1e-14,
        "a variant computes something other than the shipped slerp ({worst:e}) \
         — the timings below are not comparable"
    );

    println!("{:<44} {:>7}", "shape", "ns/elem");
    best("A  LerpSlerp::eval, loop (engine's arithmetic)", || {
        variant_a::<LerpSlerp>(black_box(&a), &b, &s, &mut out);
        black_box(&out);
    });
    best("A' ScLerp::eval, loop (the default policy)", || {
        variant_a::<ScLerp>(black_box(&a), &b, &s, &mut out);
        black_box(&out);
    });
    best("B  branch-free, array-of-structs", || {
        variant_b(black_box(&a), &b, &s, &mut out);
        black_box(&out);
    });
    best("C  branch-free, structure-of-arrays", || {
        variant_c(black_box(&qa), &qb, &s, &mut oq);
        black_box(&oq);
    });
    best("D  `[f64; 4]` blocks (0016 step 3, literally)", || {
        variant_d(black_box(&qa), &qb, &s, &mut oq);
        black_box(&oq);
    });
}
