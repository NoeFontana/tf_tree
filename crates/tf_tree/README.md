# tf_tree

[![crates.io](https://img.shields.io/crates/v/tf_tree.svg?logo=rust)](https://crates.io/crates/tf_tree)
[![docs.rs](https://img.shields.io/docsrs/tf_tree?logo=docsdotrs)](https://docs.rs/tf_tree)
[![Licence](https://img.shields.io/badge/licence-MIT%20OR%20Apache--2.0-blue.svg)](#licence)

A transform tree engine: store time-stamped rigid-body transforms between named
coordinate frames and answer *"where was frame A relative to frame B at time
t?"* — from a control loop, from many processes at once, or offline.

**This is the crate to depend on.** It is the `std` facade: it re-exports the
`no_std` engine ([`tf_tree_core`](https://crates.io/crates/tf_tree_core)) and
adds the allocating conveniences that do not belong in it — the builder, the
plan-cached `lookup`, and `Described`, the `Display` layer that turns a `Copy`
error id into prose by consulting the arena.

**It is not `tf2`, not a fork of it, and not affiliated with ROS.** It is an
independent engine that solves the same problem with a different data structure.
There is no drop-in `tf2_ros::Buffer` shim and building one is not scheduled.

## Install

```sh
cargo add tf_tree
```

That is the portable engine, and it is what the example below uses. Everything
that maps memory — a shared arena, the frozen `.tft` reader, `tf_tree::open()`'s
zero-config rendezvous — is behind the default-off `shm` feature and is **Linux
only**:

```sh
cargo add tf_tree --features shm
```

Python users want `pip install transform_tree` and `import tf_tree`: the
distribution name differs from the module because PyPI refuses `tf_tree` as too
close to the existing `tftree`. What `cargo install tf_tree` does — and does not
— install is in *Version* below, with the measurement; it is not repeated here.

## In full

```rust
use tf_tree::{Capacity, EdgeCfg, Iso3, Quat, Stamp, TreeBuilder, Vec3};

// Topology is declared up front: `build()` sizes one flat arena from exactly
// these edges, and nothing allocates after it returns.
let tree = TreeBuilder::new()
    .static_edge("base_link", "lidar_top", &Iso3::IDENTITY)
    .dynamic_edge("odom", "base_link", EdgeCfg::new(Capacity::history(100.0, 10.0)))
    .build()
    .expect("layout");

let odom = tree.frame("odom").expect("declared");
let base_link = tree.frame("base_link").expect("declared");
let lidar_top = tree.frame("lidar_top").expect("declared");

// One writer per edge, enforced by the claim table rather than by convention.
let w = tree.claim(base_link, odom).expect("unclaimed");   // (child, parent)
let at_x = |x| Iso3::new(Quat::IDENTITY, Vec3::new(x, 0.0, 0.0));
w.push(1_000_000_000, &at_x(0.0)).expect("monotonic");     // integer nanoseconds
w.push(1_010_000_000, &at_x(1.0)).expect("monotonic");

// Compile the route once and evaluate it many times: that is the whole shape of
// the fast path.
let plan = tree.plan(odom, lidar_top).expect("connected");
let g = tree.guard();

// `Stamp` carries its time domain in the type; the annotation pins the default
// `SystemDomain`, because method-call inference does not apply a type
// parameter's default.
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

Three things in there are deliberate and surprise people:

* **Stamps are integer nanoseconds, and they carry a domain in the type.** There
  is no float-seconds overload. At a 2026 epoch the ULP of `f64` seconds is
  238 ns, so every interval in a 1 kHz stream is wrong after a round trip.
* **`plan()` is the object you keep.** Compiling the route once and evaluating
  it many times is the fast path; `lookup()` by name is the convenience that
  caches a plan for you.
* **Errors are `Copy` identifiers that name the offending edge**, not formatted
  strings — a program branches on them, and the prose is a separate layer. They
  implement `Display` and `std::error::Error` as well, so `?` into
  `Box<dyn Error>` works and the identifier is still there to match on. The
  prose layer, `Described`, is what resolves a `FrameId` to the name a human
  reads.

## Linux-first

The single-process engine above is portable Rust. Everything that maps memory —
attaching to a live arena shared with other processes, the frozen `.tft`
backend — is **Linux-only and behind the default-off `shm` feature**.

## Features

| Feature | Default | What it does |
|---|---|---|
| `counters` | **on** | The diagnostic counters. Off removes the fields, the increments and the `Guard` destructor; the arena regions stay, so the layout hash does not fork and the two builds still attach to each other. |
| `shm` | off | Shared memory: `TreeBuilder::build_shared`, `Tree::attach_shared`, `tf_tree::open()`'s zero-config rendezvous, and the frozen `.tft` reader. **Linux only.** |
| `unstable` | off | Arena-shaped introspection (`tf_tree::unstable`, `Tree::arena_view`). **Enabling it is the waiver**: nothing reachable through it is covered by semver, because its shape follows an arena layout that is scheduled to change. |
| `test-hooks` | off | One injection point inside `Tree::claim`, for the repository's own reaper races. Not something a shipped build should carry. |

## Shared memory is not a sandbox

Processes sharing an arena are **mutually trusting, same-user, cooperating
processes**. A read-write participant holds a writable mapping of the same pages
and can corrupt any part of the arena; no checksum would change that. Three
things the design *does* guarantee, and they are the ones that matter on a
robot:

* **A read-only participant cannot corrupt anything**, enforced by the MMU
  rather than by convention. It is the default for consumers.
* **A participant that crashes, at any instruction, cannot corrupt the arena or
  wedge anyone else.** A killed writer's edge is reclaimed; a killed mutator
  does not leave a permanently locked topology.
* **A participant that hangs cannot be mistaken for a crashed one.** Liveness is
  the kernel's answer about a file lock, not a heartbeat timeout, so a
  `SIGSTOP`ped publisher keeps its claims.

`fork()` is the sharp edge worth knowing up front: a shared arena is mapped
`MADV_DONTFORK`, so a child has no mapping and every inherited handle reports
`ChildDetached`. Open inside the worker. A frozen `.tft` is the deliberate
exception — it is a private read-only mapping and a child inherits it intact.

## Version

**`0.0.x` promises nothing.** Cargo treats every `0.0.x` release as
incompatible with every other — `tf_tree = "0.0.1"` means `^0.0.1`, which
matches `0.0.1` and nothing else — so a later release reaches no existing
dependant through `cargo update`. That is the intended signal: pin exactly, and
expect a later release to break. The current number is deliberately not repeated
here — this line said `0.0.1` for three releases, because nothing gates a
version in prose. The reasoning is
written out in the repository's
[`Cargo.toml`](https://github.com/NoeFontana/tf_tree/blob/main/Cargo.toml) under
`[workspace.package] version`, and the release notes are in
[`CHANGELOG.md`](https://github.com/NoeFontana/tf_tree/blob/main/CHANGELOG.md).

MSRV is **1.87**; see
[`SUPPORT.md`](https://github.com/NoeFontana/tf_tree/blob/main/SUPPORT.md) for
the policy, the response expectations, and what "supported platform" currently
means.

Not everything in the repository ships to crates.io: the `tf_tree` CLI, the C
ABI and C++ wrapper, the MCAP ingest, the ROS 2 bridge and the Python bindings
are all built from source. `cargo install tf_tree` installs no command, and it
does not fail either: it exits 0 after `warning: none of the package's binaries
are available for install using the selected features`, naming `--features shm`
as what the one bin target here needs. That target is
`tf_tree_rendezvous_child`, the helper `tests/rendezvous.rs` spawns to prove a
*second process* joins the arena, so `cargo install tf_tree --features shm` does
put it in your `bin/`. It is not a tool and nothing about it is stable. It
carries the crate's name because `rendezvous_child` is not a name this crate
should own in a shared `bin/`; it installs at all because every way to install
*nothing* trades a compile-time guarantee in that test for a path resolved at
run time. `Cargo.toml` has the measurement.

## Where the rest of it is

* [`docs/PROJECT.md`](https://github.com/NoeFontana/tf_tree/blob/main/docs/PROJECT.md)
  — overview, architecture, roadmap, and the decision log.
* [`docs/API.md`](https://github.com/NoeFontana/tf_tree/blob/main/docs/API.md) —
  the cross-cutting contract: six rules every binding obeys.
* [`README.md`](https://github.com/NoeFontana/tf_tree/blob/main/README.md) — the
  offline story (bag → frozen `.tft` → `mmap` per dataloader worker), the
  benchmark policy, and what is and is not implemented.

## Licence

Dual [MIT](LICENSE-MIT) / [Apache-2.0](LICENSE-APACHE), at your option. See
[`NOTICE`](NOTICE).
