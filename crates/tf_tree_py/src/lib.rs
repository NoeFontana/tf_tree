//! PyO3 bindings for `tf_tree` — `docs/PHASE3.md`.
//!
//! # Free-threading declaration (§1.2)
//!
//! PyO3 0.29 treats an absent `gil_used` as free-threading-safe; the attribute
//! stays explicit, but what makes the claim true is that every `#[pyclass]` is
//! `Send + Sync` (the compiler asserts it) and there is no global mutable state
//! (checked only by the `3.14t` concurrency test and ThreadSanitizer, §7.3).
//! [`PyPublisher`] holds an [`OwnedWriter`](tf_tree::OwnedWriter) behind a
//! `Mutex`, since `tf_tree::Publisher` is `Send + !Sync`.
//!
//! # Time is integer nanoseconds (§3)
//!
//! `float` stamps are rejected with a `TypeError`: at a 2026 epoch the ULP of
//! `float64` seconds is 238 ns, wrong for every interval of a 1 kHz stream.
//!
//! # Build identity
//!
//! `__version__` is `env!("CARGO_PKG_VERSION")`, never a literal;
//! `importlib.metadata.version("transform_tree")` is canonical and
//! `tests/python/test_version.py` asserts they agree. `arena_format_version` and
//! `arena_layout_hash` are the words compared on attach (`docs/PHASE5.md` §1),
//! forwarded from the facade; they are functions, not constants, and are
//! code spans here because `#[pyfunction]` items cannot be intra-doc linked.
//!
//! # No views into the arena (§5.1)
//!
//! Nothing hands Python a buffer aliasing arena memory: a NumPy view would
//! bypass the seqlock and race with the writer.
#![allow(unsafe_code, clippy::needless_pass_by_value)]
// `unsafe` boundary: a foreign runtime that owns its own objects.
// See `docs/decisions/0007`.
#![deny(unsafe_op_in_unsafe_fn)]

use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;

mod errors;
mod ingest;
mod offline;
mod tree;

pub use errors::*;
pub use offline::open_file;
pub use tree::*;

/// Nanoseconds from float seconds; lossy above ~10^7 s.
///
/// The only path from a wall-clock float. Prefer `from_parts` for a
/// `(sec, nanosec)` pair and `from_ros` for a `builtin_interfaces/Time`, which
/// lose nothing (`docs/API.md` §5.1).
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
/// Refuses a `nanosec` outside `[0, 1e9)` (not normalised) and a total outside
/// `int64` (not wrapped); the sum is range-checked, not the product. Agrees with
/// `Stamp::from_parts`.
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
    // `i128`: a staged checked_mul/add would refuse representable stamps at the negative end.
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
/// Never via `to_sec()` (`docs/PHASE3.md` §13): a float round trip destroys
/// precision. Duck-typed on `.sec` and `.nanosec`, so `rclpy` is not a
/// dependency. Refusals are those of `from_parts`.
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

/// The wall-clock domain: `CLOCK_REALTIME`, ROS `/clock` off, tag `0`.
///
/// One of four names (`docs/decisions/0038-the-domain-a-binding-cannot-name.md`)
/// for `domain=` on `tree.plan(...)` / `tree.lookup(...)`. They are `int`s
/// because `tf_tree::Domain` is an open trait whose tags from `4` up belong to
/// the caller (`docs/API.md` §2.5). Not `open_arena`'s `domain=`, the `u32`
/// rendezvous namespace.
pub const SYSTEM_DOMAIN: u8 = <tf_tree::SystemDomain as tf_tree::Domain>::TAG;

/// A sensor's own clock — a lidar or camera stamping from its own oscillator,
/// undisciplined against the host. Tag `1`; see [`SYSTEM_DOMAIN`].
pub const SENSOR_DOMAIN: u8 = <tf_tree::SensorDomain as tf_tree::Domain>::TAG;

/// Simulated time — ROS `use_sim_time`, `/clock`. Tag `2`; see [`SYSTEM_DOMAIN`].
pub const SIM_DOMAIN: u8 = <tf_tree::SimDomain as tf_tree::Domain>::TAG;

/// A monotonic clock — `CLOCK_MONOTONIC`, boot-relative and never stepped.
/// Tag `3`; see [`SYSTEM_DOMAIN`].
pub const STEADY_DOMAIN: u8 = <tf_tree::SteadyDomain as tf_tree::Domain>::TAG;

/// Whether this build can share a tree between processes.
///
/// Compile-time (`shm` + Linux); elsewhere `open()` gives an in-process tree
/// (§10, §4.1).
#[pyfunction]
fn has_shared_memory() -> bool {
    cfg!(target_os = "linux")
}

/// This build's arena format version — the *set of fields* in the header.
///
/// A different version is never compatible: no conversion layer
/// (`docs/PHASE5.md` §1). Takes no lock and needs no arena.
#[pyfunction]
fn arena_format_version() -> u32 {
    tf_tree::arena_format_version()
}

/// This build's arena layout hash — the *geometry*, as distinct from the
/// format version's set of fields.
///
/// A mismatch on either word is refused on attach. Returned as an `int`;
/// format as `f"0x{tf_tree.arena_layout_hash():08X}"` to match
/// `tft doctor --explain-version`.
#[pyfunction]
fn arena_layout_hash() -> u32 {
    tf_tree::arena_layout_hash()
}

/// `tf_tree` — a transform tree engine.
#[pymodule(gil_used = false)]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    m.add_function(wrap_pyfunction!(arena_format_version, m)?)?;
    m.add_function(wrap_pyfunction!(arena_layout_hash, m)?)?;
    m.add_function(wrap_pyfunction!(from_sec, m)?)?;
    m.add_function(wrap_pyfunction!(from_parts, m)?)?;
    m.add_function(wrap_pyfunction!(from_ros, m)?)?;
    m.add_function(wrap_pyfunction!(has_shared_memory, m)?)?;
    m.add("SYSTEM_DOMAIN", SYSTEM_DOMAIN)?;
    m.add("SENSOR_DOMAIN", SENSOR_DOMAIN)?;
    m.add("SIM_DOMAIN", SIM_DOMAIN)?;
    m.add("STEADY_DOMAIN", STEADY_DOMAIN)?;
    m.add_function(wrap_pyfunction!(tree::build, m)?)?;
    m.add_function(wrap_pyfunction!(tree::push, m)?)?;
    m.add_function(wrap_pyfunction!(tree::open_arena, m)?)?;
    m.add_function(wrap_pyfunction!(offline::open_file, m)?)?;
    // No `freeze_bag`: `ingest_bag(p).freeze(out)` is the path (`docs/decisions/0046`).
    m.add_function(wrap_pyfunction!(ingest::ingest_bag, m)?)?;
    m.add_class::<PyTree>()?;
    m.add_class::<PyPlan>()?;
    m.add_class::<PyPublisher>()?;
    errors::register(m)?;
    Ok(())
}
