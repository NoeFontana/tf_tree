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

- **Linux, `x86_64`.** The architecture the project is developed and gated on:
  every `just` recipe in `CLAUDE.md`'s command table runs on it, the
  container-only ones (`ros-build`, `ros-test`, `tf2-check`, `dds-bench`)
  included. `CLAUDE.md` records that `just loom` on x86-64 is currently the
  *whole* weak-memory defence.
- **Linux, `aarch64` — supported, and now measured.** `ci.yml`'s `test` and
  `shm` matrices carry `ubuntu-24.04-arm` rows, and **as of 2026-08-16 they
  execute and pass.** They had never run once before that, and the first ones
  that did found a real defect the whole life of the project had hidden:
  `c_char` is `i8` on `x86_64` and `u8` on `aarch64`, which made six casts
  unnecessary on one target and two buffer declarations outright type errors on
  the other. That is what the rows are for. What they are *not* is a
  weak-memory proof — `just loom` remains the argument for every atomic
  ordering, and aarch64 execution corroborates it rather than replacing it.
  **These two bullets were one line reading "`x86_64` and `aarch64`", hedged
  with "'configured to' is the accurate verb right now — see the note at the end
  of this section". There was no note at the end of this section.** A caveat has
  to sit next to the claim it qualifies, or it is not a caveat.
- **The current release**, and only it. Pre-1.0 there are no backports and no
  long-term-support branch. Reports against an older version will be asked to
  reproduce on the current one. On the `0.0.x` line "the current release" is a
  stronger statement than usual: cargo treats every `0.0.x` as incompatible with
  every other (`^0.0.1` matches `0.0.1` alone), so nothing is promised to carry
  across a release and `CHANGELOG.md` says so at the top.
- **The public API of `tf_tree`, `tf_tree_core`, `tf_tree_math`,
  `tf_tree_arena` and `tf_tree_ipc`** — the five crates that are published — plus
  the Python bindings, at the maturity each is documented at. Crates marked
  `publish = false` in their manifests are internal and carry no API stability
  promise at all.
- **The C ABI's stable tier**, `crates/tf_tree_c/include/tf_tree.h`, versioned by
  `TFT_ABI_VERSION_MAJOR`/`_MINOR` independently of the crate version. This one
  needs saying separately because `tf_tree_c` is itself `publish = false` — the
  header is built from source, and the bullet above would otherwise disclaim it.
  `tf_tree_unstable.h`, behind `#define TFT_ENABLE_UNSTABLE`, is the opposite: the
  opt-in *is* the waiver, and nothing reachable through it is covered.

## What is not supported

- **macOS and Windows — best-effort, and a wheel is built for both.** This is
  the one entry that needs a longer answer than the heading, because the
  repository *does* produce artifacts for these platforms and an unqualified
  "not supported" would be false. An earlier revision of this bullet was exactly
  that: it said "nothing is tested on either" — true — and left the reader to
  discover `.github/workflows/wheels.yml` building `macos-latest`, `macos-13`
  and `windows-latest` rows.

  **What exists.** `wheels.yml` builds macOS (`aarch64`, `x86_64`) and Windows
  (`x64`) wheels alongside the Linux ones, and `crates/tf_tree_py` is written to
  degrade on those platforms rather than fail to compile:
  `tf_tree.has_shared_memory()` returns `False` off Linux, and `open_file()` /
  `Tree.freeze()` refuse a `.tft` with a message naming the platform instead of
  vanishing and raising `AttributeError` somewhere unrelated. The engine in
  those wheels is the **single-process** one, and that is structural rather than
  promised: `cargo tree -p tf_tree --features shm --target
  aarch64-apple-darwin` shows `tf_tree_ipc` — the entire rendezvous, lock-file
  and fd-passing layer — absent from the graph, because it is declared under
  `[target.'cfg(target_os = "linux")'.dependencies]`, and every `MappedArena`
  item in `tf_tree_arena` is `#[cfg(all(feature = "shm", target_os = "linux"))]`.
  So on macOS and Windows there is no `tf_tree.open()` joining another process,
  no frozen `.tft`, and no `tf_tree top`.

  **What does not exist is any evidence that the artifact works.** Nothing in
  this repository tests a wheel — `wheels.yml` builds and, on a tag, publishes;
  the suite that would run against the result is `ci.yml`'s `python` job, which
  is `runs-on: ubuntu-latest` and builds its own extension with maturin. No test
  here has ever run on macOS or Windows, no non-Linux job exists in `ci.yml` at
  all, and no wheel has ever been smoke-tested. Treat these as
  convenience builds. A bug report against one is welcome and answered
  best-effort; a patch is more welcome still.

  The C ABI, the C++ wrapper and the ROS 2 bridge are **Linux-only in practice**
  — no workflow and no `just` recipe builds any of them anywhere else.
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
where ROS 2 is installed) — so each repeats the number by hand. `just msrv`
compares every hand-written `rust-version` in the repository against the
workspace's and fails on disagreement, because the number that nothing checks is
the number that drifts. CI's `msrv` job runs the same steps — but see the third
bullet below, and the note that closes this section: the local recipe is the one
that is actually running.

