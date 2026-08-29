//! Named, deterministic abort sites — `docs/PHASE2.md` §11.3.
//!
//! §11.3 is NORMATIVE and calls itself "the core of this phase": a participant
//! killed at *any* instruction must leave state another participant can repair.
//! A `SIGKILL` from outside lands wherever the scheduler puts it, which is a
//! different and much shallower set of states — which is why
//! `shm_torture --crash-points` refuses to be quoted as §11.3 coverage. This
//! module is how a test names the instruction instead.
//!
//! # The mechanism
//!
//! `crash_point!("<name>")` marks a point inside a mutation protocol. Under the
//! default-off `crash-points` feature it expands to **nothing**. With the feature
//! on it expands to `maybe_abort`, which fires when the process was started
//! with
//!
//! ```text
//! TF_TREE_CRASH_AT=<name>:<nth_hit>
//! ```
//!
//! and this is the `nth_hit`th time *that* site has been reached. `:<nth_hit>`
//! may be omitted and then means `:1`. One site is armed per process, which is
//! all §11.3 asks for: the variable names exactly one, so there is a single
//! parse and a single hit counter rather than a map.
//!
//! The sites compiled into this crate are listed in `SITES` (which exists only
//! under the feature, like the sites themselves); the other rows of
//! §11.3's table live in the crates that own those protocols
//! (`open.*`, `hangup.*`, `reclaim.*` and `takeover.*` in the rendezvous, and
//! `topo.holding_lock` in the facade's `reparent`). `attach.*` is here, because
//! the window it names is inside `participant::fill_slot` — `tf_tree_ipc` takes
//! the byte, but the arena record is this crate's.
//!
//! # Why `abort`, not `panic!`
//!
//! §11.3, exactly: the variable "`abort()`s (not `panic!` — a panic unwinds and
//! runs `Drop`, which would clean up and defeat the test)". That is not a style
//! preference here. [`crate::edge::Publisher`]'s `Drop` releases the claim and
//! [`crate::participant::ParticipantTable::release`] frees the slot: unwinding
//! out of a crash point would repair, on the way out, precisely the damage the
//! test exists to observe, and every §11.3 row would then pass vacuously.
//!
//! So this path calls `std::process::abort`, and it takes care not to reach a
//! `panic!` on the way: the diagnostic goes out through
//! `std::io::Write` with its error discarded, rather than `eprintln!`, which
//! panics if the write fails.
//!
//! # `no_std`
//!
//! Reading an environment variable needs `std`. The feature therefore pulls
//! `std` in **for itself** — an `extern crate std` under the feature's `cfg` in
//! the crate root — rather than through a `std` feature that a default build
//! could end up enabling by unification. `#![no_std]` on the crate root is
//! unconditional and a default build links no `std`.

/// Every crash point compiled into **this crate**, in `docs/PHASE2.md` §11.3's
/// table order.
///
/// A harness that arms one at random (§11.4: "a random crash point armed in 10%
/// of children") reads it from here rather than re-spelling the literals in
/// another crate, where a typo would silently arm nothing and the run would look
/// clean.
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

/// The armed `(name, nth_hit)`, or `None` when the variable is absent, empty, or
/// does not parse.
///
/// A malformed value disarms rather than aborting at the first site it meets:
/// the caller of a fault-injection run is a test harness, and "everything died
/// immediately" is the least informative way to report a typo. Each site's test
/// asserts the abort, so a disarming typo fails loudly there instead.
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
///
/// Returns normally — after one atomic load and a string compare at worst — in
/// every other case, which is every call in a process that armed a different
/// site or none at all.
///
/// # Aborts
///
/// By `std::process::abort`, on the `nth_hit`th call naming the armed site.
/// **Not** a panic: see the module documentation for why that distinction is the
/// whole mechanism.
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
    // Not `eprintln!`: that panics on a write error, and a panic here unwinds
    // through the very `Drop`s this abort exists to skip. The results are
    // discarded for the same reason.
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
/// The expansion is `docs/PHASE2.md` §11.3's, verbatim.
#[cfg(feature = "crash-points")]
macro_rules! crash_point {
    ($name:literal) => {
        $crate::crash::maybe_abort($name)
    };
}

/// The no-op arm: with `crash-points` off a crash point expands to nothing, so
/// it costs nothing — no call, no branch, no atomic load, nothing for the
/// optimiser to have an opinion about.
#[cfg(not(feature = "crash-points"))]
macro_rules! crash_point {
    ($name:literal) => {};
}

pub(crate) use crash_point;
