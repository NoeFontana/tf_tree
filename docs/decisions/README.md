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
| [`0005`](./0005-the-shared-memory-seam.md) | **Implemented** — the `tf_tree → tf_tree_ipc` seam, §3.7 fd passing, claims as leases, reaping, fork poisoning. Amends D16. |
| [`0007`](./0007-the-unsafe-budget-and-the-c-abi.md) | **Ready** — restates the unsafe budget as a criterion rather than a crate list (it had been stale since Phase 2), and places the Phase 4 C ABI in `tf_tree_c`. |
| [`0006`](./0006-the-eight-phase-roadmap.md) | **Ready** — the roadmap re-cut from six phases to eight, adds D21/D22, and holds the alias table for the decision numbers `PHASE4.md`/`PHASE5.md` cite but this repository does not use. **Its Phase 6 is amended by `0009`.** |
| [`0008`](./0008-the-name-tf-tree.md) | **Implemented** — the name `tf_tree` is free on crates.io and PyPI, measured, and is kept. |
| [`0009`](./0009-descoping-phase-6.md) | **Ready** — Phase 6 reduced from four items to one. Covariance and copy-on-write branches are **cut**, URDF leaves the engine, cumulative B-splines stay. Amends D2 and `PROJECT.md` §1. |
| [`0010`](./0010-naming-the-record-size-refusal.md) | **Draft** — `IngestError::RecordTooLarge`, so a record past `tf_tree_ingest`'s private 256 MiB ceiling is not reported as "this file is not an MCAP". Two open questions. |

Cite `docs/PROJECT.md` and `docs/PHASE1.md`, never `0002`/`0003`; they are kept
in place, unedited apart from their banners, as history.
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
