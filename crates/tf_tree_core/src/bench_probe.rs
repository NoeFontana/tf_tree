//! The in-crate half of `docs/PHASE5.md` §9.2's embedding row (`docs/API.md`
//! §2.3 item 3): [`crate::plan::Plan::at`] is defined here, so only a probe in
//! this crate is in-crate; the `tf_tree` facade re-exports the engine and is not.
//!
//! Behind the default-off `bench-probe` feature, like `tf_tree_c`'s `test-hooks`;
//! no shipped crate enables it.

use crate::plan::{Guard, Plan, Stamp};

/// One depth-3 evaluation, compiled inside `tf_tree_core`.
///
/// Same body as `tf_tree_bench`'s out-of-crate probe, so the only difference is
/// the crate it is compiled in. Must not be generic (a generic body is
/// monomorphized in the caller's crate) and stays `#[inline(never)]` so the
/// timing measures the call. The error arm returns `NaN` to add no branch.
#[inline(never)]
pub fn depth3_lookup(plan: &Plan, g: &Guard, t: Stamp) -> f64 {
    match plan.at(g, t) {
        Ok(iso) => iso.t.x,
        Err(_) => f64::NAN,
    }
}
