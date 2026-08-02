# CLAUDE.md — agent guidance for tf_tree

This file is for AI coding agents working in this repository. Humans, see
[`CONTRIBUTING.md`](./CONTRIBUTING.md).

## What this is

`tf_tree` is a transform tree engine (a faster, more scalable alternative to ROS
`tf2`). It is being built along a fixed **eight-phase** roadmap (re-cut from six
by [`0006`](./docs/decisions/0006-the-eight-phase-roadmap.md)). **The canonical
documents in [`docs/`](./docs/) are the contract** — read
[`docs/PROJECT.md`](./docs/PROJECT.md) (project overview, architecture, roadmap,
and the decision log D1–D22 in its §5) **then** the spec for the phase you are
touching, before touching code. When these documents do not answer a question,
**stop and ask** — do not invent an answer, especially in the concurrency or
arena-layout sections.

**Phases 1–3 are implemented.** `FORMAT_VERSION = 2`; `tf_tree::open()` exists;
the rendezvous, fd passing, ownership migration, claims-as-leases, reaping, fork
poisoning and the Python bindings all shipped. `docs/PHASE2.md` §0.0 and
`docs/PHASE3.md` are the authoritative status tables.

[`docs/PHASE2.md`](./docs/PHASE2.md) §1 holds **Phase 1 amendments A1–A8, all
applied** — read them before changing any concurrency protocol, because they are
the reason several orderings look the way they do.

**Current work is Phases 4 and 5.** Two cautions specific to them:

- `docs/PHASE5.md` §1 bumps `FORMAT_VERSION` to **3**. That is a real break —
  every participant must be rebuilt and restarted together. §1.2 lists what goes
  in, *including regions Phase 6 will fill*, so the break happens exactly once.
  **Do not add arena fields opportunistically outside that commit.**
- `docs/PHASE5.md` §8 is a section about **not** building something. Visualization
  is deliberately absent, with the argument recorded. Do not propose a viewer
  integration without refuting §8.1 first.
- `docs/PHASE7.md` exists and **its existence is not permission to build it.**
  The `tf2` shim is gated on operating evidence (D21), §0.0 lists four gates and
  none is met. Its §4 is a table of *questions*; answering one from that document
  rather than from Phase 4's surprise log is the exact failure the gate exists to
  prevent.

| Document | Role |
| --- | --- |
| [`docs/PROJECT.md`](./docs/PROJECT.md) | Overview, architecture, roadmap, decision log D1–D22 (§5). Supersedes `docs/decisions/0002`. |
| [`docs/PHASE1.md`](./docs/PHASE1.md) | Normative Phase 1 spec: layouts, atomic orderings, test plan (§10), benchmark gate (§11). Supersedes `docs/decisions/0003`. **Implemented.** |
| [`docs/PHASE2.md`](./docs/PHASE2.md) | Normative Phase 2 spec; §1 holds Phase 1 amendments A1–A8 (all applied), §0.0 is the status table. **Engine half implemented.** |
| [`docs/PHASE3.md`](./docs/PHASE3.md) | Normative Phase 3 spec (Python bindings). **Implemented.** |
| [`docs/PHASE4.md`](./docs/PHASE4.md) | Normative Phase 4 spec (C ABI, C++ wrapper, ROS 2 ingest bridge, `sample_with_derivatives`). §0.0 records what this environment cannot gate. **ROS 2 *is* available, in `docker/tf2`** — an earlier revision of this line said otherwise and was wrong. What is missing is a second RMW, clang in that image, and a robot. |
| [`docs/PHASE5.md`](./docs/PHASE5.md) | Normative Phase 5 spec (frozen `.tft` arena, bag ingestion, `FORMAT_VERSION = 3`, diagnostic counters, `TFT001`–`TFT019`, `tf_tree top`). |
| [`docs/PHASE7.md`](./docs/PHASE7.md) | The `tf2`-shaped shim. **GATED by D21 and not scheduled** — §0.0 lists four gates, none met. It is a *requirements artifact*, not an authorization: §4's J-table states the semantic judgements as questions, and the only work it authorizes today is filing Phase 4's surprise log against those rows. |
| [`docs/API.md`](./docs/API.md) | **Not a phase.** The cross-cutting API contract: six rules (§1) every binding obeys, the normative surface of Rust/Python/C/C++ (§2–§5), the delta table (§6), and the §7 check a new surface passes. **Read §1 before adding public API to any binding.** It authorizes nothing on its own — §6 names the phase or record each row lands in. |
| [`docs/decisions/`](./docs/decisions/) | Decision-record process, retained for *future* decisions. `0002`–`0003` are superseded; [`0004`](./docs/decisions/0004-builder-time-edge-declaration.md), [`0005`](./docs/decisions/0005-the-shared-memory-seam.md) and [`0006`](./docs/decisions/0006-the-eight-phase-roadmap.md) are authoritative. |

