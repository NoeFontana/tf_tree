//! The `tf2::BufferCore` differential seam, behind `--features tf2` (needs ROS 2;
//! `just tf2-differential` runs it in a container).
//!
//! Only turns the shared [`crate::fixture`] into a stream tf2 can consume; the
//! comparison lives in [`crate::differential`]. Stamps start at 0, so ROS's
//! unsigned time needs no rebasing ([`tf_tree_tf2_sys::Tf2Error::NegativeStamp`]
//! catches a change), and the buffer is sized past the fixture history so tf2's
//! cache horizon never truncates the comparison.

use anyhow::{anyhow, Result};

use tf_tree::Iso3;
use tf_tree_tf2_sys::Tf2Buffer;

use crate::fixture::{self, EdgeDefKind, EDGES};

/// Cache span: the fixture's history plus slack.
const CACHE_SECS: f64 = fixture::HISTORY_SECS * 3.0;

/// A `tf2::BufferCore` loaded with the fixture's topology and history.
pub struct Tf2Fixture {
    buffer: Tf2Buffer,
}

impl Tf2Fixture {
    /// Replay the identical declarations and samples `fixture::spin_up`
    /// publishes: static edges once with tf2's static flag, dynamic edges per
    /// sample.
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
    /// `None`, not an error: the differential scores only queries both engines resolve.
    #[must_use]
    pub fn lookup(&self, target: &str, source: &str, stamp_ns: i64) -> Option<Iso3> {
        self.buffer.lookup(target, source, stamp_ns).ok()
    }

    /// The underlying buffer, for benchmarks that need to time raw tf2 calls.
    #[must_use]
    pub fn buffer(&self) -> &Tf2Buffer {
        &self.buffer
    }

    /// Consume the fixture, yielding just the loaded buffer.
    #[must_use]
    pub fn into_buffer(self) -> Tf2Buffer {
        self.buffer
    }
}
