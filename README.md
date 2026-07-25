# tf_tree

A transform tree engine: store time-stamped rigid-body transforms between named
coordinate frames and answer *"where was frame A relative to frame B at time
t?"* — fast enough to sit inside a control loop, with diagnostics good enough to
debug at 3 a.m.

`tf_tree` targets the workloads ROS 2's `tf2` was not designed for: kilohertz
sensor edges, many concurrent readers in one process, multiple processes on one
host (zero-copy shared memory), and multiple hosts on one robot.

> **Status: Phase 1 (single-process core), under construction.** The
> architecture and the full Phase 1 specification live in [`docs/`](./docs/) —
> read [`PROJECT.md`](./docs/PROJECT.md) (overview, roadmap, decision log D1–D20)
> then [`PHASE1.md`](./docs/PHASE1.md) (the normative Phase 1 spec).
> [`PHASE2.md`](./docs/PHASE2.md) specifies the shared-memory phase and, in its
> §1, the Phase 1 amendments A1–A8 that are not yet applied.

## Workspace

```
crates/
├── tf_tree_math/    no_std SE(3)/SO(3) + dual quaternions; #![forbid(unsafe_code)]
├── tf_tree_arena/   no_std+alloc pointer-free arena + layout math
├── tf_tree_core/    no_std+alloc engine: interning, topology, seqlock buffers, plans
├── tf_tree/         std facade: builder, plan-cached lookup, Display errors
├── tf_tree_bench/   criterion benches + tf2 differential harness
└── tf_tree_cli/     binary `tf_tree` (alias `tft`): tree / echo / doctor / bench
xtask/               loom, miri, and bench-gate runners
docs/                PROJECT.md, PHASE1.md, PHASE2.md, benchmarks/, decisions/
```

## Commands

```sh
just build     # cargo build --workspace
just test      # nextest + doctests
just lint      # fmt --check + clippy -D warnings + cargo-deny
just loom      # concurrency model checking
just miri      # UB checking (arena + core)
just bench     # benchmark suite + go/no-go gate
```

`just --list` for everything. Licensed dual [MIT](./LICENSE-MIT) /
[Apache-2.0](./LICENSE-APACHE).
