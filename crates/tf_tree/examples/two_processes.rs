//! One arena, two processes: the seam between a publisher and a consumer that
//! find each other by name. One target with an argv switch.
//!
//! ```text
//! just two-processes          # spawns both halves and prints what each saw
//! ```
//!
//! ```text
//! publisher   Open::new()
//!               .mode(ReadWrite)
//!               .create(CreatePolicy::IfAbsent)
//!               .require_create(true)         -- refuse to join another's arena
//!               .layout_if_creating(builder)
//!
//! consumer    Open::new()                     -- ReadOnly, CreatePolicy::Never
//!               .await_open(timeout)          -- the publisher may not be up yet
//! ```
//!
//! Without `require_create(true)` a second publisher silently joins the first's
//! arena; with it, it gets `ArenaAlreadyLive`. `await_open` retries until the
//! timeout, so startup order does not matter and no daemon is needed
//! (`docs/decisions/0019`). The consumer prints the transform and
//! `Extrapolated::by_ns`, which is `0` only when every edge bracketed the query.

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

    /// One name for both halves (`$TF_TREE_ARENA` is the other way to say it).
    const ARENA: &str = "tf_tree_two_processes_example";
    const STEP_NS: i64 = 10_000_000; // 100 Hz

    fn publisher() {
        let tree = tf_tree::Open::new()
            .name(ARENA)
            .expect("arena name")
            .mode(AttachMode::ReadWrite)
            .create(CreatePolicy::IfAbsent)
            // Refuse to join an arena somebody else declared.
            .require_create(true)
            .layout_if_creating(
                TreeBuilder::new()
                    .dynamic_edge("map", "odom", EdgeCfg::new(Capacity::slots(256)))
                    .dynamic_edge("odom", "base_link", EdgeCfg::new(Capacity::slots(256)))
                    // A constant mount is a *static* edge: folded in at plan time.
                    // `exp_se3` takes `[ω, v]`, rotation first; the wrong order is silent.
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
            // A slow drift, so the answer is visibly a function of the stamp.
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
        // The publisher may not be up yet; this is why no daemon is needed (0019).
        let tree = tf_tree::Open::new()
            .name(ARENA)
            .expect("arena name")
            .await_open(Duration::from_secs(5))
            .expect("the publisher did not come up within 5 s");

        // Wait for history, not just the arena: `await_open` returns before anything
        // worth reading is published.
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

        // Compiled once, evaluated many times. The static mount becomes a
        // `Step::Static` (one multiply) rather than a ring search plus interpolation.
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
        // The newest stamp every dynamic edge can answer for; the slowest bounds a composed answer.
        let (_, newest) = plan
            .span(&g)
            .expect("span")
            .expect("the route has a retained window");
        for offset in [0, STEP_NS / 2, STEP_NS * 3] {
            // `Stamp<D>` carries its domain in the type (D9); these edges are tag 0.
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
        // No switch: be the harness, so this is one command.
        _ => {
            let exe = std::env::current_exe().expect("current exe");
            let mut pubr = std::process::Command::new(&exe)
                .arg("--publish")
                .spawn()
                .expect("spawn the publisher");
            // No sleep: `await_open` is the synchronisation.
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
