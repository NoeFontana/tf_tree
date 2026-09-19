# Support policy

This is a **single-maintainer, pre-1.0 project**; read the expectations below as
the ceiling, not the floor (`docs/PHASE5.md` §10).

## Response expectations

| Kind of report | Expectation |
|---|---|
| Security vulnerability ([`SECURITY.md`](./SECURITY.md)) | Acknowledged within 7 days; fix or advisory within 90 |
| Soundness bug — UB reachable from safe Rust | Triaged within 7 days; highest-priority non-security work |
| Data corruption, deadlock, or a wrong lookup result | Triaged within 14 days |
| Any other bug | Best effort. No timeline promised |
| Feature request | Best effort, and likely declined — see *What is not supported* |
| Question, or "how do I …" | Best effort. `docs/` answers most |

"Triaged" means read, reproduced or not, and labelled — not fixed. A bug that
cannot be reproduced is asked for a reproduction once and closed if none arrives.

## What is supported

- **Linux, `x86_64`.** Developed and gated here, including the container-only
  recipes (`ros-build`, `ros-test`, `tf2-check`, `dds-bench`).
- **Linux, `aarch64`.** `ci.yml`'s `test` and `shm` matrices carry
  `ubuntu-24.04-arm` rows that execute and pass since 2026-08-16. They are
  corroboration, not a weak-memory proof: `just loom` remains the argument for
  every atomic ordering.
- **The current release**, and only it: no backports, no LTS branch. On `0.0.x`
  nothing is promised to carry across a release (`CHANGELOG.md`).
- **The public API of `tf_tree`, `tf_tree_core`, `tf_tree_math`, `tf_tree_arena`
  and `tf_tree_ipc`** (the published crates) plus the Python bindings, at their
  documented maturity. `publish = false` crates carry no stability promise.
- **The C ABI's stable tier**, `crates/tf_tree_c/include/tf_tree.h`, versioned by
  `TFT_ABI_VERSION_MAJOR`/`_MINOR`. `tf_tree_unstable.h`, behind
  `#define TFT_ENABLE_UNSTABLE`, is uncovered: the opt-in is the waiver.

## What is not supported

- **macOS and Windows — best-effort convenience builds.** `wheels.yml` builds
  macOS (`aarch64`, `x86_64`) and Windows (`x64`) wheels, but they contain the
  **single-process** engine only: `tf_tree_ipc` is a Linux-only dependency and
  every `MappedArena` item is `cfg(target_os = "linux")`. So there is no
  `tf_tree.open()` joining another process, no frozen `.tft`, no `tf_tree top`;
  `tf_tree.has_shared_memory()` returns `False` and `open_file()` /
  `Tree.freeze()` refuse with a message naming the platform. **No test has ever
  run on macOS or Windows** and no wheel has been smoke-tested. The C ABI, C++
  wrapper and ROS 2 bridge are Linux-only in practice.
- **ROS 1.** No path, and there will not be one.
- **Anything a `docs/` status table marks not implemented.** A "bug" there is a
  missing feature.
- **Cross-host operation.** Phase 8; it does not exist.
- **Feature requests that widen the scope.** `docs/PROJECT.md` §5 and
  `docs/PHASE5.md` §8 record what this project deliberately does not do; a request
  must refute the recorded argument first, through [`docs/decisions/`](./docs/decisions/).

## MSRV policy

The minimum supported Rust version is **1.87**, declared in the workspace
manifest's `[workspace.package] rust-version`. `tf_tree_py` and `tf_tree_tf2_sys`
are outside the workspace and repeat the number by hand. `just msrv` builds
`--locked` on exactly that toolchain (`cargo +1.87 build --workspace --lib --bins
--locked`) and fails if any hand-written `rust-version`, or the floor as stated in
prose, disagrees. CI's `msrv` job runs the same steps.

- **An MSRV bump is a minor-version bump** pre-1.0 and a breaking change after.
  **The rule is suspended for the whole `0.0.x` line**: cargo treats every `0.0.x`
  release as incompatible with every other, so no range spans two releases and the
  resolver already enforces what the rule protects. A `0.0.x` release may raise
  the MSRV; the rule returns at `0.1.0`. The root `Cargo.toml`'s comment on
  `[workspace.package] version` states the same; the two must change together.
  **PyPI does not have cargo's rule** (PEP 440): `pip install -U transform_tree`
  moves `0.0.1` to `0.0.2` unasked, so pin the wheel exactly.
- **MSRV is raised only for a reason written down in the raising commit** — a
  feature that removes real complexity, or a dependency that already moved.
  Current floor: `ruzstd 0.9.0` requires 1.87 (`tf_tree_ingest`'s pure-Rust MCAP
  chunk decompression, `docs/PHASE2.md` §2).
- Clippy and rustfmt run on `stable` (`rust-toolchain.toml`), not the MSRV
  toolchain; lint output is not part of the compatibility promise.

CI produced no run between 2026-07-23 and 2026-08-16. Gate locally with `just`
first; a green check covers what its jobs cover.

## Contributing

See [`CONTRIBUTING.md`](./CONTRIBUTING.md). A change the `docs/` do not already
cover starts as a decision record in [`docs/decisions/`](./docs/decisions/), not
a pull request.
