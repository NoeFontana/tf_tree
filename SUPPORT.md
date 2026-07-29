# Support policy

`docs/PHASE5.md` §10 asks for a support policy that is *honestly scoped*, on the
grounds that "a small-team infrastructure project dies from unanswered issues
more often than from bad design. Say what is supported, what is best-effort, and
what the response expectation is. Under-promising is fine; silence is not."

So: this is a **single-maintainer, pre-1.0 project**. Read the response
expectations below as the ceiling, not the floor.

## Response expectations

| Kind of report | Expectation |
|---|---|
| Security vulnerability ([`SECURITY.md`](./SECURITY.md)) | Acknowledged within 7 days; fix or advisory within 90 |
| Soundness bug — UB reachable from safe Rust | Triaged within 7 days; treated as the highest-priority non-security work |
| Data corruption, deadlock, or a wrong lookup result | Triaged within 14 days |
| Any other bug | Best effort. No timeline promised |
| Feature request | Best effort, and likely declined — see *What is not supported* |
| Question, or "how do I …" | Best effort. `docs/` is the answer to most of them |

"Triaged" means read, reproduced or not, and labelled — not fixed. A bug that
cannot be reproduced from the report will be asked for a reproduction once and
closed if none arrives.

## What is supported

- **Linux, `x86_64` and `aarch64`.** These are what CI is configured to cover
  and what the shared-memory implementation targets. "Configured to" is the
  accurate verb right now — see the note at the end of this section.
- **The current release**, and only it. Pre-1.0 there are no backports and no
  long-term-support branch. Reports against an older version will be asked to
  reproduce on the current one.
- **The public API of `tf_tree`, `tf_tree_core`, `tf_tree_math`,
  `tf_tree_arena` and `tf_tree_ipc`**, plus the Python bindings and the C ABI, at
  the maturity each is documented at. Crates marked `publish = false` in their
  manifests are internal and carry no API stability promise at all.

## What is not supported

- **macOS and Windows.** The single-process engine is portable and much of it
  compiles there, but nothing is tested on either and the entire shared-memory
  layer is Linux-only. Patches welcome; a bug report against an untested
  platform will be labelled and left.
- **ROS 1.** There is no ROS 1 path and there will not be one.
- **Anything behind a phase that `docs/`'s status tables mark as not
  implemented.** Those tables are the source of truth, not the README and not
  the specs' prose. A "bug" in an unimplemented section is a missing feature.
- **Cross-host operation.** That is Phase 8, and it does not exist yet.
- **Feature requests that widen the scope** — `docs/PROJECT.md` §5's decision log
  and `docs/PHASE5.md` §8 record several things this project deliberately does
  not do. A request to add one of them needs to refute the recorded argument
  first; that is a real path, not a rhetorical one, and it goes through
  [`docs/decisions/`](./docs/decisions/).

## MSRV policy

The minimum supported Rust version is **1.87**, declared in the workspace
manifest's `[workspace.package] rust-version` and inherited by every workspace
member. Two crates are deliberately *outside* the workspace and therefore cannot
inherit it — `tf_tree_py` (built by maturin) and `tf_tree_tf2_sys` (builds only
where ROS 2 is installed) — so each repeats the number by hand. CI's `msrv` job
compares every hand-written `rust-version` in the repository against the
workspace's and fails on disagreement, because the number that nothing checks is
the number that drifts.

- **An MSRV bump is a minor-version bump** pre-1.0, and a breaking change after
  1.0. It is never a patch release.
- **MSRV is raised only for a reason that is written down** in the commit that
  raises it — a language or standard-library feature that removes real
  complexity, or a dependency that has already moved. "The toolchain moved on" is
  not a reason.
- **The floor is enforced, not intended.** The `msrv` job in
  `.github/workflows/ci.yml` reads `rust-version` out of the manifest and builds
  `--locked` on exactly that toolchain, so a dependency that quietly needs a
  newer compiler fails the build instead of shipping. **Both raises so far were
  forced by a dependency, and both were measured rather than assumed:**
  - 1.83 → 1.85: `blake3` pulls `constant_time_eq 0.4.2`, which is edition 2024
    and will not even parse on 1.83.
  - 1.85 → 1.87: `ruzstd 0.9.0` declares `rust-version = "1.87"`. It is how
    `tf_tree_ingest` decompresses MCAP chunks without a C build step
    (`docs/PHASE2.md` §2). `ruzstd 0.7.3` would have held 1.85 with the same
    capability, so this one is a deliberate trade for a maintained release, and
    it is the first raise that was not strictly unavoidable.

  Verified locally with `cargo +1.87 build --workspace --lib --bins --locked`.
- Development happens on `stable` (`rust-toolchain.toml`). Clippy and rustfmt are
  run on `stable`, not on the MSRV toolchain — lint output is not part of the
  compatibility promise.

**CI has produced no run since 2026-07-23.** Until that is fixed, treat a green
check on a pull request as unverified and gate locally with `just`.

## Contributing

See [`CONTRIBUTING.md`](./CONTRIBUTING.md). The short version: a change the
documents in `docs/` do not already cover starts as a decision record in
[`docs/decisions/`](./docs/decisions/), not as a pull request.
