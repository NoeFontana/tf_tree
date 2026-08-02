//! PyO3 bindings for `tf_tree` — `docs/PHASE3.md`.
//!
//! # The declaration that matters most — and its default has flipped (§1.2)
//!
//! `docs/PHASE3.md` §1.2 says an extension that does not declare itself
//! free-threading-safe silently re-enables the GIL for the whole process. That
//! was true of PyO3 <= 0.28. **In PyO3 0.29 the default is the opposite**:
//! `pyo3-macros-backend-0.29.0/src/module.rs:394` reads
//! `options.gil_used.is_some_and(|op| op.value.value)`, and `None` yields
//! `false` — so an *absent* attribute declares the module free-threading-safe.
//! Verified by experiment as well as by reading: removing the attribute below
//! leaves `sys._is_gil_enabled()` false.
//!
//! The hazard is now worse, not better. Forgetting the declaration used to cost
//! parallelism, loudly enough that somebody would eventually profile it.
//! Forgetting to *audit* now costs correctness: the module claims a safety it
//! may not have, and the failure is a data race rather than a slowdown.
//!
//! So the attribute stays — explicit beats inherited, and a future PyO3 could
//! flip the default back — but **it is not what makes the claim true, and no
//! test of the attribute can be non-vacuous.** What makes it true is that every
//! `#[pyclass]` here is `Send + Sync` and that there is no global mutable
//! state. `Tree` and `Plan` already are both in Rust; `tf_tree::Publisher` is
//! `Send + !Sync` by design, so [`PyPublisher`] does not hold one directly —
//! it holds an [`OwnedWriter`](tf_tree::OwnedWriter) behind a `Mutex`, which is
//! `Sync` because the writer is `Send`. That is what makes exposing it as
//! `tf_tree.Publisher` sound, and an earlier revision of this paragraph said
//! it was *not* exposed, which had not been true since it was added.
//!
//! **The `Send + Sync` half is the compiler's, not ours.** A `#[pyclass]`
//! without `unsendable` expands to an `assert_pyclass_send_sync::<Self>()`
//! (`pyo3-macros-backend-0.29.0/src/pyclass.rs:2932`), so a field that is not
//! both stops the build at the attribute — verified by giving `PyPublisher` a
//! `PhantomData<*const u8>`, which produces `error[E0277]: *const u8 cannot be
//! shared between threads safely` pointing at its `#[pyclass]` line. **The
//! no-global-mutable-state half is not checked by anything**, and that is the
//! one the concurrent evaluation test on a `3.14t` interpreter and
//! ThreadSanitizer (§7.3) are for.
//!
//! # Time is integer nanoseconds (§3)
//!
//! `float` stamps are rejected with a `TypeError` that states the measurement:
//! at a 2026 epoch the ULP of `float64` seconds is 238 ns, so **every**
//! consecutive-sample interval in a 1 kHz stream is wrong after a round trip.
//! For a library whose purpose is sub-millisecond temporal accuracy, silently
//! accepting that input would produce interpolation errors users would blame on
//! the interpolator.
//!
//! # No views into the arena (§5.1)
//!
//! Nothing here hands Python a buffer that aliases arena memory. An edge's
//! sample storage is a ring being overwritten by another process, and correct
//! reads go through the seqlock protocol; a NumPy array pointing into it would
//! bypass that entirely — a data race by construction, producing torn poses
//! that look like occasional impossible transforms. "Zero-copy" here means no
//! *intermediate* allocation: results are computed by interpolation and written
//! exactly once, into their final home.
#![allow(unsafe_code, clippy::needless_pass_by_value)]
// `unsafe` boundary: a foreign runtime that owns its own objects.
// See `docs/decisions/0007`.
#![deny(unsafe_op_in_unsafe_fn)]

use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;

mod errors;
mod offline;
mod tree;

pub use errors::*;
pub use offline::open_file;
pub use tree::*;

/// Nanoseconds from float seconds, and the exact reason it is lossy.
///
/// The only sanctioned path from a wall-clock float. Documented as lossy above
/// ~10^7 s rather than silently accepted, because the loss is invisible: the
/// value still *looks* like a timestamp.
///
/// **It now has exact siblings to point at**, which is what turns the warning
/// from true into actionable (`docs/API.md` §5.1): [`from_parts`] for a
/// `(sec, nanosec)` pair and [`from_ros`] for a `builtin_interfaces/Time`.
/// Neither takes a float and neither loses a bit.
#[pyfunction]
#[pyo3(signature = (seconds, /))]
fn from_sec(seconds: f64) -> PyResult<i64> {
    if !seconds.is_finite() {
        return Err(PyValueError::new_err("stamp must be finite"));
    }
    Ok((seconds * 1e9) as i64)
}

/// Nanoseconds in a second — the `[0, 1e9)` bound `from_parts` enforces.
const NANOS_PER_SEC: i64 = 1_000_000_000;