## Project shape (Phase 1 — pure Rust)

```
crates/tf_tree_math/    no_std; SE(3)/SO(3), quats, dual quats; #![forbid(unsafe_code)]
crates/tf_tree_arena/   no_std+alloc; pointer-free arena + layout math (unsafe allowed)
crates/tf_tree_core/    no_std+alloc; the engine; unsafe only in buffer.rs / arena_view.rs
crates/tf_tree/         std facade; #![deny(unsafe_code)] + one #[allow]: OwnedWriter (0017)
crates/tf_tree_ipc/     std; rendezvous, lock file, fd passing (unsafe: one atfork shim)
crates/tf_tree_py/      PyO3 bindings; binds the Rust core directly, not the C ABI
crates/tf_tree_bench/   criterion + tf2 differential harness
crates/tf_tree_tf2_sys/ the tf2 side of the differential harness
crates/tf_tree_cli/     binary `tf_tree` (alias `tft`)
ros/tf_tree_ros/        ament_cmake package: the §5 ingest bridge. NOT a cargo crate.
ros/tf_tree_bench_ros/  ament_cmake package: PHASE5 §9.1's DDS comparison. Benchmark-only, never shipped.
xtask/                  loom / miri / bench-gate runners
```

`ros/tf_tree_ros/` needs `rclcpp`, which exists only inside `docker/tf2`, so it
is outside the cargo workspace and outside every host recipe — `cargo fmt`,
`clippy` and `nextest` cannot see it, exactly like `crates/tf_tree_tf2_sys`.
**`just ros-build` and `just ros-test` are its entire gate** (both cover `tf_tree_bench_ros` too — `colcon --base-paths ros` picks up every package under `ros/`); run them after
touching anything under `ros/`, and note that `just ros-test` also rebuilds
`tf_tree_c --features bridge`, so a change on the Rust side of the seam is
covered by it too. CI's `tf2` job runs `just ros-test` after `just tf2-check`. It reaches the engine only through
`find_package(tf_tree CONFIG)` and `tf_tree_c`'s default-off `bridge` feature.

Phase 4 adds a C ABI crate and a header-only C++ wrapper. **The Python binding
does not go through it** — PyO3 binds Rust directly, and `docs/PHASE3.md` §0
records why (typed errors and zero-copy buffers do not survive a C boundary).

## Hard rules (from `docs/PROJECT.md` and `docs/PHASE1.md` — do not relitigate)

- **Dependency budget:** `tf_tree_core` = `libm` + `bytemuck` + `blake3` (no_std)
  and nothing else. `tf_tree_math` = `libm` + `bytemuck`. blake3 is the one
  addition, resolving a spec conflict over frame-name hashing.
