//! What one [`Ingest::offer`] costs, in instructions — `docs/PHASE4.md` §7.
//!
//! # Instructions, not time, and that is not a compromise
//!
//! This host fails `tf_tree_bench`'s `Fitness::probe` for two reasons that do
//! not go away by trying harder: four physical cores with SMT on, and a governor
//! it cannot read. Every timing row in `crates/tf_tree_bench/baseline/results.json`
//! is `unavailable` in consequence, and `/proc/sys/kernel/perf_event_paranoid` is
//! `4`, so hardware counters are denied outright.
//!
//! `cachegrind` **simulates**, so its `Ir` is exact under load, independent of
//! the governor, the scheduler and the SMT sibling. It is the only cost-shaped
//! quantity this machine can state as a fact. It is emphatically **not** a
//! latency figure and it does not convert into one: cachegrind models no
//! out-of-order execution, no prefetch and no store buffer.
//!
//! # Three properties copied from `tf_tree_bench::bin::footprint`
//!
//! 1. **`N = 0` performs setup and no offers**, so `Ir(N) - Ir(0)` removes
//!    construction, teardown and process startup *exactly*.
//! 2. **No clock reads inside the loop.** The receipt stamp is synthesised
//!    arithmetically. This is the one place it diverges deliberately from
//!    `crates/tf_tree_c/examples/bridge_cost.rs`, which reads the real clock
//!    because it is measuring time and this is not.
//! 3. **One mode per process**, so allocator state cannot leak between subjects.
//!
//! # Why it sweeps the edge count, which is the whole point
//!
//! A `BTreeMap` with one key never compares anything. A single-edge measurement
//! would report descents that do no work, which is exactly the trap
//! `crates/tf_tree_c/examples/bridge_cost.rs` records in its own header. The
//! sweep is what turns "it got faster" into a *shape*: the `ByEdge` tables cost
//! `O(log E)` descents times `O(shared prefix)` per `memcmp`, so `Ir` per offer
//! should **rise with the edge count** before an index is introduced and be
//! **flat** after. If it does not rise before, the premise of the refactor is
//! wrong and this binary is how that would be discovered.
//!
//! Name style matters for the same reason and is swept for the same reason:
//! `link0`..`link99` share a four-byte prefix, while a real robot's
//! `robot1/arm/wrist_1_link` shares fifteen or more and costs proportionally
//! more per node visited. Both rows are reported; quoting only the long-name row
//! would be picking the flattering measurement.
//!
//! Run: `just bridge-footprint` (needs the container — `valgrind` is not
//! installed on the host).
// This binary's entire output *is* its result.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr
)]

use std::hint::black_box;
use std::time::Instant;

use tf_tree_bridge::{Action, Ingest, Publisher, Sample, SteadyNanos, Topic, TopologyConfig};

/// `docs/PHASE4.md` §7's row is "1 kHz × 20 edges", so 20 is the headline.
const EDGES_DEFAULT: usize = 20;
/// Never zero: `SteadyNanos(0)` is the "no receipt clock" sentinel and would
/// leave the offset table dormant, measuring a path no robot takes.
const T0: i64 = 5_000_000_000_000;
const STAMP0: i64 = 10_000_000_000;
const MS: i64 = 1_000_000;

/// How frame names are spelled, which decides what a `memcmp` costs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Names {
    /// `link0 -> link1`. Four shared bytes.
    Short,
    /// `robot1/arm/wrist_0_link -> robot1/arm/wrist_1_link`. Fifteen shared.
    Ros,
}

fn edge_names(style: Names, i: usize) -> (String, String) {
    match style {
        Names::Short => (format!("link{i}"), format!("link{}", i + 1)),
        Names::Ros => (
            format!("robot1/arm/wrist_{i}_link"),
            format!("robot1/arm/wrist_{}_link", i + 1),
        ),
    }
}

/// A chain of `edges` dynamic edges.
fn topology(edges: usize, style: Names) -> String {
    let mut s = String::new();
    for i in 0..edges {
        let (p, c) = edge_names(style, i);
        s.push_str(&format!(
            "[[edge]]\nparent = \"{p}\"\nchild = \"{c}\"\nkind = \"dynamic\"\ncapacity = 256\n\n"
        ));
    }
    s
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// The accepted `/tf` transform. The path a healthy robot spends its life on.
    Declared,
    /// An edge the config does not declare. A different table, and a path with
    /// its own allocation budget.
    Undeclared,
    /// A publisher replaying stamps from five seconds ago, forever. Refused
    /// every time, and — by design — never latching, so a stuck node occupies
    /// this path indefinitely at full rate. `steady_state_alloc.rs` records that
    /// this is where a real two-allocation-per-message defect once hid with
    /// every other test green.
    Regressing,
}

