//! Loading a recorded [`TfStream`] into a `tf2::BufferCore`.
//!
//! Separate from [`crate::replay`] so that module stays ROS-free and builds
//! everywhere; this one is reached only behind `--features tf2`.

use anyhow::{anyhow, Result};

use tf_tree_tf2_sys::Tf2Buffer;

use crate::replay::TfStream;

/// Load a recording into a `tf2::BufferCore`, replaying the identical stream the
/// engine receives.
///
/// The cache is sized to the recording's own duration plus generous slack, so
/// tf2's horizon never truncates a query the engine can answer — otherwise a
/// horizon miss would show up as a disagreement rather than as a decline.
///
/// # Errors
///
/// If the buffer cannot be allocated, or tf2 rejects a transform the recording
/// contains.
pub fn load_tf2(stream: &TfStream) -> Result<Tf2Buffer> {
    let span_ns = stream
        .samples
        .last()
        .map_or(0, |s| s.stamp_ns)
        .saturating_sub(stream.samples.first().map_or(0, |s| s.stamp_ns));
    let cache_secs = (span_ns as f64 / 1e9).mul_add(2.0, 60.0);

    let buffer = Tf2Buffer::new(cache_secs).map_err(|e| anyhow!("tf2 buffer: {e}"))?;

    for (parent, child, pose) in &stream.static_edges {
        buffer
            .set_transform(parent, child, 0, pose, true)
            .map_err(|e| anyhow!("tf2 static {parent}->{child}: {e}"))?;
    }
    for s in &stream.samples {
        let (parent, child) = &stream.dynamic_edges[s.edge];
        buffer
            .set_transform(parent, child, s.stamp_ns, &s.pose, false)
            .map_err(|e| anyhow!("tf2 dynamic {parent}->{child}@{}: {e}", s.stamp_ns))?;
    }
    Ok(buffer)
}
