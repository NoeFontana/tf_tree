# tf_tree

[![crates.io](https://img.shields.io/crates/v/tf_tree.svg?logo=rust)](https://crates.io/crates/tf_tree)
[![docs.rs](https://img.shields.io/docsrs/tf_tree?logo=docsdotrs)](https://docs.rs/tf_tree)
[![Licence](https://img.shields.io/badge/licence-MIT%20OR%20Apache--2.0-blue.svg)](#licence)

A transform tree engine: store time-stamped rigid-body transforms between named
coordinate frames and answer *"where was frame A relative to frame B at time
t?"* — from a control loop, from many processes at once, or offline.

**This is the crate to depend on**: the `std` facade over the `no_std` engine
([`tf_tree_core`](https://crates.io/crates/tf_tree_core)). **It is not `tf2` and
not affiliated with ROS**; there is no drop-in `tf2_ros::Buffer` shim.

## Install

```sh
cargo add tf_tree
```

Shared arenas, the frozen `.tft` reader and `tf_tree::open()` are behind the
default-off, **Linux-only** `shm` feature: `cargo add tf_tree --features shm`.
Python: `pip install transform_tree`.

## In full

```rust
use tf_tree::{Capacity, EdgeCfg, Iso3, Quat, Stamp, TreeBuilder, Vec3};

// Topology is declared up front; nothing allocates after `build()`.
let tree = TreeBuilder::new()
    .static_edge("base_link", "lidar_top", &Iso3::IDENTITY)
    .dynamic_edge("odom", "base_link", EdgeCfg::new(Capacity::history(100.0, 10.0)))
    .build()
    .expect("layout");

let odom = tree.frame("odom").expect("declared");
let base_link = tree.frame("base_link").expect("declared");
let lidar_top = tree.frame("lidar_top").expect("declared");

// One writer per edge, enforced by the claim table.
let w = tree.claim(base_link, odom).expect("unclaimed");   // (child, parent)
let at_x = |x| Iso3::new(Quat::IDENTITY, Vec3::new(x, 0.0, 0.0));
w.push(1_000_000_000, &at_x(0.0)).expect("monotonic");     // integer nanoseconds
w.push(1_010_000_000, &at_x(1.0)).expect("monotonic");

// Compile the route once, evaluate it many times.
let plan = tree.plan(odom, lidar_top).expect("connected");
let g = tree.guard();

// The annotation pins the default `SystemDomain`.
let t: Stamp = Stamp::from_nanos(1_005_000_000);
match plan.at(&g, t) {
    Ok(pose) => println!("x = {}", pose.t.x),          // -> x = 0.5
    Err(e) => println!("{}", tree.describe(e)),
}

let late: Stamp = Stamp::from_nanos(3_000_000_000);
match plan.at(&g, late) {
    Ok(_) => unreachable!(),
    // -> lookup on odom->base_link (edge#2) would extrapolate:
    //    requested 3000000000 ns, history [1000000000, 1010000000] ns
    Err(e) => println!("{}", tree.describe(e)),
}
```

**Stamps are integer nanoseconds and carry a domain in the type**; **`plan()` is
the object you keep** (`lookup()` by name caches one); **errors are `Copy`
identifiers naming the offending edge**, also `Display` and `std::error::Error`.

## Features

| Feature | Default | What it does |
|---|---|---|
| `counters` | **on** | Diagnostic counters; off does not fork the layout hash |
| `shm` | off | `TreeBuilder::build_shared`, `Tree::attach_shared`, `tf_tree::open()`, the frozen `.tft` reader. **Linux only** |
| `unstable` | off | Arena-shaped introspection (`tf_tree::unstable`, `Tree::arena_view`); **enabling it is the waiver** of semver |
| `test-hooks` | off | An injection point inside `Tree::claim` for the repository's tests. Not for shipped builds |

## Shared memory is not a sandbox

Processes sharing an arena are **mutually trusting, same-user, cooperating
processes**; a read-write participant can corrupt any part of the arena
([`SECURITY.md`](https://github.com/NoeFontana/tf_tree/blob/main/SECURITY.md)).
`fork()` is the sharp edge: a shared arena is mapped `MADV_DONTFORK`, so a child's
handles report `ChildDetached`; open inside the worker. A frozen `.tft` is
inherited intact.

## Version and docs

**`0.0.x` promises nothing**: pin exactly
([`CHANGELOG.md`](https://github.com/NoeFontana/tf_tree/blob/main/CHANGELOG.md)).
MSRV is **1.87**
([`SUPPORT.md`](https://github.com/NoeFontana/tf_tree/blob/main/SUPPORT.md)).

The CLI, C ABI and C++ wrapper, MCAP ingest, ROS 2 bridge and Python bindings are
built from source; `cargo install tf_tree` installs no command. See the repository
[`README.md`](https://github.com/NoeFontana/tf_tree/blob/main/README.md),
[`docs/PROJECT.md`](https://github.com/NoeFontana/tf_tree/blob/main/docs/PROJECT.md)
and [`docs/API.md`](https://github.com/NoeFontana/tf_tree/blob/main/docs/API.md).

## Licence

Dual [MIT](LICENSE-MIT) / [Apache-2.0](LICENSE-APACHE), at your option; see [`NOTICE`](NOTICE).
