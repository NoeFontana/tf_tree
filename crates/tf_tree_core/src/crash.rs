//! Named, deterministic abort sites — `docs/PHASE2.md` §11.3 (NORMATIVE).
//!
//! `crash_point!("<name>")` expands to nothing unless the default-off
//! `crash-points` feature is on, when it aborts on the `nth_hit`th reach of the
//! site named by
//!
//! ```text
//! TF_TREE_CRASH_AT=<name>:<nth_hit>
//! ```
//!
//! (`:<nth_hit>` defaults to `:1`; one site is armed per process). `SITES` lists
//! this crate's sites; the other §11.3 rows live with the crates that own them.
//!
//! # Why `abort`, not `panic!`
//!
//! A panic unwinds and runs `Drop` ([`crate::edge::Publisher`] releases its
//! claim, [`crate::participant::ParticipantTable::release`] frees the slot),
//! repairing the damage under test; so the diagnostic uses `std::io::Write`,
//! not `eprintln!`.
//!
//! # `no_std`
//!
//! The feature pulls in `std` via `extern crate std`; a default build links none.

/// Every crash point compiled into **this crate**, in `docs/PHASE2.md` §11.3's
/// table order, so harnesses (§11.4) cannot arm nothing by typo.
#[cfg(feature = "crash-points")]
pub const SITES: &[&str] = &[
    "push.after_seq_odd",
    "push.after_data_before_seq_even",
    "push.after_seq_even_before_head",
    "topo.after_copy_before_publish",
    "claim.after_cas",
    "intern.after_hash_cas_before_id_store",
    "attach.after_slot_assigned_before_publish",
];

/// The environment variable that arms a site: `TF_TREE_CRASH_AT=<name>:<nth>`.
#[cfg(feature = "crash-points")]
pub const ENV_VAR: &str = "TF_TREE_CRASH_AT";

/// Parsed once per process: the armed site's name and the hit it fires on.
#[cfg(feature = "crash-points")]
static ARMED: std::sync::OnceLock<Option<(alloc::string::String, u64)>> =
    std::sync::OnceLock::new();

/// Hits on the armed site only. One site is armed, so one counter suffices.
#[cfg(feature = "crash-points")]
static HITS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The armed `(name, nth_hit)`, or `None` if the variable is absent, empty or
/// malformed.
#[cfg(feature = "crash-points")]
fn spec() -> Option<&'static (alloc::string::String, u64)> {
    ARMED
        .get_or_init(|| {
            let raw = std::env::var(ENV_VAR).ok()?;
            let (name, nth) = match raw.rsplit_once(':') {
                Some((name, nth)) => (name, nth.parse::<u64>().ok()?.max(1)),
                None => (raw.as_str(), 1),
            };
            if name.is_empty() {
                return None;
            }
            Some((alloc::string::String::from(name), nth))
        })
        .as_ref()
}

/// Abort this process if `name` is the armed site and this is its armed hit.
/// # Aborts
///
/// By `std::process::abort`, on the `nth_hit`th call naming the armed site (not a panic).
#[cfg(feature = "crash-points")]
pub fn maybe_abort(name: &str) {
    use core::sync::atomic::Ordering;

    let Some((armed_name, nth)) = spec() else {
        return;
    };
    if armed_name != name {
        return;
    }
    let hit = HITS.fetch_add(1, Ordering::Relaxed) + 1;
    if hit < *nth {
        return;
    }
    // Not `eprintln!`: a write-error panic would unwind through the `Drop`s.
    {
        use std::io::Write as _;
        let mut err = std::io::stderr();
        let _ = writeln!(err, "tf_tree_core: crash point {name} hit {hit}, aborting");
        let _ = err.flush();
    }
    std::process::abort()
}

/// Expand to an abort site named `$name` (`crash-points` on) or to nothing
/// (`crash-points` off).
///
#[cfg(feature = "crash-points")]
macro_rules! crash_point {
    ($name:literal) => {
        $crate::crash::maybe_abort($name)
    };
}

/// With `crash-points` off a crash point expands to nothing.
#[cfg(not(feature = "crash-points"))]
macro_rules! crash_point {
    ($name:literal) => {};
}

pub(crate) use crash_point;
