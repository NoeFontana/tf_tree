# Changelog

Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Released
entries live in [`docs/changelog/`](./docs/changelog/).

**`0.0.x` is not ordinary semver.** Cargo treats every `0.0.x` as incompatible
with every other; every release may break the Rust API, Python API, C ABI and
arena format. Pin exactly, including the PyPI wheel. What is implemented is
defined by the status tables in `docs/`; they win over this file.

---

## [Unreleased]

### Changed

- **`tf_tree_math` uses `helicoid` 0.0.8** (was 0.0.5). It brings helicoid's portable
  `atan2` kernel and compensated single-rounded `SO3::log` exact arm (maximum error
  falls from 2.317 to 1.405 `u`), with no API break. Lockfiles across the workspace and
  excluded crates follow. Bench baseline re-cut / verified.

---

## [0.0.8] — 2026-10-16 (helicoid 0.0.5)

### Changed

- **`tf_tree_math` uses `helicoid` 0.0.5** (was 0.0.4). It has no API break; helicoid's SO(3)
  coefficients and `Log` change in their last bits, so the delegated bodies may too. The bench
  baseline is re-cut: its one measured row, differential agreement, still reads `2.5e-16`
  over 47,922 queries.

---

## Released versions

Frozen history, newest first, in [`docs/changelog/`](./docs/changelog/); leave it
out of searches and sweeps.

- [`0.0.7`](./docs/changelog/0.0.7.md) — 2026-10-14 (helicoid by default)
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
[0.0.8]: https://github.com/NoeFontana/tf_tree/releases/tag/v0.0.8
