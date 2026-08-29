//! What `ScLerp` buys over `LerpSlerp`, as a function of publish rate — the
//! measurement D5 demands and nobody had taken.
//!
//! ```sh
//! cargo run --release -p tf_tree_bench --example interp_accuracy
//! ```
//!
//! # Why this exists
//!
//! `docs/PROJECT.md` §5 **D5** makes `ScLerp` the default and ends: *"`LerpSlerp`
//! stays available for bit-compatible differential testing against `tf2` and for
//! latency-critical plans. **Do not** remove `LerpSlerp`; **do not** make it the
//! default without a measurement justifying it."*
//!
//! The second half of that sentence has always been read as a rule about
//! *changing* the default. It is also a standing obligation on the default that
//! shipped: `ScLerp` is the default, it costs more than `LerpSlerp`, and the
//! measurement justifying the trade did not exist. `examples/interp_cost.rs`
//! measures the **cost** of each policy in three regimes; this measures the
//! **accuracy** they differ by, so the two together answer the question a
//! deployment actually asks:
//!
//! > *At the rate my estimator publishes, does the default cost me anything I
//! > can measure — and would switching lose me anything I care about?*
//!
//! # What is being compared, and which one is right
//!
//! `ScLerp` **is** the SE(3) geodesic: for a body moving on a constant screw —
//! rotating about an axis while translating along it, which is what any rigid
//! body doing a smooth motion is doing between two close samples — screw-linear
//! interpolation reproduces the true intermediate pose exactly. So `ScLerp` is
//! not one approximation among two here; it is the ground truth, and what is
//! measured is how far `LerpSlerp` departs from it.
//!
//! `LerpSlerp` SLERPs the rotation and LERPs the translation *independently*.
//! The rotation half is the same geodesic, so the two agree there. The
//! translation half is a straight line through space where the truth is a helix,
//! and a chord is shorter than its arc — which is why the error below is a pure
//! position error, grows with the angle turned between samples, and grows with
//! the **lever arm**: how far the frame sits from the axis it is turning about.
//!
//! A sensor bolted 0.5 m off a robot's turn centre is exactly that lever arm,
//! which is why the sweep below carries one rather than rotating in place.
#![allow(clippy::unwrap_used, clippy::print_stdout)]

use tf_tree_math::{Interp, Iso3, LerpSlerp, Quat, ScLerp, Vec3};

/// Samples of `s` across one segment. The maximum deviation is interior — both
/// policies agree at the endpoints by construction — so the grid has to be fine
/// enough to find it rather than to sample near it.
const STEPS: usize = 512;

/// A rotation of `theta` about a unit axis.
fn axis_angle(theta: f64, x: f64, y: f64, z: f64) -> Quat {
    let (s, c) = ((theta * 0.5).sin(), (theta * 0.5).cos());
    Quat {
        w: c,
        x: x * s,
        y: y * s,
        z: z * s,
    }
}

/// One segment of a body turning `theta` about the world `z` axis while sitting
/// `lever` metres off that axis, and climbing slightly — a constant screw.
///
/// Returned as the pair of endpoint poses a ring would hold for two adjacent
/// samples, which is exactly what the sampler hands `Interp::eval`.
fn segment(theta: f64, lever: f64) -> (Iso3, Iso3) {
    // `Iso3::new` rather than a struct literal. The reason written here was that
    // the type carried a private `_pad` a consumer could not fill; `0042`
    // removed it, so the literal compiles now. `new` stays because it is the
    // constructor the rest of this crate uses.
    let pose_at = |a: f64| {
        Iso3::new(
            axis_angle(a, 0.0, 0.0, 1.0),
            Vec3::new(lever * a.cos(), lever * a.sin(), 0.05 * a),
        )
    };
    (pose_at(0.0), pose_at(theta))
}

/// The worst deviation between the two policies across one segment, as
/// (translation metres, rotation radians).
fn deviation(a: &Iso3, b: &Iso3) -> (f64, f64) {
    let (mut dt, mut dr) = (0.0f64, 0.0f64);
    for i in 0..=STEPS {
        let s = i as f64 / STEPS as f64;
        let truth = ScLerp::eval(a, b, s);
        let approx = LerpSlerp::eval(a, b, s);
        // `Vec3` has no `Sub`; the core's arena types stay minimal on purpose.
        let d = Vec3::new(
            truth.t.x - approx.t.x,
            truth.t.y - approx.t.y,
            truth.t.z - approx.t.z,
        );
        dt = dt.max(d.norm());
        // Angle of the relative rotation, via the quaternion dot — the `acos`
        // form is fine here because this is an offline characterisation and not
        // the hot path D12 is about.
        let dot = (truth.q.w * approx.q.w
            + truth.q.x * approx.q.x
            + truth.q.y * approx.q.y
            + truth.q.z * approx.q.z)
            .abs()
            .min(1.0);
        dr = dr.max(2.0 * dot.acos());
    }
    (dt, dr)
}

fn main() {
    // 180 deg/s — `interp_cost`'s "brisk" body, so the two files describe the
    // same motion and their rows can be read side by side.
    const OMEGA: f64 = core::f64::consts::PI;
    const LEVER: f64 = 0.5; // a sensor half a metre off the turn centre

    println!("ScLerp vs LerpSlerp: how far the chord departs from the helix");
    println!(
        "body at {:.0} deg/s, frame {LEVER} m off the rotation axis, worst point in the segment\n",
        OMEGA.to_degrees()
    );
    println!("  rate     angle/sample   position error   rotation error");
    println!("  ------   ------------   --------------   --------------");

    for rate in [1000.0, 500.0, 200.0, 100.0, 50.0, 20.0, 10.0, 5.0] {
        let theta = OMEGA / rate;
        let (dt, dr) = deviation(&segment(theta, LEVER).0, &segment(theta, LEVER).1);
        println!(
            "  {rate:>5.0} Hz   {:>7.1} mrad   {:>9.3} mm      {:>9.3} urad",
            theta * 1e3,
            dt * 1e3,
            dr * 1e6,
        );
    }

    println!();
    println!("How to read it, and what it settles:");
    println!();
    println!("  * The rotation column is ~0 at every rate, and that is structural rather");
    println!("    than lucky: both policies SLERP the rotation, so they cannot disagree");
    println!("    about it. Every bit of the difference is position.");
    println!("  * The position error is a chord-vs-arc error and scales as the lever arm");
    println!("    times theta^2/8. Halve the rate and it quadruples.");
    println!("  * So the default is free where it matters and matters where it is not free:");
    println!("    at kilohertz rates the two policies agree to well under a micrometre and");
    println!("    `interp_cost` shows both taking the transcendental-free series path, so");
    println!("    D5's default costs a fast edge nothing measurable. At 10 Hz — a SLAM or");
    println!("    map edge — the gap is millimetres, which is the regime where a cheaper");
    println!("    interpolator would be silently wrong about where a sensor was.");
    println!();
    println!("  D5 says do not make `LerpSlerp` the default without a measurement. This is");
    println!("  the measurement, and it points the other way: the rates at which `LerpSlerp`");
    println!("  would save anything are the rates at which the two answers are identical.");
}
