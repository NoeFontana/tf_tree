//! The `tf2::BufferCore` differential seam — **compile-gated, not built here**.
//!
//! This module is behind `#[cfg(feature = "tf2")]`. It is the migration-
//! credibility test from decision `0003`: drive ROS 2's `tf2::BufferCore` with
//! the *identical* tree and sample stream as [`crate::fixture`] and compare
//! `lookupTransform` against tf_tree across 10⁵ random `LerpSlerp` queries,
//! asserting agreement within `1e-12`.
//!
//! It is **not** compiled or run in this PR: the build host has no ROS 2
//! (`/opt/ros` does not exist), so no tf2 numbers are produced and none are
//! claimed. The seam is laid out here so a ROS-equipped machine can finish it
//! without touching any other crate.
//!
//! # What a ROS-equipped machine needs to do
//!
//! 1. Install ROS 2 (Humble or newer) and source it (`. /opt/ros/<distro>/setup.bash`).
//! 2. Provide a C++ shim (a `build.rs` + `cc`/`cxx` bridge, or a small extern-"C"
//!    wrapper library) exposing `tf2::BufferCore::setTransform` and
//!    `lookupTransform`. `tf2` is a C++ library, so the FFI seam is a thin C++
//!    shim compiled and linked against `libtf2`.
//! 3. Fill in [`Tf2Buffer`] below to call that shim: `set_transform` mirrors each
//!    [`crate::fixture::PushSample`] (converting `Iso3` → `geometry_msgs`
//!    `TransformStamped`, remembering tf2 stores `w`-last quaternions — transpose
//!    the storage order), and `lookup` calls `lookupTransform` and converts back.
//! 4. Run `cargo test -p tf_tree_bench --features tf2 -- tf2_differential`.
//!
//! Enabling the feature without that shim + a linked `libtf2` is *expected* to
//! fail to link; that is the honest signal that the ROS toolchain is absent.

use anyhow::{anyhow, Result};

use tf_tree::Iso3;

/// A handle to a `tf2::BufferCore` behind the FFI shim.
///
/// The fields are intentionally empty until the C++ shim exists; the methods
/// return an error naming the missing seam rather than fabricating a result.
pub struct Tf2Buffer {
    _private: (),
}

impl Tf2Buffer {
    /// Construct a `BufferCore` with a cache long enough to hold the fixture's
    /// history.
    ///
    /// # Errors
    ///
    /// Always errors until the ROS 2 FFI shim (see the module docs) is wired in.
    pub fn new() -> Result<Tf2Buffer> {
        Err(anyhow!(
            "tf2::BufferCore FFI shim is not present; this needs a ROS 2 install \
             and the C++ bridge described in the `tf2` module docs"
        ))
    }

    /// Mirror one published transform into the `BufferCore`.
    ///
    /// `// FFI SEAM:` convert `T_parent_child` to a `geometry_msgs`
    /// `TransformStamped` (transpose the quaternion to tf2's `w`-last storage) and
    /// call `tf2::BufferCore::setTransform`.
    ///
    /// # Errors
    ///
    /// Always errors until the shim exists.
    pub fn set_transform(
        &self,
        _parent: &str,
        _child: &str,
        _stamp_ns: i64,
        _pose: &Iso3,
    ) -> Result<()> {
        Err(anyhow!("tf2 set_transform: FFI shim absent"))
    }

    /// Look up `T_target_source` at `stamp_ns` via `tf2::BufferCore::lookupTransform`.
    ///
    /// `// FFI SEAM:` call `lookupTransform` and convert the returned
    /// `TransformStamped` back to an [`Iso3`] (transpose the quaternion back to
    /// `w`-first).
    ///
    /// # Errors
    ///
    /// Always errors until the shim exists.
    pub fn lookup(&self, _target: &str, _source: &str, _stamp_ns: i64) -> Result<Iso3> {
        Err(anyhow!("tf2 lookup: FFI shim absent"))
    }
}
