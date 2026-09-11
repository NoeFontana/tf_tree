//! Sample the machine's busy fraction and refuse a measurement taken on a loud
//! host — `docs/decisions/0023` step 5.
//!
//! # Why a separate binary rather than a call
//!
//! [`tf_tree_bench::mp::require_quiet_machine`] is the one spelling of this
//! check, and every harness that owns its own `main` simply calls it. The
//! `docs/PHASE4.md` §7 gate cannot: its instrument is
//! `crates/tf_tree_c/examples/abi_cost.rs`, an **example of `tf_tree_c`**, and
//! `tf_tree_bench` depends on `tf_tree_c` — so a dev-dependency the other way
//! is a cycle. The two ways out were a second implementation of the sampler,
//! which is the duplicate-spelling defect `docs/PROJECT.md` §6 names, and this:
//! one entry point that a recipe brackets the run with. `0023` step 5 chose
//! this one, and the choice is a **crate boundary**, not a two-line print.
//!
//! # The sample is taken BEFORE the run, and that is the whole point
//!
//! [`busy_fraction`] reads the aggregate `cpu` line of `/proc/stat`, which
//! includes *the measuring process*. `abi_cost` saturates one core; on an
//! 8-logical-CPU host that is ~12.5%, already above
//! [`QUIET_ENOUGH`] (0.10). So a fraction sampled inside the timed loop can
//! **never** pass, for a reason that has nothing to do with the host — and a
//! threshold that can never be met is the same defect as one that can never
//! fail. Bracketing the run keeps the sampler outside the workload it is
//! judging.
//!
//! The `after` sample is taken once `abi_cost` has exited, so its own load is
//! gone from the window too. A pair that reads quiet-then-loud says somebody
//! else started work *during* the run, which is the case a single pre-run
//! sample cannot see.
//!
//! # Exit codes: INVALID is not FAIL
//!
//! `abi_cost` exits **1** when the §7 ratio misses its allowance. This binary
//! exits **2** when the host is too loud, so a recipe and a reader can tell
//! "the ABI got slower" from "this machine could not state the number". They
//! are different findings and only one of them is about the code.
//!
//! | exit | meaning |
//! |---|---|
//! | 0 | quiet — the reading that follows is admissible |
//! | 2 | too loud — no number should be recorded from this run |
//!
//! # The override announces itself
//!
//! `TF_TREE_BENCH_FORCE=1` makes `require_quiet_machine` return `Ok` at any
//! reading. A check that is silently unfailable under an environment variable
//! is worth nothing, so a forced pass over the threshold prints **FORCED** and
//! says in the same line that the run is not a quiet-host reading. `0023` step
//! 5 wants twelve runs each *recording* `busy <= QUIET_ENOUGH`; a forced line
//! is not one of them.
//!
//! Run: `just abi-cost` (which brackets), or `cargo run -p tf_tree_bench --bin
//! quiet_check -- <label>`.
// This binary's entire output *is* its result: `just abi-cost` prints it around
// the measurement so the recorded number carries the host state it was taken at.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use tf_tree_bench::mp::{busy_fraction, require_quiet_machine, QUIET_ENOUGH};

fn main() {
    // A label so the two samples in a bracket are distinguishable in a log that
    // scrolled. Free-form: the recipe passes `before` and `after`.
    let label = std::env::args().nth(1).unwrap_or_else(|| "sample".into());

    match require_quiet_machine() {
        Ok(busy) if busy <= QUIET_ENOUGH => {
            println!(
                "quiet_check[{label}]: busy {:.1}% (threshold {:.0}%) — QUIET",
                busy * 100.0,
                QUIET_ENOUGH * 100.0
            );
        }
        // `require_quiet_machine` returns `Ok` above the threshold only when
        // `TF_TREE_BENCH_FORCE` is set. Saying so is the difference between a
        // check and a formality.
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
            // Re-read for the headline so the number appears in the same shape
            // as the passing line; the refusal text below names the consumers.
            let busy = busy_fraction(std::time::Duration::from_millis(300));
            println!(
                "quiet_check[{label}]: busy {:.1}% (threshold {:.0}%) — NOT QUIET",
                busy * 100.0,
                QUIET_ENOUGH * 100.0
            );
            eprintln!("quiet_check: {msg}");
            // 2, not 1: see the exit-code table above.
            std::process::exit(2);
        }
    }
}
