#![allow(clippy::print_stdout, clippy::print_stderr)]
//! Workspace automation runner (`cargo xtask <task>`).
//!
//! `loom` and `bench-gate` are wired up (steps 5 and 9 of `docs/PHASE1.md`'s
//! implementation order; §10.2 and §11.3); `miri`
//! is wired up by its own Phase 1 PR. `headers` generates and drift-checks the
//! C ABI's committed headers (`docs/PHASE4.md` §3.1, `docs/decisions/0007`).

mod headers;

use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let task = std::env::args().nth(1);
    match task.as_deref() {
        Some("loom") => run_loom(),
        Some("bench-gate") => run_bench_gate(),
        // `--check` fails on drift instead of rewriting; that is the form CI runs.
        Some("headers") => headers::run(std::env::args().any(|a| a == "--check")),
        Some("miri") => {
            eprintln!("xtask: 'miri' is wired up by its Phase 1 PR");
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("usage: cargo xtask <loom|miri|bench-gate|headers [--check]>");
            ExitCode::FAILURE
        }
    }
}

/// Run the `tf_tree_core` concurrency model-checking suite under `--cfg loom`.
///
/// Builds and runs the crate's tests with `RUSTFLAGS="--cfg loom"`, which selects
/// the loom atomics in `crate::sync` and enables the `#[cfg(all(test, loom))]`
/// test module. A bounded preemption count keeps the state space tractable while
/// still exercising every ordering; override via `LOOM_MAX_PREEMPTIONS`.
fn run_loom() -> ExitCode {
    // Fold `--cfg loom` into any existing RUSTFLAGS rather than clobbering them.
    let mut rustflags = std::env::var("RUSTFLAGS").unwrap_or_default();
    if !rustflags.is_empty() {
        rustflags.push(' ');
    }
    rustflags.push_str("--cfg loom");

    let max_preemptions = std::env::var("LOOM_MAX_PREEMPTIONS").unwrap_or_else(|_| "3".into());

    println!("xtask loom: RUSTFLAGS=\"{rustflags}\" LOOM_MAX_PREEMPTIONS={max_preemptions}");
    let status = Command::new(env!("CARGO"))
        .args(["test", "-p", "tf_tree_core", "--tests", "--release"])
        .env("RUSTFLAGS", rustflags)
        .env("LOOM_MAX_PREEMPTIONS", max_preemptions)
        .status();

    match status {
        Ok(s) if s.success() => ExitCode::SUCCESS,
        Ok(s) => {
            eprintln!("xtask loom: tests failed ({s})");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("xtask loom: failed to spawn cargo: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Evaluate the `docs/PHASE1.md` §11.3 go/no-go gate — honestly.
///
/// Two of the three gate criteria (depth-3 p50 latency; read throughput scaling
/// 1→8 threads) and the `tf2::BufferCore` comparison ratio are *measurements*
/// that only mean something on dedicated, core-pinned hardware, and the tf2 path
/// additionally needs a ROS 2 install this host does not have. This runner does
/// **not** fabricate those numbers. It runs what is decisive and portable here —
/// the zero-allocation gate and the differential correctness check — and reports
/// the hardware-dependent criteria as UNAVAILABLE with the reason.
///
/// Exit status: non-zero iff a *decisive, runnable* gate fails.
fn run_bench_gate() -> ExitCode {
    let cargo = env!("CARGO");
    println!("xtask bench-gate: docs/PHASE1.md §11.3 go/no-go gate\n");

    // 0. The criterion benches must at least compile and link.
    print!("[compile]     criterion benches build ... ");
    let benches_built =
        run_ok(Command::new(cargo).args(["bench", "-p", "tf_tree_bench", "--no-run"]));
    println!("{}", if benches_built { "PASS" } else { "FAIL" });

    // 1. Zero-allocation gate — decisive and runnable here.
    print!("[gate]        zero allocations after construction ... ");
    let zero_alloc = run_ok(Command::new(cargo).args([
        "test",
        "-p",
        "tf_tree_bench",
        "--test",
        "zero_alloc",
        "--release",
    ]));
    println!("{}", if zero_alloc { "PASS" } else { "FAIL" });

    // 2. Differential correctness (naive-Rust reference) — decisive, runnable.
    print!("[correctness] naive-Rust differential within 1e-12 ... ");
    let differential = run_ok(Command::new(cargo).args([
        "test",
        "-p",
        "tf_tree_bench",
        "--test",
        "differential",
        "--release",
    ]));
    println!("{}", if differential { "PASS" } else { "FAIL" });

    // 3. The hardware-dependent criteria — reported honestly, never faked.
    println!(
        "[gate]        depth-3 hot p50 < 150 ns (ScLerp) / < 100 ns (LerpSlerp) ... UNAVAILABLE"
    );
    println!("                  reason: p50 latency needs dedicated, core-pinned hardware;");
    println!("                  measure indicatively with `cargo bench -p tf_tree_bench --bench lookup`.");
    println!("[gate]        read throughput scales >= 6x from 1 to 8 threads ... UNAVAILABLE");
    println!("                  reason: needs pinned cores + concurrent writers;");
    println!("                  measure indicatively with `cargo bench -p tf_tree_bench --bench read_scaling`.");
    // 4. The tf2 differential — decisive wherever ROS 2 is reachable. Unlike the
    //    latency rows this is a *correctness* comparison, so it does not need
    //    pinned hardware: run it whenever we can, report honestly when we can't.
    print!("[correctness] tf2::BufferCore differential within 1e-12 ... ");
    let tf2 = if ros_available() {
        let ok = run_ok(Command::new(cargo).args([
            "test",
            "-p",
            "tf_tree_bench",
            "--features",
            "tf2",
            "--release",
            "--test",
            "differential",
        ]));
        println!("{}", if ok { "PASS" } else { "FAIL" });
        Some(ok)
    } else {
        println!("UNAVAILABLE");
        println!("                  reason: no ROS 2 install found on this host.");
        println!("                  run `just tf2-differential` to run it in a container.");
        None
    };

    println!("[compare]     tf2::BufferCore latency ratio per row ... UNAVAILABLE");
    println!("                  reason: a ratio needs dedicated, core-pinned hardware;");
    println!("                  run `just tf2-bench` for indicative numbers.");

    println!("\nSummary: the correctness gates (zero-alloc, differentials) are decisive here;");
    println!("the latency/scaling rows require dedicated hardware and are NOT claimed passed.");

    if benches_built && zero_alloc && differential && tf2.unwrap_or(true) {
        ExitCode::SUCCESS
    } else {
        eprintln!("\nxtask bench-gate: a runnable gate FAILED (see rows above)");
        ExitCode::FAILURE
    }
}

/// Whether a ROS 2 install with `tf2` is reachable from this environment.
///
/// Mirrors `tf_tree_tf2_sys`'s `build.rs` discovery so the gate never *attempts*
/// a build it knows will fail with a wall of missing-header errors.
fn ros_available() -> bool {
    let has_tf2 = |prefix: &std::path::Path| {
        prefix.join("include/tf2").is_dir() && prefix.join("lib/libtf2.so").exists()
    };
    if let Ok(p) = std::env::var("TF_TREE_ROS_PREFIX") {
        return has_tf2(std::path::Path::new(&p));
    }
    if let Ok(distro) = std::env::var("ROS_DISTRO") {
        if has_tf2(std::path::Path::new(&format!("/opt/ros/{distro}"))) {
            return true;
        }
    }
    std::fs::read_dir("/opt/ros")
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .any(|e| has_tf2(&e.path()))
}

/// Run a command inheriting stdio, returning whether it exited successfully.
fn run_ok(cmd: &mut Command) -> bool {
    // Quiet the child's own output so the gate table stays readable; failures
    // still surface via the PASS/FAIL column and the non-zero exit.
    match cmd.arg("--quiet").output() {
        Ok(out) => out.status.success(),
        Err(e) => {
            eprintln!("(failed to spawn cargo: {e})");
            false
        }
    }
}