/// The only loop. Both the counted and the timed entry points reach it, so the
/// two cannot drift about what work is being measured.
#[inline(never)]
fn run(mode: Mode, edges: usize, style: Names, n: usize) -> u64 {
    let cfg = TopologyConfig::parse(&topology(edges, style)).unwrap();
    let mut ingest = Ingest::new(&cfg);
    let publisher = Publisher::Node("/ekf".to_string());

    // **Built before the loop, and this is not a detail.** Assigning
    // `s.frame_id = p.clone()` inside the loop would put a `String` allocation
    // and a memcpy into the instruction count and charge them to `offer`. An
    // `rclcpp` caller does not do that either: it holds the `TransformStamped`
    // the message already owns.
    let mut samples: Vec<Sample> = Vec::with_capacity(edges);
    for i in 0..edges {
        let (p, c) = match mode {
            // Deliberately absent from the config, and distinct per index so the
            // undeclared table is exercised rather than one key being re-probed.
            Mode::Undeclared => (format!("ghost{i}"), format!("ghost{}", i + 1)),
            _ => edge_names(style, i),
        };
        samples.push(Sample::identity(&p, &c, STAMP0).received_at(SteadyNanos(T0)));
    }

    // **Warm-up outside the counted window.** An edge's first sighting
    // legitimately interns, allocates its counter row and seeds its offset
    // baseline; charging that to the steady state would flatter whichever
    // variant amortises it better.
    let mut stamp = STAMP0;
    let mut received = T0;
    for k in 0..(edges * 4) {
        let i = k % edges;
        if i == 0 {
            stamp += MS;
            received += MS;
        }
        let mut s = samples[i].clone();
        s.stamp_nanos = stamp;
        s.received = SteadyNanos(received);
        let _ = ingest.offer(Topic::Tf, &s, &publisher);
    }

    // The regressing fixture rewinds five seconds and stays there: every offer
    // from here is past the threshold, refused, and — with one publisher — never
    // promoted to a reset.
    let base_stamp = if mode == Mode::Regressing {
        stamp - 5_000 * MS
    } else {
        stamp
    };

    let mut accepted = 0u64;
    for k in 0..n {
        let i = k % edges;
        if i == 0 && mode != Mode::Regressing {
            stamp += MS;
            received += MS;
        }
        if mode == Mode::Regressing {
            received += MS;
        }
        let s = &mut samples[i];
        s.stamp_nanos = black_box(if mode == Mode::Regressing {
            base_stamp
        } else {
            stamp
        });
        s.received = SteadyNanos(black_box(received));
        if matches!(
            ingest.offer(Topic::Tf, black_box(&*s), &publisher),
            Action::Publish { .. }
        ) {
            accepted += 1;
        }
    }
    black_box(accepted)
}

/// Wall-clock cost per offer, over `threads` **independent** bridges.
///
/// # Why independent bridges, and what that does and does not test
///
/// `Ingest::offer` takes `&mut self` and the C ABI is thread-affine —
/// `tft_bridge_offer` is legal only on the thread that created the bridge — so
/// there is no such thing as N threads sharing one `Ingest`, and a harness that
/// pretended otherwise would be measuring a configuration the design forbids.
/// N threads here is N bridges, which is the shape a multi-robot host actually
/// runs.
///
/// So this does **not** measure lock contention; there are no locks. What it
/// does measure, and what a single-threaded instruction count structurally
/// cannot see, is whether the change is bought back by the allocator or by
/// memory bandwidth once several bridges run at once. An interning refactor
/// trades a scattered `BTreeMap` walk for a denser table, and denser tables
/// share a last-level cache.
///
/// # The statistic
///
/// Min of `ROUNDS` rounds. Every source of noise on this host **adds** time and
/// none removes it, so the fastest round is the closest thing to the work
/// itself — the same argument `crates/tf_tree_c/examples/bridge_cost.rs` makes.
/// The figure is ns per offer *per thread*, so a perfectly scaling change holds
/// it flat as `threads` rises and a bandwidth-bound one does not.
fn time(mode: Mode, edges: usize, style: Names, n: usize, threads: usize) {
    const ROUNDS: usize = 7;
    let mut best = f64::INFINITY;
    for _ in 0..ROUNDS {
        let t0 = Instant::now();
        std::thread::scope(|sc| {
            for _ in 0..threads {
                sc.spawn(move || run(mode, edges, style, n));
            }
        });
        let per = t0.elapsed().as_secs_f64() * 1e9 / (n as f64);
        if per < best {
            best = per;
        }
    }
    println!("offer_time mode={mode:?} edges={edges} threads={threads} ns_per_offer={best:.1}");
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mode = match a.get(1).map(String::as_str).unwrap_or("declared") {
        "declared" => Mode::Declared,
        "undeclared" => Mode::Undeclared,
        "regressing" => Mode::Regressing,
        other => {
            eprintln!("offer_cost: unknown mode {other:?}");
            eprintln!("usage: offer_cost <declared|undeclared|regressing> N [EDGES] [short|ros]");
            std::process::exit(2);
        }
    };
    let n: usize = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(200_000);
    let edges: usize = a
        .get(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(EDGES_DEFAULT)
        .max(1);
    let style = match a.get(4).map(String::as_str).unwrap_or("short") {
        "ros" => Names::Ros,
        _ => Names::Short,
    };

    // A `threads` argument switches from the counted mode to the timed one.
    // They share `run`, so the two cannot measure different work.
    if let Some(threads) = a.get(5).and_then(|s| s.parse::<usize>().ok()) {
        time(mode, edges, style, n, threads.max(1));
        return;
    }
    let accepted = run(mode, edges, style, n);
    // One line, so `just bridge-footprint` can prefix it with the cachegrind
    // ledger without reformatting. `accepted` is printed so a run that silently
    // stopped doing work cannot masquerade as a fast one.
    println!("offer_cost n={n} edges={edges} accepted={accepted}");
}
