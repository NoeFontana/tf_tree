//! The facade's math re-exports are the **same items** as `tf_tree_math`'s, and
//! they are on the stable tier.
//!
//! # Why nothing here compares two return values
//!
//! `pub use` does not copy a function; it names one. A test that calls
//! `tf_tree::slerp` and `tf_tree_math::slerp` and compares the results compares
//! one function to itself and cannot fail — the objection
//! `tf_tree_math/tests/slerp_public.rs` records against its own first version of
//! the same idea. What *can* fail, and is the whole risk, is the re-export going
//! missing or being replaced by a wrapper of the facade's own: a consumer who
//! cannot reach the kernel through `tf_tree` has to add `tf_tree_math` as a
//! second direct dependency and pin it in lockstep on a `0.0.x` line where every
//! release breaks every other, which is worse than the `Iso3` round trip
//! `docs/API.md` §2.7 told them to abandon.
//!
//! [`same_item`] rejects both failures at compile time. A function item has a
//! unique zero-sized type per definition, so passing two of them to one generic
//! parameter compiles **only** if they are the same definition — a wrapper with
//! an identical signature is `E0308`, and a missing re-export is `E0425`.
//! Verified both ways while this file was written: `same_item(a, b)` on two
//! distinct `fn(i32) -> i32` fails with *"expected fn item, found a different fn
//! item"*.
//!
//! (Two `use` declarations naming one item from two paths would be the shorter
//! spelling and does not work: `use tf_tree::slerp; use tf_tree_math::slerp;` is
//! `E0252` even though both resolve to the same function.)
//!
//! This target carries no `#[cfg(feature = ...)]`, so it compiles in the
//! facade's default feature set: the re-exports below are stable-tier, not
//! behind `unstable` (`docs/API.md` §2.6).

use tf_tree::{Interp, Iso3, LerpSlerp, Quat, ScLerp, Twist, Vec3};

/// Compiles only if both arguments are the *same* item, not merely two items of
/// the same signature. See the module docs.
fn same_item<T>(_: T, _: T) {}

/// Every function the facade re-exports from `tf_tree_math` is that crate's own
/// item, `slerp` included.
///
/// `slerp` is the row this test was added for (`docs/API.md` §6 row 16): the
/// commit that made it `pub` re-exported it at `tf_tree_math`'s root and not
/// here, so the consumer the change was made for still could not reach it
/// without a second direct dependency. The others are listed because a
/// re-export list is exactly the kind of thing that loses a name in a rebase.
#[test]
fn every_re_exported_function_is_the_same_item_as_tf_tree_maths() {
    same_item(tf_tree::slerp, tf_tree_math::slerp);
    same_item(tf_tree::exp_se3, tf_tree_math::exp_se3);
    same_item(tf_tree::exp_so3, tf_tree_math::exp_so3);
    same_item(tf_tree::log_se3, tf_tree_math::log_se3);
    same_item(tf_tree::log_so3, tf_tree_math::log_so3);
    same_item(tf_tree::quat_from_rot3, tf_tree_math::quat_from_rot3);
}

/// `ScLerp`'s kernel is reachable through the facade under the **same** path,
/// not a second spelling of it.
///
/// `pub use tf_tree_math::dualquat` re-exports the module, so
/// `tf_tree::dualquat::screw_pow` and `tf_tree_math::dualquat::screw_pow` are
/// one path with one prefix swapped — where a bare `tf_tree::screw_pow` would
/// have been a second name for one item (`PROJECT.md` §6). The first revision of
/// the facade change left this out and so reproduced, one layer up, the exact
/// asymmetry it was closing: `LerpSlerp` and its kernel both re-exported,
/// `ScLerp` with no route to its own.
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

/// The re-exported *types* are the same types, so a value crosses the boundary
/// without a conversion.
///
/// A `const` rather than a `#[test]`: it is a statement about types, and it
/// fails wherever this target is compiled rather than only where it is run.
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
/// crate alone, which is the point of re-exporting the kernel at all.
#[test]
fn the_kernel_and_its_policy_are_both_reachable_through_the_facade() {
    let qa = Quat::IDENTITY;
    let qb = tf_tree::exp_so3(Vec3::new(0.0, 0.0, core::f64::consts::FRAC_PI_2));

    let mid = tf_tree::slerp(qa, qb, 0.5);
    let via_policy =
        <LerpSlerp as Interp>::eval(&Iso3::new(qa, Vec3::ZERO), &Iso3::new(qb, Vec3::ZERO), 0.5).q;
    // Away from the endpoints the two agree exactly; the two endpoint
    // differences are `tf_tree_math`'s own
    // `the_iso3_round_trip_it_replaces_agrees_as_a_rotation`, not this file's.
    assert_eq!(mid.w.to_bits(), via_policy.w.to_bits());
    assert_eq!(mid.z.to_bits(), via_policy.z.to_bits());

    // The same statement for the other policy, which is what re-exporting
    // `dualquat` bought: both reached from this crate alone. `docs/API.md` §2.7
    // records why `eval(&Iso3::IDENTITY, &rel, s)` is not a "degenerate input"
    // in the sense the older wording claimed — the two agree bitwise, so the
    // manufactured identity is read only by `inv_mul` and the trailing compose.
    let rel = Iso3::new(qb, Vec3::new(1.5, -2.0, 0.25));
    let via_policy = <ScLerp as Interp>::eval(&Iso3::IDENTITY, &rel, 0.5);
    let via_kernel = tf_tree::dualquat::screw_pow(&rel, 0.5);
    assert_eq!(via_policy.q.w.to_bits(), via_kernel.q.w.to_bits());
    assert_eq!(via_policy.t.x.to_bits(), via_kernel.t.x.to_bits());
    let _: Twist = Twist::from_se3(tf_tree::log_se3(tf_tree::exp_se3([
        0.1, -0.2, 0.3, 0.4, -0.5, 0.6,
    ])));
}
