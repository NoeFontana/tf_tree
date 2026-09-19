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

**It is not `tf2`, not a fork of it, and not affiliated with ROS**
([`0008`](./docs/decisions/0008-the-name-tf-tree.md)); no drop-in
`tf2_ros::Buffer` shim exists, only a one-way `/tf` ingest bridge
(`docs/PHASE4.md` §5).

**Linux-first.** The single-process engine is portable Rust; attaching to a live
arena, the frozen `.tft` backend and `tf_tree freeze` are Linux-only, behind the
default-off `shm` feature.

## Install

| What | How | From |
|---|---|---|
| Rust engine | `cargo add tf_tree` | crates.io |
| Python bindings | `pip install transform_tree`, then `import tf_tree` (PyPI refused `tf_tree`; [`0008`](./docs/decisions/0008-the-name-tf-tree.md)) | PyPI |
| `tf_tree` CLI | a prebuilt Linux binary from [the latest release](https://github.com/NoeFontana/tf_tree/releases/latest) (`{x86_64, aarch64}` × `{gnu, musl}`, glibc 2.34+ for gnu, all with `--features shm`) | GitHub Releases |
| CLI from source | `cargo install --path crates/tf_tree_cli --features shm` (the CLI is `publish = false`) | a clone |
| C ABI, C++ header, ROS 2 bridge | `just c-abi-check`, `just cpp-check`, `just ros-build` | a clone |

**`0.0.x` promises nothing between releases**: pin exactly, crate and wheel
([`CHANGELOG.md`](./CHANGELOG.md)).

## Start from a recording you already have

```sh
# unpack tf_tree-<tag>-x86_64-unknown-linux-musl.tar.gz from the latest release
tft=./tf_tree-<tag>-x86_64-unknown-linux-musl/tf_tree
$tft doctor --from-bag drive.mcap                # what is wrong with this /tf traffic
$tft freeze --from-bag drive.mcap -o drive.tft   # keep the answer
```

From Python: `tf_tree.ingest_bag("drive.mcap").freeze("drive.tft")`. A `.tft` is a
**frozen transform index**: opening one is an `mmap`, shared by dataloader workers:

```python
import numpy as np, tf_tree

# Open per worker, after the fork/spawn — docs/PHASE5.md §4.3 says why.
tree   = tf_tree.open_file("drive.tft")
plan   = tree.plan("base_link", "lidar_top")           # compile the route once
stamps = np.asarray(batch_stamps_ns, dtype=np.int64)   # integer nanoseconds
poses  = plan.at(stamps, layout="quat_twist")          # (N, 13) float64
```

Ingestion is **MCAP only**; convert a `.db3` bag with `ros2 bag convert`.

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

The query lands halfway between two samples. Keep the `plan()` and evaluate it
many times. The same shape in Rust
([`crates/tf_tree/README.md`](./crates/tf_tree/README.md) has the annotated version):

```rust
use tf_tree::{Capacity, EdgeCfg, Iso3, Quat, Stamp, TreeBuilder, Vec3};

let tree = TreeBuilder::new()
    .static_edge("base_link", "lidar_top", &Iso3::IDENTITY)   // (parent, child)
    .dynamic_edge("odom", "base_link", EdgeCfg::new(Capacity::history(100.0, 10.0)))
    .build()
    .expect("layout");
let odom = tree.frame("odom").expect("declared");
let base_link = tree.frame("base_link").expect("declared");
let lidar_top = tree.frame("lidar_top").expect("declared");

let w = tree.claim(base_link, odom).expect("unclaimed");   // (child, parent)
let at_x = |x| Iso3::new(Quat::IDENTITY, Vec3::new(x, 0.0, 0.0));
w.push(1_000_000_000, &at_x(0.0)).expect("monotonic");
w.push(1_010_000_000, &at_x(1.0)).expect("monotonic");

let plan = tree.plan(odom, lidar_top).expect("connected");
let g = tree.guard();
let t: Stamp = Stamp::from_nanos(1_005_000_000);
let pose = plan.at(&g, t).expect("in range");
assert!((pose.t.x - 0.5).abs() < 1e-12);
```

**Stamps are integer nanoseconds**, and **nothing returns a view
into shared memory** (`Plan.at_into` supplies the destination).

## When to use it, and when not

Use it for lookups in a deadline loop, many readers on one host, dataloader
transforms without a ROS node, kilohertz-class edges, or debugging a tree
(`tf_tree doctor`, `tf_tree top`, `TFT001`–`TFT019`). **Look elsewhere when:**

| You need | Why not this | Written down |
|---|---|---|
| A drop-in `tf2_ros::Buffer` | Phase 7, gated, not scheduled; only a one-way ingest bridge exists | [`PHASE7.md`](./docs/PHASE7.md) §0.0 |
| Covariance; multi-parent frames, loop closure, copy-on-write branches | Cut | [`0009`](./docs/decisions/0009-descoping-phase-6.md), [`PROJECT.md`](./docs/PROJECT.md) §5 D2 |
| Transforms across hosts | Phase 8. Not started | [`PROJECT.md`](./docs/PROJECT.md) §4 |
| Shared memory or `.tft` off Linux | The mapping code does not exist elsewhere | [`SUPPORT.md`](./SUPPORT.md) |
| A viewer, or point-cloud deskewing | Deliberately absent | [`PHASE5.md`](./docs/PHASE5.md) §8, [`PROJECT.md`](./docs/PROJECT.md) §5 D8 |

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
[`PHASE5`](./docs/PHASE5.md#00-implementation-status).

## Shared memory is not a sandbox

Processes sharing an arena are **mutually trusting, same-user, cooperating
processes**: a read-write participant can corrupt any part of the arena
([`PHASE2.md`](./docs/PHASE2.md) §3.10); [`SECURITY.md`](./SECURITY.md) draws the
line against a vulnerability. A **read-only participant cannot corrupt anything**
(the consumer default, D18), and a crashed or hung participant cannot corrupt the
arena or wedge anyone else.

**`fork()` is the sharp edge.** The arena is mapped `MADV_DONTFORK`, so a child's
inherited handles report `ChildDetached`. Python's `multiprocessing` defaults to
`fork` on Linux — open inside the worker, or use `spawn`. A frozen `.tft` is the
exception: a child inherits it intact.

## Workspace

`crates/` holds `tf_tree_math`, `tf_tree_arena`, `tf_tree_core`, `tf_tree` (std
facade), `tf_tree_ipc`, `tf_tree_c` (C ABI + C++ wrapper), `tf_tree_bridge`,
`tf_tree_ingest`, `tf_tree_py`, `tf_tree_bench`, `tf_tree_tf2_sys` and
`tf_tree_cli` (binary `tf_tree`, alias `tft`); `ros/` the ament_cmake packages.
`tf_tree_py`, `tf_tree_tf2_sys` and `ros/` are outside the cargo workspace (own
recipes: `just py-*`, `just tf2-check`, `just ros-build`). Five crates publish:
`tf_tree`, `tf_tree_core`, `tf_tree_math`, `tf_tree_arena`, `tf_tree_ipc`.

## Contributing and support

[`CONTRIBUTING.md`](./CONTRIBUTING.md) · [`SUPPORT.md`](./SUPPORT.md) ·
[`SECURITY.md`](./SECURITY.md) · [`CODE_OF_CONDUCT.md`](./CODE_OF_CONDUCT.md) ·
docs map: [`docs/README.md`](./docs/README.md). MSRV is **1.87**.

## Licence

Dual [MIT](./LICENSE-MIT) / [Apache-2.0](./LICENSE-APACHE), at your option. See
[`NOTICE`](./NOTICE).
