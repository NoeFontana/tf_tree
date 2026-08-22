# CLAUDE.md — agent guidance for tf_tree

For AI agents. Humans: [`CONTRIBUTING.md`](./CONTRIBUTING.md).

`tf_tree` is a transform tree engine — a faster, more scalable alternative to ROS
`tf2`. Built along a fixed **eight-phase** roadmap ([`0006`](./docs/decisions/0006-the-eight-phase-roadmap.md)).

**The documents in [`docs/`](./docs/) are the contract.** Read
[`docs/PROJECT.md`](./docs/PROJECT.md) (architecture, roadmap, decision log
D1–D22 in §5, design smells in §6), then the spec for the phase you touch,
**before** touching code. When they do not answer a question — **stop and ask.**
Never invent an answer in the concurrency or arena-layout sections.

Each spec's **§0.0 status table is the source of truth**, over its own prose,
over the README, and over this file.

## Status

- **Phases 1–3: implemented.** Rendezvous, fd passing, ownership migration,
  claims-as-leases, reaping, fork poisoning, Python bindings.
- **Phase 4: implemented except** §5.9 affinity knobs and §6.3 replay rows. C ABI
  and C++ wrapper frozen; ROS 2 ingest bridge done. §1's operational exit
  criterion is **open and not satisfiable by code**.
- **Phase 5: partial.** `FORMAT_VERSION = 3` **landed** (`layout_hash`
  `0x3D10_4195`); frozen `.tft`, MCAP ingest, counters, `tf_tree top` done;
  catalogue detects 16 of 19.
- **Published** to crates.io and PyPI since 2026-08-17, on the `0.0.x` line —
  cargo treats every `0.0.x` as incompatible with every other, so *every release
  may break every other* and that is the whole promise. The number itself is not
  repeated here because nothing would gate it: read `[workspace.package]
  version`, which `just artifact-versions` holds every other site to. MSRV
  **1.87**.

## Read before changing X

| Doc | Role |
|---|---|
| [`PROJECT.md`](./docs/PROJECT.md) | Overview, roadmap, **D1–D22 (§5)**, design smells (§6). Supersedes `0002`. |
| [`PHASE1.md`](./docs/PHASE1.md) | Layouts, atomic orderings, tests (§10), bench gate (§11). Supersedes `0003`. |
| [`PHASE2.md`](./docs/PHASE2.md) | Shared memory. **§1 holds Phase 1 amendments A1–A8, all applied** — read before changing any concurrency protocol; they are why several orderings look odd. |
| [`PHASE3.md`](./docs/PHASE3.md) | Python bindings. |
| [`PHASE4.md`](./docs/PHASE4.md) | C ABI, C++ wrapper, ROS 2 ingest, `sample_with_derivatives`. ROS 2 **is** available in `docker/tf2`; what is missing is a second RMW, clang in that image, and a robot. |
| [`PHASE5.md`](./docs/PHASE5.md) | Frozen `.tft`, bag ingestion, counters, `TFT001`–`TFT019`, `tf_tree top`. |
| [`PHASE7.md`](./docs/PHASE7.md) | The `tf2` shim. **GATED (D21), not scheduled.** |
| [`API.md`](./docs/API.md) | **Not a phase.** Cross-cutting API contract: six rules (§1), per-binding surface (§2–§5), delta table (§6), new-surface checklist (§7). |
| [`decisions/`](./docs/decisions/) | `0002`–`0003` superseded; `0004`–`0030` authoritative. `0028` is **implemented and frozen** — do not edit it; drift is fixed by a record that supersedes it. Its fork half is [`0030`](./docs/decisions/0030-the-atfork-handler-and-inherited-descriptors.md), still `draft`. |

Three cautions specific to current work:

- **`FORMAT_VERSION = 3` already happened** — for the **header**. Phase 6's
  header fields are reserved; its **region table is not**, so the region break
  is still owed ([`0032`](./docs/decisions/0032-the-region-table-was-not-part-of-the-purchase.md),
  `draft`). **Do not add arena fields opportunistically** — that instruction is
  unchanged, and `0032` gives it a queue to join rather than an indefinite wait.
