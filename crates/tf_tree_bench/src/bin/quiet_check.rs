//! Sample the machine's busy fraction and refuse a measurement taken on a loud
//! host — `docs/decisions/0023` step 5.
//!
//! # Why a separate binary rather than a call
//!
//! [`tf_tree_bench::mp::require_quiet_machine`] is the one spelling of this
//! check, and every harness that owns its own `main` simply calls it. The
//! `docs/PHASE4.md` §7 gate does not: its instrument is
//! `crates/tf_tree_c/examples/abi_cost.rs`, an **example of `tf_tree_c`**,
//! while `tf_tree_bench` depends on `tf_tree_c`.
//!
//! **The reason is not that cargo forbids the dev-dependency, and saying so
//! was wrong.** `0023` step 5 argues `tf_tree_c` "cannot gain one without a
//! cycle", and an earlier version of this comment repeated it. Cargo **permits**
//! a dev-dependency cycle: a package's dev-dependencies may depend back on it,
//! because dev-dependencies do not participate in the library's own build
//! graph. Measured rather than reasoned about — a two-crate scratch workspace
//! in exactly this shape (`a` dev-depends on `b`, `b` depends on `a`, an
//! *example* of `a` calls into `b`) compiles.
//!
//! What is true, and is the reason: a dev-dependency would pull `tf_tree_bench`
//! and its whole tree into every `tf_tree_c` example build, for a 300 ms read of
//! `/proc/stat`; and — the load-bearing half — the sampler must not run **inside
//! the measured process**, which is the next section. A separate entry point the
//! recipe brackets the run with satisfies both. The alternative rejected is a
//! second implementation of the sampler, which is the duplicate-spelling defect
//! `docs/PROJECT.md` §6 names.
//!
//! # The sample is taken BEFORE the run, and that is the whole point
//!
//! [`tf_tree_bench::mp::busy_fraction`] reads the aggregate `cpu` line of
//! `/proc/stat`, which includes *the measuring process*. `abi_cost` saturates
//! one core; on an 8-logical-CPU host that is ~12.5%, already above
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
//! | 0 | quiet, **or** loud with `TF_TREE_BENCH_FORCE` set — see below |
//! | 2 | too loud — no number should be recorded from this run |
//!
//! The two cases behind 0 are distinguished in the printed line, not in the
//! status, because the override's whole purpose is to let a run proceed. A
//! caller that must tell them apart reads the line for `FORCED`.
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

use tf_tree_bench::mp::{require_quiet_machine, QUIET_ENOUGH};

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
            // **No second sample.** An earlier version re-read `busy_fraction`
            // here so the headline would have the same shape as the passing
            // line — which printed a *different reading* from the one that
            // failed the threshold, 300 ms later and on a machine whose load
            // was by then demonstrably moving. A refusal that quotes a number
            // other than the one it refused on is worse than one that quotes
            // none. `require_quiet_machine`'s message carries the reading it
            // actually took, and the top consumers with it.
            println!("quiet_check[{label}]: NOT QUIET — the reading is below");
            eprintln!("quiet_check: {msg}");
            // 2, not 1: see the exit-code table above.
            std::process::exit(2);
        }
    }
}
