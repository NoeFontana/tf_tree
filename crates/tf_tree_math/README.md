# tf_tree_math

[![crates.io](https://img.shields.io/crates/v/tf_tree_math.svg?logo=rust)](https://crates.io/crates/tf_tree_math)
[![docs.rs](https://img.shields.io/docsrs/tf_tree_math?logo=docsdotrs)](https://docs.rs/tf_tree_math)
[![Licence](https://img.shields.io/badge/licence-MIT%20OR%20Apache--2.0-blue.svg)](#licence)

`no_std` SE(3)/SO(3), quaternion and dual-quaternion math for the
[`tf_tree`](https://crates.io/crates/tf_tree) transform engine. No allocator, no
`unsafe`, two dependencies (`libm`, `bytemuck`). **To look up transforms, depend
on `tf_tree` instead**; `cargo add tf_tree_math` is for the geometry alone.

## The five conventions

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

// Convention 2: `w` first. A 90° yaw as `[w, x, y, z]`.
let yaw90 = Quat::new(FRAC_1_SQRT_2, 0.0, 0.0, FRAC_1_SQRT_2);

// Convention 3: active, so x̂ goes to ŷ.
let v = yaw90.rotate(Vec3::new(1.0, 0.0, 0.0));
assert!(v.x.abs() < 1e-15 && (v.y - 1.0).abs() < 1e-15);

// Convention 4: odom←base composed with base←sensor.
let t_odom_base = Iso3::new(yaw90, Vec3::new(2.0, 0.0, 0.0));
let t_base_sensor = Iso3::new(Quat::IDENTITY, Vec3::new(1.0, 0.0, 0.0));
let t_odom_sensor = t_odom_base * t_base_sensor;

// Base's x points along odom's y, so the sensor lands at (2, 1, 0).
assert!((t_odom_sensor.t.x - 2.0).abs() < 1e-15);
assert!((t_odom_sensor.t.y - 1.0).abs() < 1e-15);
```

## Numerics

`log_so3` goes through the quaternion, never `acos((tr − 1)/2)`. The `V`/`V⁻¹`
small-angle threshold is `θ < 0.1` with four terms; `slerp`'s series/exact
crossover is `0.15` rad of **quaternion** angle.

## Two interpolation policies

`ScLerp` is the SE(3) screw geodesic and the engine's default: left- **and**
right-invariant. `LerpSlerp` is the `tf2`-compatible one (translation LERP plus
shortest-arc SLERP): left-invariant but **not** right-invariant, and the test
showing it is expected to fail.

Both kernels are callable directly: `slerp(qa, qb, s)` and
`dualquat::screw_pow(&rel, s)`. `s` is a fraction of the segment, `[0, 1]`, and
unchecked.

## Version and docs

**`0.0.x` promises nothing**: pin exactly
([`CHANGELOG.md`](https://github.com/NoeFontana/tf_tree/blob/main/CHANGELOG.md)).
MSRV is **1.87** ([`SUPPORT.md`](https://github.com/NoeFontana/tf_tree/blob/main/SUPPORT.md)).
Conventions and numerics evidence:
[`docs/PHASE1.md`](https://github.com/NoeFontana/tf_tree/blob/main/docs/PHASE1.md) §3.

## Licence

Dual [MIT](LICENSE-MIT) / [Apache-2.0](LICENSE-APACHE), at your option; see [`NOTICE`](NOTICE).
