//! The **in-crate** half of `docs/PHASE5.md` §9.2's embedding row.
//!
//! `docs/API.md` §2.3 item 3 requires a benchmark row measuring the facade path
//! *from a separate crate* against the **in-crate** path, gated at 5% — the same
//! gate `docs/PHASE4.md` §7 applies to the C ABI. The separate-crate half is
//! ordinary benchmark code in `tf_tree_bench`. The in-crate half cannot be: the
//! whole point of the comparison is which crate the call is compiled in, and
//! [`crate::plan::Plan::at`] and the fold beneath it are defined **here**.
//!
//! That is not a matter of taste, it was measured. A probe placed in the
//! `tf_tree` facade is *not* in-crate — the facade re-exports the engine rather
//! than containing it, so it measured 241.5 ns against an outside caller's 243.6
//! (`crates/tf_tree_bench/src/embed.rs` records the method). A row built on a
//! facade or benchmark-crate "in-crate" column would report ≈1.00× for ever
//! while the real boundary went unmeasured.
//!
//! # Why this is a feature and not simply `pub`
//!
//! The precedent is `tf_tree_c`'s `test-hooks`: `tft_guarded_noop` exists only
//! so `examples/abi_cost.rs` can measure what the `catch_unwind` guard costs,
//! and it is compiled out of every shipped build. This module is that pattern
//! for the crate boundary instead of the ABI boundary. **`bench-probe` is
//! default-off**, so no shipped configuration of `tf_tree_core` contains this
//! function, and `tf_tree`, `tf_tree_c`, `tf_tree_py` and `tf_tree_cli` never
//! enable it.

use crate::plan::{Guard, Plan, Stamp};

/// One depth-3 evaluation, **compiled inside `tf_tree_core`**.
///
/// Byte-for-byte the body `tf_tree_bench`'s out-of-crate probe uses, so the only
/// difference between the two timed calls is which crate this body was compiled
/// in. That is the measurement.
///
/// **It must not be generic, and that is not a style choice.** A generic
/// function is monomorphized in the crate that *calls* it, so
/// `fn depth3_lookup<D: Domain>` would have had its body codegen'd in
/// `tf_tree_bench` alongside the out-of-crate twin — both columns would be
/// out-of-crate and the row would report 1.00× by construction while measuring
/// nothing. This takes `Stamp` at its default [`crate::plan::SystemDomain`],
/// concretely, which is also what the out-of-crate twin takes.
///
/// `#[inline(never)]` is what makes it a measurement of the *call*: without it
/// the caller's timing loop and the fold merge, and the number becomes a
/// property of whichever loop happened to be written around it. The
/// out-of-crate twin carries the same attribute for the same reason.
///
/// The error arm returns `NaN` rather than a `Result` so the timed body has no
/// branch the shipped call does not have; the caller checks its accumulator
/// afterwards, which catches a failing lookup just as surely.
#[inline(never)]
pub fn depth3_lookup(plan: &Plan, g: &Guard, t: Stamp) -> f64 {
    match plan.at(g, t) {
        Ok(iso) => iso.t.x,
        Err(_) => f64::NAN,
    }
}