- **Unsafe budget** (restated by [`0007`](./docs/decisions/0007-the-unsafe-budget-and-the-c-abi.md);
  the old enumeration had been stale since Phase 2): `unsafe` is permitted **only
  at a boundary the compiler cannot see across**, and there are four —
  the arena's raw memory (`tf_tree_arena`, `tf_tree_core::{buffer, arena_view}`),
  the OS (`tf_tree_ipc`), a foreign runtime (`tf_tree_py`), and a foreign caller
  (`tf_tree_c`). A fifth kind needs a decision record.
  `#![forbid(unsafe_code)]` stays on `tf_tree_math` and `tf_tree_cli`.
  Every `unsafe` block carries a `// SAFETY:` comment naming the invariant it
  relies on; every crate with `unsafe` carries a module `// SAFETY:` block and
  `#![deny(unsafe_op_in_unsafe_fn)]`.
  **`tf_tree` is `#![deny(unsafe_code)]` with exactly one `#[allow]`**, granted by
  [`0017`](./docs/decisions/0017-owned-handles-and-the-lifetime-rule.md): the
  lifetime extension inside `OwnedWriter`. It is the place a lifetime is extended
  *in the facade* and — since `0017` steps 6–7 landed — **the only one in the
  workspace**: those steps deleted the hand-rolled
  `tf_tree_c::publisher::extend_to_static` (used by `tft_tree_claim` and by
  `bridge.rs`'s writer map) and `tf_tree_py`'s copy, and both bindings now claim
  through `Tree::claim_owned`. `OwnedWriter` existed to delete those
  two: an ancestor of one of them leaked a claim lease — so no
  reaper would ever collect the edge — and bypassed the fork guard. That was the
  first exercise of `0007`'s budget as a *criterion* rather than a crate list, and
  the argument for it is that the facade is where an embedder looks. `deny` rather
  than `forbid` so the exception is greppable: `rg 'allow\(unsafe_code\)'
  crates/tf_tree/src` must return one line. **A second site there needs a new
  record**, and so does a fifth kind of boundary.
- **API shape is checked against [`docs/API.md`](./docs/API.md) §1 before it is
  written.** Six rules: three tiers always (R1); the hot tier never allocates,
  locks or converts (R2); integer-nanosecond stamps carrying a domain (R3);
  layout stated, never inferred (R4); errors are `Copy` identifiers with prose in
  a separate layer (R5); read-only by default (R6). A question those six do not
  answer is a decision record, not an API choice. §7 is the checklist a whole new
  surface passes.
- **No pointers in the arena; fixed capacity; no growth/realloc; `#[repr(C)]`
  everywhere; append-only `FrameId`/`EdgeId` (tombstone, never recycle).**
- **`ArcSwap`/`Arc`/`Box`/`Vec` inside an arena structure is forbidden**
  (`docs/PROJECT.md` §5 D4).
- **Do not weaken an atomic ordering** because a test passes on x86-64 — the loom
  tests exist for exactly that. **The other half of that sentence used to read
  "and the aarch64 CI target", and it was not true.** The `ubuntu-24.04-arm`
  rows are in `.github/workflows/ci.yml`'s `test` and `shm` matrices and they
  have **never executed**: GitHub Actions has produced no run of any workflow
  for this repository since 2026-07-23, while pushes to `main` kept arriving
  (`ci.yml`'s header carries the API evidence, and the caveat that the *cause*
  — an account-level Actions billing state on private repositories — is
  inferred from which repositories stopped, not read from an API this token can
  reach). Either way it is not a repository setting, not a missing runner label,
  and not fixable from here. Until runs resume, the
  weak-memory defence is `just loom` on x86-64 and nothing else, and a change to
  an ordering needs a human to say which architecture they ran on.
- No `String` in any error type or hot path; errors are `Copy` and name the
  offending edge. No `async`/runtime. No GPU/point-cloud/`deskew`. `f64` only.
- `LerpSlerp`'s right-invariance test is **supposed to fail** — do not "fix" it
  (`docs/PROJECT.md` §5 D5; `docs/PHASE1.md` §3.4).

## Commands

Everything goes through `just`; CI mirrors it 1:1.

**Two things about that sentence, both measured against
`.github/workflows/*.yml` rather than assumed.** *First*, "1:1" is now literal
for `lint`, `test` and `docs`, which used to transcribe a subset of their
recipe's lines into YAML and lost real coverage doing it — `just lint`'s
`-p tf_tree_c --features test-hooks` clippy row and `just test`'s
`-p tf_tree_c --features bridge` nextest row ran in **no workflow at all** under
the old spelling. *Second*, and it is the more important half: **CI has produced
no run for this repository since 2026-07-23**, so no workflow is evidence of
anything today. See `ci.yml`'s header for the diagnosis. The recipes in this
table, run locally, are the gate; a PR's checks are not.

| Recipe | What it does |
| --- | --- |
| `just build` | `cargo build --workspace --all-targets` |
| `just test` | `cargo nextest run --workspace` + doctests. **`--workspace` builds default features**, so anything `#[cfg]`-ed on `shm` is compiled out of it — those targets are `just shm-check`'s, not this one's |
| `just shm-check` | fmt/clippy/tests for everything behind the default-off `shm` feature, named target by target. A new `shm`-only test target belongs on that list in the commit that adds it |
| `just embed-cost` / `embed-cost-check` | `docs/PHASE5.md` §9.2's two embedding measurements — the **gated** crate-boundary row and the **exploratory** profile comparison. `embed-cost-check` is the gate for the default-off `embed-probe` / `bench-probe` features, which `just test` compiles out exactly like `shm`. `embed-cost` builds a third target directory (`--profile embedder`, 166 MiB measured) and `just bench-check` / `just bench-baseline-update` both depend on it — they must, or the baseline gate fails on the difference between the two recipes rather than on the code |
| `just test-doc-error-codes` | the `compile_fail,E0277` pins, on nightly — stable rustdoc ignores the error code, so `just test-doc` does not check them |
| `just lint` | `cargo fmt --check` plus **six** `clippy -D warnings` passes: the `--workspace --all-targets` one, then five that name a feature the workspace pass compiles out (`tf_tree_c` with `bridge` and with `test-hooks`; `tf_tree_ingest` with `fixture` and with `--no-default-features`; `tf_tree_cli --no-default-features`). It does **not** run `cargo deny check` — that is `just audit`, and CI supplies it through the cargo-deny action |
| `just audit` | `cargo deny check` — advisories, licences, bans, sources |
| `just doc` | rustdoc with warnings denied. Line 1 is the nine crates that document a public surface, at `--all-features --cfg docsrs` — what docs.rs renders for the five publishable ones. Line 2 is `tf_tree_bench` at `shm,embed-probe` (not `--all-features`: that would enable `tf2` and need ROS 2), line 3 is `xtask`. `tf_tree_bench`'s `tf2` code is **not** covered here; `just tf2-check` is where it compiles. No recipe gated rustdoc before this one and `cargo doc --no-deps --workspace` had accumulated 80 warnings; they are fixed and this is what keeps them fixed |
| `just fmt` | auto-format + clippy `--fix` |
| `just loom` | concurrency model checking (`cargo xtask loom`) |
| `just miri` | UB checking on arena, core, and the facade's one `unsafe` (`OwnedWriter`) |
| `just msrv` | the declared MSRV floor: `--locked` build on it, plus every hand-written `rust-version` |
| `just bench` | benchmark suite + go/no-go gate |
| `just contended-scaling` / `scale-sweep` / `soak` | the exploratory performance suite: `docs/PHASE1.md` §11.2's writers-and-pinning row, the width/depth/ring/fan-out axes, and multi-minute drift. Exploratory by design — they emit JSON and do **not** feed `just bench-check` |
| `just bench-run` / `just bench-ab` | the A/B loop: run the suite, change the core, run again, get a per-row verdict. Non-zero exit on a regression past the tolerance the baseline itself recorded |
| `just dds-bench` | `docs/PHASE5.md` §9.1 end-to-end over a real DDS, in the container. The only measurement here that includes the transport |
| `just ros-build` / `just ros-test` | `ros/tf_tree_ros` in the container — nothing on the host can |
| `just tf2-check` | the container-only crates: `tf_tree_tf2_sys`, and `tf_tree_c --features bridge` |

Single test — Rust: `cargo nextest run -p tf_tree_math -- exp_log_roundtrip`.

## Decision-document workflow

The phase specs (`docs/PROJECT.md`, `docs/PHASE1.md`, `docs/PHASE2.md`) are the
contract for work already scoped. A change that those documents do not cover —
new public API, new crate boundary, a different concurrency protocol — still
starts as a `draft` decision record in [`docs/decisions/`](./docs/decisions/),
not as a PR. Read [`docs/decisions/README.md`](./docs/decisions/README.md) for
the lifecycle. When a decision is `ready`, implement it as stated; its
*Implementation plan* is the per-PR work breakdown.

Records `0002` and `0003` are **superseded** by `docs/PROJECT.md` and
`docs/PHASE1.md` and are kept only as history — cite the new documents, never
them. `0004` is still authoritative for the builder-time edge declaration API.
