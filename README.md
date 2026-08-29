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
| Python bindings | `pip install transform_tree`, then `import tf_tree` | PyPI |
| `tf_tree` CLI | a prebuilt Linux binary from [the latest release](https://github.com/NoeFontana/tf_tree/releases/latest) | GitHub Releases |
| CLI from source | `cargo install --path crates/tf_tree_cli --features shm` | a clone |
| C ABI, C++ header, ROS 2 bridge | `just c-abi-check`, `just cpp-check`, `just ros-build` | a clone |

- **The PyPI name is not the import name.** PyPI refused `tf_tree` as too close
  to the existing `tftree`, so the distribution is `transform_tree` and the
  module stays `tf_tree` ([`0008`](./docs/decisions/0008-the-name-tf-tree.md)).
- **`cargo install tf_tree` installs no command.** It exits 0 with a warning. The
  CLI is a separate crate that is `publish = false`, because three of its
  dependencies are path-only and have no version to publish against. Take the
  prebuilt binary or build from a clone.
- **Released CLI binaries cover `{x86_64, aarch64}` × `{gnu, musl}`.** The musl
  builds are static and have no glibc floor; the gnu builds need glibc 2.34 or
  newer (Ubuntu 22.04 / ROS 2 Humble and later) and pick up a system libc update
  without a new release. Both carry `--features shm`.
- **`0.0.x` promises nothing between releases.** Cargo treats every `0.0.x` as
  incompatible with every other, so pin exactly and expect a later release to
  break. PEP 440 has no such rule, so pin the wheel yourself
  ([`CHANGELOG.md`](./CHANGELOG.md)).

## Start from a recording you already have

No clone, no toolchain, and nothing on your robot changes.

```sh
TAG=$(curl -fsSL https://api.github.com/repos/NoeFontana/tf_tree/releases/latest \
      | grep -m1 '"tag_name"' | cut -d'"' -f4)
curl -fsSL "https://github.com/NoeFontana/tf_tree/releases/download/${TAG}/tf_tree-${TAG}-x86_64-unknown-linux-musl.tar.gz" | tar xz
tft=./tf_tree-${TAG}-x86_64-unknown-linux-musl/tf_tree

$tft doctor --from-bag drive.mcap                # what is wrong with this /tf traffic
$tft freeze --from-bag drive.mcap -o drive.tft   # keep the answer
```

The same two steps from Python, with no CLI:

```python
import tf_tree

tree = tf_tree.ingest_bag("drive.mcap")   # returns the ordinary Tree
tree.freeze("drive.tft")                  # records drive.mcap's BLAKE3 digest
```

`tree.source` carries that digest, so a `.tft` stays traceable to the recording
it came from. Taking a publisher on the tree clears it, because from that point
the tree may hold samples the recording does not.

`drive.tft` is a **frozen transform index**: the arena itself, written to disk.
The arena holds no pointers — every internal reference is an offset — so opening
one is an `mmap`, with no parsing and no fixups. Sixteen dataloader workers map
the same file, the kernel charges the shared clean pages once, and each worker
queries in its own address space:

```python
import numpy as np, tf_tree

# Open per worker, after the fork/spawn — docs/PHASE5.md §4.3 says why.
tree   = tf_tree.open_file("drive.tft")
plan   = tree.plan("base_link", "lidar_top")           # compile the route once
stamps = np.asarray(batch_stamps_ns, dtype=np.int64)   # integer nanoseconds
poses  = plan.at(stamps, layout="quat_twist")          # (N, 13) float64
```

Ingestion is **MCAP only**. A `.db3` rosbag2 bag is diagnosed as one, with the
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
evaluate it many times.

The same shape in Rust:

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
version, including what a failed lookup prints. That one is compiled by
`cargo test --doc`; this one is not, so treat the crate README as authoritative
if they ever disagree.

Two things surprise people, and both are deliberate:

- **Stamps are integer nanoseconds.** There is no float-seconds overload. At a
  2026 epoch the ULP of `float64` seconds is 238 ns, so every interval in a
  1 kHz stream is wrong after a round trip.
- **Nothing returns a view into shared memory.** An edge's samples are a ring
  another process is overwriting, and correct reads go through a seqlock. "Zero
  copy" means no *intermediate* allocation; use `Plan.at_into` to supply the
  destination.

## What you get

- **A compiled query.** `plan()` resolves the topology and folds static edges
  once. `at()` then does *d* binary searches, *d* interpolations and *d−1*
  compositions, and nothing else — no allocation, no hashing, no lock. Frames
  are interned to integer ids at plan time, so no lookup hashes a string.
- **One tree, not one copy per process.** The same arena bytes map into every
  reader: threads in a process, cooperating processes on a host, or dataloader
  workers on a frozen `.tft`. Consumers attach read-only by default, and the MMU
  enforces it.
- **Errors you can branch on.** Every error is a `Copy` identifier naming the
  offending edge as data rather than a formatted string. `tf_tree doctor` runs
  the `TFT001`–`TFT019` catalogue against a live arena, a frozen index, or an
  MCAP recording. Nineteen ids are reported; seventeen can detect, and the two
  that cannot say so rather than reporting a pass.

## When to use it, and when not

**Reach for it when:**

- You look up transforms **inside a loop with a deadline**, and per-lookup cost
  is something you have measured.
- **Many readers share one host** — one arena serves all of them, read-only by
  default, with no middleware between the reader and the bytes.
- You want **transforms in a dataloader** without a ROS node in the training
  loop, or a pickle of precomputed poses that cannot answer at an arbitrary time.
- Your edges are **kilohertz-class** and float-seconds stamps have cost you
  resolution.
- You need to **debug** a transform tree: typed errors, the catalogue,
  `tf_tree top`, and `doctor` against a bag with nothing deployed.

**Look elsewhere when:**

| You need | Why not this | Written down |
|---|---|---|
| A drop-in `tf2_ros::Buffer` | Phase 7, gated on operating evidence, not scheduled. What exists is a one-way ingest bridge | [`PHASE7.md`](./docs/PHASE7.md) §0.0 |
| Covariance or joint uncertainty | A tree cannot compose a correct one; composing marginals as independent is wrong in the optimistic direction. You need a factor graph | [`0009`](./docs/decisions/0009-descoping-phase-6.md) |
| Multi-parent frames, loop closure, copy-on-write branches | Cut: copy-on-write contradicts fixed capacity, one-writer-per-edge and append-only ids at once | [`0009`](./docs/decisions/0009-descoping-phase-6.md), [`PROJECT.md`](./docs/PROJECT.md) §5 D2 |
| Transforms across hosts | Phase 8. Not started | [`PROJECT.md`](./docs/PROJECT.md) §4 |
| Shared memory or `.tft` off Linux | The engine compiles; the mapping code does not exist elsewhere | [`SUPPORT.md`](./SUPPORT.md) |
| A viewer, or point-cloud deskewing | Deliberately absent. `at_adaptive` emits knots; the consumer transforms points where they already live | [`PHASE5.md`](./docs/PHASE5.md) §8, [`PROJECT.md`](./docs/PROJECT.md) §5 D8 |
| An API that will not move under you | `0.0.x`: every release may break every other | [`CHANGELOG.md`](./CHANGELOG.md) |

## Status

| Phase | What it is | Status |
|---|---|---|
| 1 | Single-process engine: arena, seqlock buffers, plans, SE(3) math | **Implemented** |
| 2 | Shared memory: rendezvous, fd passing, claims as leases, reaping | **Implemented**, with gaps |
| 3 | Python bindings (PyO3, zero intermediate allocation) | **Implemented** |
| 4 | C ABI, C++ wrapper, ROS 2 ingest bridge, derivatives | **Implemented**, with gaps |
| 5 | Frozen `.tft` arena, bag ingestion, diagnostics, `tf_tree top` | **Mostly implemented** |
| 6–8 | Continuous-time interpolation, `tf2` shim, multi-host replication | Not started |

The gaps:

- **Phase 2** — the daemon and recorder surface (§9–§10) are absent. §3.5's
  ownership migration works: kill the arena's owner and a surviving read-write
  participant inherits the role. Its trigger is caller-driven — `owner_lost()`
  is a non-blocking check a survivor makes in its own loop, and nothing makes it
  for you, because there is no daemon.
- **Phase 4** — everything except §5.9's affinity knobs and §6.3's replay rows.
  §1's exit criterion is **operational, not a feature list**: it is open, and no
  amount of code closes it.
- **Phase 5** — ingestion is MCAP only (§3). The catalogue reports all nineteen
  ids, of which seventeen can detect (§6). §8 is deliberately not built. §9's
  benchmark artifact and §10's release readiness are partial.
- **Phase 7** is gated by D21; none of its four gates is met.

**The per-phase `§0.0` tables in [`docs/`](./docs/) are the source of truth**, not
this one — [`PHASE2`](./docs/PHASE2.md#00-implementation-status),
[`PHASE4`](./docs/PHASE4.md#00-implementation-status),
[`PHASE5`](./docs/PHASE5.md#00-implementation-status). Where they disagree with
this table, they are right.

CI runs again as of 2026-08-16, after a gap since 2026-07-23 that ended when this
repository was made public. Gate locally with `just` first; CI is the second
opinion.

## Relationship to `tf2`

It is an independent engine, deliberately named so that people looking for a
`tf2` alternative can find it ([`0008`](./docs/decisions/0008-the-name-tf-tree.md)).

**There is no drop-in `tf2_ros::Buffer` shim and building one is not scheduled.**
That is Phase 7, gated by D21 on operating evidence this project has not produced.
[`PHASE7.md`](./docs/PHASE7.md) describes what such a shim would have to be,
including where it would deliberately refuse to reproduce `tf2`'s behaviour. What
exists today is the ingest bridge (`docs/PHASE4.md` §5): a node that subscribes
to `/tf` and fills an arena — one-way, not a compatibility layer.

Two claims stated precisely, because the loose versions are wrong:

- **The documented `tf2` cost is listener and buffer CPU per node, not `/tf`
  bandwidth.** Autoware's `ManagedTransformBuffer` reports taking a LiDAR
  pipeline from **13 TF listener nodes to 0**
  ([`autowarefoundation/autoware#5385`](https://github.com/autowarefoundation/autoware/issues/5385);
  upstream discussion at
  [`ros2/geometry2#758`](https://github.com/ros2/geometry2/issues/758)). That is
  a third-party report about their own stack, not a `tf_tree` measurement, cited
  for the shape of the problem. No claim is made about `/tf` bandwidth: no
  quantified public source exists.
- **Errors are `Copy` identifiers a program can branch on**, prose in a separate
  layer ([`API.md`](./docs/API.md) R5). That is a durable API-shape difference
  and *not* a claim about any `tf2` defect — the misattributed-extrapolation bug
  people cite
  ([`ros2/geometry2#832`](https://github.com/ros2/geometry2/issues/832)) was
  fixed by [PR #896](https://github.com/ros2/geometry2/pull/896), backported to
  Kilted, Jazzy and Humble.

Numbers belong where they can be reproduced: `just bench-report` measures your
host, and the standing figures and their caveats are in
[`docs/benchmarks/`](./docs/benchmarks/).

## Shared memory is not a sandbox

Processes sharing an arena are **mutually trusting, same-user, cooperating
processes**. A read-write participant can corrupt any part of the arena, and no
checksum would change that — it holds a writable mapping of the same pages
([`PHASE2.md`](./docs/PHASE2.md) §3.10). Do not attach a process you would not
run as yourself. [`SECURITY.md`](./SECURITY.md) draws the line between this and
an actual vulnerability.

Three things the design does guarantee:

- **A read-only participant cannot corrupt anything**, enforced by the MMU. It is
  the consumer default (D18), and it turns a class of whole-system failures into
  a single-process fault.
- **A participant that crashes, at any instruction, cannot corrupt the arena or
  wedge anyone else.** A killed writer's edge is reclaimed; a killed interner's
  entry is recoverable; a killed mutator leaves no permanently locked topology.
- **A participant that hangs is not mistaken for a crashed one.** Liveness is the
  kernel's answer about a file lock, not a heartbeat timeout, so a `SIGSTOP`ped
  publisher keeps its claims.

**`fork()` is the sharp edge.** The arena is mapped `MADV_DONTFORK`, so a child
has no mapping and every inherited handle reports `ChildDetached`. Python's
`multiprocessing` defaults to `fork` on Linux — open inside the worker, or use
`spawn`. A frozen `.tft` is the deliberate exception: it is a private read-only
mapping a child inherits intact.

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
├── tf_tree_ingest/  MCAP -> arena: the two passes behind `ingest` and `freeze --from-bag`
├── tf_tree_py/      PyO3 bindings — binds the Rust core directly, not the C ABI
├── tf_tree_bench/   criterion benches, tf2 differential harness, the §9 report
├── tf_tree_tf2_sys/ the tf2 side of that harness — needs a ROS 2 install
└── tf_tree_cli/     binary `tf_tree` (alias `tft`): tree / echo / doctor / top /
                     ingest / freeze / topology / participants / bench
ros/                 ament_cmake packages: the §5 rclcpp bridge, and the DDS comparison
xtask/               loom, miri, and bench-gate runners
docs/                PROJECT.md, API.md, PHASE1-5.md, RUNBOOK.md, benchmarks/, decisions/
```

`tf_tree_py` and `tf_tree_tf2_sys` are outside the cargo workspace on purpose —
they link libpython and a ROS 2 install respectively. `ros/` is outside it too
and is not cargo at all. Each has its own recipes (`just py-*`, `just ros-build`,
`just tf2-check`).

Five crates publish: `tf_tree`, `tf_tree_core`, `tf_tree_math`, `tf_tree_arena`,
`tf_tree_ipc`. The rest carry `publish = false` with the reason in their manifest.

## Commands

```sh
just quickstart     # clean clone -> a Python REPL with the extension installed
just build          # cargo build --workspace --all-targets
just test           # nextest + doctests
just lint           # fmt --check + clippy -D warnings
just loom           # concurrency model checking
just miri           # UB checking
just bench-report   # the PHASE5 §9 artifact -> report/{results.json,index.html}
just bench-check    # that artifact against the committed baseline
just shm-torture    # PHASE2 §11.4's multi-process soak (30 min; nightly)
```

`just --list` for everything.

`bench-report` cannot print a number it has no right to: it probes the host, and
a row it cannot measure fairly comes out `UNAVAILABLE` with the reason and the
command that would produce it elsewhere. On a 4-core development machine most
rows are gaps. `bench-check` compares **claims, not hosts** — CPU model, cores,
kernel, governor and load are ignored — so the gate means the same thing on any
machine.

## Documentation

| Document | What it is |
|---|---|
| [`API.md`](./docs/API.md) | The cross-cutting contract: six rules every binding obeys (§1), the normative Rust/Python/C/C++ surfaces (§2–§5), and the checklist a new surface passes (§7) |
| [`RUNBOOK.md`](./docs/RUNBOOK.md) | Organised by **symptom**. Every row names an error type and, where one exists, the `doctor` check that finds it |
| [`PROJECT.md`](./docs/PROJECT.md) | Overview, architecture, roadmap, and the decision log D1–D22 (§5). Several obvious-looking simplifications are excluded on purpose |
| [`PHASE1`](./docs/PHASE1.md)–[`PHASE5`](./docs/PHASE5.md) | The phase specs. `PHASE2`/`4`/`5`/`7` open with a `§0.0` status table that outranks every other document |
| [`decisions/`](./docs/decisions/) | Records for what the phase specs do not cover, and where a change of that kind starts |
| [`benchmarks/`](./docs/benchmarks/) | Standing measurements, each row naming the command that produced it |

## Contributing and support

[`CONTRIBUTING.md`](./CONTRIBUTING.md) · [`SUPPORT.md`](./SUPPORT.md) ·
[`SECURITY.md`](./SECURITY.md) · [`CODE_OF_CONDUCT.md`](./CODE_OF_CONDUCT.md)

MSRV is **1.87**. `just msrv` reads it from `[workspace.package] rust-version`,
builds `--locked` on exactly that toolchain, and checks that every hand-written
`rust-version` — and this line — still agrees.

## Licence

Dual [MIT](./LICENSE-MIT) / [Apache-2.0](./LICENSE-APACHE), at your option. See
[`NOTICE`](./NOTICE).
