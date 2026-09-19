//! One reader or writer process for the contended-scaling harness, selected by
//! argv.
//!
//! * `reader` attaches read-only to the segment on stdin (`shm_util`) and makes
//!   throughput, service and cycle passes (`docs/PHASE1.md` §11.2).
//! * `writer` joins through the rendezvous (`tf_tree::Open`), claims one edge and
//!   publishes at its rate; a read-write attach over a bare descriptor is refused
//!   (`docs/decisions/0028` step 0b). The join precedes the rate loop and is
//!   reported as `join_ns`.
//!
//! A reader is a process so `taskset` can place it without `unsafe`
//! (`docs/decisions/0007` rule 1). Output is a line protocol on stdout; usage
//! errors go to stderr.
// `panic!` is allowed: every use names the frame, pair or edge that failed.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    clippy::print_stderr
)]

use std::hint::black_box;
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use tf_tree::{AttachMode, Plan, Stamp, Tree};
use tf_tree_bench::fixture;
use tf_tree_bench::mp::{Histogram, ProcStats, RateLoop};
use tf_tree_bench::shm_util::WRITER_SLACK_S;

/// How many clock-pair reads the overhead control performs.
const CLOCK_CALIBRATION_ITERS: usize = 100_000;

fn usage() -> ! {
    eprintln!(
        "usage:\n  \
         load_child reader <hz> <seconds> <lo_ns> <hi_ns> <target|source> [...]\n  \
         load_child writer <arena> <rate_hz> <seconds> <seed> <next_stamp_ns> <parent> <child>\n\
         \n\
         A reader takes the shared arena on its standard input; a writer joins\n\
         <arena> through the rendezvous, because a read-write attach over a bare\n\
         descriptor is refused (docs/decisions/0028 step 0b)."
    );
    std::process::exit(2)
}

/// The segment arrives on stdin, mapped read-only.
fn attach_read_only() -> Tree {
    let fd = std::io::stdin()
        .as_fd()
        .try_clone_to_owned()
        .expect("segment from stdin");
    Tree::attach_shared(fd, AttachMode::ReadOnly).expect("attach to the shared arena")
}

/// Joins through the rendezvous; returns the tree and the join cost in ns.
/// `await_open` retries only retryable errors, bounded by [`WRITER_SLACK_S`].
fn join_read_write(arena: &str) -> (Tree, u64) {
    let start = Instant::now();
    let tree = tf_tree::Open::new()
        .name(arena)
        .and_then(|o| {
            o.mode(AttachMode::ReadWrite)
                .create(tf_tree::CreatePolicy::Never)
                .await_open(Duration::from_secs_f64(WRITER_SLACK_S))
        })
        .unwrap_or_else(|e| {
            panic!(
                "joining the arena {arena:?} through the rendezvous, within a \
                 {WRITER_SLACK_S} s budget: {e}. Only ArenaAbsent and \
                 ArenaHeldButUnreachable are retried inside it; anything else \
                 came back on the first attempt. The coordinator creates the \
                 arena with Backing::Served; a writer cannot attach over the \
                 descriptor on stdin since docs/decisions/0028 step 0b"
            )
        });
    (tree, start.elapsed().as_nanos() as u64)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("reader") if args.len() >= 6 => reader(&args[1..]),
        Some("writer") if args.len() == 8 => writer(&args[1..]),
        _ => usage(),
    }
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

fn reader(args: &[String]) {
    let hz: f64 = args[0].parse().expect("hz");
    let seconds: f64 = args[1].parse().expect("seconds");
    let lo: i64 = args[2].parse().expect("lo_ns");
    let hi: i64 = args[3].parse().expect("hi_ns");

    let tree = attach_read_only();
    let plans: Vec<Plan> = args[4..]
        .iter()
        .map(|spec| {
            let (t, s) = spec
                .split_once('|')
                .unwrap_or_else(|| panic!("pair {spec:?} is not `target|source`"));
            let tid = tree.frame(t).unwrap_or_else(|e| panic!("frame {t}: {e:?}"));
            let sid = tree.frame(s).unwrap_or_else(|e| panic!("frame {s}: {e:?}"));
            tree.plan(tid, sid)
                .unwrap_or_else(|e| panic!("plan {t} <- {s}: {e:?}"))
        })
        .collect();

    // Sweep the whole retained window; a collapsed sweep is a degenerate best case.
    let stamps: Vec<i64> = (0..STAMP_STEPS)
        .map(|k| lo + (hi - lo) * k as i64 / STAMP_STEPS as i64)
        .collect();

    // Warm plans and pages first (`docs/PHASE2.md` §7.1).
    {
        let guard = tree.guard();
        for p in &plans {
            let warm: Stamp = Stamp::from_nanos(stamps[0]);
            let _ = p.at(&guard, warm);
        }
    }

    let third = Duration::from_secs_f64(seconds / 3.0);
    let before = ProcStats::read();

    let (ops, elapsed_ns, declined_t) = throughput_pass(&tree, &plans, &stamps, third);
    let (service, declined_s, done) = service_pass(&tree, &plans, &stamps, third);
    let (cycle, declined_c) = cycle_pass(&tree, &plans, &stamps, hz, third);
    let clock_overhead_ns = clock_overhead();

    let after = ProcStats::read();
    let d = after.since(before);

    println!("throughput {ops} {elapsed_ns}");
    println!("service {}", service.encode());
    println!("cycle {}", cycle.encode());
    println!("clock_overhead_ns {clock_overhead_ns}");
    println!("declined {}", declined_t + declined_s + declined_c);
    println!("attempted {}", ops + done + cycle.count());
    println!("cpu_ns {}", d.cpu_ns);
    println!("pss_kib {}", d.pss_kib);
}

