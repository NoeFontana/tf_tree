# tf_tree

[![crates.io](https://img.shields.io/crates/v/tf_tree.svg?logo=rust)](https://crates.io/crates/tf_tree)
[![docs.rs](https://img.shields.io/docsrs/tf_tree?logo=docsdotrs)](https://docs.rs/tf_tree)
[![PyPI](https://img.shields.io/pypi/v/transform_tree.svg?logo=pypi&logoColor=white)](https://pypi.org/project/transform_tree/)
[![CI](https://github.com/NoeFontana/tf_tree/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/NoeFontana/tf_tree/actions/workflows/ci.yml)
[![Licence](https://img.shields.io/badge/licence-MIT%20OR%20Apache--2.0-blue.svg)](#licence)

A transform tree engine: store time-stamped rigid-body transforms between named
coordinate frames and answer *"where was frame A relative to frame B at time
t?"* — fast enough to sit inside a control loop, with diagnostics good enough to
debug at 3 a.m.

**Jump to:** [Install](#install) · [Start with a bag you already
have](#start-where-you-change-nothing) · [First five
minutes](#first-five-minutes-with-no-data-at-all) · [Where it
fits](#where-it-fits-and-where-it-does-not) · [Status](#status) · [Is this a
`tf2` replacement?](#is-this-a-tf2-replacement)

## What you get

- **A compiled query.** `plan()` resolves the topology and folds static edges
  once. `at()` then does *d* binary searches, *d* interpolations and *d−1*
  compositions, and nothing else — no allocation, no hashing, no lock. Frames
  are interned to integer ids at plan time, so no lookup hashes a string.
- **One tree, not one copy per process.** The arena holds no pointers — every
  internal reference is an offset — so the same bytes map into every reader:
  threads in one process, cooperating processes on a host, or sixteen dataloader
  workers on a frozen `.tft` file. Consumers attach read-only by default, and
  the MMU is what enforces it, not convention.
- **Failures you can act on.** Every error is a `Copy` identifier that names the
  offending edge as data rather than as a formatted string, and `tf_tree doctor`
  runs the `TFT001`–`TFT019` catalogue against a live arena, a frozen index, or
  an MCAP recording you already have. Nineteen ids are reported; seventeen can
  detect today, and the two that cannot say so rather than reporting a pass.

**Linux-first.** The single-process engine is portable Rust and much of it
compiles elsewhere; everything that maps memory — attaching to a live arena, the
frozen `.tft` backend, `tf_tree freeze` — is **Linux-only and behind a
default-off `shm` feature**. That sentence is here rather than in
[`SUPPORT.md`](./SUPPORT.md) alone because nobody should meet it as a build
error.

## Install

| What you want | How to get it | Where it comes from |
|---|---|---|
| The Rust engine | `cargo add tf_tree` | crates.io |
| The Python bindings | `pip install transform_tree`, then `import tf_tree` | PyPI |
| The `tf_tree` CLI | `cargo install --path crates/tf_tree_cli --features shm`, from a clone | source; the CLI is `publish = false` |
| C ABI, C++ header, ROS 2 bridge | `just c-abi-check`, `just cpp-check`, `just ros-build` | source |

Three notes on that table, each of which surprises somebody:

- **The distribution name is not the import name.** PyPI refuses `tf_tree` as
  too close to the existing `tftree`, so the wheel is `transform_tree` and the
  module stays `tf_tree` ([`0008`](./docs/decisions/0008-the-name-tf-tree.md)
  records the measurement).
- **`cargo add tf_tree` gives you the portable engine.** Shared memory and the
  frozen `.tft` reader need `--features shm`, on Linux.
- **`cargo install tf_tree` installs no command**, and does not fail either:
  it exits 0 with a warning naming `--features shm`. Adding that flag *does*
  install `tf_tree_rendezvous_child`, which is a test helper and not a tool —
  [the crate's own page](./crates/tf_tree/README.md) has the whole story. The
  CLI is a separate, unpublished crate; build it from a checkout as above.

**`0.0.x` promises nothing between releases.** Cargo treats every `0.0.x` as
incompatible with every other, so pin exactly and expect a later release to
break. That is the whole promise; see [`CHANGELOG.md`](./CHANGELOG.md).

## Start where you change nothing

Point it at a recording you already have. No node joins anyone's launch file, no
robot is redeployed, and `doctor --from-bag` needs no features at all:

```sh
git clone https://github.com/NoeFontana/tf_tree && cd tf_tree
cargo install --path crates/tf_tree_cli --features shm   # shm: `freeze` maps memory

tf_tree doctor --from-bag drive.mcap                # what is wrong with this /tf traffic
tf_tree freeze --from-bag drive.mcap -o drive.tft   # keep the answer
```

`drive.tft` is a **frozen transform index**, and it is the arena itself written
to disk. There are no pointers anywhere in the arena — every internal reference
is an offset — so opening one is an `mmap`, with no parsing, no deserialization
and no fixups ([`PHASE5`](./docs/PHASE5.md) §2.1). Sixteen dataloader workers map
the same file, the kernel charges the shared clean pages exactly once and the
untouched ones not at all (measured basis: [`PHASE2`](./docs/PHASE2.md) §3.8),
and each worker queries in its own address space: no IPC, and no ROS node inside
the training loop.

```python
import numpy as np, tf_tree

# Open per worker, after the fork/spawn — docs/PHASE5.md §4.3 says why.
tree   = tf_tree.open_file("drive.tft")
plan   = tree.plan("base_link", "lidar_top")           # compile the route once
stamps = np.asarray(batch_stamps_ns, dtype=np.int64)   # integer nanoseconds
poses  = plan.at(stamps, layout="quat_twist")          # (N, 13) float64
```

The whole batch is one call into Rust. `layout="quat_twist"` appends the body
twist to each pose, in the plan's source frame: it is the analytic derivative of
the same interpolation the pose came from, not a finite difference between two
lookups.

[`docs/PHASE5.md`](./docs/PHASE5.md) §2.2 is where that argument is made in the
project's own words. A perception dataloader today does one of three bad things
— re-parses the bag in every worker, precomputes poses into a pickle and loses
the ability to query at arbitrary times, or runs a ROS node to serve transforms
during training. This replaces all three and asks nobody to migrate anything,
which is also why it is the part that shipped first.

**Running it inside a loop instead?** `just control-loop` is the other half of
this page — `cargo run --release -p tf_tree --features shm --example control_loop`.
It is a 1 kHz controller against a 200 Hz estimate, showing the four things a
runtime consumer has to get right (compile the plan once, hoist the guard,
extrapolate on purpose and read how far it reached, treat a contended slot as
data) and printing the tail a deadline is set against. The offline path above
asks nobody to change their robot; this one is what happens when they do.

**Numbers belong where they can be reproduced**, not in this section.
`just bench-report` measures your host and writes
`report/{results.json,index.html}`; the standing figures and their caveats are in
[`docs/benchmarks/`](./docs/benchmarks/). *Benchmarks, and what they are worth*
below explains why a row there may legitimately read `UNAVAILABLE`.

## First five minutes, with no data at all

The block above assumes you have a recording. This one assumes only the clone:

```sh
just quickstart        # uv-managed interpreter + venv, with the extension installed into it
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

That is a real result and not a toy: the query lands halfway between two samples,
so the printed translation is their interpolated midpoint, and `plan()` is the
object you keep — compiling the route once and evaluating it many times is the
whole shape of the fast path.

The same shape in Rust, which is where the engine actually lives. This block is
compiled by `cargo test --doc`, so it cannot drift away from the API:

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

// One writer per edge, enforced by the claim table rather than by convention.
// Note the order flips: the builder takes (parent, child), `claim` takes
// (child, parent). Both are annotated here because getting it wrong builds a
// silently inverted tree rather than failing.
let w = tree.claim(base_link, odom).expect("unclaimed");   // (child, parent)
let at_x = |x| Iso3::new(Quat::IDENTITY, Vec3::new(x, 0.0, 0.0));
w.push(1_000_000_000, &at_x(0.0)).expect("monotonic");     // integer nanoseconds
w.push(1_010_000_000, &at_x(1.0)).expect("monotonic");

// Compile the route once, evaluate it many times.
let plan = tree.plan(odom, lidar_top).expect("connected");
let g = tree.guard();
let t: Stamp = Stamp::from_nanos(1_005_000_000);
let pose = plan.at(&g, t).expect("in range");
assert!((pose.t.x - 0.5).abs() < 1e-12);
```

[`crates/tf_tree/README.md`](./crates/tf_tree/README.md) carries the annotated
version, including what a failed lookup prints. Two things surprise people about
both languages, and both are deliberate:

- **Stamps are integer nanoseconds.** There is no float-seconds overload. At a
  2026 epoch the ULP of `float64` seconds is 238 ns, so every interval in a
  1 kHz stream is wrong after a round trip.
- **Nothing returns a view into shared memory.** An edge's samples are a ring
  another process is overwriting, and correct reads go through a seqlock. "Zero
  copy" here means no *intermediate* allocation — use `Plan.at_into` to supply
  the destination.

`just` alone lists everything the repository can do.

## Where it fits, and where it does not

The fastest way to evaluate this is to find yourself on one of these two lists.
The second one is not a roadmap: most of its rows are decisions, recorded, that
will not be reversed.

**Reach for `tf_tree` when:**

- You look up transforms **inside a loop that has a deadline** — a controller, a
  perception front end — and the per-lookup cost is a thing you have measured.
- **Many readers share one host**: threads in a process, or several processes.
  One arena serves all of them, read-only by default, with no middleware between
  the reader and the bytes.
- You train on recorded data and want **transforms in the dataloader** without a
  ROS node in the training loop, or a pickle of precomputed poses that can no
  longer answer at an arbitrary time.
- Your edges are **fast**, kilohertz-class, and float-seconds stamps have
  already cost you resolution.
- You need to **debug** a transform tree — typed errors that name the edge, the
  `TFT001`–`TFT019` catalogue, `tf_tree top`, and `doctor` against a bag with
  nothing deployed.

**Look elsewhere when:**

| You need | Why not this | Where it is written down |
|---|---|---|
| A drop-in `tf2_ros::Buffer` | Phase 7, gated on operating evidence, not scheduled. What exists is a one-way ingest bridge | [`PHASE7.md`](./docs/PHASE7.md) §0.0 |
| Covariance or joint uncertainty | A tree cannot compose a correct one; composing marginals as independent is wrong in the optimistic direction. You need a factor graph | [`PROJECT.md`](./docs/PROJECT.md) §1, [`0009`](./docs/decisions/0009-descoping-phase-6.md) |
| Multi-parent frames, loop closure, copy-on-write branches | Multi-parent is the row above. Copy-on-write was cut for reasons of its own: it serves the use case D2 rejects *and* contradicts fixed capacity, one-writer-per-edge and append-only ids at once | [`0009`](./docs/decisions/0009-descoping-phase-6.md), [`PROJECT.md`](./docs/PROJECT.md) §5 D2 |
| Transforms across hosts | Phase 8. Not started | [`PROJECT.md`](./docs/PROJECT.md) §4 |
| Shared memory or `.tft` off Linux | The engine compiles; the mapping code does not exist elsewhere | [`SUPPORT.md`](./SUPPORT.md) |
| A viewer, or point-cloud deskewing | Deliberately absent, argument recorded. `at_adaptive` emits knots; the consumer transforms points where they already live | [`PHASE5.md`](./docs/PHASE5.md) §8, [`PROJECT.md`](./docs/PROJECT.md) §5 D8 |
| An API that will not move under you | `0.0.x`: every release may break every other | [`CHANGELOG.md`](./CHANGELOG.md) |

## Status

**On crates.io from 0.0.1; on PyPI from 0.0.2.** The five engine crates are
published — `cargo add tf_tree`. The Python wheel starts at 0.0.2 because the
0.0.1 commit did not compile off Linux, so no wheel for it exists or can; see
[`CHANGELOG.md`](./CHANGELOG.md).

| Phase | What it is | Status |
|---|---|---|
| 1 | Single-process engine: arena, seqlock buffers, plans, SE(3) math | **Implemented** |
| 2 | Shared memory: rendezvous, fd passing, claims as leases, reaping | **Implemented**, with gaps |
| 3 | Python bindings (PyO3, zero intermediate allocation) | **Implemented** |
| 4 | C ABI, C++ wrapper, ROS 2 ingest bridge, derivatives | **Implemented**, with gaps |
| 5 | Frozen `.tft` arena, bag ingestion, diagnostics, `tf_tree top` | **Mostly implemented** |
| 6–8 | Continuous-time interpolation, `tf2` shim, multi-host replication | Not started |

The gaps, named — because "with gaps" on its own is not a status:

- **Phase 2** — the daemon and recorder surface (§9–§10) are absent, and
  §11.3's fault injection is a separate gap still being worked. §3.5's ownership
  migration **landed on 2026-08-28**: kill the arena's owner and a surviving
  read-write participant inherits the role, so new processes can join again. Its
  trigger is caller-driven — `Tree::owner_lost()` is a non-blocking check a
  survivor makes in its own loop, and nothing makes it for you, because there is
  no daemon.
- **Phase 4** — everything except §5.9's affinity knobs and §6.3's replay rows.
  `at_with_derivatives`, both headers, the header-only C++ wrapper with its
  CMake package, and **both halves of the ingest bridge** — the `rclcpp` package
  in `ros/tf_tree_ros` included. §7's benchmark gate is partial, and §1's exit
  criterion is **operational, not a feature list**: it is open, and no amount of
  code closes it.
- **Phase 5** — `FORMAT_VERSION = 3`, the frozen arena (§2), the offline Python
  API (§4), the §5 counters and `tf_tree top` (terminal *and* `--web`, §7) have
  all landed. Ingestion is **MCAP only** (§3). The `TFT001`–`TFT019` catalogue
  reports all nineteen ids, of which seventeen can detect (§6). §8 is
  **deliberately not built**. §9's benchmark artifact and §10's release
  readiness are partial.
- **Phase 7** is gated by D21 and none of its four gates is met.

**The per-phase `§0.0` tables in [`docs/`](./docs/) are the source of truth**, not
this one — [`PHASE2.md`](./docs/PHASE2.md#00-implementation-status),
[`PHASE4.md`](./docs/PHASE4.md#00-implementation-status),
[`PHASE5.md`](./docs/PHASE5.md#00-implementation-status). If this table and one
of those disagree, the phase document is right and this is stale.

**CI runs again as of 2026-08-16**, after a gap since 2026-07-23 that ended
when this repository was made public. A green check is evidence once more — of
what the jobs cover. Gate locally with `just` first; CI is the second opinion.

## Is this a `tf2` replacement?

**It is not `tf2`, not a fork of it, and not affiliated with ROS.** It is an
independent engine that solves the same problem with a different data structure,
and it is deliberately named so that people looking for a `tf2` alternative can
find it ([`0008`](./docs/decisions/0008-the-name-tf-tree.md) records that
decision).

**There is no drop-in `tf2_ros::Buffer` shim, and building one is not scheduled.**
That is Phase 7, gated by D21 on operating evidence this project has not yet
produced; [`docs/PHASE7.md`](./docs/PHASE7.md) is what such a shim would have to
be — including the places it would deliberately *refuse* to reproduce `tf2`'s
behaviour — and its §0.0 lists four gates, none of them met. What exists today
instead is the ingest bridge (`docs/PHASE4.md` §5): a node that subscribes to
`/tf` and fills an arena, which is a one-way seam and not a compatibility layer.

Two things worth stating precisely, because the loose versions of both are wrong:

- **The documented `tf2` cost is listener and buffer CPU, per node — not `/tf`
  bandwidth.** Autoware's `ManagedTransformBuffer` (in `autoware_universe`)
  reports taking a LiDAR sensing pipeline from **13 TF listener nodes to 0** —
  four per-sensor legs at 3 each plus the concatenation node
  ([`autowarefoundation/autoware#5385`](https://github.com/autowarefoundation/autoware/issues/5385);
  the upstream discussion about moving it into `tf2` is
  [`ros2/geometry2#758`](https://github.com/ros2/geometry2/issues/758)). That is a
  **third-party report about their own stack**, not a `tf_tree` measurement, and
  it is cited for the *shape* of the problem this engine's process model
  addresses. No claim is made about `/tf` bandwidth: no quantified public source
  for one exists.
- **Errors are `Copy` identifiers a program can branch on**, with the prose in a
  separate layer ([`docs/API.md`](./docs/API.md) R5) — a lookup failure names the
  offending edge as data, not as a formatted string. That is a durable
  API-shape difference and deliberately *not* a claim about any particular `tf2`
  defect; the misattributed-extrapolation one people cite
  ([`ros2/geometry2#832`](https://github.com/ros2/geometry2/issues/832)) was
  **fixed** by [PR #896](https://github.com/ros2/geometry2/pull/896), merged
  2026-03-18 and backported to Kilted, Jazzy and Humble (#897–#899). Marketing
  against a bug somebody already fixed is how a README goes stale in public.

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
inside the worker, or use the `spawn` start method. A frozen `.tft` is the
deliberate exception: it is a private read-only mapping, a child inherits it
intact, and poisoning it would break `multiprocessing` for offline users to
defend against a hazard they do not have (`docs/PHASE5.md` §4.3).

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
├── tf_tree_tf2_sys/ the tf2 side of that differential harness — needs a ROS 2 install
└── tf_tree_cli/     binary `tf_tree` (alias `tft`): tree / echo / doctor / top /
                     ingest / freeze / topology / participants / bench
ros/                 ament_cmake packages: the §5 rclcpp bridge, and the DDS comparison
xtask/               loom, miri, and bench-gate runners
docs/                PROJECT.md, API.md, PHASE1–5.md, RUNBOOK.md, benchmarks/, decisions/
```

`tf_tree_py` and `tf_tree_tf2_sys` are outside the `cargo --workspace` build on
purpose — they link libpython and a ROS 2 install respectively, neither of which
a clean checkout can assume. `ros/` is outside it for the same reason and is not
cargo at all. Each has `just` recipes of its own (`just py-*`, `just ros-build` /
`just ros-test`, `just tf2-check`), which is what "gate locally" means for them.

Five crates are intended for crates.io: `tf_tree`, `tf_tree_core`,
`tf_tree_math`, `tf_tree_arena`, `tf_tree_ipc`. The rest carry `publish = false`
with the reason in their manifest.

## Commands

```sh
just quickstart     # clean clone -> a Python REPL with the extension installed
just build          # cargo build --workspace --all-targets
just test           # nextest + doctests
just lint           # fmt --check + clippy -D warnings
just loom           # concurrency model checking
just miri           # UB checking (arena + core + the facade's one unsafe)
just bench          # benchmark suite + go/no-go gate
just bench-report   # the PHASE5 §9 artifact -> report/{results.json,index.html}
just bench-check    # the same artifact against the committed baseline
just shm-torture    # PHASE2 §11.4's multi-process soak (30 min; nightly)
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

`just bench-check` re-runs it and compares against the committed baseline in
`crates/tf_tree_bench/baseline/results.json`, failing if a claim was withdrawn,
a row was dropped, the arena layout changed, or a directional number moved past
the slack the baseline itself records. It compares **claims, not hosts**: CPU
model, core count, kernel, governor, load and every reason string are ignored,
so the gate means the same thing on any machine. `just bench-baseline-update`
regenerates the baseline; that diff belongs in the commit that causes it.

Standing numbers and their caveats live in
[`docs/benchmarks/`](./docs/benchmarks/).

## Where to go next

**Using it.**

1. [`docs/API.md`](./docs/API.md) — the cross-cutting contract: six rules (§1)
   every binding obeys, and the normative Rust, Python, C and C++ surfaces
   (§2–§5). This is the reference.
2. [`docs/RUNBOOK.md`](./docs/RUNBOOK.md) — organised by **symptom**, because
   that is what you have when a robot's transform tree misbehaves. Every row
   names an error type and, where one exists, the `doctor` check that finds it.
3. [`docs/benchmarks/`](./docs/benchmarks/) — the standing measurements, each
   row naming the command that produced it.

**Changing it.**

1. [`docs/PROJECT.md`](./docs/PROJECT.md) — overview, architecture, roadmap, and
   the decision log D1–D22 in §5. Read it before proposing anything: several
   obvious-looking simplifications are excluded on purpose, and the reasons are
   there.
2. The phase spec you are touching: [`PHASE1`](./docs/PHASE1.md) …
   [`PHASE5`](./docs/PHASE5.md). `PHASE2`, `PHASE4`, `PHASE5` and `PHASE7` open
   with a `§0.0` status table, and it outranks every other document including
   this one. `PHASE1` has none because Phase 1 is implemented whole, and
   `PHASE3` has none because it records deviations inline, in the section each
   belongs to.
3. [`docs/API.md`](./docs/API.md) again, and **before** writing any public
   surface — §1's six rules generate every binding, and §7 is the checklist a
   new surface passes. It authorizes nothing on its own: its §6 delta table
   names the phase or decision record each row lands in.
4. [`docs/decisions/`](./docs/decisions/) — the records for what the phase specs
   do not cover, and where a change of that kind starts.
5. [`CONTRIBUTING.md`](./CONTRIBUTING.md) — the local gates, and the order to
   run them in.

## Contributing and support

[`CONTRIBUTING.md`](./CONTRIBUTING.md) · [`SUPPORT.md`](./SUPPORT.md) (response
expectations, platform support, MSRV policy) ·
[`SECURITY.md`](./SECURITY.md) · [`CODE_OF_CONDUCT.md`](./CODE_OF_CONDUCT.md)

MSRV is **1.87**. `just msrv` reads the number out of `[workspace.package]
rust-version`, builds `--locked` on exactly that toolchain, and checks that
every hand-written `rust-version` — and this line — still agrees with it.

## Licence

Dual [MIT](./LICENSE-MIT) / [Apache-2.0](./LICENSE-APACHE), at your option. See
[`NOTICE`](./NOTICE).
