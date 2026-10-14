# Changelog

Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Released
entries live in [`docs/changelog/`](./docs/changelog/).

**`0.0.x` is not ordinary semver.** Cargo treats every `0.0.x` as incompatible
with every other; every release may break the Rust API, Python API, C ABI and
arena format. Pin exactly, including the PyPI wheel. What is implemented is
defined by the status tables in `docs/`; they win over this file.

---

## [Unreleased]

---

## [0.0.7] — 2026-10-14 (helicoid by default)

### Added

- **`tf_tree_math` uses [`helicoid`](https://crates.io/crates/helicoid) 0.0.4 by default**
  (feature `helicoid`, [`0063`](docs/decisions/0063-the-math-crate-delegates-behind-a-feature.md),
  [`0064`](docs/decisions/0064-the-math-crate-uses-helicoid-by-default.md)): `exp_so3`,
  `log_so3`, `quat_from_rot3`, `slerp` and `Quat::rotate` call it on its assertion-free
  carried path, so drifted quaternions and NaN behave as they do natively.
  `default-features = false` restores the native bodies. D14 now names `helicoid` and
  `helicoid-linalg`, which are `no_std`, have no `unsafe`, and bring only `libm`. With the
  feature:
  - `log_so3` is accurate at subnormal angles, where the native body lost every digit.
  - `Quat::rotate` is bit-identical to the native body.
  - `quat_from_rot3` returns the native body's bits on 97 % of rotations; the rest, where
    the two pick different pivots, are within `1e-15` of the normalized native quaternion. Off
    the sphere it normalizes, and a matrix entry past about `2^511` gives NaN.
  - `slerp` extrapolates off `[0, 1]`. For identical inputs it returns `qa` to a few ulp,
    not the bit, and a NaN `s` gives NaN.
  - On near pairs of drifted quaternions, `slerp` stays at rounding level.
  - `slerp`'s latency is within about 1 % of the native body.

---

## Released versions

Frozen history, newest first, in [`docs/changelog/`](./docs/changelog/); leave it
out of searches and sweeps.

- [`0.0.6`](./docs/changelog/0.0.6.md) — 2026-09-20 (the prose cut)
- [`0.0.5`](./docs/changelog/0.0.5.md) — 2026-08-29 (the entry paths that need no toolchain)
- [`0.0.4`](./docs/changelog/0.0.4.md) — 2026-08-22 (the slot a killed participant keeps)
- [`0.0.3`](./docs/changelog/0.0.3.md) — 2026-08-19 (first with a source distribution)
- [`0.0.2`](./docs/changelog/0.0.2.md) — 2026-08-17 (wheels, no sdist)
- [`0.0.1`](./docs/changelog/0.0.1.md) — 2026-08-17 (crates.io only)

[0.0.1]: https://github.com/NoeFontana/tf_tree/releases/tag/v0.0.1
[0.0.2]: https://github.com/NoeFontana/tf_tree/releases/tag/v0.0.2
[0.0.3]: https://github.com/NoeFontana/tf_tree/releases/tag/v0.0.3
[0.0.4]: https://github.com/NoeFontana/tf_tree/releases/tag/v0.0.4
[0.0.5]: https://github.com/NoeFontana/tf_tree/releases/tag/v0.0.5
[0.0.6]: https://github.com/NoeFontana/tf_tree/releases/tag/v0.0.6
[0.0.7]: https://github.com/NoeFontana/tf_tree/releases/tag/v0.0.7
