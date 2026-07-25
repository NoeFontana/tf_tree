//! Safe Rust bindings to ROS 2's `tf2::BufferCore`, for the `tf_tree`
//! differential and benchmark harnesses.
//!
//! # Why this crate exists separately
//!
//! `tf_tree_bench` is `#![forbid(unsafe_code)]` and the workspace unsafe budget
//! (see `CLAUDE.md`) permits `unsafe` only in `tf_tree_arena` and
//! `tf_tree_core::{buffer, arena_view}`. FFI is irreducibly `unsafe`, so it is
//! isolated here in a dedicated `-sys` crate — the idiomatic Rust split, and the
//! one that leaves every existing crate's guarantee exactly as documented. This
//! crate is `publish = false`, is reached only through
//! `tf_tree_bench --features tf2`, and is never part of the shipped library.
//!
//! # SAFETY (module invariant)
//!
//! Every `unsafe` call below crosses into `src/shim.cpp`. The bridge is sound
//! because:
//!
//! * [`Tf2Buffer`] owns its handle. It is created by exactly one `tft2_new`,
//!   passed to no other owner, and freed by exactly one `tft2_free` in [`Drop`].
//!   The handle is never null (checked at construction) and never dangling (it
//!   outlives every borrow, being a private field).
//! * The C++ side catches **all** exceptions at the boundary and converts them
//!   to return codes. No unwinding crosses the FFI edge.
//! * Every `*const c_char` passed in is a [`CString`] that outlives the call.
//!   Frame names are validated to be NUL-free before conversion.
//! * Every pose buffer passed in or out is exactly `[f64; 7]`, matching the
//!   `double[7]` the shim reads and writes. The layout is
//!   `{qw, qx, qy, qz, tx, ty, tz}` on both sides — the same order as
//!   [`Iso3::to_bits`], so no reordering happens in Rust.
//!
//! `Tf2Buffer` is deliberately **not** `Sync`: `tf2::BufferCore` guards itself
//! with a mutex, but the `last_error` slot in the shim does not, so sharing one
//! handle across threads could interleave error messages. Benchmarks give each
//! thread its own buffer.

use std::ffi::{c_char, c_double, c_int, CStr, CString};
use std::marker::PhantomData;

use tf_tree_math::Iso3;

#[allow(non_camel_case_types)]
mod ffi {
    use std::ffi::{c_char, c_double, c_int, c_void};

    extern "C" {
        pub(super) fn tft2_new(cache_secs: c_double) -> *mut c_void;
        pub(super) fn tft2_free(h: *mut c_void);
        pub(super) fn tft2_set(
            h: *mut c_void,
            parent: *const c_char,
            child: *const c_char,
            stamp_ns: i64,
            pose: *const c_double,
            is_static: c_int,
        ) -> c_int;
        pub(super) fn tft2_lookup(
            h: *mut c_void,
            target: *const c_char,
            source: *const c_char,
            stamp_ns: i64,
            out: *mut c_double,
        ) -> c_int;
        pub(super) fn tft2_can_transform(
            h: *mut c_void,
            target: *const c_char,
            source: *const c_char,
            stamp_ns: i64,
        ) -> c_int;
        pub(super) fn tft2_clear(h: *mut c_void);
        pub(super) fn tft2_last_error(h: *mut c_void) -> *const c_char;
    }
}

/// Something the tf2 bridge refused to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tf2Error {
    /// `BufferCore` could not be allocated.
    Alloc,
    /// A frame name contained an interior NUL and cannot cross into C.
    FrameNameHasNul(String),
    /// A negative stamp. ROS time is unsigned; the caller must rebase its
    /// timeline so the earliest sample is at or after zero.
    NegativeStamp(i64),
    /// `setTransform` rejected the transform (tf2's own validation).
    SetRejected(String),
    /// `lookupTransform` threw — extrapolation, a disconnected pair, or an
    /// unknown frame. The string is tf2's own message.
    Lookup(String),
}

impl std::fmt::Display for Tf2Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Tf2Error::Alloc => write!(f, "could not allocate tf2::BufferCore"),
            Tf2Error::FrameNameHasNul(n) => {
                write!(f, "frame name {n:?} contains an interior NUL")
            }
            Tf2Error::NegativeStamp(t) => {
                write!(f, "stamp {t} is negative; ROS time is unsigned")
            }
            Tf2Error::SetRejected(m) => write!(f, "tf2 setTransform rejected it: {m}"),
            Tf2Error::Lookup(m) => write!(f, "tf2 lookupTransform failed: {m}"),
        }
    }
}

impl std::error::Error for Tf2Error {}

/// An owned `tf2::BufferCore`.
///
/// Mirrors the operations `tf_tree` exposes, so the differential harness can
/// drive both engines from one loop: insert transforms, then look them up.
pub struct Tf2Buffer {
    handle: *mut std::ffi::c_void,
    // `BufferCore` is internally locked, but the shim's error slot is not, so
    // this handle is Send-but-not-Sync. `PhantomData<Cell<()>>` projects exactly
    // that auto-trait profile regardless of the raw pointer field.
    _not_sync: PhantomData<std::cell::Cell<()>>,
}

