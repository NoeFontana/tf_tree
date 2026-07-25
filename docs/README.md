# Docs

Four canonical documents carry the project. Read them in this order:

1. [`PROJECT.md`](./PROJECT.md) — what tf_tree is, the architecture in one page,
   the six-phase roadmap, and the **decision log D1–D20** (§5) with the rationale
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
