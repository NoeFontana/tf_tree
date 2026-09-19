//! The facade's math re-exports are the same items as `tf_tree_math`'s, on the
//! stable tier (`docs/API.md` §2.6; no `cfg(feature)` on this target).
//!
//! No test compares return values: `pub use` names one function, so that
//! comparison cannot fail. What can fail is a missing re-export or a facade
//! wrapper. [`same_item`] rejects both at compile time: a function item's type is
//! unique per definition, so two of them share one generic parameter only if they
//! are the same definition.

use tf_tree::{Interp, Iso3, LerpSlerp, Quat, ScLerp, Twist, Vec3};

/// Compiles only if both arguments are the same item.
fn same_item<T>(_: T, _: T) {}

/// Every function the facade re-exports from `tf_tree_math` is that crate's own
/// item, `slerp` included (`docs/API.md` §6 row 16).
#[test]
fn every_re_exported_function_is_the_same_item_as_tf_tree_maths() {
    same_item(tf_tree::slerp, tf_tree_math::slerp);
    same_item(tf_tree::exp_se3, tf_tree_math::exp_se3);
    same_item(tf_tree::exp_so3, tf_tree_math::exp_so3);
    same_item(tf_tree::log_se3, tf_tree_math::log_se3);
    same_item(tf_tree::log_so3, tf_tree_math::log_so3);
    same_item(tf_tree::quat_from_rot3, tf_tree_math::quat_from_rot3);
}

/// `ScLerp`'s kernel is reachable through the same `dualquat` path, not a second
/// spelling (`PROJECT.md` §6).
#[test]
fn sclerps_kernel_is_the_same_item_through_either_prefix() {
    same_item(
        tf_tree::dualquat::screw_pow,
        tf_tree_math::dualquat::screw_pow,
    );
    same_item(
        tf_tree::dualquat::screw_twist,
        tf_tree_math::dualquat::screw_twist,
    );
    same_item(
        tf_tree::dualquat::screw_pow_with_twist,
        tf_tree_math::dualquat::screw_pow_with_twist,
    );
}

/// The re-exported types are the same types. A `const`, so it fails wherever the
/// target compiles.
const _: () = {
    fn _iso3(x: tf_tree::Iso3) -> tf_tree_math::Iso3 {
        x
    }
    fn _quat(x: tf_tree::Quat) -> tf_tree_math::Quat {
        x
    }
    fn _vec3(x: tf_tree::Vec3) -> tf_tree_math::Vec3 {
        x
    }
    fn _twist(x: tf_tree::Twist) -> tf_tree_math::Twist {
        x
    }
    fn _lerpslerp(x: tf_tree::LerpSlerp) -> tf_tree_math::LerpSlerp {
        x
    }
    fn _sclerp(x: tf_tree::ScLerp) -> tf_tree_math::ScLerp {
        x
    }
};

/// The kernel and the policy that evaluates it are both reachable from this
/// crate alone.
#[test]
fn the_kernel_and_its_policy_are_both_reachable_through_the_facade() {
    let qa = Quat::IDENTITY;
    let qb = tf_tree::exp_so3(Vec3::new(0.0, 0.0, core::f64::consts::FRAC_PI_2));

    let mid = tf_tree::slerp(qa, qb, 0.5);
    let via_policy =
        <LerpSlerp as Interp>::eval(&Iso3::new(qa, Vec3::ZERO), &Iso3::new(qb, Vec3::ZERO), 0.5).q;
    // Endpoint differences are `tf_tree_math`'s own
    // `the_iso3_round_trip_it_replaces_agrees_as_a_rotation`.
    assert_eq!(mid.w.to_bits(), via_policy.w.to_bits());
    assert_eq!(mid.z.to_bits(), via_policy.z.to_bits());

    // The other policy; see `docs/API.md` §2.7 on `eval(&Iso3::IDENTITY, ..)`.
    let rel = Iso3::new(qb, Vec3::new(1.5, -2.0, 0.25));
    let via_policy = <ScLerp as Interp>::eval(&Iso3::IDENTITY, &rel, 0.5);
    let via_kernel = tf_tree::dualquat::screw_pow(&rel, 0.5);
    assert_eq!(via_policy.q.w.to_bits(), via_kernel.q.w.to_bits());
    assert_eq!(via_policy.t.x.to_bits(), via_kernel.t.x.to_bits());
    let _: Twist = Twist::from_se3(tf_tree::log_se3(tf_tree::exp_se3([
        0.1, -0.2, 0.3, 0.4, -0.5, 0.6,
    ])));
}