/// Distinct stamps a reader sweeps across the retained window.
const STAMP_STEPS: usize = 1024;

/// Closed loop, one clock read per batch. Returns `(lookups, elapsed_ns, declined)`.
fn throughput_pass(
    tree: &Tree,
    plans: &[Plan],
    stamps: &[i64],
    window: Duration,
) -> (u64, u64, u64) {
    const BATCH: usize = 1024;
    let start = Instant::now();
    let (mut ops, mut declined) = (0u64, 0u64);
    let mut k = 0usize;
    let mut acc = 0.0f64;

    while start.elapsed() < window {
        let guard = tree.guard();
        for _ in 0..BATCH {
            let stamp: Stamp = Stamp::from_nanos(stamps[k % stamps.len()]);
            let plan = &plans[k % plans.len()];
            match plan.at(&guard, stamp) {
                Ok(p) => acc += p.t.x,
                Err(_) => declined += 1,
            }
            k += 1;
        }
        ops += BATCH as u64;
    }
    black_box(acc);
    (ops, start.elapsed().as_nanos() as u64, declined)
}

/// Dense, one clock pair per lookup, not rate limited. Returns
/// `(histogram, declined, attempted)`; `clock_overhead_ns` is in every sample.
fn service_pass(
    tree: &Tree,
    plans: &[Plan],
    stamps: &[i64],
    window: Duration,
) -> (Histogram, u64, u64) {
    let mut hist = Histogram::new();
    let (mut declined, mut attempted) = (0u64, 0u64);
    let start = Instant::now();
    let mut k = 0usize;
    let mut acc = 0.0f64;

    while start.elapsed() < window {
        let guard = tree.guard();
        for _ in 0..256 {
            let stamp: Stamp = Stamp::from_nanos(stamps[k % stamps.len()]);
            let plan = &plans[k % plans.len()];
            let t0 = Instant::now();
            match plan.at(&guard, stamp) {
                Ok(p) => acc += p.t.x,
                Err(_) => declined += 1,
            }
            hist.record(t0.elapsed().as_nanos() as u64);
            attempted += 1;
            k += 1;
        }
    }
    black_box(acc);
    (hist, declined, attempted)
}

/// Open loop at `hz`, measured from each tick's intended time (`src/mp.rs`).
fn cycle_pass(
    tree: &Tree,
    plans: &[Plan],
    stamps: &[i64],
    hz: f64,
    window: Duration,
) -> (Histogram, u64) {
    let mut hist = Histogram::new();
    let mut declined = 0u64;
    let mut rate = RateLoop::new(hz);
    let start = Instant::now();
    let mut k = 0usize;
    let mut acc = 0.0f64;

    while start.elapsed() < window {
        let due = rate.next_due();
        let stamp: Stamp = Stamp::from_nanos(stamps[k % stamps.len()]);
        let plan = &plans[k % plans.len()];
        let guard = tree.guard();
        match plan.at(&guard, stamp) {
            Ok(p) => acc += p.t.x,
            Err(_) => declined += 1,
        }
        drop(guard);
        hist.record(Instant::now().duration_since(due).as_nanos() as u64);
        k += 1;
    }
    black_box(acc);
    (hist, declined)
}

/// Cost of one `Instant::now()` pair, in nanoseconds.
fn clock_overhead() -> u64 {
    let start = Instant::now();
    for _ in 0..CLOCK_CALIBRATION_ITERS {
        black_box(Instant::now());
    }
    start.elapsed().as_nanos() as u64 / CLOCK_CALIBRATION_ITERS as u64
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

fn writer(args: &[String]) {
    let arena = &args[0];
    let rate_hz: f64 = args[1].parse().expect("rate_hz");
    let seconds: f64 = args[2].parse().expect("seconds");
    let seed: f64 = args[3].parse().expect("seed");
    let next_stamp_ns: i64 = args[4].parse().expect("next_stamp_ns");
    let (parent_name, child_name) = (&args[5], &args[6]);

    let (tree, join_ns) = join_read_write(arena);
    let parent = tree
        .frame(parent_name)
        .unwrap_or_else(|e| panic!("frame {parent_name}: {e:?}"));
    let child = tree
        .frame(child_name)
        .unwrap_or_else(|e| panic!("frame {child_name}: {e:?}"));
    let w = tree
        .claim(child, parent)
        .unwrap_or_else(|e| panic!("claim {parent_name}->{child_name}: {e}"));

    let step_ns = (1e9 / rate_hz) as i64;
    let mut rate = RateLoop::new(rate_hz);
    let start = Instant::now();
    let window = Duration::from_secs_f64(seconds);
    let (mut pushed, mut rejected) = (0u64, 0u64);
    let mut stamp = next_stamp_ns;

    let before = ProcStats::read();
    while start.elapsed() < window {
        let _due = rate.next_due();
        match w.push(stamp, &fixture::dynamic_pose(seed, stamp)) {
            Ok(()) => pushed += 1,
            Err(_) => rejected += 1,
        }
        stamp += step_ns;
    }
    let after = ProcStats::read();
    let d = after.since(before);

    println!("join_ns {join_ns}");
    println!("pushed {pushed}");
    println!("rejected {rejected}");
    println!("cpu_ns {}", d.cpu_ns);
    println!("pss_kib {}", d.pss_kib);
}
