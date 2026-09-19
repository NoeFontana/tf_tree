# Support policy

A **single-maintainer, pre-1.0 project**; the expectations below are the ceiling
(`docs/PHASE5.md` §10).

## Response expectations

| Kind of report | Expectation |
|---|---|
| Security vulnerability ([`SECURITY.md`](./SECURITY.md)) | Acknowledged within 7 days; fix or advisory within 90 |
| Soundness bug: UB reachable from safe Rust | Triaged within 7 days; highest-priority non-security work |
| Data corruption, deadlock, or a wrong lookup result | Triaged within 14 days |
| Any other bug | Best effort. No timeline promised |
| Feature request | Best effort, and likely declined; see *What is not supported* |
| Question, or "how do I ..." | Best effort. `docs/` answers most |

"Triaged" means read and labelled, not fixed.

## What is supported

- **Linux `x86_64` and `aarch64`.**
- **The current release**, and only it: no backports.
- **The public API of `tf_tree`, `tf_tree_core`, `tf_tree_math`, `tf_tree_arena`
  and `tf_tree_ipc`** plus the Python bindings, at their documented maturity.
  `publish = false` crates carry no stability promise.
- **The C ABI's stable tier**, `crates/tf_tree_c/include/tf_tree.h`
  (`TFT_ABI_VERSION_MAJOR`/`_MINOR`); `tf_tree_unstable.h` is uncovered.

## What is not supported

- **macOS and Windows: best-effort convenience wheels** with the
  **single-process** engine only: no cross-process `tf_tree.open()`, no frozen
  `.tft`, no `tf_tree top`. **No test has run on either.** The C ABI, C++ wrapper
  and ROS 2 bridge are Linux-only in practice.
- **Anything a `docs/` status table marks not implemented**, and cross-host
  operation (Phase 8).
- **Feature requests that widen the scope.** `docs/PROJECT.md` §5 and
  `docs/PHASE5.md` §8 record what the project does not do; refute the recorded
  argument first, through [`docs/decisions/`](./docs/decisions/).

## MSRV policy

The minimum supported Rust version is **1.87**, declared in the workspace
manifest's `[workspace.package] rust-version`; `just msrv` fails if any
declaration or prose statement disagrees.

- **An MSRV bump is a minor-version bump** pre-1.0 and a breaking change after.
  **Suspended for the whole `0.0.x` line** (cargo already treats every `0.0.x` as
  incompatible with every other); the rule returns at `0.1.0`. The root
  `Cargo.toml`'s `[workspace.package] version` comment says the same; change them
  together.
- **MSRV is raised only for a reason written in the raising commit.**

## Contributing

See [`CONTRIBUTING.md`](./CONTRIBUTING.md). A change the `docs/` do not cover
starts as a decision record in [`docs/decisions/`](./docs/decisions/), not a pull
request.
