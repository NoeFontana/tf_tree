# tf_tree

A transform tree engine: store time-stamped rigid-body transforms between named
coordinate frames and answer *"where was frame A relative to frame B at time
t?"* — fast enough to sit inside a control loop, with diagnostics good enough to
debug at 3 a.m.

`tf_tree` targets the workloads ROS 2's `tf2` was not designed for: kilohertz
sensor edges, many concurrent readers in one process, multiple processes on one
host (zero-copy shared memory), and multiple hosts on one robot.

> **Status: Phase 1 complete; Phase 2 (shared memory) complete except for the
> daemon and recorder.** The architecture and specifications live in
> [`docs/`](./docs/) — read [`PROJECT.md`](./docs/PROJECT.md) (overview,
> roadmap, decision log D1–D20), then [`PHASE1.md`](./docs/PHASE1.md) and
> [`PHASE2.md`](./docs/PHASE2.md), whose §0.0 has the current status table.
> Phase 3 (Python bindings) is specified in [`PHASE3.md`](./docs/PHASE3.md).

## Shared memory IPC is not a sandbox

Processes sharing a `tf_tree` arena are **mutually trusting, same-user,
cooperating processes**. A read-write participant can corrupt any part of the
arena, and no checksum would change that — it holds a writable mapping of the
same pages (`PHASE2.md` §3.10). Do not attach a process you would not run as
yourself.

Three things the design *does* guarantee, and they are the ones that matter on a
robot:

- **A read-only participant cannot corrupt anything**, enforced by the MMU, not
  by convention. It is the default for consumers (D18), and it converts a class
  of whole-system failures into a single-process fault.
- **A participant that crashes, at any instruction, cannot corrupt the arena or
  wedge anyone else.** A killed writer's edge is reclaimed; a killed interner's
  entry is recoverable; a killed mutator does not leave a permanently locked
  topology.
- **A participant that hangs cannot be mistaken for a crashed one.** Liveness is
  the kernel's answer about a file lock, not a heartbeat timeout, so a
  `SIGSTOP`ped publisher keeps its claims and a stalled one is never reaped out
  from under itself.

`fork()` is the sharp edge worth knowing about up front: the arena is mapped
`MADV_DONTFORK`, so a child has no mapping and every inherited handle reports
`ChildDetached`. Python's `multiprocessing` defaults to `fork` on Linux — open
inside the worker, or use the `spawn` start method.

## Workspace

```
crates/
├── tf_tree_math/    no_std SE(3)/SO(3) + dual quaternions; #![forbid(unsafe_code)]
├── tf_tree_arena/   no_std+alloc pointer-free arena + layout math
├── tf_tree_core/    no_std+alloc engine: interning, topology, seqlock buffers, plans
├── tf_tree/         std facade: builder, plan-cached lookup, Display errors
├── tf_tree_ipc/     zero-config rendezvous: runtime dir, OFD lock file, attach protocol
├── tf_tree_bench/   criterion benches + tf2 differential harness
└── tf_tree_cli/     binary `tf_tree` (alias `tft`): tree / echo / doctor / participants / bench
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
