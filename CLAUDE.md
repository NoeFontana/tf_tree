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

- **Phases 1–3: implemented, §3.5 ownership migration included since
  2026-08-28.** Rendezvous, fd passing, claims-as-leases, reaping, fork
  poisoning, Python bindings. **§3.5 is `0037`'s shape, not the one #275
  deleted**: `Session::take_over_ownership` takes byte 0 on the description the
  survivor's session already holds — so nothing has to be verified — and
  `Tree::inherit_ownership` binds and serves the *existing* segment.
  `Tree::owner_lost` is the trigger, which had never existed at all. **It is
  caller-driven by design** ([`0019`](./docs/decisions/0019-one-binary-and-topology-you-can-wait-for.md)):
  no background thread, no daemon, so a survivor that never calls it never
  becomes owner and its arena stays ownerless. `OpenOutcome::TookOver` and
  `OpenError::TakeoverUnsupported` are **deleted** — `0037` question 3 answered
  that the variant does not survive, because inheritance is a method on an
  attached session and not an `open()` outcome, and the removal has since been
  made. `PHASE2.md` §0.0's row is authoritative.
- **Recovery reaches C, C++ and Python since 2026-08-29**
  ([`0044`](./docs/decisions/0044-recovery-the-languages-a-robot-is-written-in-cannot-reach.md)):
  `inherit_ownership` takes `&self` — both bindings hold the tree in an `Arc`,
  where `Arc::get_mut` fails as soon as a plan or publisher holds a clone —
  and the *unstable* C header gains `tft_tree_open_named` (the only read-write
  attachment C has ever had), `tft_tree_owner_lost`,
  `tft_tree_inherit_ownership` and `tft_tree_reap_dead`. **`owner_lost` answers
  "the arena has no owner", not "my socket is dead"**
  ([`0043`](./docs/decisions/0043-owner-lost-is-a-question-about-the-owner.md));
  before that it was permanently `true` for every survivor but the winner. And
  the owner's hangup callback now revokes a dead participant's **claims**, not
  only its record, so a restarted publisher granted its predecessor's slot can
  take its own edges back. **Two producers of a stale claim remain and have no
  hangup**: a dead owner, and a `build_shared` participant with no socket.
- **Phase 4: implemented except** §5.9 affinity knobs and §6.3 replay rows. C ABI
  and C++ wrapper frozen; ROS 2 ingest bridge done. §1's operational exit
  criterion is **open and not satisfiable by code**.
- **Phase 5: partial.** `FORMAT_VERSION = 3` **landed** (`layout_hash`
  `0x3D10_4195`); frozen `.tft`, MCAP ingest, counters, `tf_tree top` done;
  catalogue detects 17 of 19. **`TFT009` also reports the gap that has not
  ended** — a publisher that stopped, which every rule in the catalogue was blind
  to because they all measure *between retained stamps*, and a full ring of
  perfectly spaced samples from three weeks ago reads healthy.
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
| [`decisions/`](./docs/decisions/) | **A record's own `**Status:**` line is what says whether it is authoritative — never a range quoted here.** `grep -m1 -H '^\*\*Status:' docs/decisions/0*.md` is the whole answer — **with `-H`, because the loop this row printed first (`for f in …; do grep -m1 … "$f"; done`) prints the statuses without the file names and a reader cannot tell which belongs to which except by trusting glob order.** [`README.md`](./docs/decisions/README.md) is the per-record index, and it **deliberately does not restate a status**: it says what each record decided and carries the errata a frozen record cannot take in place. Its own header paragraph records that it used to restate one and had drifted. *This row used to read* `0004`–`0030` *authoritative*, *which had gone stale in both directions*: records had landed past the top of the range — including several this file cites in its own Status section above — and the range also swallowed [`0016`](./docs/decisions/0016-portable-simd-and-the-dependency-budget.md), whose status is `withdrawn`. What a status line does **not** carry, and so is stated here: [`0002`](./docs/decisions/0002-tf-tree-architecture.md)/[`0003`](./docs/decisions/0003-phase-1-single-process-core.md) are superseded into `PROJECT.md`/`PHASE1.md` and must not be cited; [`0028`](./docs/decisions/0028-the-slot-a-killed-participant-keeps.md) is **implemented and frozen** — do not edit it, drift is fixed by a record that supersedes it — and its fork half is [`0030`](./docs/decisions/0030-the-atfork-handler-and-inherited-descriptors.md). |