- **`PHASE5.md` §8 is a section about *not* building something.** Visualization
  is deliberately absent, argument recorded. **Do not propose a viewer
  integration without refuting §8.1 first.**
- **`PHASE7.md` existing is not permission to build it.** §0.0 lists four gates,
  none met. Its §4 is a table of *questions*; answering one from that document
  rather than from Phase 4's surprise log is the exact failure the gate prevents.

## Project shape

```
crates/tf_tree_math/    no_std; SE(3)/SO(3), quats, dual quats; #![forbid(unsafe_code)]
crates/tf_tree_arena/   no_std+alloc; pointer-free arena + layout math (unsafe allowed)
crates/tf_tree_core/    no_std+alloc; the engine; unsafe only in buffer.rs / arena_view.rs
crates/tf_tree/         std facade; #![deny(unsafe_code)] + one #[allow]: OwnedWriter (0017)
crates/tf_tree_ipc/     std; rendezvous, lock file, fd passing (unsafe: one atfork shim)
crates/tf_tree_bridge/  ROS-independent half of the /tf ingest bridge
crates/tf_tree_ingest/  MCAP bag ingestion (§3); not in core/arena, not in the CLI
crates/tf_tree_py/      PyO3; binds the Rust core directly, NOT the C ABI
crates/tf_tree_c/       C ABI + header-only C++ wrapper
crates/tf_tree_bench/   criterion + tf2 differential harness
crates/tf_tree_tf2_sys/ tf2 side of the differential harness
crates/tf_tree_cli/     binary `tf_tree` (alias `tft`)
ros/tf_tree_ros/        ament_cmake: the §5 ingest bridge. NOT a cargo crate.
ros/tf_tree_bench_ros/  ament_cmake: PHASE5 §9.1 DDS comparison. Benchmark-only.
xtask/                  loom / miri / bench-gate runners
```

**Five crates publish:** `tf_tree`, `tf_tree_core`, `tf_tree_math`,
`tf_tree_arena`, `tf_tree_ipc`. The rest carry `publish = false` with the reason
in their manifest.

`tf_tree_py` and `tf_tree_tf2_sys` are **excluded** from the cargo workspace
(libpython, ROS 2). `ros/*` needs `rclcpp`, so `cargo fmt`/`clippy`/`nextest`
cannot see it — **`just ros-build` and `just ros-test` are its entire gate**;
`ros-test` also rebuilds `tf_tree_c --features bridge,shm`.

## Hard rules — do not relitigate

- **Dependency budget:** `tf_tree_core` = `libm` + `bytemuck` + `blake3`.
  `tf_tree_math` = `libm` + `bytemuck`. Nothing else. No `serde`, `tokio`,
  `nalgebra`, or logging framework in the core (D14).
- **Unsafe budget** ([`0007`](./docs/decisions/0007-the-unsafe-budget-and-the-c-abi.md)):
  permitted **only at a boundary the compiler cannot see across**, and there are
  four — arena memory (`tf_tree_arena`, `tf_tree_core::{buffer, arena_view}`),
  the OS (`tf_tree_ipc`), a foreign runtime (`tf_tree_py`), a foreign caller
  (`tf_tree_c`). A fifth kind needs a decision record.
  `#![forbid(unsafe_code)]` stays on `tf_tree_math` and `tf_tree_cli`.
  Every `unsafe` block carries a `// SAFETY:` naming its invariant; every crate
  with `unsafe` carries a module `// SAFETY:` block and
  `#![deny(unsafe_op_in_unsafe_fn)]`.