- **An MSRV bump is a minor-version bump** pre-1.0, and a breaking change after
  1.0. It is never a patch release. **This rule is suspended for the whole
  `0.0.x` line, and the suspension is a real cost rather than a convenience.**
  Under `0.0.x` cargo treats every release as incompatible with every other —
  `^0.0.1`, which is what a bare `tf_tree = "0.0.1"` means, matches `0.0.1` and
  nothing else — so there is no minor slot left for the rule to occupy and the
  only field that can move is the patch, which is precisely what the rule
  forbids. What resolves the deadlock rather than merely dodging it: the rule
  exists to stop the floor moving under a dependant who resolved by a range, and
  on `0.0.x` no range spans two releases, so the resolver is already enforcing
  what the rule was written to enforce. **A `0.0.x` release may therefore raise
  the MSRV**, and this bullet comes back into force at `0.1.0`, where a minor
  slot exists again. The root `Cargo.toml`'s comment on `[workspace.package]
  version` states the same thing from the other side; that field and this bullet
  must not be changed independently.

  One asymmetry, because it would otherwise be discovered by a user: **PyPI does
  not have cargo's rule.** PEP 440 gives `0.0.x` no special meaning, so
  `pip install -U transform_tree` will move someone from `0.0.1` to `0.0.2` without
  asking, and the wheel's "no compatibility between releases" promise is carried
  by `CHANGELOG.md` and by this document instead of by the version number.
  Pin the wheel exactly if that matters to you.
- **MSRV is raised only for a reason that is written down** in the commit that
  raises it — a language or standard-library feature that removes real
  complexity, or a dependency that has already moved. "The toolchain moved on" is
  not a reason.
- **The floor is enforced, not intended, and enforced on a host as well as in
  CI.** `just msrv` reads `rust-version` out of the manifest, builds `--locked` on
  exactly that toolchain, and compares every hand-written `rust-version` in the
  repository against the workspace's; the `msrv` job in
  `.github/workflows/ci.yml` runs the same two steps. So a dependency that quietly
  needs a newer compiler fails the build instead of shipping. The local recipe
  exists because for one release the CI job was the *only* thing enforcing the
  floor, and CI stopped running — a floor gated solely by a workflow nobody runs
  is back to being intended. **Both raises so far were
  forced by a dependency, and both were measured rather than assumed:**
  - 1.83 → 1.85: `blake3` pulls `constant_time_eq 0.4.2`, which is edition 2024
    and will not even parse on 1.83.
  - 1.85 → 1.87: `ruzstd 0.9.0` declares `rust-version = "1.87"`. It is how
    `tf_tree_ingest` decompresses MCAP chunks without a C build step
    (`docs/PHASE2.md` §2). `ruzstd 0.7.3` would have held 1.85 with the same
    capability, so this one is a deliberate trade for a maintained release, and
    it is the first raise that was not strictly unavoidable.

  Verified locally with `just msrv`, whose build step is
  `cargo +1.87 build --workspace --lib --bins --locked`. Both of its arms were
  checked against a deliberate mutant rather than assumed: `cargo +1.85` fails with
  `ruzstd@0.9.0 requires rustc 1.87`, and a crate's hand-written `rust-version` set
  to `1.86` is reported by name.
- Development happens on `stable` (`rust-toolchain.toml`). Clippy and rustfmt are
  run on `stable`, not on the MSRV toolchain — lint output is not part of the
  compatibility promise.

**CI produced no run between 2026-07-23 and 2026-08-16**, and now does again —
making the repository public restored it. Gate locally with `just` before
pushing; treat a green check as covering what its jobs cover and no more.

## Contributing

See [`CONTRIBUTING.md`](./CONTRIBUTING.md). The short version: a change the
documents in `docs/` do not already cover starts as a decision record in
[`docs/decisions/`](./docs/decisions/), not as a pull request.
