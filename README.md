# tf_tree

[![crates.io](https://img.shields.io/crates/v/tf_tree.svg?logo=rust)](https://crates.io/crates/tf_tree)
[![docs.rs](https://img.shields.io/docsrs/tf_tree?logo=docsdotrs)](https://docs.rs/tf_tree)
[![PyPI](https://img.shields.io/pypi/v/transform_tree.svg?logo=pypi&logoColor=white)](https://pypi.org/project/transform_tree/)
[![CI](https://github.com/NoeFontana/tf_tree/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/NoeFontana/tf_tree/actions/workflows/ci.yml)
[![Licence](https://img.shields.io/badge/licence-MIT%20OR%20Apache--2.0-blue.svg)](#licence)

A transform tree: it stores time-stamped poses between named frames and answers
"where was frame A relative to frame B at time t?". Use it in a control loop,
across processes on one host, or in a dataloader with no robot attached.

It is faster than ROS `tf2` (below) but is not `tf2`: no ROS dependency and no
drop-in `tf2_ros::Buffer`, only a one-way `/tf` ingest bridge
([`0008`](./docs/decisions/0008-the-name-tf-tree.md)). Linux first: shared memory
and frozen `.tft` files are Linux-only (`shm` feature); the in-process engine is
portable.

**`0.0.x` promises nothing between releases.** Pin exactly ([`CHANGELOG.md`](./CHANGELOG.md)).

## Install

- Rust: `cargo add tf_tree`
- Python: `pip install transform_tree`, then `import tf_tree`
- CLI (`tf_tree`, alias `tft`): a binary from the [latest release](https://github.com/NoeFontana/tf_tree/releases/latest), or `cargo install --path crates/tf_tree_cli --features shm`
- C ABI, C++ header, ROS 2 bridge: `just c-abi-check`, `just cpp-check`, `just ros-build`

## Use

Python, from scratch. Stamps are integer nanoseconds; a pose is `[qw, qx, qy, qz, x, y, z]`.

```python
import tf_tree

tree = tf_tree.build([("map", "base"), ("base", "cam")])
tf_tree.push(tree, "base", "map", 1_000, [1, 0, 0, 0, 1.0, 2.0, 3.0])
tf_tree.push(tree, "base", "map", 2_000, [1, 0, 0, 0, 3.0, 4.0, 5.0])

plan = tree.plan("map", "base")   # compile the route once, reuse it
print(plan.at(1_500)[:3, 3])      # [2. 3. 4.]
```

Rust:

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
let pose = plan.at(&g, Stamp::from_nanos(1_005_000_000)).expect("in range");
assert!((pose.t.x - 0.5).abs() < 1e-12);
```

From a recorded MCAP bag (`.db3`: run `ros2 bag convert` first):

```sh
tft doctor --from-bag drive.mcap                 # what is wrong with this /tf traffic
tft freeze --from-bag drive.mcap -o drive.tft    # index it
```
```python
tree = tf_tree.open_file("drive.tft")   # an mmap; open per worker, after fork/spawn
poses = tree.plan("base_link", "lidar_top").at(stamps_ns, layout="quat_twist")  # (N, 13)
```

## Against `tf2`

In-process, one thread, same interpolation, `tf2` called natively; 4-core EPYC-Milan,
not core-pinned. Both agree to 7e-15 on 50,000 recorded queries.

| | `tf_tree` | `tf2` | |
|---|---|---|---|
| Lookup, depth 3 | 94 ns | 253 ns | 2.7× |
| Lookup, depth 15 | 1193 ns | 7337 ns | 6.2× |
| Publish | 9.4 ns | 114 ns | 12.1× |

Without LTO the lookup gap is 1.8×; through the C ABI, 1.5×. Method:
[`tf2.md`](./docs/benchmarks/tf2.md); sources: [`EVIDENCE.md`](./docs/benchmarks/EVIDENCE.md).

## Not for

| Need | Why | See |
|---|---|---|
| Drop-in `tf2_ros::Buffer` | gated, not scheduled | [`PHASE7.md`](./docs/PHASE7.md) |
| Covariance, multi-parent frames, loop closure | cut | [`0009`](./docs/decisions/0009-descoping-phase-6.md) |
| Transforms across hosts | not started | [`PROJECT.md`](./docs/PROJECT.md) §4 |
| Shared memory or `.tft` off Linux | not implemented | [`SUPPORT.md`](./SUPPORT.md) |
| A viewer, point-cloud deskewing | deliberately absent | [`PHASE5.md`](./docs/PHASE5.md) §8 |

## Status

| Phase | | Status |
|---|---|---|
| 1 | Engine: arena, seqlock buffers, plans, SE(3) math | Implemented |
| 2 | Shared memory across processes | Implemented, with gaps |
| 3 | Python bindings | Implemented |
| 4 | C ABI, C++ wrapper, ROS 2 ingest, derivatives | Implemented, with gaps |
| 5 | Frozen `.tft`, bag ingestion, diagnostics, `tf_tree top` | Mostly implemented |
| 6–8 | Continuous time, `tf2` shim, multi-host | Not started |

The `§0.0` tables in [`PHASE2`](./docs/PHASE2.md#00-implementation-status),
[`PHASE4`](./docs/PHASE4.md#00-implementation-status) and
[`PHASE5`](./docs/PHASE5.md#00-implementation-status) win over this one.

## Shared memory is not a sandbox

Processes sharing an arena trust each other: a read-write participant can corrupt
it ([`PHASE2.md`](./docs/PHASE2.md) §3.10; [`SECURITY.md`](./SECURITY.md)). A
read-only participant (the default for readers) cannot, and a crashed or hung one
cannot wedge the others.

`fork()` is the sharp edge: a child's inherited handles report `ChildDetached`.
Python's `multiprocessing` forks by default on Linux, so open the arena inside the
worker or use `spawn`. A frozen `.tft` survives a fork.

## Layout

`crates/`: `tf_tree_math`, `tf_tree_arena`, `tf_tree_core`, `tf_tree` (facade),
`tf_tree_ipc`, `tf_tree_c`, `tf_tree_bridge`, `tf_tree_ingest`, `tf_tree_py`,
`tf_tree_bench`, `tf_tree_tf2_sys`, `tf_tree_cli`. `ros/` holds the ROS 2
packages. `tf_tree_py`, `tf_tree_tf2_sys` and `ros/` sit outside the cargo
workspace and have their own `just` recipes. Five crates publish: `tf_tree`,
`tf_tree_core`, `tf_tree_math`, `tf_tree_arena`, `tf_tree_ipc`. MSRV 1.87.

Docs: [`docs/README.md`](./docs/README.md) ·
[`CONTRIBUTING.md`](./CONTRIBUTING.md) · [`SUPPORT.md`](./SUPPORT.md) ·
[`SECURITY.md`](./SECURITY.md) · [`CODE_OF_CONDUCT.md`](./CODE_OF_CONDUCT.md)

## Licence

Dual [MIT](./LICENSE-MIT) / [Apache-2.0](./LICENSE-APACHE), at your option. See
[`NOTICE`](./NOTICE).
