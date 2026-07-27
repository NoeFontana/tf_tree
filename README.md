# tf_tree

A transform tree engine: store time-stamped rigid-body transforms between named
coordinate frames and answer *"where was frame A relative to frame B at time
t?"* — fast enough to sit inside a control loop, with diagnostics good enough to
debug at 3 a.m.

`tf_tree` targets the workloads ROS 2's `tf2` was not designed for: kilohertz
sensor edges, many concurrent readers in one process, multiple processes on one
host (zero-copy shared memory), and multiple hosts on one robot.

**It is not `tf2`, not a fork of it, and not affiliated with ROS.** It is an
independent engine that solves the same problem with a different data structure,
and it is deliberately named so that people looking for a `tf2` alternative can
find it ([`0008`](./docs/decisions/0008-the-name-tf-tree.md) records that
decision). There is no drop-in `tf2_ros::Buffer` shim today; that is Phase 7 and
it is gated on evidence this project has not yet produced.

## Status

**Nothing is published yet** — not on crates.io, not on PyPI. Build from source.

| Phase | What it is | Status |
|---|---|---|
| 1 | Single-process engine: arena, seqlock buffers, plans, SE(3) math | **Implemented** |
| 2 | Shared memory: rendezvous, fd passing, claims as leases, reaping | **Implemented** except the daemon/recorder surface (§9–§10) and the long-running fault harness |
| 3 | Python bindings (PyO3, zero intermediate allocation) | **Implemented** |
| 4 | C ABI, C++ wrapper, ROS 2 ingest bridge, derivatives | **Partial.** C ABI and C++ wrapper done; the bridge's `rclcpp` half is **BLOCKED** on a spec correction |
| 5 | Frozen `.tft` arena, bag ingestion, diagnostics, `tf_tree top` | **Partial.** `FORMAT_VERSION = 3` and the diagnostic counters landed; the frozen arena, ingest, catalogue and TUI have not |
| 6–8 | Multi-host, `tf2` compatibility shim, replication | Not started |

