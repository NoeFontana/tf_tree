# tf_tree

[![crates.io](https://img.shields.io/crates/v/tf_tree.svg?logo=rust)](https://crates.io/crates/tf_tree)
[![docs.rs](https://img.shields.io/docsrs/tf_tree?logo=docsdotrs)](https://docs.rs/tf_tree)
[![PyPI](https://img.shields.io/pypi/v/transform_tree.svg?logo=pypi&logoColor=white)](https://pypi.org/project/transform_tree/)
[![CI](https://github.com/NoeFontana/tf_tree/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/NoeFontana/tf_tree/actions/workflows/ci.yml)
[![Licence](https://img.shields.io/badge/licence-MIT%20OR%20Apache--2.0-blue.svg)](#licence)

A transform tree engine. It stores time-stamped rigid-body transforms between
named coordinate frames and answers *"where was frame A relative to frame B at
time t?"* — from a control loop, from many processes at once, or from a
dataloader with no robot attached.

**It is not `tf2`, not a fork of it, and not affiliated with ROS.** It solves the
same problem with a different data structure. See
[Relationship to `tf2`](#relationship-to-tf2).

**Linux-first.** The single-process engine is portable Rust. Everything that maps
memory — attaching to a live arena, the frozen `.tft` backend, `tf_tree freeze` —
is Linux-only, behind a default-off `shm` feature.

## Install

| What | How | From |
|---|---|---|
| Rust engine | `cargo add tf_tree` | crates.io |
| Python bindings | `pip install transform_tree`, then `import tf_tree` (PyPI refused `tf_tree`; [`0008`](./docs/decisions/0008-the-name-tf-tree.md)) | PyPI |
| `tf_tree` CLI | a prebuilt Linux binary from [the latest release](https://github.com/NoeFontana/tf_tree/releases/latest) (`{x86_64, aarch64}` × `{gnu, musl}`, glibc 2.34+ for gnu, all with `--features shm`) | GitHub Releases |
| CLI from source | `cargo install --path crates/tf_tree_cli --features shm` (`cargo install tf_tree` installs no command; the CLI is `publish = false`) | a clone |
| C ABI, C++ header, ROS 2 bridge | `just c-abi-check`, `just cpp-check`, `just ros-build` | a clone |

**`0.0.x` promises nothing between releases.** Cargo treats every `0.0.x` as
incompatible with every other, so pin exactly; PEP 440 has no such rule, so pin
the wheel yourself ([`CHANGELOG.md`](./CHANGELOG.md)).

## Start from a recording you already have

No clone, no toolchain, and nothing on your robot changes.

```sh
# fetch and unpack tf_tree-<tag>-x86_64-unknown-linux-musl.tar.gz from the latest release
tft=./tf_tree-<tag>-x86_64-unknown-linux-musl/tf_tree

$tft doctor --from-bag drive.mcap                # what is wrong with this /tf traffic
$tft freeze --from-bag drive.mcap -o drive.tft   # keep the answer
```

From Python: `tf_tree.ingest_bag("drive.mcap").freeze("drive.tft")`.

`drive.tft` is a **frozen transform index**: the arena itself, with no pointers,
so opening one is an `mmap`. Sixteen dataloader workers map the same file, the
kernel charges the shared clean pages once, and each queries in its own address
space:

```python
import numpy as np, tf_tree

# Open per worker, after the fork/spawn — docs/PHASE5.md §4.3 says why.
tree   = tf_tree.open_file("drive.tft")
plan   = tree.plan("base_link", "lidar_top")           # compile the route once
stamps = np.asarray(batch_stamps_ns, dtype=np.int64)   # integer nanoseconds
poses  = plan.at(stamps, layout="quat_twist")          # (N, 13) float64
```

Ingestion is **MCAP only**; a `.db3` rosbag2 bag is diagnosed as one, with the
`ros2 bag convert` remedy.

## Start with no data at all

```sh
just quickstart        # uv-managed interpreter + venv, extension installed
.venv/bin/python
```

```python
import tf_tree

tree = tf_tree.build([("map", "base"), ("base", "cam")])
# stamp in integer nanoseconds; pose is [qw, qx, qy, qz, x, y, z]
tf_tree.push(tree, "base", "map", 1_000, [1.0, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0])
tf_tree.push(tree, "base", "map", 2_000, [1.0, 0.0, 0.0, 0.0, 3.0, 4.0, 5.0])

print(tree.plan("map", "base").at(1_500)[:3, 3])   # -> [2. 3. 4.]
```

The query lands halfway between two samples, so the printed translation is their
interpolated midpoint. `plan()` is the object to keep: compile the route once,
evaluate it many times. The same shape in Rust:

```rust
use tf_tree::{Capacity, EdgeCfg, Iso3, Quat, Stamp, TreeBuilder, Vec3};

// Topology is declared up front: `build()` sizes one flat arena from exactly
// these edges, and nothing allocates after it returns.
let tree = TreeBuilder::new()
    .static_edge("base_link", "lidar_top", &Iso3::IDENTITY)   // (parent, child)
    .dynamic_edge("odom", "base_link", EdgeCfg::new(Capacity::history(100.0, 10.0)))
    .build()
    .expect("layout");

let odom = tree.frame("odom").expect("declared");
let base_link = tree.frame("base_link").expect("declared");
let lidar_top = tree.frame("lidar_top").expect("declared");

// One writer per edge, enforced by the claim table. Note the order flips: the
// builder takes (parent, child), `claim` takes (child, parent). Getting it
// wrong builds a silently inverted tree rather than failing.
let w = tree.claim(base_link, odom).expect("unclaimed");
let at_x = |x| Iso3::new(Quat::IDENTITY, Vec3::new(x, 0.0, 0.0));
w.push(1_000_000_000, &at_x(0.0)).expect("monotonic");   // integer nanoseconds
w.push(1_010_000_000, &at_x(1.0)).expect("monotonic");

let plan = tree.plan(odom, lidar_top).expect("connected");
let g = tree.guard();
let t: Stamp = Stamp::from_nanos(1_005_000_000);   // the annotation names the domain
let pose = plan.at(&g, t).expect("in range");
assert!((pose.t.x - 0.5).abs() < 1e-12);
```

[`crates/tf_tree/README.md`](./crates/tf_tree/README.md) carries the annotated
version; `cargo test --doc` compiles both.

Two deliberate surprises: **stamps are integer nanoseconds** (no float-seconds
overload; at a 2026 epoch the ULP of `float64` seconds is 238 ns), and **nothing
returns a view into shared memory** (a ring another process overwrites, read
through a seqlock; `Plan.at_into` supplies the destination).

## When to use it, and when not

Reach for it for lookups **inside a loop with a deadline**, **many readers on one
host**, **transforms in a dataloader** without a ROS node, **kilohertz-class**
edges, or **debugging** a transform tree (typed errors, the `TFT001`–`TFT019`
catalogue via `tf_tree doctor` on a live arena, `.tft` or MCAP recording, and
`tf_tree top`). **Look elsewhere when:**

| You need | Why not this | Written down |
|---|---|---|
| A drop-in `tf2_ros::Buffer` | Phase 7, gated, not scheduled; only a one-way ingest bridge exists | [`PHASE7.md`](./docs/PHASE7.md) §0.0 |
| Covariance; multi-parent frames, loop closure, copy-on-write branches | Cut | [`0009`](./docs/decisions/0009-descoping-phase-6.md), [`PROJECT.md`](./docs/PROJECT.md) §5 D2 |
| Transforms across hosts | Phase 8. Not started | [`PROJECT.md`](./docs/PROJECT.md) §4 |
| Shared memory or `.tft` off Linux | The mapping code does not exist elsewhere | [`SUPPORT.md`](./SUPPORT.md) |
| A viewer, or point-cloud deskewing | Deliberately absent | [`PHASE5.md`](./docs/PHASE5.md) §8, [`PROJECT.md`](./docs/PROJECT.md) §5 D8 |
| An API that will not move under you | `0.0.x` | [`CHANGELOG.md`](./CHANGELOG.md) |

## Status

| Phase | What it is | Status |
|---|---|---|
| 1 | Single-process engine: arena, seqlock buffers, plans, SE(3) math | **Implemented** |
| 2 | Shared memory: rendezvous, fd passing, claims as leases, reaping | **Implemented**, with gaps |
| 3 | Python bindings (PyO3, zero intermediate allocation) | **Implemented** |
| 4 | C ABI, C++ wrapper, ROS 2 ingest bridge, derivatives | **Implemented**, with gaps |
| 5 | Frozen `.tft` arena, bag ingestion, diagnostics, `tf_tree top` | **Mostly implemented** |
| 6–8 | Continuous-time interpolation, `tf2` shim, multi-host replication | Not started |

**The per-phase `§0.0` tables are the source of truth**, not this one —
[`PHASE2`](./docs/PHASE2.md#00-implementation-status),
[`PHASE4`](./docs/PHASE4.md#00-implementation-status),
[`PHASE5`](./docs/PHASE5.md#00-implementation-status). Notably: Phase 2's daemon
and recorder surface (§9–§10) is absent and §3.5's ownership migration is
caller-driven ([`PHASE2.md` §3.5](./docs/PHASE2.md#35-ownership-migrates-the-data-plane-never-pauses--normative));
Phase 4's §1 exit criterion is operational and open; Phase 5 ingests MCAP only,
detects 17 of 19 catalogue ids, and deliberately omits §8; Phase 7 is gated by
D21 with no gate met. CI runs again as of 2026-08-16; gate locally with `just` first.

## Relationship to `tf2`

An independent engine, named so people looking for a `tf2` alternative can find it
([`0008`](./docs/decisions/0008-the-name-tf-tree.md)). **There is no drop-in
`tf2_ros::Buffer` shim and building one is not scheduled** (Phase 7, gated by D21;
[`PHASE7.md`](./docs/PHASE7.md)). What exists is the ingest bridge
(`docs/PHASE4.md` §5): a node that subscribes to `/tf` and fills an arena, one-way.

The documented `tf2` cost is listener and buffer CPU per node, not `/tf`
bandwidth: Autoware's `ManagedTransformBuffer` reports taking a LiDAR pipeline
from **13 TF listener nodes to 0**
([`autowarefoundation/autoware#5385`](https://github.com/autowarefoundation/autoware/issues/5385)) —
a third-party report, not a `tf_tree` measurement. Errors being `Copy` identifiers
([`API.md`](./docs/API.md) R5) is an API-shape difference, not a claim about a
`tf2` defect. `just bench-report` measures your host; standing figures are in
[`docs/benchmarks/`](./docs/benchmarks/).

## Shared memory is not a sandbox

Processes sharing an arena are **mutually trusting, same-user, cooperating
processes**: a read-write participant can corrupt any part of the arena
([`PHASE2.md`](./docs/PHASE2.md) §3.10). Do not attach a process you would not run
as yourself; [`SECURITY.md`](./SECURITY.md) draws the line against a vulnerability.
The design does guarantee that **a read-only participant cannot corrupt anything**
(the MMU; the consumer default, D18), that **a participant that crashes at any
instruction cannot corrupt the arena or wedge anyone else**, and that **a
participant that hangs is not mistaken for a crashed one** (liveness is a kernel
file lock, so a `SIGSTOP`ped publisher keeps its claims).

**`fork()` is the sharp edge.** The arena is mapped `MADV_DONTFORK`, so a child's
inherited handles report `ChildDetached`. Python's `multiprocessing` defaults to
`fork` on Linux — open inside the worker, or use `spawn`. A frozen `.tft` is the
exception: a child inherits it intact.

## Workspace

```text
crates/
├── tf_tree_math/    no_std SE(3)/SO(3) + dual quaternions; #![forbid(unsafe_code)]
├── tf_tree_arena/   no_std+alloc pointer-free arena + layout math
├── tf_tree_core/    no_std+alloc engine: interning, topology, seqlock buffers, plans
├── tf_tree/         std facade: builder, plan-cached lookup, Display errors
├── tf_tree_ipc/     zero-config rendezvous: runtime dir, OFD lock file, attach protocol
├── tf_tree_c/       C ABI + header-only C++ wrapper
├── tf_tree_bridge/  the ROS-independent half of the /tf ingest bridge
├── tf_tree_ingest/  MCAP -> arena: the passes behind `ingest` and `freeze --from-bag`
├── tf_tree_py/      PyO3 bindings — binds the Rust core directly, not the C ABI
├── tf_tree_bench/   criterion benches, tf2 differential harness, the §9 report
├── tf_tree_tf2_sys/ the tf2 side of that harness — needs a ROS 2 install
└── tf_tree_cli/     binary `tf_tree` (alias `tft`)
ros/                 ament_cmake packages: the §5 rclcpp bridge, and the DDS comparison
xtask/               loom, bench-gate, and headers runners
```

`tf_tree_py`, `tf_tree_tf2_sys` and `ros/` are outside the cargo workspace on
purpose (libpython; a ROS 2 install; not cargo) and have their own recipes
(`just py-*`, `just tf2-check`, `just ros-build`). Five crates publish: `tf_tree`,
`tf_tree_core`, `tf_tree_math`, `tf_tree_arena`, `tf_tree_ipc`; the rest carry
`publish = false`.

## Commands

`just --list` for everything; the usual ones are `quickstart`, `build`, `test`,
`lint`, `loom`, `miri`, `bench-report` (the PHASE5 §9 artifact; a row it cannot
measure fairly is `UNAVAILABLE` with the reason) and `bench-check` (against the
committed baseline, comparing **claims, not hosts**). Docs are mapped in
[`docs/README.md`](./docs/README.md).

## Contributing and support

[`CONTRIBUTING.md`](./CONTRIBUTING.md) · [`SUPPORT.md`](./SUPPORT.md) ·
[`SECURITY.md`](./SECURITY.md) · [`CODE_OF_CONDUCT.md`](./CODE_OF_CONDUCT.md)

MSRV is **1.87**; `just msrv` checks it against `[workspace.package] rust-version`.

## Licence

Dual [MIT](./LICENSE-MIT) / [Apache-2.0](./LICENSE-APACHE), at your option. See
[`NOTICE`](./NOTICE).
