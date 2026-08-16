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

## Numerics, and where the constants came from

* `log_so3` goes through the quaternion (`2·atan2(‖q_v‖, q_w)`) and never
  through `acos((tr − 1)/2)`, which loses nine digits near `θ = π`.
* The small-angle threshold for the `V`/`V⁻¹` series is `θ < 0.1` with four
  terms — not the `1e-8` most libraries use. Both were checked against a
  50-digit reference.
* `slerp`'s series/exact crossover is `0.15` rad, measured rather than
  eyeballed: the constant's doc comment carries the per-term-count error table
  and the publish rates it covers. An earlier `0.25` looked fine and was 3e-9
  off.

## Two interpolation policies, and one of them is deliberately asymmetric

`ScLerp` is the SE(3) screw geodesic and the engine's default: left- **and**
right-invariant, computed by dual-quaternion power. `LerpSlerp` is the
`tf2`-compatible one — translation LERP plus shortest-arc SLERP — and it is
left-invariant but **not** right-invariant. That asymmetry is a property of the
policy, not a defect in this crate; it is why `ScLerp` is the default, and the
test that demonstrates it is expected to fail for `LerpSlerp`.

## Version

**0.0.1, and `0.0.x` promises nothing.** Cargo treats every `0.0.x` release as
incompatible with every other, which is the intended signal: pin exactly, and
expect a later release to break. The reasoning is written out in the
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
