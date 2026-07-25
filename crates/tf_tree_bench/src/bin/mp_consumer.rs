//! One consumer node in the multi-process evaluation.
//!
//! Models what a robotics node actually does: wake at a fixed rate, do a small
//! burst of lookups, go back to sleep. Not a tight loop — see `mp.rs` for why
//! that measures the wrong thing and hides the tail.
//!
//! Two engines, selected by argv so the same schedule, the same query mix and
//! the same measurement code drive both:
//!
//! * `tf_tree` — attaches to the shared arena on stdin (`shm_util`).
//! * `tf2` — its own private `tf2::BufferCore`, loaded with the identical
//!   stream. This is tf2's **best case**: no DDS, no deserialization, just the
//!   per-process memory and CPU duplication that having no shared arena forces.
//!   A real `tf2_ros` consumer additionally pays the transport (§ below).
//!
//! Emits one histogram line plus CPU and PSS on stdout for the coordinator.
//!
//! # What the `tf2` mode does and does not represent
//!
//! It is a **floor**, and it must be labelled as one wherever it is reported.
//! Every other benchmark in this repo deliberately excludes middleware, because
//! for a single-process library-vs-library comparison DDS would measure the
//! transport rather than the engine. That reasoning does not survive the
//! multi-process question: across processes, the transport **is** tf2's
//! mechanism — there is no other way for a second process to obtain the tree.
//! So excluding it here understates tf2's real cost, and the honest presentation
//! is to measure this floor *and* say plainly that the deployed number is worse
//! by the cost of a `TransformListener` and its DDS fan-out.
// stdout is this binary's protocol with the coordinator; stderr carries its
// usage errors, which must not land in that stream.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr
)]

use std::hint::black_box;
use std::time::Instant;

use tf_tree_bench::fixture;
use tf_tree_bench::mp::{Histogram, ProcStats, RateLoop};

/// Lookups performed per tick — a node resolving a handful of frames per cycle.
const LOOKUPS_PER_TICK: usize = 8;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let engine = args[1].as_str();
    let hz: f64 = args[2].parse().expect("hz");
    let seconds: f64 = args[3].parse().expect("seconds");

    match engine {
        "tf_tree" => run_tf_tree(hz, seconds),
        #[cfg(feature = "tf2")]
        "tf2" => tf2_mode::run(hz, seconds),
        #[cfg(not(feature = "tf2"))]
        "tf2" => {
            eprintln!("mp_consumer: tf2 mode needs --features tf2 (build in the container)");
            std::process::exit(2);
        }
        other => {
            eprintln!("mp_consumer: unknown engine {other:?}");
            std::process::exit(2);
        }
    }
}

/// Report the measurement in the coordinator's line protocol.
///
/// **Two clocks, because they answer different questions and only one of them
/// is about the engine.**
///
/// * `cycle` — intended tick time to completion. What the node experiences, and
///   the number a control loop lives with. At 100 Hz it is dominated by
///   `sleep` granularity and scheduler wakeup: ~75 us of it is the OS deciding
///   to run you, against ~2 us of actual work.
/// * `service` — first instruction of the burst to its last. What the engine
///   costs. This is the column a comparison between two engines belongs in.
///
/// Reporting only the first would have made both engines look identical and
/// excellent; reporting only the second would hide the fact that a node's real
/// latency is mostly not up to the engine at all.
fn report(cycle: &Histogram, service: &Histogram, before: ProcStats, after: ProcStats) {
    let d = after.since(before);
    println!("cycle {}", cycle.encode());
    println!("service {}", service.encode());
    println!("cpu_ns {}", d.cpu_ns);
    println!("pss_kib {}", d.pss_kib);
}

/// Stamps for tick `t`: a 100 ms trailing window, the §11.2 query mix's shape.
///
/// Recomputed per tick from a moving "now" so the consumer keeps chasing fresh
/// data rather than re-reading one warm slot — which would measure the branch
/// predictor rather than the engine.
fn stamps_for(tick: u64, out: &mut [i64]) {
    let now = fixture::NOW_NS - (tick as i64 % 1000) * 1_000_000;
    for (i, s) in out.iter_mut().enumerate() {
        *s = now - (i as i64) * (100_000_000 / LOOKUPS_PER_TICK as i64);
    }
}