- **`tf_tree` is `#![deny(unsafe_code)]` with exactly one `#[allow]`** —
  `OwnedWriter`'s lifetime extension ([`0017`](./docs/decisions/0017-owned-handles-and-the-lifetime-rule.md)),
  and it is **the only lifetime extension in the workspace**. `0017` steps 6–7
  deleted the two that were not (`tf_tree_c::publisher::extend_to_static` and
  `tf_tree_py`'s copy); both bindings now claim through `Tree::claim_owned`. One
  of the deleted pair leaked a claim lease — so no reaper would ever collect the
  edge — and bypassed the fork guard. `deny` not `forbid` so the exception is
  greppable: **`rg 'allow\(unsafe_code\)' crates/tf_tree/src` must return one
  line.** A second site needs a new record.
- **Check API shape against [`API.md`](./docs/API.md) §1 *before* writing it.**
  R1 three tiers always; R2 hot tier never allocates/locks/converts; R3
  integer-nanosecond stamps carrying a domain; R4 layout stated, never inferred;
  R5 errors are `Copy` identifiers, prose in a separate layer; R6 read-only by
  default. A question those six do not answer is a decision record, not an API
  choice.
- **Arena:** no pointers, fixed capacity, no growth/realloc, `#[repr(C)]`
  everywhere, append-only `FrameId`/`EdgeId` (tombstone, never recycle — D10 is
  what makes a stale `Plan` safe).
- **`ArcSwap`/`Arc`/`Box`/`Vec` inside an arena structure is forbidden** (D4).
  `Arc` refcounts do not cross a process boundary; it is the most tempting
  simplification in the codebase.
- **Do not weaken an atomic ordering because a test passes on x86-64.**
  `just loom` is the model checker and it runs on x86-64. **aarch64 CI became
  real on 2026-08-16** — the `ubuntu-24.04-arm` rows had never executed in the
  life of the project and now run and pass — so an ordering claim has a second
  architecture behind it for the first time. That is *execution*, not a
  weak-memory proof: aarch64 hardware reorders more than x86-64, so it can
  surface a missing barrier, but a green run is not evidence one is absent.
  Loom remains the argument; aarch64 is corroboration.
- **CI is back, and what it found on arrival is the reason to trust it.**
  Actions produced no run for this repository between 2026-07-23 and
  2026-08-16, an account-level state on private repositories — confirmed rather
  than inferred when making the repository public restored runs immediately,
  and visible throughout in that GitHub-managed Dependabot runs never stopped
  while *user* workflows did. The first runs after it returned found three
  latent bugs that had been invisible for the project's whole life: `c_char`
  signedness on aarch64 (`i8` vs `u8`, six casts and two hard type errors), an
  exported symbol in no header tier, and a container with no writable
  `ROS_HOME`. A green check is evidence again — but only of what the jobs
  cover, and `just` is still what gates a change before it is pushed.
- No `String` in any error type or hot path; errors are `Copy` and name the
  offending edge (D11). No `async`/runtime. No GPU/point-cloud/`deskew` (D8).
  `f64` only (D6).
- **`LerpSlerp`'s right-invariance test is *supposed* to fail** — do not "fix" it
  (D5; `PHASE1.md` §3.4).
- One writer per edge, enforced at runtime and in the type system (D7). Read-only
  attach is the consumer default (D18). Liveness is the socket, never a heartbeat
  timeout (D17). Every mutation protocol walks the crash matrix (D15).
- Do not add a **second spelling** of an existing path (a `coverage` beside
  `span`, a `resample` beside `at(arange(...))`). Document the one that exists.
- No blocking wait, futex, or notification primitive **in the arena**
  ([`0018`](./docs/decisions/0018-blocking-waits-belong-in-the-shim.md)).
- **Cut, not deferred** ([`0009`](./docs/decisions/0009-descoping-phase-6.md)):
  covariance (a tree cannot compose a correct one), copy-on-write branches,
  multi-parent edges, URDF in the engine. Do not reintroduce any of them.

## Commands

Everything goes through `just`, and **CI mirrors it 1:1 by invoking the recipes
rather than transcribing them** — a job that re-spells a recipe's lines drifts
from it, which is how `just lint`'s `tf_tree_c --features test-hooks` clippy row
once ran in no workflow at all. Run them locally before pushing: CI is the
second opinion, not the first.

| Recipe | What it does |
|---|---|
| `just build` | `cargo build --workspace --all-targets` |
| `just test` | nextest `--workspace` + doctests + ingest-check. **Builds default features**, so anything `#[cfg]`-ed on `shm` is compiled out — that is `just shm-check`'s job |
| `just lint` | `cargo fmt --check` + **eight** `clippy -D warnings` passes (workspace, then seven naming a feature the workspace pass compiles out), behind four dependencies: `no-build-output` (6 ms; rejects tracked build output by signature, after 358 MiB of it was merged across three PRs), then `py-compile` (fmt + clippy for the workspace-excluded `tf_tree_py`, against `.venv`'s interpreter or else the one on `PATH`; it skips only where there is no Python at all), then `evidence-audit` and `artifact-versions`, which cost under a second between them and are the only place either runs. **Not** `cargo deny` — that is `just audit` |
| `just shm-check` | fmt/clippy/tests for the default-off `shm` feature, named target by target. A new `shm`-only target belongs on that list in the commit that adds it |
| `just stable-tier-check` | compiles `tf_tree`'s default tier with `unstable` **off** — the configuration `--workspace` unifies away |
| `just artifact-versions` | version-skew gate: every hand-kept version site agrees, the publishable set is the five named crates, `CHANGELOG.md` has the current section, every `just <recipe>` reference in docs and workflows resolves, and **no Markdown table row disagrees with its header** — GFM deletes the extra cells silently, which hid a whole benchmark result and a stale number for five days (#208) |
| `just quickstart` | `py-setup` → `maturin develop` → runs the README snippet and asserts its output. **Not** a clean clone; it builds both venvs from scratch but reuses the checkout |
| `just doc` | rustdoc, warnings denied, three lines (nine crates at `--all-features --cfg docsrs`; `tf_tree_bench` at `shm,embed-probe`; `xtask`) |
| `just py-test` / `py-lint` | Python on two interpreters; `py-lint` also carries `tf_tree_py`'s rustdoc, which `just doc` structurally cannot reach (excluded crate) |
| `just msrv` | **three** arms: a `--locked` `cargo +<floor>` build, every hand-written `rust-version`, and the floor as stated in the prose a user reads (`README.md`, `SUPPORT.md`, `lib.rs`) — the third exists because the first two both passed while README said 1.85 and the manifest said 1.87 |
| `just loom` / `miri` / `tsan` | concurrency model checking / UB / thread sanitizer. `miri` appends to `$MIRIFLAGS`; CI sets `-Zmiri-strict-provenance` there, so CI is the stricter run of the same recipe |
| `just audit` | `cargo deny check` |
| `just bench` / `bench-check` / `bench-baseline-update` | suite + go/no-go gate against the committed baseline |
| `just embed-cost` / `embed-cost-check` | PHASE5 §9.2's two embedding measurements; `bench-check` depends on it |
| `just gate4` / `gate4-python` | PHASE5 §12 gate 4 — 16 workers on one frozen `.tft` — with its two worker arms. `gate4` **is** the criterion (Rust worker) and runs nightly; `gate4-python` **reports** the same measurement through the same driver with a spawned CPython worker and exits 0 on the FAIL it prints, because `S ≥ 74p` makes the verdict a function of the worker's private cost and a second *gated* arm is a decision. Both arms' current readings live in `docs/benchmarks/EVIDENCE.md`, not here — nothing audits this table |
| `just test-doc-error-codes` | the `compile_fail,E0277` pins, on nightly (stable rustdoc ignores the code) |
| `just ros-build` / `ros-test` / `tf2-check` / `dds-bench` | container-only; nothing on the host can run them |
| `just contended-scaling` / `scale-sweep` / `soak` / `bench-run` / `bench-ab` | exploratory; emit JSON, do **not** feed `bench-check` |

Single test: `cargo nextest run -p tf_tree_math -- exp_log_roundtrip`.

## Decision workflow

A change the specs do not cover — new public API, new crate boundary, a different
concurrency protocol — starts as a **`draft` decision record** in
[`docs/decisions/`](./docs/decisions/), **not** as a PR. See its `README.md` for
the lifecycle. When a record is `ready`, implement it as stated; its
*Implementation plan* is the per-PR breakdown.

Cite `PROJECT.md`/`PHASE1.md`, never the superseded `0002`/`0003`. `0004` remains
authoritative for builder-time edge declaration.