**The per-phase `§0.0` tables in [`docs/`](./docs/) are the source of truth**, not
this one — [`PHASE2.md`](./docs/PHASE2.md#00-implementation-status),
[`PHASE4.md`](./docs/PHASE4.md#00-implementation-status),
[`PHASE5.md`](./docs/PHASE5.md#00-implementation-status). If this table and one
of those disagree, the phase document is right and this is stale.

**GitHub Actions has produced no run since 2026-07-23.** A green check on a pull
request is not evidence. Gate locally with `just`.

## First five minutes

Python, from a source checkout:

```sh
git clone https://github.com/NoeFontana/tf_tree && cd tf_tree
just py-setup          # uv-managed interpreter + venv
just py-wheel          # maturin build + install into .venv
```

```python
import tf_tree

tree = tf_tree.build([("map", "base"), ("base", "cam")])
# stamp in integer nanoseconds; pose is [qw, qx, qy, qz, x, y, z]
tf_tree.push(tree, "base", "map", 1_000, [1.0, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0])
tf_tree.push(tree, "base", "map", 2_000, [1.0, 0.0, 0.0, 0.0, 3.0, 4.0, 5.0])

print(tree.plan("map", "base").at(1_500)[:3, 3])   # -> [2. 3. 4.]
```

That is a real result and not a toy: the query lands halfway between two samples,
so the printed translation is their interpolated midpoint, and `plan()` is the
object you keep — compiling the route once and evaluating it many times is the
whole shape of the fast path.

Two things surprise people, both deliberate:

- **Stamps are integer nanoseconds.** There is no float-seconds overload. At a
  2026 epoch the ULP of `float64` seconds is 238 ns, so every interval in a
  1 kHz stream is wrong after a round trip.
- **Nothing returns a view into shared memory.** An edge's samples are a ring
  another process is overwriting, and correct reads go through a seqlock. "Zero
  copy" here means no *intermediate* allocation — use `Plan.at_into` to supply
  the destination.

Rust is `cargo add tf_tree` once published; until then, a path dependency on
`crates/tf_tree`. `just` alone lists everything the repository can do.

## Shared memory IPC is not a sandbox

Processes sharing a `tf_tree` arena are **mutually trusting, same-user,
cooperating processes**. A read-write participant can corrupt any part of the
arena, and no checksum would change that — it holds a writable mapping of the
same pages ([`PHASE2.md`](./docs/PHASE2.md) §3.10). Do not attach a process you
would not run as yourself. [`SECURITY.md`](./SECURITY.md) draws the line between
this and an actual vulnerability.

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
├── tf_tree_c/       C ABI + header-only C++ wrapper
├── tf_tree_bridge/  the ROS-independent half of the /tf ingest bridge
├── tf_tree_py/      PyO3 bindings — binds the Rust core directly, not the C ABI
├── tf_tree_bench/   criterion benches, tf2 differential harness, the §9 report
└── tf_tree_cli/     binary `tf_tree` (alias `tft`): tree / echo / doctor / participants / bench
xtask/               loom, miri, and bench-gate runners
docs/                PROJECT.md, PHASE1–5.md, benchmarks/, decisions/
```

`tf_tree_py` and `tf_tree_tf2_sys` are outside the `cargo --workspace` build on
purpose — they link libpython and a ROS 2 install respectively, neither of which
a clean checkout can assume. They have their own `just` recipes, and CI has jobs
for both (`python bindings compile`, `tf2 bridge (container)`).

Five crates are intended for crates.io: `tf_tree`, `tf_tree_core`,
`tf_tree_math`, `tf_tree_arena`, `tf_tree_ipc`. The rest carry `publish = false`
with the reason in their manifest.

## Commands

```sh
just build          # cargo build --workspace --all-targets
just test           # nextest + doctests
just lint           # fmt --check + clippy -D warnings + cargo-deny
just loom           # concurrency model checking
just miri           # UB checking (arena + core)
just bench          # benchmark suite + go/no-go gate
just bench-report    # the PHASE5 §9 artifact -> report/{results.json,index.html}
```

`just --list` for everything.

## Benchmarks, and what they are worth

`just bench-report` emits `report/results.json` and `report/index.html` with a
full provenance header. It is built so it **cannot** print a number it has no
right to: it probes the host, and a row it cannot measure fairly comes out
`UNAVAILABLE` with the reason and the command that produces it on a host that
can. On a 4-core development machine that means most rows are gaps — which is
the correct output, not a broken tool. `docs/PHASE5.md` §9.3 is the rule it
enforces, and it includes a "where `tf_tree` is worse" section, in the same
table.

Standing numbers and their caveats live in
[`docs/benchmarks/`](./docs/benchmarks/).

## Reading order

1. [`docs/PROJECT.md`](./docs/PROJECT.md) — overview, architecture, roadmap, and
   the decision log D1–D22 in §5.
2. The phase spec you care about: [`PHASE1`](./docs/PHASE1.md) …
   [`PHASE5`](./docs/PHASE5.md). Each opens with its own status table.
3. [`docs/decisions/`](./docs/decisions/) — the records for things the phase
   specs do not cover.

## Contributing and support

[`CONTRIBUTING.md`](./CONTRIBUTING.md) · [`SUPPORT.md`](./SUPPORT.md) (response
expectations, platform support, MSRV policy) ·
[`SECURITY.md`](./SECURITY.md) · [`CODE_OF_CONDUCT.md`](./CODE_OF_CONDUCT.md)

MSRV is **1.85**, enforced by a CI job that reads it from the manifest.

## Licence

Dual [MIT](./LICENSE-MIT) / [Apache-2.0](./LICENSE-APACHE), at your option. See
[`NOTICE`](./NOTICE).
