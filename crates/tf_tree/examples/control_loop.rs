//! A 1 kHz control loop against a 200 Hz state estimate: the shape of a node's
//! inner loop, not a benchmark.
//!
//! ```sh
//! cargo run --release -p tf_tree --features shm --example control_loop
//! ```
//!
//! It fills `docs/API.md` §8.4's missing executor for the tail: it runs the query
//! under a concurrent writer and reports p99.9 (`docs/PHASE1.md` §11.2). It is not
//! the §11.3 gate, which needs core-pinned hardware.
//!
//! 1. **Compile the plan once**, outside the loop (R1, D3); `Tree::lookup` is the
//!    convenience tier.
//! 2. **One `Guard` per cycle**, covering every query in it.
//! 3. **Ask past the newest sample.** A 1 kHz controller on a 200 Hz estimator
//!    always extrapolates; `ExtrapPolicy::ConstantTwist` extends the last twist and
//!    `Extrapolated::by_ns` is the number to gate on. The slowest edge on the route
//!    sets it: `map -> lidar` also crosses the 10 Hz edge, so a budget belongs to a route.
//! 4. **`SlotContended` is data** (`docs/API.md` §8.2): a writer was mid-publish;
//!    reuse the previous pose rather than block.
//! 5. **Never allocate inside the loop**; the histogram is pre-sized.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

fn main() {
    #[cfg(all(feature = "shm", target_os = "linux"))]
    run();
    #[cfg(not(all(feature = "shm", target_os = "linux")))]
    println!("control_loop needs `--features shm` on Linux; nothing was measured");
}