fn run_tf_tree(hz: f64, seconds: f64) {
    use std::os::fd::AsFd;
    use tf_tree::{AttachMode, Stamp, Tree};

    let fd = std::io::stdin()
        .as_fd()
        .try_clone_to_owned()
        .expect("segment from stdin");
    let tree = Tree::attach_shared(fd, AttachMode::ReadOnly).expect("attach");

    let t = tree.frame("imu_link").expect("imu_link");
    let s = tree.frame("map").expect("map");
    let plan = tree.plan(t, s).expect("plan");

    let mut cycle = Histogram::new();
    let mut service = Histogram::new();
    let mut stamps = [0i64; LOOKUPS_PER_TICK];
    let ticks = (hz * seconds) as u64;

    // Warm the plan and the pages before the clock starts; the first-touch cost
    // is a separate measurement (`docs/PHASE2.md` §7.1), not part of steady state.
    {
        let guard = tree.guard();
        stamps_for(0, &mut stamps);
        for &ns in &stamps {
            let stamp: Stamp = Stamp::from_nanos(ns);
            let _ = plan.at(&guard, stamp);
        }
    }

    let before = ProcStats::read();
    let mut rate = RateLoop::new(hz);
    for tick in 0..ticks {
        let due = rate.next_due();
        let started = Instant::now();
        stamps_for(tick, &mut stamps);
        // A fresh guard per tick: that is what a node does, and it is where the
        // topology generation is pinned.
        let guard = tree.guard();
        let mut acc = 0.0f64;
        for &ns in &stamps {
            let stamp: Stamp = Stamp::from_nanos(ns);
            if let Ok(p) = plan.at(&guard, stamp) {
                acc += p.t.x;
            }
        }
        black_box(acc);
        let done = Instant::now();
        // Measured from when the tick was *due*, so a consumer that was still
        // busy records the backlog instead of hiding it.
        cycle.record(done.duration_since(due).as_nanos() as u64);
        service.record(done.duration_since(started).as_nanos() as u64);
    }
    let after = ProcStats::read();
    report(&cycle, &service, before, after);
}

#[cfg(feature = "tf2")]
mod tf2_mode {
    use super::{
        black_box, report, stamps_for, Histogram, Instant, ProcStats, RateLoop, LOOKUPS_PER_TICK,
    };
    use tf_tree_bench::tf2::Tf2Fixture;
    use tf_tree_tf2_sys::FrameName;

    pub fn run(hz: f64, seconds: f64) {
        // Each consumer builds its **own** buffer from the identical stream.
        // That duplication is the point of the measurement: it is what having no
        // shared arena costs, in memory and in the CPU spent filling it.
        let fixture = Tf2Fixture::load().expect("load tf2 fixture");
        let target = FrameName::new("imu_link").expect("imu_link");
        let source = FrameName::new("map").expect("map");

        let mut cycle = Histogram::new();
        let mut service = Histogram::new();
        let mut stamps = [0i64; LOOKUPS_PER_TICK];
        let ticks = (hz * seconds) as u64;

        stamps_for(0, &mut stamps);
        for &ns in &stamps {
            let _ = fixture.buffer().lookup_by_name(&target, &source, ns);
        }

        let before = ProcStats::read();
        let mut rate = RateLoop::new(hz);
        for tick in 0..ticks {
            let due = rate.next_due();
            let started = Instant::now();
            stamps_for(tick, &mut stamps);
            let mut acc = 0.0f64;
            for &ns in &stamps {
                if let Ok(p) = fixture.buffer().lookup_by_name(&target, &source, ns) {
                    acc += p.t.x;
                }
            }
            black_box(acc);
            let done = Instant::now();
            cycle.record(done.duration_since(due).as_nanos() as u64);
            service.record(done.duration_since(started).as_nanos() as u64);
        }
        let after = ProcStats::read();
        report(&cycle, &service, before, after);
    }
}
