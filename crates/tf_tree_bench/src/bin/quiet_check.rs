//! Sample the machine's busy fraction and refuse a measurement taken on a loud
//! host: `docs/decisions/0023` step 5.
//!
//! # Why a separate binary
//!
//! [`tf_tree_bench::mp::require_quiet_machine`] is the one spelling of the check;
//! the sampler must not run inside the measured process (`abi_cost`, in
//! `tf_tree_c`), and [`tf_tree_bench::mp::busy_fraction`] would count it.
//!
//! # Exit codes: INVALID is not FAIL
//!
//! `abi_cost` exits 1 when the §7 ratio misses its allowance; this binary exits 2
//! when the host is too loud.
//!
//! | exit | meaning |
//! |---|---|
//! | 0 | quiet, **or** loud with `TF_TREE_BENCH_FORCE` set |
//! | 2 | too loud: no number should be recorded from this run |
//!
//! A forced pass prints **FORCED** and is not a quiet-host reading (`0023` step 5).
//!
//! Run: `just abi-cost` (which brackets before and after), or `cargo run -p
//! tf_tree_bench --bin quiet_check -- <label>`.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use tf_tree_bench::mp::{require_quiet_machine, QUIET_ENOUGH};

fn main() {
    let label = std::env::args().nth(1).unwrap_or_else(|| "sample".into());

    match require_quiet_machine() {
        Ok(busy) if busy <= QUIET_ENOUGH => {
            println!(
                "quiet_check[{label}]: busy {:.1}% (threshold {:.0}%) — QUIET",
                busy * 100.0,
                QUIET_ENOUGH * 100.0
            );
        }
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
            println!("quiet_check[{label}]: NOT QUIET — the reading is below");
            eprintln!("quiet_check: {msg}");
            std::process::exit(2);
        }
    }
}
