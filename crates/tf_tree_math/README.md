# tf_tree_math

`no_std` SE(3)/SO(3), quaternion and dual-quaternion math for the
[`tf_tree`](https://crates.io/crates/tf_tree) transform engine.

No allocator, no `unsafe` (`#![forbid(unsafe_code)]`), two dependencies
(`libm`, `bytemuck`). It is a leaf: it knows nothing about arenas, time stamps
or frames, and it is usable on its own if these are the conventions you want.

**If you want to look up transforms, depend on
[`tf_tree`](https://crates.io/crates/tf_tree) instead.** This crate is the
geometry underneath it.

## The five conventions, because they are what a bug traces back to

Fix these before writing a line against this crate; three of them differ from
something popular.

1. **Hamilton** quaternions, not JPL.
2. **`w` first** storage: `[w, x, y, z]`. Eigen stores `w` last — a C++ caller
   transposes.
3. **Active** rotations. Applying a `Quat` to a vector rotates the vector inside
   a fixed frame; applying an `Iso3` `T_parent_child` to a point expressed in
   `child` yields that point expressed in `parent`.
4. `Iso3` composition `a * b` means `T_a_x * T_x_b`: the right operand's parent
   must be the left operand's child.
5. The adjoint convention is **right-perturbation**, `T = T̂ · exp(ξ^)`, so
   `log_se3` returns the twist `ξ = [ω, v]` of the right-multiplied increment
   and `exp_se3` consumes the same ordering.

Three of them, spelled as assertions — a convention stated in prose is a
convention nothing checks, and this block is a doctest:

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

## Numerics, and where the constants came from

* `log_so3` goes through the quaternion (`2·atan2(‖q_v‖, q_w)`) and never
  through `acos((tr − 1)/2)`, which loses nine digits near `θ = π`.
* The small-angle threshold for the `V`/`V⁻¹` series is `θ < 0.1` with four
  terms — not the `1e-8` most libraries use. Both were checked against a
  50-digit reference.
* `slerp`'s series/exact crossover is `0.15` rad of **quaternion** angle —
  `acos(qa·qb)`, half the rotation the pair spans, so a rotation of `0.30` rad.
  Measured rather than eyeballed: the constant's doc comment carries the
  per-term-count error table and the publish rates it covers, in both angles.
  An earlier `0.25` looked fine and was 3e-9 off.

## Two interpolation policies, and one of them is deliberately asymmetric

`ScLerp` is the SE(3) screw geodesic and the engine's default: left- **and**
right-invariant, computed by dual-quaternion power. `LerpSlerp` is the
`tf2`-compatible one — translation LERP plus shortest-arc SLERP — and it is
left-invariant but **not** right-invariant. That asymmetry is a property of the
policy, not a defect in this crate; it is why `ScLerp` is the default, and the
test that demonstrates it is expected to fail for `LerpSlerp`.

**Both policies' kernels are callable on their own**, without going through
`Interp::eval`: `slerp(qa, qb, s)` for the shortest-arc quaternion
interpolation `LerpSlerp` uses, `dualquat::screw_pow(&rel, s)` for `ScLerp`'s
screw power. The two are not the same shape and the difference is the reason
one of them changed. `screw_pow` takes an `Iso3` — the relative transform,
raised to a real power — because a screw is an SE(3) object and there is
nothing smaller to hand it. `slerp` takes two `Quat`, and until recently the
only way to reach it was to build two `Iso3` with throwaway zero translations
and call `LerpSlerp::eval`: a caller who holds rotations and no translation had
to invent the translation. That is what became public, and the numerics were
never the problem — the entry point was missing.

`slerp` is re-exported at the `tf_tree` facade's root as well, so an engine
consumer reaches it without adding this crate as a second direct dependency.
`screw_pow` is not: it lives behind a module path (`tf_tree_math::dualquat`),
and a bare `screw_pow` at the facade root would be a second spelling of it
rather than the same one.

`s` is a fraction of the segment, `[0, 1]`, and unchecked: out of range the
three branches extrapolate and degrade differently, so which accuracy a caller
gets is decided by the publish rate. `slerp`'s doc comment measures all three
and says why extrapolation is not offered here.

## Version

**`0.0.x` promises nothing.** Cargo treats every `0.0.x` release as
incompatible with every other, which is the intended signal: pin exactly, and
expect a later release to break. The number is deliberately not repeated here —
this line read `0.0.1` while the crate was `0.0.2`, because nothing gates a
version in prose. The reasoning is written out in the
repository's [`Cargo.toml`](https://github.com/NoeFontana/tf_tree/blob/main/Cargo.toml)
under `[workspace.package] version`, and the release notes are in
[`CHANGELOG.md`](https://github.com/NoeFontana/tf_tree/blob/main/CHANGELOG.md).

MSRV is **1.87**; see
[`SUPPORT.md`](https://github.com/NoeFontana/tf_tree/blob/main/SUPPORT.md) for
the policy and for what "supported platform" currently means.

## Where the rest of it is

The full story — architecture, the eight-phase roadmap, the decision log — is in
[`docs/PROJECT.md`](https://github.com/NoeFontana/tf_tree/blob/main/docs/PROJECT.md);
these conventions and the numerics evidence are
[`docs/PHASE1.md`](https://github.com/NoeFontana/tf_tree/blob/main/docs/PHASE1.md)
§3.

## Licence

Dual [MIT](LICENSE-MIT) / [Apache-2.0](LICENSE-APACHE), at your option. See
[`NOTICE`](NOTICE).
