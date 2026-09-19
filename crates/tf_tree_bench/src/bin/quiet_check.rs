//! Sample the machine's busy fraction and refuse a measurement taken on a loud
//! host: `docs/decisions/0023` step 5.
//!
//! # Why a separate binary
//!
//! [`tf_tree_bench::mp::require_quiet_machine`] is the one spelling of the check.
//! The `docs/PHASE4.md` §7 gate's instrument is
//! `crates/tf_tree_c/examples/abi_cost.rs`, an example of `tf_tree_c`, which
//! `tf_tree_bench` depends on. Cargo permits that dev-dependency cycle; the reason
//! is that it would pull `tf_tree_bench` into every `tf_tree_c` example build, and
//! that the sampler must not run **inside the measured process**. A second
//! implementation of the sampler is the duplicate-spelling defect of
//! `docs/PROJECT.md` §6.
//!
//! # The sample is taken BEFORE the run
//!
//! [`tf_tree_bench::mp::busy_fraction`] reads the aggregate `/proc/stat` line,
//! which includes the measuring process: `abi_cost` saturates one core, ~12.5% on 8
//! logical CPUs, already above [`QUIET_ENOUGH`] (0.10), so an in-loop sample could
//! never pass. The `after` sample, taken once `abi_cost` has exited, catches
//! somebody starting work *during* the run.
//!
//! # Exit codes: INVALID is not FAIL
//!
//! `abi_cost` exits **1** when the §7 ratio misses its allowance; this binary exits
//! **2** when the host is too loud, telling "the ABI got slower" from "this machine
//! could not state the number".
//!
//! | exit | meaning |
//! |---|---|
//! | 0 | quiet, **or** loud with `TF_TREE_BENCH_FORCE` set |
//! | 2 | too loud: no number should be recorded from this run |
//!
//! The two cases behind 0 differ in the printed line (`FORCED`), not the status.
//! `TF_TREE_BENCH_FORCE=1` makes `require_quiet_machine` return `Ok` at any
//! reading, so a forced pass prints **FORCED** and says the run is not a quiet-host
//! reading (`0023` step 5 wants twelve runs each recording `busy <= QUIET_ENOUGH`).
//!
//! Run: `just abi-cost` (which brackets), or `cargo run -p tf_tree_bench --bin
//! quiet_check -- <label>`.
// Output IS the result: `just abi-cost` prints it around the measurement.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use tf_tree_bench::mp::{require_quiet_machine, QUIET_ENOUGH};

fn main() {
    // A label to tell the two samples in a bracket apart; the recipe passes
    // `before` and `after`.
    let label = std::env::args().nth(1).unwrap_or_else(|| "sample".into());

    match require_quiet_machine() {
        Ok(busy) if busy <= QUIET_ENOUGH => {
            println!(
                "quiet_check[{label}]: busy {:.1}% (threshold {:.0}%) — QUIET",
                busy * 100.0,
                QUIET_ENOUGH * 100.0
            );
        }
        // Above the threshold `Ok` only under `TF_TREE_BENCH_FORCE`; say so.
        Ok(busy) => {
            println!(
                "quiet_check[{label}]: busy {:.1}% (threshold {:.0}%) — FORCED by \
                 TF_TREE_BENCH_FORCE: this run is NOT a quiet-host reading and must not be \
                 recorded as one",
                busy * 100.0,
                QUIET_ENOUGH * 100.0
            );
        }
        Err(msg) => {
            // No second sample: the message carries the reading it refused on.
            println!("quiet_check[{label}]: NOT QUIET — the reading is below");
            eprintln!("quiet_check: {msg}");
            // 2, not 1: see the exit-code table above.
            std::process::exit(2);
        }
    }
}
