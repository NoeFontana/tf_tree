# Docs

Six canonical documents carry the project, plus two that are not phases. Read
them in this order:

1. [`PROJECT.md`](./PROJECT.md) — what tf_tree is, the architecture in one page,
   the eight-phase roadmap, and the **decision log D1–D22** (§5) with the rationale
   behind each entry. Start here.
2. [`PHASE1.md`](./PHASE1.md) — the **normative** Phase 1 specification:
   workspace layout, load-bearing invariants (§2), math requirements (§3), arena
   layout (§4), records (§5), the concurrency core and its atomic orderings (§6),
   plans (§7), public API (§8), errors (§9), the test plan (§10), and the
   benchmark go/no-go gate (§11).
3. [`PHASE2.md`](./PHASE2.md) — the **normative** Phase 2 specification (shared
   memory, discovery and rendezvous, ownership migration, liveness,
   crash-consistency, fault injection). Its §1 holds **Phase 1 amendments
   A1–A8**; §0.0 tracks which are applied. Read §3 before writing any IPC code —
   the rendezvous borrows the kernel's file locks rather than implementing leader
   election, and that decision shapes everything else.
4. [`PHASE3.md`](./PHASE3.md) — the **normative** Phase 3 specification (Python
   bindings). Its §1 corrects a Phase 2 handoff constraint and adds the
   free-threading declaration that, if missed, silently re-enables the GIL for a
   user's entire process. §2's measured call-overhead budgets drive the whole
   API shape.
5. [`PHASE4.md`](./PHASE4.md) — the **normative** Phase 4 specification
   (dogfooding integration: `sample_with_derivatives`, the two-tier C ABI, the
   header-only C++ wrapper, the one-way ROS 2 ingest bridge). Its §1 is the
   thing to read first: **the exit criterion is operational, not a feature
   list**, and §0.0 records which parts this development environment cannot
   gate at all.
6. [`PHASE5.md`](./PHASE5.md) — the **normative** Phase 5 specification
   (offline, observability, and the adoption wedge: the frozen `.tft` arena,
   bag ingestion, `FORMAT_VERSION = 3`, diagnostic counters, the `TFT001`–`TFT019`
   catalogue, `tf_tree top`). §8 is a section about **not** building something,
   and it is deliberate — read it before proposing a viewer integration.

Two documents cut across the phases:

- [`API.md`](./API.md) — the **API contract**: the six rules (§1) that generate
  every binding, the normative surface of Rust, Python, C and C++, and the §7
  checklist any new surface has to pass. Read it before adding public API to any
  of them. It authorizes nothing on its own — its §6 delta table names the phase
  or decision record each row lands in.
- [`PHASE7.md`](./PHASE7.md) — the `tf2`-shaped compatibility shim, **gated by
  D21 and not scheduled**. Its §0.0 lists the four gates and none is met. Before
  they are, the only work it authorizes is filing Phase 4's surprise log against
  its §4 J-rows.

The roadmap was re-cut from six phases to eight by
[`decisions/0006`](./decisions/0006-the-eight-phase-roadmap.md), which also holds
the alias table for the decision numbers `PHASE4.md`/`PHASE5.md` cite (D28, D29,
D30, D34) against this repository's log.

Supporting material:

- [`RUNBOOK.md`](./RUNBOOK.md) — operator runbook, organised by symptom. Every
  row names a distinct error type and, where one exists, the `tf_tree doctor`
  check that detects it.
- [`benchmarks/`](./benchmarks/) — measured results, each row naming the command
  that produced it.
- [`design/`](./design/) — design notes for work not yet in a phase spec,
  including which proposals were **falsified by measurement** and why.
- [`decisions/`](./decisions/) — architectural decision records. The process is
  retained for **future** decisions; see [`decisions/README.md`](./decisions/README.md)
  for the lifecycle. Records `0002` and `0003` have been consolidated into
  `PROJECT.md` and `PHASE1.md` and are superseded;
  [`0004`](./decisions/0004-builder-time-edge-declaration.md) is still
  authoritative for the builder-time edge declaration API.

User-facing documentation (Diátaxis: tutorials, how-tos, reference, explanation)
is an opt-in extension; see the "Opt-in extensions" section of the decisions
README for the recipe.