/// Exact nanoseconds from a `(sec, nanosec)` pair — `docs/API.md` §5.1.
///
/// The Python spelling of `Stamp::from_parts`, and it refuses exactly what that
/// refuses. **The refusals are the interesting half**, because both
/// alternatives are the silent wrongness §5.1 exists to remove:
///
/// * a `nanosec` outside `[0, 1e9)` is **refused, not normalised** — carrying a
///   malformed field into a plausible-looking stamp is how a wrong message
///   becomes an unexplainable transform;
/// * a sum outside `int64` is **refused, not wrapped** — a wrapped stamp lands
///   on the other side of the epoch and then compares, interpolates and prints
///   perfectly.
///
/// Note it is the *sum* that is range-checked and not the product: staging the
/// check would refuse a one-second band of representable stamps at the negative
/// end, exactly as the Rust side's comment records.
///
/// A negative `nanosec` is refused rather than being a type error, so that this
/// and `Stamp::from_timespec` agree: POSIX permits a negative `tv_nsec` only in
/// a *relative* interval, and converting one as an instant is a whole category
/// of wrong.
#[pyfunction]
#[pyo3(signature = (sec, nanosec, /))]
fn from_parts(sec: i64, nanosec: i64) -> PyResult<i64> {
    if !(0..NANOS_PER_SEC).contains(&nanosec) {
        return Err(PyValueError::new_err(format!(
            "nanosec must be in [0, 1000000000), got {nanosec}. It is refused \
             rather than normalised: a malformed field carried into a \
             plausible-looking stamp is unrecoverable downstream"
        )));
    }
    // `i128`, not a staged `checked_mul`/`checked_add`, for the reason the Rust
    // side records: the staged form refuses a one-second band of *representable*
    // stamps at the negative end. `i64 * 1e9 + u32` cannot overflow `i128`, so
    // this arrives with the exact answer in hand and the only question left is
    // whether it fits.
    let total = i128::from(sec) * i128::from(NANOS_PER_SEC) + i128::from(nanosec);
    i64::try_from(total).map_err(|_| {
        PyValueError::new_err(format!(
            "sec={sec}, nanosec={nanosec} is {total} ns, outside int64 \
             (+/-292 years). It is refused rather than wrapped: a wrapped stamp \
             lands on the other side of the epoch and still compares and \
             interpolates perfectly"
        ))
    })
}

/// Exact nanoseconds from a ROS 2 `builtin_interfaces/Time`.
///
/// ```python
/// t = tf_tree.from_ros(msg.header.stamp)
/// ```
///
/// **Never via `to_sec()`** (`docs/PHASE3.md` §13, `docs/API.md` §5.1): the
/// message is `{int32 sec, uint32 nanosec}` and converts exactly, so a float
/// round trip would destroy precision this API exists to preserve — at a 2026
/// epoch the ULP of `float64` seconds is 238 ns, which is every interval in a
/// 1 kHz stream.
///
/// # Duck-typed, and deliberately
///
/// It reads `.sec` and `.nanosec` off whatever it is handed. **`rclpy` is not a
/// dependency of this wheel and must not become one** — the package needs only
/// NumPy, and a binding that imported `rclpy` to read two integers would be
/// unusable in the notebook and the dataloader that are most of its users. Any
/// object with those two fields works: the real message, a `dataclass`, a
/// `SimpleNamespace` in a test.
///
/// Refusals are [`from_parts`]'s, unchanged.
#[pyfunction]
#[pyo3(signature = (stamp, /))]
fn from_ros(stamp: &Bound<'_, PyAny>) -> PyResult<i64> {
    let field = |name: &str| -> PyResult<i64> {
        let v = stamp.getattr(name).map_err(|_| {
            PyTypeError::new_err(format!(
                "expected a builtin_interfaces/Time (anything with .sec and \
                 .nanosec); this object has no .{name}. An rclpy.time.Time is \
                 already integer nanoseconds — use its .nanoseconds directly"
            ))
        })?;
        v.extract::<i64>()
    };
    from_parts(field("sec")?, field("nanosec")?)
}

/// Whether this build can share a tree between processes.
///
/// Compile-time on the Rust side (`shm` + Linux), so a caller does not have to
/// infer it from an error it was going to get anyway (§4.1).
#[pyfunction]
fn has_shared_memory() -> bool {
    // This crate always builds `tf_tree` with `shm`, so the only remaining
    // question is the platform. On macOS and Windows `open()` gives an
    // in-process tree with a documented one-process limitation (§10), and a
    // caller should be able to branch on that rather than infer it from an
    // error it was going to get.
    cfg!(target_os = "linux")
}

/// `tf_tree` — a transform tree engine.
#[pymodule(gil_used = false)]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(from_sec, m)?)?;
    m.add_function(wrap_pyfunction!(from_parts, m)?)?;
    m.add_function(wrap_pyfunction!(from_ros, m)?)?;
    m.add_function(wrap_pyfunction!(has_shared_memory, m)?)?;
    m.add_function(wrap_pyfunction!(tree::build, m)?)?;
    m.add_function(wrap_pyfunction!(tree::push, m)?)?;
    m.add_function(wrap_pyfunction!(tree::open_arena, m)?)?;
    m.add_function(wrap_pyfunction!(offline::open_file, m)?)?;
    m.add_class::<PyTree>()?;
    m.add_class::<PyPlan>()?;
    m.add_class::<PyPublisher>()?;
    errors::register(m)?;
    Ok(())
}