Three cautions specific to current work:

- **`FORMAT_VERSION = 3` already happened** — for the **header**. Phase 6's
  header fields are reserved; its **region table is not**, so the region break
  is still owed ([`0032`](./docs/decisions/0032-the-region-table-was-not-part-of-the-purchase.md),
  `ready` — read its status line, not this word). **Do not add arena fields
  opportunistically** — that instruction is unchanged, and the queue it joins is
  `PROJECT.md` §5.1, the scheduled-break ledger `0032` part 2 opened.
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
crates/tf_tree_bench/   criterion + tf2 differential harness. Its LIBRARY forbids
                        unsafe; several of its bins and examples carry it — a bin is
                        a separate crate root (0048; the register is
                        scripts/unsafe-budget.txt)
crates/tf_tree_tf2_sys/ tf2 side of the differential harness; unsafe = 0007 kind 3
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
- **Unsafe budget** ([`0007`](./docs/decisions/0007-the-unsafe-budget-and-the-c-abi.md),
  rule 1 amended by [`0048`](./docs/decisions/0048-a-kind-is-not-a-crate-name.md)):
  permitted **only at a boundary the compiler cannot see across**. The kinds are
  **properties, and this bullet deliberately names no crate beside them** —
  arena memory, the OS, a foreign runtime **or library** that owns its own
  objects, a foreign caller, our own C ABI called from Rust to exercise or
  measure it, and a trait the language requires be implemented unsafely in a
  target that never ships. A new kind needs a decision record; a new *file*
  needs a row in `scripts/unsafe-budget.txt`, which is the index. `just lint`
  checks that index against a compiler census **over `crates/` and `xtask/`
  only** — the script's own *What it does NOT prove* section is the list of what
  that leaves out, and it is not short.
  **This bullet used to write a crate name beside each kind, and that is what
  `0048` is about**: `0007` rule 1 did the same, every downstream reader copied
  the bracket rather than the criterion, and the budget was overtaken in two
  crates for months with every recipe green.
  **The budget binds a crate ROOT, not a package.** `#![forbid(unsafe_code)]` on
  a `src/lib.rs` governs that root and no bin, test, bench or example of the same
  package — several claims in this repository rested on the wrong scope until
  `0048` measured it. `#![forbid(unsafe_code)]` stays on `tf_tree_math` and
  `tf_tree_cli`'s libraries.
  Every `unsafe` block carries a `// SAFETY:` naming its invariant; every crate
  **root** with `unsafe` declares its posture explicitly, carries a module
  `// SAFETY:` block and `#![deny(unsafe_op_in_unsafe_fn)]`. **`0048`'s plan
  landed that for the `tf_tree_bench` bins and `tf_tree_tf2_sys` and not for
  `tf_tree_c`'s own tests and examples** — read its *Implementation plan*.
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
| `just lint` | `cargo fmt --check` + one `clippy -D warnings` pass for the workspace and one for each feature configuration that pass compiles out — **the recipe is the list, and no count of it is kept here or in the justfile comment beside it, because the two disagreed** — behind these dependencies, in this order: `no-build-output` (6 ms; rejects tracked build output by signature, after 358 MiB of it was merged across three PRs), then `no-conflict-markers`, then `py-compile` (fmt + clippy for the workspace-excluded `tf_tree_py`, against `.venv`'s interpreter or else the one on `PATH`; it skips only where there is no Python at all), then `evidence-audit` and `artifact-versions`, which cost under a second between them and are the only place either runs, then `sbom`, which checks nothing and is there because its only other caller is a tag-gated release job that has never run, and last `unsafe-budget` — `0048`'s compiler-driven census, the only dependency that compiles anything (9 s warm, 55 s cold, ~1 GiB of extra `target/` because `RUSTFLAGS` is part of cargo's fingerprint). **Not** `cargo deny` — that is `just audit` |
| `just shm-check` | fmt/clippy/tests for the default-off `shm` feature, named target by target. A new `shm`-only target belongs on that list in the commit that adds it |
| `just stable-tier-check` | compiles `tf_tree`'s default tier with `unstable` **off** — the configuration `--workspace` unifies away |
| `just artifact-versions` | version-skew gate: every hand-kept version site agrees **and so does every tracked `Cargo.lock`** (that set read from `git ls-files`, not listed in the script) (`crates/tf_tree_tf2_sys/Cargo.lock` stood at `0.0.1` four releases on, because cargo only rewrites a lock where somebody builds that crate and that one builds only in the ROS 2 container), the publishable set is the five named crates, `CHANGELOG.md` has the current section, every `just <recipe>` reference in docs and workflows resolves, **and no version literal appears in prose on a package-index front page** (#238 — four of the five crates.io pages said `0.0.1` for three releases; the root `README.md`, which `pyproject.toml` names as PyPI's, was outside the rule until 2026-09-05), and **no Markdown table row disagrees with its header** — GFM deletes the extra cells silently, which hid a whole benchmark result and a stale number for five days (#208) — **and every relative Markdown link resolves**, which nothing checked until 2026-09-05 and two did not |
| `just quickstart` | `py-setup` → `maturin develop` → runs the README snippet and asserts its output. **Not** a clean clone; it builds both venvs from scratch but reuses the checkout |
| `just doc` | rustdoc, warnings denied, three lines (nine crates at `--all-features --cfg docsrs`; `tf_tree_bench` at `shm,embed-probe`; `xtask`) |
| `just py-cross-check` | `cargo check` for `tf_tree_py` at `{x86_64,aarch64}-apple-darwin` and `x86_64-pc-windows-msvc`, with `pure-hash`. The only thing that compiles that feature — `bindings-non-linux` uses native runners. Guards #180's 2027 fallback; `check` not `build`, because linking needs an Apple SDK no Linux host has |
| `just py-test` / `py-lint` | Python on two interpreters; `py-lint` also carries `tf_tree_py`'s rustdoc, which `just doc` structurally cannot reach (excluded crate) |
| `just msrv` | **three** arms: a `--locked` `cargo +<floor>` build, every hand-written `rust-version`, and the floor as stated in the prose a user reads (`README.md`, `SUPPORT.md`, `lib.rs`) — the third exists because the first two both passed while README said 1.85 and the manifest said 1.87 |
| `just no-network` | PHASE5 §5.1's NORMATIVE assertion and §13's box 4: every `socket(2)` in the **library**'s test binaries names `AF_UNIX`, under `strace -f`. Runs in `ci.yml`'s `shm` job. It **refuses** rather than skips — no `strace`, a `strace` that cannot see a socket it is shown on purpose, a traced binary that exited non-zero, or a run in which `tests/rendezvous.rs` was not traced. `tf_tree top --web`'s `AF_INET` listener is the recipe's **positive control**, traced separately and required to be found, not an exception inside the scanner |
| `just loom` / `miri` / `tsan` | concurrency model checking / UB / thread sanitizer. `miri` appends to `$MIRIFLAGS`; CI sets `-Zmiri-strict-provenance` there, so CI is the stricter run of the same recipe |
| `just audit` | `cargo deny check` |
| `just bench` / `bench-check` / `bench-baseline-update` | suite + go/no-go gate against the committed baseline |
| `just embed-cost` / `embed-cost-check` | PHASE5 §9.2's two embedding measurements; `bench-check` depends on it |
| `just gate4` / `gate4-python` | PHASE5 §12 gate 4 — 16 workers on one frozen `.tft` — with its two worker arms. `gate4` **is** the criterion (Rust worker), runs nightly, and **exits non-zero on a FAIL since 2026-09-04**; it did not before, so that job could not go red. `gate4-python` **reports** the same measurement through the same driver with a spawned CPython worker and exits 0 on the FAIL it prints, because `S ≥ 74p` makes the verdict a function of the worker's private cost and a second *gated* arm is a decision. The mechanism is a `--gate` flag the caller passes: the binary refuses `--gate --python` rather than letting the deferred decision arrive as a flag pair. Both arms' current readings live in `docs/benchmarks/EVIDENCE.md`, not here — nothing audits this table |
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
