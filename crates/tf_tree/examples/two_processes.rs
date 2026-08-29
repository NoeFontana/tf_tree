//! **One arena, two processes** — the capability `README.md` leads with, as
//! something you can run.
//!
//! ```text
//! just two-processes          # spawns both halves and prints what each saw
//! ```
//!
//! `control_loop.rs` is the shape of a node's *inner loop*, and its reader is a
//! thread — deliberately, because that example is about latency and a thread
//! keeps the measurement about the fold. This one is about the **seam**: what a
//! publisher writes, what a consumer opens, and how each finds the other from
//! nothing but a name.
//!
//! It is one target with an argv switch rather than two, so it stays one `just`
//! recipe and one thing to keep compiling.
//!
//! # What each half does, and why in that order
//!
//! ```text
//! publisher   Open::new()
//!               .mode(ReadWrite)              -- it writes; ReadOnly is the default
//!               .create(CreatePolicy::IfAbsent)
//!               .require_create(true)         -- refuse to *join* somebody else's
//!               .layout_if_creating(builder)  -- the topology, only used when creating
//!
//! consumer    Open::new()                     -- ReadOnly, CreatePolicy::Never
//!               .await_open(timeout)          -- the publisher may not be up yet
//! ```
//!
//! **`require_create(true)` is the half a newcomer gets wrong.** Without it a
//! publisher that races a second copy of itself silently *joins* the other one's
//! arena and publishes into a topology it did not declare. With it, the second
//! copy is refused with `ArenaAlreadyLive`, which is a supervisor's problem
//! rather than a silent one.
//!
//! **`await_open` rather than `open`,** because startup order is not something a
//! launch file guarantees. A consumer that opens too early gets `ArenaAbsent`;
//! `await_open` retries with backoff until the timeout, which is the whole
//! reason `docs/decisions/0019` argues a daemon is not needed for this.
//!
//! # What it prints, and what to look at
//!
//! The consumer reports the transform it read and how far past the newest sample
//! it had to reach. That second number is the point: it is
//! `Extrapolated::by_ns`, and it is `0` only when every edge on the route
//! actually bracketed the query.

// An example's stdout IS its output, and its panics are its assertions.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr
)]

