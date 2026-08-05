# Architectural decisions

Significant architectural changes start as a decision document in this folder,
not as a PR. The decision is the artifact; the PR(s) implementing it link back
here.

## Current state of this folder

**The decision-record process below is retained for future decisions.** What
changed is where the *existing* architecture and Phase 1 contract live:

| Record | State |
|---|---|
| [`0001`](./0001-record-architectural-decisions.md) | Active — the meta-decision that this folder exists. |
| [`0002`](./0002-tf-tree-architecture.md) | **Superseded.** Consolidated into [`docs/PROJECT.md`](../PROJECT.md) (overview, roadmap, decision log D1–D22 in §5). |
| [`0003`](./0003-phase-1-single-process-core.md) | **Superseded.** Consolidated into [`docs/PHASE1.md`](../PHASE1.md) (normative Phase 1 spec, §0–§13). |
| [`0004`](./0004-builder-time-edge-declaration.md) | **Still authoritative** — builder-time edge declaration and arena sizing from the declared edges are not represented in `PHASE1.md`. |
| [`0005`](./0005-the-shared-memory-seam.md) | **Implemented** — the `tf_tree → tf_tree_ipc` seam, §3.7 fd passing, claims as leases, reaping, fork poisoning. Amends D16. **Its `tf_tree` stays `#![forbid(unsafe_code)]` commitment (§1, and the "What we commit to" list) is amended by [`0017`](./0017-owned-handles-and-the-lifetime-rule.md)** — the crate is `deny` with one `#[allow]`; nothing else about the seam changed, and both `unsafe` sites this record names are still outside the crate. |
| [`0007`](./0007-the-unsafe-budget-and-the-c-abi.md) | **Ready** — restates the unsafe budget as a criterion rather than a crate list (it had been stale since Phase 2), and places the Phase 4 C ABI in `tf_tree_c`. |
| [`0006`](./0006-the-eight-phase-roadmap.md) | **Ready** — the roadmap re-cut from six phases to eight, adds D21/D22, and holds the alias table for the decision numbers `PHASE4.md`/`PHASE5.md` cite but this repository does not use. **Its Phase 6 is amended by `0009`.** |
| [`0008`](./0008-the-name-tf-tree.md) | **Implemented** — the name `tf_tree` is free on crates.io and PyPI, measured, and is kept. |
| [`0009`](./0009-descoping-phase-6.md) | **Ready** — Phase 6 reduced from four items to one. Covariance and copy-on-write branches are **cut**, URDF leaves the engine, cumulative B-splines stay. Amends D2 and `PROJECT.md` §1. |
| [`0010`](./0010-naming-the-record-size-refusal.md) | **Draft** — `IngestError::RecordTooLarge`, so a record past `tf_tree_ingest`'s private 256 MiB ceiling is not reported as "this file is not an MCAP". Two open questions. |
| [`0011`](./0011-the-bridge-clock-guard-and-the-static-conflict-disposition.md) | **Ready, except its clock half — that is superseded by [`0012`](./0012-the-authoritative-clock-jump-signal-and-the-degradation-ladder.md).** Its §*Decision* 1 (the per-edge guard's promotion by a **quorum of distinct publishers**, floored by `Authority::distinct_owners()`) is retired — the per-edge `ClockGuard` survives, the quorum does not. Its §*Decision* 2 (`AuthorityPolicy::Strict` gets the real **startup window** §5.4 always specified, accumulating every conflict and halting once at its close) and §*Decision* 3 (`Action::Drop` keeps its shape; the `rclcpp` side throttles instead) **still stand**. Read its scoping note first. |
| [`0012`](./0012-the-authoritative-clock-jump-signal-and-the-degradation-ladder.md) | **Ready** — supersedes `0011`'s clock half. Three successive inference rules for "`/clock` was reset" all failed, the third by making **attribution a correctness dependency**, which §5.3 forbids. Replaced by a layered design: `rcl`'s **authoritative** jump callbacks first; inference demoted to **common-mode rejection** over a per-publisher offset against an injected **steady** receipt clock (`SteadyNanos`), where agreeing step magnitudes — not coincidence — are the evidence and forward jumps are detectable; and a **degradation ladder** in which a single witness never halts, so there is no corroboration floor to get wrong. |
| [`0013`](./0013-the-benchmark-gate-never-interpolated.md) | **Ready** — the Phase 1 gate queried on-grid stamps, so `I::eval` never ran and the "150 ns" depth-3 number is really **192.7 ns**. The stamp fix and the measurement landed earlier (note the process irregularity in its header: plan items 1–2 were implemented while it was `draft`). **All four open questions are now resolved in its *Resolution*, and §11.3 is amended**: the ceilings are **≤ 300 ns ScLerp / ≤ 220 ns LerpSlerp**, set above each *observed maximum* rather than the median so they cannot flap, plus a 25 % regression clause; the call shape is pinned **inlined**, with the non-inlined boundary left to `PHASE5.md` §9.2's `embedding_cross_crate` because one number for both could not say which had moved; and criterion 3's unmeasurable "≥ 6× from 1 to 8 threads" is re-cut to **≥ 2.5× from 1 to 4** plus **≥ 5× tf2's factor** (measured 7.75×), with the 8-thread figure retained as informational. `depth3/sclerp/exact_hit` is now a labelled bench row so the 4.8× on-grid/off-grid gap is documented rather than a trap. What remains is the re-baseline on a host that passes `Fitness::probe`. |
| [`0014`](./0014-the-push-heartbeat-is-a-store.md) | **Ready** (#114) — the heartbeat is a plain store, not a locked RMW. `push` 8.66 ns → 4.65 ns. Cited by [`0018`](./0018-blocking-waits-belong-in-the-shim.md) as the reason nothing goes back onto the push path. |
| [`0015`](./0015-the-bridge-fills-a-shared-arena.md) | **Ready** (moved from draft by [`0019`](./0019-one-binary-and-topology-you-can-wait-for.md), which answers all three of its open questions and scopes it as *the ROS answer* — the bridge owns the arena when a ROS stack is the source of truth, `tf_tree serve` owns it when nothing else is a natural owner) — `tft_bridge_options` gained an optional `arena_name`, so a bridge fills a *shared* arena and separate consumer processes attach to it. **All eight steps have landed** (#139 step 0, #141, #142, #143, plus step 7's `PHASE4.md` §5.8 half), and `docs/PHASE5.md` §9.1's "one bridge plus N `tf_tree` consumers" arm — the one `just dds-bench` used to print an apology for above its own table on every run — now runs. It stays **`ready` and not `implemented`** because two things it is answerable for are still unbuilt: the fork test its *Invariants to maintain* clause demands, and §9.2's *Scaling curve, N = 1…16* for the new arm, which has only ever run at N = 4. Its three open questions are resolved in `0019` §3. |
| [`0016`](./0016-portable-simd-and-the-dependency-budget.md) | **Draft** — portable SIMD in the fold, and what it costs D14's dependency budget. The spike was reverted; see its *Open questions*. |
| [`0017`](./0017-owned-handles-and-the-lifetime-rule.md) | **Ready** — `Tree::claim_owned` → `OwnedWriter`, and the rule that no type a user stores carries a lifetime ([`API.md`](../API.md) §2.1). Three consumers needed the owned shape and two built it by hand; the first hand-rolled one leaked a claim lease and bypassed the fork guard for the life of every Python publisher. First exercise of [`0007`](./0007-the-unsafe-budget-and-the-c-abi.md)'s unsafe budget as a *criterion*: `tf_tree` goes from `forbid` to `deny` with one documented exception. |
| [`0018`](./0018-blocking-waits-belong-in-the-shim.md) | **Ready** — no blocking primitive in the arena; the `tf2`-shaped timeout is a predicted sleep in the caller. The decisive argument is D18: a `PROT_READ` consumer physically cannot register on a futex. Core gains one read (`Plan::slowest_nominal_rate_mhz`); `Plan::span` already supplies the coverage interval. |
| [`0019`](./0019-one-binary-and-topology-you-can-wait-for.md) | **Ready** — supersedes `PHASE2.md` §9. There is **no `tf_treed` binary**; the capability is `tf_tree serve`, a subcommand, and an escalation rather than a prerequisite. More importantly it removes the reason the daemon was reached for: a read-only attach now implies `CreatePolicy::Never` (`Open::new()`'s defaults ask to create an arena they cannot write — a latent class, not the observed empty-arena failure an early revision claimed), a consumer waits with `Open::await_open` and `Tree::await_frames`, and headroom covers late frames. Resolves [`0015`](./0015-the-bridge-fills-a-shared-arena.md)'s three open questions and fixes a `RUNBOOK.md` row that told operators to run a program that never existed. **Steps 1–5 landed** (#137, #139); steps 6–7 are `tf_tree serve` itself and are deliberately unscheduled, which is why the record stays `ready` rather than being frozen as `implemented`. |
| [`0020`](./0020-the-consumer-side-of-the-arena-refusal.md) | **Draft** — `tft_tree_open` collapses every join failure onto `TFT_ERR_INTERNAL`, so the process with the most actionable failure ("the publisher has not started yet") cannot tell it from "your build is broken". `0015` gave the *bridge* `TFT_ERR_ARENA_UNAVAILABLE` for precisely this argument and never applied it to the consumer; both in-tree C++ consumers poll blind and say so in their own comments. Recommends one retryable-class status plus `tft_tree_open_wait`, because widening an existing function's return set is the one kind of addition this ABI's minor-bump precedent does not cover. Three open questions. |
| [`0021`](./0021-the-idle-arena-is-resident-because-of-its-alignment.md) | **Draft** — `PHASE5.md` §9.3's `arena_memory_floor` entry was arithmetic on the layout, so it was measured; it did not shrink, it came out ~100% resident. The cause is not fixed capacity but alignment: `PoseSlot` is one cache line, `HeapArena` therefore asks `alloc_zeroed` for 64-byte alignment, and Rust routes that to `calloc` only at alignment ≤ 16 — above it, `posix_memalign` plus a hand-rolled zero-fill that touches every page. Measured at the §9.3 geometry: **4 KiB resident at align 16, 2356 KiB at align 64**. Proposes over-allocating by 63 bytes at align 16 and offsetting by hand (verified: 2352 → 8 KiB, base still 64-aligned). `MappedArena` is memfd-backed and unaffected. Three open questions. |

Cite `docs/PROJECT.md` and `docs/PHASE1.md`, never `0002`/`0003`; they are kept
in place, unedited apart from their banners, as history.

[`docs/API.md`](../API.md) is the cross-cutting **API contract** — the six rules
every binding obeys and the §7 checklist a new surface passes. It is not a phase
and authorizes no work on its own; its §6 delta table names, for each row, the
phase or record the work lands in. A decision about the *shape* of public API is
checked against it before it is written.
[`docs/PHASE2.md`](../PHASE2.md) is the Phase 2 spec and its §1 holds the Phase 1
amendments A1–A8, which are all applied; §0.0 is the live status table.

A new decision is still the right vehicle for anything the phase specs do not
already answer.

## Lifecycle

Every decision has one of four statuses on a single document type. There are no
folder moves and no archive subdirectory.

- **draft** — being written; open questions present; under team review.
- **ready** — open questions resolved; implementation plan concrete; can be
  handed to an agent or engineer.
- **implemented** — code shipped; PRs linked; document frozen.
- **superseded by NNNN** — replaced by a later decision. The doc stays in
  place as history.

### Gates

Two transitions are gates:

- **draft → ready** is the architectural review. Open questions are resolved,
  alternatives are named in *Rationale*, and the *Implementation plan* is
  detailed enough that the implementer does not need to invent.
- **implemented** is the immutability lock. Once the document is marked
  implemented, it does not get edited to match what the code does. If reality
  has drifted, write a new decision that supersedes this one.

## Numbering

Sequential four-digit (`0001`, `0002`, ...), append-only. Never renumber.
Never rewrite history.

## Filename

`NNNN-kebab-case-title.md`. Pick a title that reads as a noun phrase ("Adopt
PyO3 abi3 for wheels"), not as a verb ("We should use abi3").

## Agent handoff bar

A `ready` document is the contract between a decision-maker and an
implementer (human or agent). What an agent can assume when picking up a
`ready` doc:

- The *Decision* is final; the agent must implement it as stated, not redesign.
- The *Implementation plan* is the work breakdown; each numbered step lands as
  one PR, in order, with the verification listed.
- There are zero *Open questions*. If the agent finds one while implementing,
  it stops and asks — it does not invent an answer.

To revise a decision that is `implemented`, write a new decision with
`Status: draft` that explicitly supersedes the old one. When the new decision
reaches `implemented`, change the old decision's status line to
`superseded by NNNN` (this is the only edit ever made to an `implemented`
document).

## Template

See [`template.md`](./template.md) for the document skeleton. The first
decision, [`0001-record-architectural-decisions.md`](./0001-record-architectural-decisions.md),
is the meta-decision that this folder exists.

## Opt-in extensions

These extensions are documented but unimplemented. Adopt only when the
project genuinely needs them:

- **CLI crate** — promote a third workspace member `<name>-cli`, pure-Rust
  (depends on `-core` only, no PyO3). Add `dist-workspace.toml` and a
  cargo-dist-generated `release.yml`. Earns its keep only when there is a
  file-in/file-out batch operation for non-Python users.
- **Fuzzing** — `tools/fuzz/` with detached `[workspace]`, cargo-fuzz, and a
  slow-tier workflow.
- **Benchmarks** — `bench/` with its own uv env, `divan` or `criterion`, and
  `just bench` recipes.
- **Reference-impl oracle** — vendoring policy, `THIRD_PARTY_NOTICES.md`,
  test-only vendored code that never ships in the wheel.
- **Additional feature crates** — sibling-to-`-core` pattern; `-ffi`
  re-exports.
- **Real-model integration tests** — `[project.optional-dependencies]
  real-models = [...]` with `@pytest.mark.real_models` that skips cleanly
  when the extra is not installed.
- **Diátaxis user docs** — `docs/tutorials/`, `docs/how-to/`,
  `docs/reference/`, `docs/explanation/` under mkdocs.
- **Design docs** — `docs/design/` for living subsystem documentation when
  the codebase outgrows "read the decisions + the code."
- **`slow.yml` workflow** — release-mode tests, full Python-version matrix,
  env-gated whole-dataset smokes; triggered by `workflow_dispatch` before
  cutting a release tag.
