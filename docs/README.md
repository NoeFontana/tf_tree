# Docs

Normative specifications for whoever is changing the engine, not tutorials.

| If you want to | Read |
|---|---|
| Call the API from Rust, Python, C or C++ | [`API.md`](./API.md) §2–§5, after its six rules in §1 |
| Understand a lookup that failed, or a robot that is misbehaving | [`RUNBOOK.md`](./RUNBOOK.md) |
| Know whether a number is measured, and on what | [`benchmarks/`](./benchmarks/) |
| Know whether a feature exists yet | The `§0.0` table heading `PHASE2`, `PHASE4`, `PHASE5`, `PHASE7`. `PHASE1` has none; `PHASE3` records deviations inline |
| Know why it works the way it does | [`PROJECT.md`](./PROJECT.md) §5, the decision log |
| Know why something was *not* built | [`0009`](./decisions/0009-descoping-phase-6.md), [`PHASE5.md`](./PHASE5.md) §8, [`PHASE7.md`](./PHASE7.md) §0.0 |

## Reading order for changing the project

1. [`PROJECT.md`](./PROJECT.md): architecture, the eight-phase roadmap, and the
   **decision log D1–D22** (§5).
2. [`PHASE1.md`](./PHASE1.md): the engine: invariants (§2), arena layout (§4),
   concurrency core and atomic orderings (§6), plans (§7), tests (§10), bench gate (§11).
3. [`PHASE2.md`](./PHASE2.md): shared memory, rendezvous, ownership migration,
   liveness, crash-consistency. Its §1 holds amendments A1–A8; read §3 before
   writing IPC code.
4. [`PHASE3.md`](./PHASE3.md): Python bindings.
5. [`PHASE4.md`](./PHASE4.md): `sample_with_derivatives`, the C ABI, the C++
   wrapper, the one-way ROS 2 bridge. Read §1 first: **the exit criterion is
   operational**.
6. [`PHASE5.md`](./PHASE5.md): the frozen `.tft` arena, bag ingestion, counters,
   `TFT001`–`TFT019`, `tf_tree top`. §8 is about **not** building a viewer.

Cross-cutting:

- [`API.md`](./API.md): the six rules (§1), the normative surfaces, the §7
  checklist a new surface passes, the §6 delta table.
- [`PHASE7.md`](./PHASE7.md): the `tf2` shim, **gated by D21 and not scheduled**.

[`decisions/0006`](./decisions/0006-the-eight-phase-roadmap.md) holds the alias
table for D28, D29, D30, D34. Also: [`design/`](./design/) (design notes) and
[`decisions/`](./decisions/) (records; [`README.md`](./decisions/README.md) has the
lifecycle; [`0004`](./decisions/0004-builder-time-edge-declaration.md) is
authoritative for builder-time edge declaration).
