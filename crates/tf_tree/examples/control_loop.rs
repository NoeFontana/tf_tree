//! A 1 kHz control loop against a 200 Hz state estimate — the runtime path, end
//! to end, in the order a node actually does it.
//!
//! ```sh
//! cargo run --release -p tf_tree --features shm --example control_loop
//! ```
//!
//! # Why this file exists
//!
//! The pitch is *"fast enough to sit inside a control loop"*, and until this
//! example there was nothing to copy. The README's worked example is an offline
//! dataloader — deliberately, because that is the adoption wedge that asks
//! nobody to change their robot — but it shows none of the discipline a runtime
//! consumer needs, and every one of those disciplines is a place to get it
//! wrong. So this is the other half: **read it as the shape of a node's inner
//! loop, not as a benchmark.**
//!
//! It also carries the second job `docs/API.md` §8.4 admits is unfilled. That
//! section states the real-time envelope and then says plainly that only the
//! allocation claim has an executor. This one runs the query under a concurrent
//! writer and reports the **tail**, which is the number a deadline is set
//! against — `docs/PHASE1.md` §11.2: *"p99.9 is the number that matters, not the
//! mean."* It is not the §11.3 gate, which needs core-pinned hardware; it is an
//! honest reading on whatever host runs it.
//!
//! # The five things this shows, and why each one is a trap
//!
//! 1. **Compile the plan once, outside the loop** (R1, D3). `Tree::lookup`
//!    resolves names on every call and is the convenience tier; a hot loop that
//!    uses it is paying for a hash and a topology walk per cycle.
//! 2. **Hoist the `Guard`** — one per *cycle*, not one per query. A control
//!    cycle usually needs several transforms, and one guard covers all of them:
//!    it pins a topology generation, so they see one consistent view and pay the
//!    validation once. This loop asks for two, under one guard.
//! 3. **Ask past the newest sample on purpose.** A 1 kHz controller against a
//!    200 Hz estimator is *always* extrapolating; refusing is not an answer a
//!    controller can act on. `ExtrapPolicy::ConstantTwist` extends the screw
//!    twist the last two samples imply, and `Extrapolated::by_ns` says how far
//!    it reached — which is the number to gate on, not a wall-clock guess.
//!
//!    **And it is the *slowest edge on the route* that sets it.** The two routes
//!    below make that visible: `odom -> lidar` crosses only the 200 Hz edge, and
//!    `map -> lidar` also crosses the 10 Hz one, so the second is an order of
//!    magnitude staler at the same instant for the same reason a map-relative
//!    query always is. A budget belongs to a route, not to a robot.
//! 4. **Treat `SlotContended` as data, not as an error to log and forget.** It
//!    is the bounded worst case of the seqlock read (`docs/API.md` §8.2): a
//!    writer was mid-publish and the reader gave up after a fixed number of
//!    retries rather than blocking. Reusing the previous cycle's pose is
//!    correct; blocking would not be.
//! 5. **Never allocate inside the loop.** The histogram below is pre-sized
//!    before the first cycle, for the same reason the engine's own hot path
//!    allocates nothing: a `realloc` inside a control cycle is a deadline miss.
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

    /// The controller's rate. One query per cycle.
    const CONTROL_HZ: u64 = 1_000;
    /// The state estimator's rate — five times slower, which is the ordinary
    /// case and the reason extrapolation is not an edge case here.
    const ESTIMATE_HZ: u64 = 200;
    /// How long to run. Long enough for a p99.9 to mean something.
    const CYCLES: usize = 200_000;
    /// What each route may be extrapolated by before the controller must
    /// degrade rather than steer on it. **Per route, because staleness is set by
    /// the slowest edge on the route** — these two differ by the ratio of the
    /// estimator rates, and that is the point rather than a tuning accident.
    /// The numbers are a robot's to choose; these are one estimator period plus
    /// a margin.
    const FAST_BUDGET_NS: i64 = 10_000_000; // 10 ms, over the 200 Hz route
    const FULL_BUDGET_NS: i64 = 150_000_000; // 150 ms, over the 10 Hz one

    // ---- declare the topology, once, at startup ---------------------------
    //
    // Capacities are per edge and are sized in *time*, not in slots:
    // `Capacity::history(rate, secs)` is how long a consumer may lag before the
    // ring laps it. One global capacity would either starve the fast edge or
    // waste the slow one.
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
            // A sensor mount is *static*: it is folded into a constant at plan
            // time and costs the loop nothing. Publishing it as a dynamic edge
            // — which is what a latched topic amounts to — would put a binary
            // search and an interpolation in the loop for a number that never
            // changes.
            .static_edge(
                "base_link",
                "lidar",
                &tf_tree::exp_se3([0.0, 0.0, 0.0, 0.12, 0.0, 0.31]),
            )
            .build_shared("tf_tree_control_loop_example")
            .expect("create the shared arena"),
    );

    let stop = Arc::new(AtomicBool::new(false));
    // **One clock origin, shared.** The writer and the control loop both stamp
    // against this. Taking an `Instant::now()` in each — which this example did
    // until a review caught it — makes the reader's stamps trail the writer's by
    // however long startup took, so `by_ns` measures that startup gap rather
    // than the age of the estimate, and the number the example exists to teach
    // becomes an artefact of when two threads happened to begin.
    let t0 = Instant::now();

    // ---- the estimator, publishing on its own thread ----------------------
    let writer = {
        let tree = Arc::clone(&tree);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            // `t0` is `Copy`, and it is the *same* origin the control loop below
            // stamps against.
            let odom = tree.frame("odom").unwrap();
            let map = tree.frame("map").unwrap();
            let base = tree.frame("base_link").unwrap();
            let slow = tree.claim(odom, map).expect("claim map->odom");
            let fast = tree.claim(base, odom).expect("claim odom->base_link");

            let period = Duration::from_nanos(1_000_000_000 / ESTIMATE_HZ);
            let mut n: i64 = 0;
            while !stop.load(Ordering::Relaxed) {
                let t = t0.elapsed().as_nanos() as i64;
                // A body moving on a smooth screw, so `ConstantTwist` has
                // something real to extend and the two policies differ.
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

    // ---- the loop ---------------------------------------------------------
    //
    // Everything above this line happens once. Everything below happens 1000
    // times a second and allocates nothing.
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
        // The stamp a controller wants is *now*, which is past the newest
        // sample by up to one estimator period. That is the normal case.
        let now_ns = t0.elapsed().as_nanos() as i64;
        let t = Stamp::<SystemDomain>::from_nanos(now_ns);

        // **One guard for the whole cycle**, covering both transforms. That is
        // the hoisting this example is about: two queries, one topology
        // validation, one consistent view.
        let started = Instant::now();
        let g = tree.guard();
        let fast = fast_plan.at_extrapolating(&g, t, ExtrapPolicy::ConstantTwist);
        let full = plan.at_extrapolating(&g, t, ExtrapPolicy::ConstantTwist);
        // Both queries and the guard, timed together — that is the per-cycle
        // cost a deadline is actually set against.
        lat_ns.push(started.elapsed().as_nanos() as u64);

        for (answer, budget, stale) in [
            (fast, FAST_BUDGET_NS, &mut stale_fast),
            (full, FULL_BUDGET_NS, &mut stale_full),
        ] {
            match answer {
                Ok(e) => {
                    stale.push(e.by_ns);
                    if e.by_ns > budget {
                        // Degrade. The pose is well-formed; it is just further
                        // from real data than this route's budget allows, and
                        // `by_ns` is what makes that judgeable at all.
                        too_stale += 1;
                    } else {
                        last_good = e.pose;
                    }
                }
                // The bounded worst case, not a failure: a writer was
                // mid-publish and the reader returned instead of blocking. One
                // cycle of the previous pose is right for a controller.
                Err(LookupError::SlotContended { .. }) => contended += 1,
                // Before the estimator's first samples, or after it dies.
                Err(LookupError::NoData { .. } | LookupError::Extrapolation { .. }) => {
                    no_data += 1;
                }
                // Anything else is a real fault and names the edge it is about;
                // `Tree::describe` resolves that id to a frame name.
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