// SAFETY: the handle is uniquely owned by this value and `BufferCore`'s own
// state is mutex-guarded, so moving one to another thread is sound. `Sync` is
// deliberately NOT implemented (see `_not_sync`).
unsafe impl Send for Tf2Buffer {}

impl Tf2Buffer {
    /// Create a buffer whose cache spans `cache_secs` of history.
    ///
    /// Size this to at least the span the harness will query; tf2 silently drops
    /// transforms older than the cache and then reports extrapolation.
    ///
    /// # Errors
    ///
    /// [`Tf2Error::Alloc`] if the allocation failed.
    pub fn new(cache_secs: f64) -> Result<Tf2Buffer, Tf2Error> {
        // SAFETY: module invariant — `tft2_new` allocates or returns null, and
        // takes no borrowed arguments.
        let handle = unsafe { ffi::tft2_new(cache_secs) };
        if handle.is_null() {
            return Err(Tf2Error::Alloc);
        }
        Ok(Tf2Buffer {
            handle,
            _not_sync: PhantomData,
        })
    }

    /// Insert `T_parent_child` at `stamp_ns`.
    ///
    /// `is_static` mirrors `/tf_static`: one entry, valid at any query time.
    ///
    /// # Errors
    ///
    /// [`Tf2Error::FrameNameHasNul`], [`Tf2Error::NegativeStamp`], or
    /// [`Tf2Error::SetRejected`] if tf2's own validation refused it.
    pub fn set_transform(
        &self,
        parent: &str,
        child: &str,
        stamp_ns: i64,
        pose: &Iso3,
        is_static: bool,
    ) -> Result<(), Tf2Error> {
        if stamp_ns < 0 {
            return Err(Tf2Error::NegativeStamp(stamp_ns));
        }
        let p = cstr(parent)?;
        let c = cstr(child)?;
        let bits = pose.to_bits();
        // `to_bits` is f64 bit patterns; the shim wants the values themselves.
        let vals: [f64; 7] = core::array::from_fn(|i| f64::from_bits(bits[i]));

        // SAFETY: module invariant — `self.handle` is live and uniquely owned;
        // `p`/`c` are NUL-terminated and outlive the call; `vals` is exactly the
        // `double[7]` the shim reads.
        let rc = unsafe {
            ffi::tft2_set(
                self.handle,
                p.as_ptr(),
                c.as_ptr(),
                stamp_ns,
                vals.as_ptr() as *const c_double,
                c_int::from(is_static),
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(Tf2Error::SetRejected(self.last_error()))
        }
    }

    /// Look up `T_target_source` at `stamp_ns`.
    ///
    /// # Errors
    ///
    /// [`Tf2Error::Lookup`] carrying tf2's own message — extrapolation, an
    /// unknown frame, or a disconnected pair.
    pub fn lookup(&self, target: &str, source: &str, stamp_ns: i64) -> Result<Iso3, Tf2Error> {
        if stamp_ns < 0 {
            return Err(Tf2Error::NegativeStamp(stamp_ns));
        }
        let t = cstr(target)?;
        let s = cstr(source)?;
        let mut out = [0.0f64; 7];

        // SAFETY: module invariant — `self.handle` is live; `t`/`s` are
        // NUL-terminated and outlive the call; `out` is exactly the `double[7]`
        // the shim writes, and is written only on rc == 0.
        let rc = unsafe {
            ffi::tft2_lookup(
                self.handle,
                t.as_ptr(),
                s.as_ptr(),
                stamp_ns,
                out.as_mut_ptr() as *mut c_double,
            )
        };
        if rc != 0 {
            return Err(Tf2Error::Lookup(self.last_error()));
        }
        let bits: [u64; 7] = core::array::from_fn(|i| out[i].to_bits());
        Ok(Iso3::from_bits(&bits))
    }

    /// Whether tf2 believes the lookup would succeed. Never throws.
    ///
    /// The differential uses this to compare only the queries *both* engines can
    /// answer, so a tf2 cache-horizon miss is not scored as a disagreement.
    #[must_use]
    pub fn can_transform(&self, target: &str, source: &str, stamp_ns: i64) -> bool {
        let (Ok(t), Ok(s)) = (cstr(target), cstr(source)) else {
            return false;
        };
        if stamp_ns < 0 {
            return false;
        }
        // SAFETY: module invariant — live handle, NUL-terminated names that
        // outlive the call. The shim swallows every exception.
        unsafe { ffi::tft2_can_transform(self.handle, t.as_ptr(), s.as_ptr(), stamp_ns) != 0 }
    }

    /// Drop every stored transform, keeping the buffer allocated.
    pub fn clear(&self) {
        // SAFETY: module invariant — live, uniquely-owned handle.
        unsafe { ffi::tft2_clear(self.handle) }
    }

    /// The most recent failure message from the C++ side.
    fn last_error(&self) -> String {
        // SAFETY: module invariant — `tft2_last_error` returns a NUL-terminated
        // pointer into a `std::string` owned by the live handle. It is copied
        // into an owned `String` before any further call can invalidate it.
        unsafe {
            let p: *const c_char = ffi::tft2_last_error(self.handle);
            if p.is_null() {
                return String::new();
            }
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}

impl Drop for Tf2Buffer {
    fn drop(&mut self) {
        // SAFETY: module invariant — the handle came from exactly one
        // `tft2_new`, was never duplicated, and is freed exactly once here.
        unsafe { ffi::tft2_free(self.handle) }
    }
}

/// Convert a frame name for the FFI boundary, rejecting interior NULs.
fn cstr(s: &str) -> Result<CString, Tf2Error> {
    CString::new(s).map_err(|_| Tf2Error::FrameNameHasNul(s.to_owned()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use tf_tree_math::exp_se3;

    fn pose(seed: f64) -> Iso3 {
        exp_se3([
            0.11 * seed,
            -0.23 * seed,
            0.37 * seed,
            1.5 * seed,
            -0.5 * seed,
            0.25 * seed,
        ])
    }

    /// The single most dangerous line in the shim is the quaternion
    /// transposition: tf2 stores `w` last, `Iso3` stores it first, and getting it
    /// wrong produces a *plausible* rotation rather than an obvious failure. A
    /// single-edge round trip at an exact stamp must return the pose bit-for-bit.
    #[test]
    fn quaternion_convention_round_trips_exactly() {
        let buf = Tf2Buffer::new(60.0).unwrap();
        for k in 1..8 {
            let p = pose(k as f64 * 0.3);
            let stamp = k as i64 * 1_000_000_000;
            buf.set_transform("map", "odom", stamp, &p, false).unwrap();
            let got = buf.lookup("map", "odom", stamp).unwrap();
            // tf2 stores doubles verbatim, so an exact-stamp lookup of a
            // single edge is a pure round trip through the two conventions.
            let (a, b) = (got.to_bits(), p.to_bits());
            assert_eq!(a, b, "round trip differs at k={k}: {got:?} vs {p:?}");
        }
    }

    /// tf2 must interpolate between samples, not hold the previous one — this is
    /// what makes it comparable to tf_tree's `LerpSlerp` policy at all.
    #[test]
    fn interpolates_between_samples() {
        let buf = Tf2Buffer::new(60.0).unwrap();
        let mut a = Iso3::IDENTITY;
        a.t.x = 0.0;
        let mut b = Iso3::IDENTITY;
        b.t.x = 10.0;
        buf.set_transform("map", "odom", 0, &a, false).unwrap();
        buf.set_transform("map", "odom", 1_000_000_000, &b, false)
            .unwrap();

        let mid = buf.lookup("map", "odom", 500_000_000).unwrap();
        assert!(
            (mid.t.x - 5.0).abs() < 1e-12,
            "expected LERP to 5.0, got {}",
            mid.t.x
        );
    }

    /// A static transform answers at any stamp; that is the `/tf_static`
    /// contract the bag replay depends on.
    #[test]
    fn static_transforms_answer_at_any_stamp() {
        let buf = Tf2Buffer::new(10.0).unwrap();
        let p = pose(0.7);
        buf.set_transform("base_link", "lidar", 0, &p, true)
            .unwrap();
        for &t in &[0i64, 5_000_000_000, 900_000_000_000] {
            let got = buf.lookup("base_link", "lidar", t).unwrap();
            assert_eq!(got.to_bits(), p.to_bits(), "static lookup at t={t}");
        }
    }

    /// Failures must arrive as errors carrying tf2's reason, never as an
    /// exception crossing the FFI boundary.
    #[test]
    fn failures_are_errors_not_unwinds() {
        let buf = Tf2Buffer::new(10.0).unwrap();
        let err = buf.lookup("nope", "also_nope", 0).unwrap_err();
        assert!(matches!(err, Tf2Error::Lookup(_)), "{err:?}");
        assert!(!buf.can_transform("nope", "also_nope", 0));
        // A negative stamp is caught in Rust before it can truncate in C++.
        assert_eq!(
            buf.lookup("a", "b", -1).unwrap_err(),
            Tf2Error::NegativeStamp(-1)
        );
    }

    /// A composed chain must match manual composition, confirming tf2's frame
    /// direction matches tf_tree's `T_parent_child` convention.
    #[test]
    fn chain_composition_matches_manual() {
        let buf = Tf2Buffer::new(60.0).unwrap();
        let mo = pose(0.4);
        let ob = pose(-0.9);
        buf.set_transform("map", "odom", 0, &mo, false).unwrap();
        buf.set_transform("odom", "base", 0, &ob, false).unwrap();

        // tf2's lookupTransform(target, source) returns T_target_source.
        let got = buf.lookup("map", "base", 0).unwrap();
        let want = mo * ob;
        let dt = got.t.sub(want.t).norm();
        assert!(dt < 1e-12, "chain differs by {dt}: {got:?} vs {want:?}");
    }

    #[test]
    fn buffer_is_send_but_not_sync() {
        fn assert_send<T: Send>() {}
        assert_send::<Tf2Buffer>();
        // `Sync` is intentionally absent; see the module SAFETY block.
    }
}
