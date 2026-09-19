//! What `ScLerp` buys over `LerpSlerp`, as a function of publish rate — the measurement D5 demands.
//!
//! ```sh
//! cargo run --release -p tf_tree_bench --example interp_accuracy
//! ```
//!
//! # Why this exists
//!
//! `docs/PROJECT.md` §5 **D5** makes `ScLerp` the default: do not make `LerpSlerp` the default without a
//! measurement. `interp_cost` measures the **cost**; this measures the **accuracy** they differ by: at
//! my publish rate, does the default cost anything measurable, and would switching lose anything?
//!
//! # What is being compared
//!
//! `ScLerp` **is** the SE(3) geodesic for a body on a constant screw, so it is the ground truth; what
//! is measured is how far `LerpSlerp` departs. Its rotation half agrees; its translation half is a
//! chord where the truth is a helix, so the error is a pure position error growing with the angle
//! turned and the **lever arm** (distance from the turn axis). The sweep carries a 0.5 m lever arm.
#![allow(clippy::unwrap_used, clippy::print_stdout)]

use tf_tree_math::{Interp, Iso3, LerpSlerp, Quat, ScLerp, Vec3};

/// Samples of `s` across one segment; the maximum deviation is interior (endpoints agree by construction).
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

/// One segment of a body turning `theta` about the world `z` axis, `lever` metres off it and climbing
/// slightly (a constant screw), as the pair of endpoint poses the sampler hands `Interp::eval`.
fn segment(theta: f64, lever: f64) -> (Iso3, Iso3) {
    // `Iso3::new`, the constructor the rest of this crate uses.
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
        // Angle of the relative rotation via the quaternion dot; `acos` is fine off the hot path (D12).
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
    // 180 deg/s — `interp_cost`'s "brisk" body, so rows read side by side.
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
