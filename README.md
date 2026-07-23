# tf_tree

A transform tree engine: store time-stamped rigid-body transforms between named
coordinate frames and answer *"where was frame A relative to frame B at time
t?"* — fast enough to sit inside a control loop, with diagnostics good enough to
debug at 3 a.m.

`tf_tree` targets the workloads ROS 2's `tf2` was not designed for: kilohertz
sensor edges, many concurrent readers in one process, multiple processes on one
host (zero-copy shared memory), and multiple hosts on one robot.

> **Status: Phase 1 (single-process core), under construction.** The
> architecture and the full Phase 1 specification live in
> [`docs/decisions/`](./docs/decisions/) — read
> [`0002`](./docs/decisions/0002-tf-tree-architecture.md) then
> [`0003`](./docs/decisions/0003-phase-1-single-process-core.md).

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
