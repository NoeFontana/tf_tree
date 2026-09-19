# tf_tree_math

[![crates.io](https://img.shields.io/crates/v/tf_tree_math.svg?logo=rust)](https://crates.io/crates/tf_tree_math)
[![docs.rs](https://img.shields.io/docsrs/tf_tree_math?logo=docsdotrs)](https://docs.rs/tf_tree_math)
[![Licence](https://img.shields.io/badge/licence-MIT%20OR%20Apache--2.0-blue.svg)](#licence)

`no_std` SE(3)/SO(3), quaternion and dual-quaternion math for the
[`tf_tree`](https://crates.io/crates/tf_tree) transform engine. No allocator, no
`unsafe` (`#![forbid(unsafe_code)]`), two dependencies (`libm`, `bytemuck`); a
leaf that knows nothing of arenas, stamps or frames. **To look up transforms,
depend on [`tf_tree`](https://crates.io/crates/tf_tree) instead.**

`cargo add tf_tree_math` for the geometry alone.

## The five conventions

Three differ from something popular; a bug usually traces back to one.

1. **Hamilton** quaternions, not JPL.
2. **`w` first** storage: `[w, x, y, z]`. Eigen stores `w` last.
3. **Active** rotations: applying a `Quat` rotates the vector inside a fixed
   frame; applying an `Iso3` `T_parent_child` to a point in `child` yields it in
   `parent`.
4. `Iso3` composition `a * b` means `T_a_x * T_x_b`: the right operand's parent
   must be the left operand's child.
5. The adjoint convention is **right-perturbation**, `T = T̂ · exp(ξ^)`, so
   `log_se3` returns the twist `ξ = [ω, v]` of the right-multiplied increment and
   `exp_se3` consumes the same ordering.

Three of them as assertions (a doctest):

```rust
use core::f64::consts::FRAC_1_SQRT_2;
use tf_tree_math::{Iso3, Quat, Vec3};

// Convention 2 — `w` first. This is a 90° yaw written `[w, x, y, z]`: the
// scalar leads. An Eigen caller with the same four numbers in Eigen's order
// gets a different rotation, which is the whole reason the list exists.
let yaw90 = Quat::new(FRAC_1_SQRT_2, 0.0, 0.0, FRAC_1_SQRT_2);

// Convention 3 — active. The rotation moves the vector inside a fixed frame,
// so x̂ goes to ŷ. (A passive reading would send it to −ŷ.)
let v = yaw90.rotate(Vec3::new(1.0, 0.0, 0.0));
assert!(v.x.abs() < 1e-15 && (v.y - 1.0).abs() < 1e-15);

// Convention 4 — `a * b` is `T_a_x * T_x_b`, so the right operand's parent is
// the left operand's child: odom←base composed with base←sensor.
let t_odom_base = Iso3::new(yaw90, Vec3::new(2.0, 0.0, 0.0));
let t_base_sensor = Iso3::new(Quat::IDENTITY, Vec3::new(1.0, 0.0, 0.0));
let t_odom_sensor = t_odom_base * t_base_sensor;

// The sensor is 1 m along *base's* x, and base's x points along odom's y — so
// it lands at (2, 1, 0), not (3, 0, 0). Swapping the operands gives the latter.
assert!((t_odom_sensor.t.x - 2.0).abs() < 1e-15);
assert!((t_odom_sensor.t.y - 1.0).abs() < 1e-15);
```

## Numerics

`log_so3` goes through the quaternion, never `acos((tr − 1)/2)` (nine digits lost
near `θ = π`). The `V`/`V⁻¹` small-angle threshold is `θ < 0.1` with four terms.
`slerp`'s series/exact crossover is `0.15` rad of **quaternion** angle; the
constant's doc comment carries the error table.

## Two interpolation policies

`ScLerp` is the SE(3) screw geodesic and the engine's default: left- **and**
right-invariant, by dual-quaternion power. `LerpSlerp` is the `tf2`-compatible
one — translation LERP plus shortest-arc SLERP — left-invariant but **not**
right-invariant; that asymmetry is the policy's, and the test showing it is
expected to fail for `LerpSlerp`.

Both kernels are callable directly: `slerp(qa, qb, s)` on two `Quat` (re-exported
at the `tf_tree` facade root) and `dualquat::screw_pow(&rel, s)` on the relative
`Iso3`. `s` is a fraction of the segment, `[0, 1]`, and unchecked; out of range
the branches degrade differently, per `slerp`'s doc comment.

## Version and docs

**`0.0.x` promises nothing**: pin exactly and expect a later release to break
([`CHANGELOG.md`](https://github.com/NoeFontana/tf_tree/blob/main/CHANGELOG.md)).
MSRV is **1.87** ([`SUPPORT.md`](https://github.com/NoeFontana/tf_tree/blob/main/SUPPORT.md)).
Conventions and numerics evidence:
[`docs/PHASE1.md`](https://github.com/NoeFontana/tf_tree/blob/main/docs/PHASE1.md) §3.

## Licence

Dual [MIT](LICENSE-MIT) / [Apache-2.0](LICENSE-APACHE), at your option. See
[`NOTICE`](NOTICE).