#[cfg(all(feature = "shm", target_os = "linux"))]
fn main() {
    use std::time::Duration;

    use tf_tree::{
        AttachMode, Capacity, CreatePolicy, EdgeCfg, ExtrapPolicy, Iso3, Stamp, Step, SystemDomain,
        TreeBuilder,
    };

    /// One name for both halves. A robot with two trees gives them two names;
    /// `$TF_TREE_ARENA` is the other way to say it.
    const ARENA: &str = "tf_tree_two_processes_example";
    const STEP_NS: i64 = 10_000_000; // 100 Hz

    fn publisher() {
        let tree = tf_tree::Open::new()
            .name(ARENA)
            .expect("arena name")
            .mode(AttachMode::ReadWrite)
            .create(CreatePolicy::IfAbsent)
            // Refuse to join an arena somebody else declared: publishing into a
            // topology this process did not write is the failure that presents
            // as "the transform is subtly wrong" three subsystems away.
            .require_create(true)
            .layout_if_creating(
                TreeBuilder::new()
                    .dynamic_edge("map", "odom", EdgeCfg::new(Capacity::slots(256)))
                    .dynamic_edge("odom", "base_link", EdgeCfg::new(Capacity::slots(256)))
                    // A sensor mount is a constant, so it is a *static* edge: it
                    // folds into the plan at compile time and costs the query
                    // path nothing, forever. Declaring it dynamic would make it
                    // a ring somebody has to publish into for the life of the
                    // robot.
                    // **`exp_se3` takes `[ω, v]` — rotation first, translation
                    // last.** Getting that backwards is silent: the transform
                    // is well-formed, it is just not the one you meant. The
                    // first version of this example put its drift in the
                    // rotation slots and printed three identical zero
                    // translations, which is how it was found.
                    .static_edge(
                        "base_link",
                        "lidar",
                        &tf_tree::exp_se3([0.0, 0.0, 0.0, 0.12, 0.0, 0.31]),
                    ),
            )
            .open()
            .expect("create the arena");

        let odom = tree.frame("odom").expect("odom");
        let map = tree.frame("map").expect("map");
        let base = tree.frame("base_link").expect("base_link");

        let a = tree.claim(odom, map).expect("claim map->odom");
        let b = tree.claim(base, odom).expect("claim odom->base_link");

        println!("publisher: arena `{ARENA}` created, publishing 100 samples at 100 Hz");
        for k in 0..100i64 {
            let t = k * STEP_NS;
            // A slow drift, so the consumer's answer is visibly a function of
            // the stamp rather than a constant.
            a.push(
                t,
                &tf_tree::exp_se3([0.0, 0.0, 0.0, 0.01 * k as f64, 0.0, 0.0]),
            )
            .expect("push map->odom");
            b.push(
                t,
                &tf_tree::exp_se3([0.0, 0.0, 0.0, 0.0, 0.002 * k as f64, 0.0]),
            )
            .expect("push odom->base_link");
            std::thread::sleep(Duration::from_millis(2));
        }
        println!("publisher: done; holding the arena open for the consumer");
        std::thread::sleep(Duration::from_millis(500));
    }

    fn consumer() {
        // The publisher may not be up yet, and a launch file does not promise
        // otherwise. This is the answer to that, and it is why no daemon is
        // needed to pre-declare a topology (`docs/decisions/0019`).
        let tree = tf_tree::Open::new()
            .name(ARENA)
            .expect("arena name")
            .await_open(Duration::from_secs(5))
            .expect("the publisher did not come up within 5 s");

        // **Wait for history, not just for the arena.** `await_open` returns as
        // soon as the publisher exists, which is usually before it has published
        // anything worth reading — the first thing this example printed was
        // three identical identity poses, because the consumer had won the race
        // and was reading sample zero. A consumer that needs a *window* rather
        // than an attachment has to say so; this is the smallest way to say it.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let g = tree.guard();
            let wide_enough = tree
                .frame("lidar")
                .ok()
                .zip(tree.frame("map").ok())
                .and_then(|(l, m)| tree.plan(l, m).ok())
                .and_then(|p| p.span(&g).ok().flatten())
                .is_some_and(|(oldest, newest)| newest - oldest >= 50 * STEP_NS);
            if wide_enough || std::time::Instant::now() > deadline {
                break;
            }
            drop(g);
            std::thread::sleep(Duration::from_millis(5));
        }

        let lidar = tree.frame("lidar").expect("lidar");
        let map = tree.frame("map").expect("map");
        let plan = tree.plan(lidar, map).expect("compile map -> lidar");

        // Compiled once, evaluated many times: this is the object to keep.
        // `plan.edges()` is two, not three — the static mount folded in.
        // **What the static mount costs, said precisely.** It does not vanish
        // from the plan — it becomes a `Step::Static`, a constant composed
        // directly — so `plan.len()` is still 3 for a three-edge route. What it
        // costs at *evaluation* is the difference: a `Step::Dyn` is a ring
        // search, a seqlock read and an interpolation, and a `Step::Static` is
        // one multiply. And nobody ever has to publish the mount.
        let dynamic = plan
            .steps()
            .iter()
            .filter(|s| matches!(s, Step::Dyn { .. }))
            .count();
        println!(
            "consumer:  attached; route map -> lidar is {} step(s), {dynamic} of them sampled",
            plan.len()
        );

        // One guard per cycle, covering every query the cycle makes.
        let g = tree.guard();
        // The newest stamp *every* dynamic edge on the route can answer for —
        // the slowest edge is what bounds a composed answer, which is why this
        // is a fold and not a max.
        let (_, newest) = plan
            .span(&g)
            .expect("span")
            .expect("the route has a retained window");
        for offset in [0, STEP_NS / 2, STEP_NS * 3] {
            // `Stamp<D>` carries its time domain in the type (D9). These edges
            // are tag 0, so `SystemDomain` — naming it is what makes a domain
            // mistake a compile error rather than a wrong transform.
            let t: Stamp<SystemDomain> = Stamp::from_nanos(newest + offset);
            match plan.at_extrapolating(&g, t, ExtrapPolicy::ConstantTwist) {
                Ok(e) => {
                    let d: Iso3 = e.pose;
                    println!(
                        "consumer:  t = newest{:+9} ns -> x {:+.4} y {:+.4} z {:+.4}   \
                         extrapolated by {} ns",
                        offset, d.t.x, d.t.y, d.t.z, e.by_ns
                    );
                }
                Err(err) => println!("consumer:  t = newest{offset:+9} ns -> refused: {err}"),
            }
        }
    }

    match std::env::args().nth(1).as_deref() {
        Some("--publish") => publisher(),
        Some("--consume") => consumer(),
        // No switch: be the harness. Spawning both halves is what makes this
        // one command rather than two terminals and a race.
        _ => {
            let exe = std::env::current_exe().expect("current exe");
            let mut pubr = std::process::Command::new(&exe)
                .arg("--publish")
                .spawn()
                .expect("spawn the publisher");
            // No sleep here: `await_open` in the consumer is the synchronisation,
            // and demonstrating that is half the point.
            let cons = std::process::Command::new(&exe)
                .arg("--consume")
                .status()
                .expect("run the consumer");
            let _ = pubr.wait();
            assert!(cons.success(), "the consumer failed");
        }
    }
}

#[cfg(not(all(feature = "shm", target_os = "linux")))]
fn main() {
    eprintln!("this example needs --features shm on Linux");
}
