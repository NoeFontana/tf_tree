//! The `tf2::BufferCore` differential seam — the migration-credibility test.
//!
//! Compile-gated behind `--features tf2`, which pulls in `tf_tree_tf2_sys` (the
//! FFI bridge; see that crate for why the `unsafe` lives there and not here).
//! Building this module needs a ROS 2 install; `just tf2-differential` runs it
//! in a container so no host setup is required.
//!
//! # What this module owns
//!
//! Turning the shared [`crate::fixture`] into a stream tf2 can consume, and
//! nothing else. The comparison logic lives in [`crate::differential`], so the
//! tf2 and naive-Rust references go through the identical query loop and any
//! disagreement is attributable to the engine, not the harness.
//!
//! # Two conventions this has to reconcile
//!
//! * **Time.** tf_tree stamps are `i64` nanoseconds and may be negative; ROS
//!   time is unsigned. The fixture starts at 0, so no rebasing is needed, but
//!   [`tf_tree_tf2_sys::Tf2Error::NegativeStamp`] catches it if that ever changes.
//! * **Cache horizon.** tf2 drops transforms older than its cache and then
//!   reports extrapolation. The buffer is sized to the fixture's full history
//!   plus slack so the horizon never silently truncates the comparison.

use anyhow::{anyhow, Result};

use tf_tree::Iso3;
use tf_tree_tf2_sys::Tf2Buffer;

use crate::fixture::{self, EdgeDefKind, EDGES};

/// Cache span for the comparison buffer: the fixture's history plus generous
/// slack, so tf2's horizon never truncates a query the engine can answer.
const CACHE_SECS: f64 = fixture::HISTORY_SECS * 3.0;

/// A `tf2::BufferCore` loaded with the fixture's topology and history.
pub struct Tf2Fixture {
    buffer: Tf2Buffer,
}

impl Tf2Fixture {
    /// Build a `BufferCore` and replay the *identical* declarations and sample
    /// stream the engine tree receives.
    ///
    /// Static edges are inserted once with tf2's static flag (`/tf_static`
    /// semantics). Dynamic edges are replayed sample by sample, reproducing the
    /// same `dynamic_pose(seed, stamp)` values `fixture::spin_up` publishes — so
    /// the two engines hold bit-identical inputs and every observed difference is
    /// a difference in lookup, not in data.
    ///
    /// # Errors
    ///
    /// If the buffer cannot be allocated or tf2 rejects a transform.
    pub fn load() -> Result<Tf2Fixture> {
        let buffer = Tf2Buffer::new(CACHE_SECS).map_err(|e| anyhow!("tf2 buffer: {e}"))?;

        let mut dyn_seed = 0.0f64;
        for e in EDGES {
            match e.kind {
                EdgeDefKind::Static { xi } => {
                    let pose = tf_tree_math::exp_se3(xi);
                    buffer
                        .set_transform(e.parent, e.child, 0, &pose, true)
                        .map_err(|err| anyhow!("tf2 static {}->{}: {err}", e.parent, e.child))?;
                }
                EdgeDefKind::Dynamic { rate_hz } => {
                    let period_ns = (1e9 / rate_hz) as i64;
                    let count = (fixture::HISTORY_SECS * rate_hz) as i64;
                    for k in 0..count {
                        let stamp = k * period_ns;
                        let pose = fixture::dynamic_pose(dyn_seed, stamp);
                        buffer
                            .set_transform(e.parent, e.child, stamp, &pose, false)
                            .map_err(|err| {
                                anyhow!("tf2 dynamic {}->{}@{stamp}: {err}", e.parent, e.child)
                            })?;
                    }
                    dyn_seed += 1.0;
                }
            }
        }

        Ok(Tf2Fixture { buffer })
    }

    /// `T_target_source` at `stamp_ns` per tf2, or `None` if tf2 cannot answer
    /// (extrapolation past its horizon, or an unknown pair).
    ///
    /// Returning `None` rather than an error is deliberate: the differential
    /// scores only the queries *both* engines can resolve, so a tf2-side horizon
    /// miss is skipped rather than counted as a disagreement.
    #[must_use]
    pub fn lookup(&self, target: &str, source: &str, stamp_ns: i64) -> Option<Iso3> {
        self.buffer.lookup(target, source, stamp_ns).ok()
    }

    /// The underlying buffer, for benchmarks that need to time raw tf2 calls.
    #[must_use]
    pub fn buffer(&self) -> &Tf2Buffer {
        &self.buffer
    }
}