#[cfg(all(feature = "shm", target_os = "linux"))]
fn run() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use tf_tree::{
        Capacity, EdgeCfg, ExtrapPolicy, InterpPolicy, LookupError, Stamp, SystemDomain,
        TreeBuilder,
    };

    /// The controller's rate.
    const CONTROL_HZ: u64 = 1_000;
    /// The estimator's rate; extrapolation is the ordinary case.
    const ESTIMATE_HZ: u64 = 200;
    /// Long enough for a p99.9 to mean something.
    const CYCLES: usize = 200_000;
    /// Per-route extrapolation budget before the controller must degrade; staleness
    /// is set by the slowest edge on the route.
    const FAST_BUDGET_NS: i64 = 10_000_000; // 10 ms, over the 200 Hz route
    const FULL_BUDGET_NS: i64 = 150_000_000; // 150 ms, over the 10 Hz one

    // Declare the topology once. Capacities are per edge and sized in time
    // (`Capacity::history(rate, secs)`): how long a consumer may lag before the ring laps it.
    let tree = Arc::new(
        TreeBuilder::new()
            .default_interp(InterpPolicy::ScLerp)
            .dynamic_edge(
                "map",
                "odom",
                EdgeCfg::new(Capacity::history(10.0, 5.0)).nominal_rate_hz(10.0),
            )
            .dynamic_edge(
                "odom",
                "base_link",
                EdgeCfg::new(Capacity::history(ESTIMATE_HZ as f64, 2.0))
                    .nominal_rate_hz(ESTIMATE_HZ as f64),
            )
            // Static: folded into a constant at plan time, free in the loop.
            .static_edge(
                "base_link",
                "lidar",
                &tf_tree::exp_se3([0.0, 0.0, 0.0, 0.12, 0.0, 0.31]),
            )
            .build_shared("tf_tree_control_loop_example")
            .expect("create the shared arena"),
    );

    let stop = Arc::new(AtomicBool::new(false));
    // One clock origin for writer and loop; separate `Instant::now()`s would make
    // `by_ns` measure startup skew.
    let t0 = Instant::now();

    // ---- the estimator, publishing on its own thread ----------------------
    let writer = {
        let tree = Arc::clone(&tree);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            // Same `t0` as the control loop.
            let odom = tree.frame("odom").unwrap();
            let map = tree.frame("map").unwrap();
            let base = tree.frame("base_link").unwrap();
            let slow = tree.claim(odom, map).expect("claim map->odom");
            let fast = tree.claim(base, odom).expect("claim odom->base_link");

            let period = Duration::from_nanos(1_000_000_000 / ESTIMATE_HZ);
            let mut n: i64 = 0;
            while !stop.load(Ordering::Relaxed) {
                let t = t0.elapsed().as_nanos() as i64;
                // A smooth screw so `ConstantTwist` has something to extend.
                let s = t as f64 * 1e-9;
                fast.push(
                    t,
                    &tf_tree::exp_se3([0.0, 0.0, 0.4 * s, 1.2 * s, 0.3 * s, 0.0]),
                )
                .ok();
                if n % (ESTIMATE_HZ as i64 / 10) == 0 {
                    slow.push(t, &tf_tree::exp_se3([0.0, 0.0, 0.01 * s, 0.0, 0.0, 0.0]))
                        .ok();
                }
                n += 1;
                std::thread::sleep(period);
            }
        })
    };

    // Wait until both edges have the two samples `ConstantTwist` needs.
    let deadline = Instant::now() + Duration::from_secs(5);
    let (map, lidar) = (tree.frame("map").unwrap(), tree.frame("lidar").unwrap());
    let odom = tree.frame("odom").unwrap();
    let plan = loop {
        let p = tree.plan(map, lidar).expect("compile the plan");
        let g = tree.guard();
        if p.at(&g, Stamp::<SystemDomain>::from_nanos(0)).is_ok() || p.latest_common(&g).is_ok() {
            break p;
        }
        assert!(Instant::now() < deadline, "the estimator never published");
        std::thread::sleep(Duration::from_millis(5));
    };
    // The same route minus the 10 Hz edge. Compiled once, like the other.
    let fast_plan = tree.plan(odom, lidar).expect("compile the fast plan");

    // Everything above happens once; everything below allocates nothing.
    let mut lat_ns: Vec<u64> = Vec::with_capacity(CYCLES); // pre-sized: see §5
    let mut stale_fast: Vec<i64> = Vec::with_capacity(CYCLES);
    let mut stale_full: Vec<i64> = Vec::with_capacity(CYCLES);
    let mut contended = 0usize;
    let mut too_stale = 0usize;
    let mut no_data = 0usize;
    let mut last_good = tf_tree::Iso3::IDENTITY;

    let period = Duration::from_nanos(1_000_000_000 / CONTROL_HZ);
    let mut next = Instant::now();

    for _ in 0..CYCLES {
        next += period;
        // The stamp is now, past the newest sample by up to one estimator period.
        let now_ns = t0.elapsed().as_nanos() as i64;
        let t = Stamp::<SystemDomain>::from_nanos(now_ns);

        // One guard for both transforms.
        let started = Instant::now();
        let g = tree.guard();
        let fast = fast_plan.at_extrapolating(&g, t, ExtrapPolicy::ConstantTwist);
        let full = plan.at_extrapolating(&g, t, ExtrapPolicy::ConstantTwist);
        // Both queries and the guard, timed together.
        lat_ns.push(started.elapsed().as_nanos() as u64);

        for (answer, budget, stale) in [
            (fast, FAST_BUDGET_NS, &mut stale_fast),
            (full, FULL_BUDGET_NS, &mut stale_full),
        ] {
            match answer {
                Ok(e) => {
                    stale.push(e.by_ns);
                    if e.by_ns > budget {
                        // Degrade: `by_ns` makes the staleness judgeable.
                        too_stale += 1;
                    } else {
                        last_good = e.pose;
                    }
                }
                // Bounded worst case, not a failure: reuse the previous pose.
                Err(LookupError::SlotContended { .. }) => contended += 1,
                // Before the estimator's first samples, or after it dies.
                Err(LookupError::NoData { .. } | LookupError::Extrapolation { .. }) => {
                    no_data += 1;
                }
                // A real fault; `Tree::describe` resolves the edge id to names.
                Err(other) => println!("unexpected: {}", tree.describe(other)),
            }
        }
        let _ = last_good;

        if let Some(sleep) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(sleep);
        }
    }

    stop.store(true, Ordering::Relaxed);
    writer.join().ok();

    // ---- what a deadline is set against -----------------------------------
    lat_ns.sort_unstable();
    let pct = |p: f64| lat_ns[((lat_ns.len() - 1) as f64 * p) as usize];
    let max_of = |v: &[i64]| v.iter().copied().max().unwrap_or(0) as f64 / 1e6;

    println!("tf_tree control loop: {CYCLES} cycles at {CONTROL_HZ} Hz against a {ESTIMATE_HZ} Hz estimate");
    println!("  route            odom -> lidar ({} steps) and map -> lidar ({} steps), after static folding",
        fast_plan.len(), plan.len());
    println!("  per-cycle cost   two queries under one guard:");
    println!(
        "                   p50 {} ns   p99 {} ns   p99.9 {} ns   max {} ns",
        pct(0.50),
        pct(0.99),
        pct(0.999),
        lat_ns[lat_ns.len() - 1]
    );
    println!(
        "  extrapolated by  odom -> lidar  max {:.2} ms (budget {:.0} ms)",
        max_of(&stale_fast),
        FAST_BUDGET_NS as f64 / 1e6
    );
    println!("                   map  -> lidar  max {:.2} ms (budget {:.0} ms)  <- the 10 Hz edge sets this",
        max_of(&stale_full), FULL_BUDGET_NS as f64 / 1e6);
    println!("  slot contended   {contended} of {} queries", 2 * CYCLES);
    println!("  over budget      {too_stale} queries");
    println!("  no data          {no_data} queries");
    println!();
    println!("  Read the tail, not the p50 — a control loop misses a deadline on the max.");
    println!("  Two caveats on these numbers, both of which inflate them:");
    println!("    * the timing calls bracket a ~sub-microsecond operation, so the clock is");
    println!("      a visible share of the p50;");
    println!("    * this host is not core-pinned and runs no real-time scheduler, so the max");
    println!("      is dominated by preemption rather than by the engine.");
    println!("  docs/PHASE1.md §11.3's gate is the pinned-hardware measurement; this is not it.");
}
