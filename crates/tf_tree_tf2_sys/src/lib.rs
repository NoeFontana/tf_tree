//! Safe Rust bindings to ROS 2's `tf2::BufferCore`, for the `tf_tree` differential and
//! benchmark harnesses.
//!
//! A `-sys` crate because FFI is `unsafe` and `tf_tree_bench`'s library root forbids it;
//! `publish = false`. Unsafe budget kind 3 (`0007`, `0048`).
//!
//! # SAFETY (module invariant)
//!
//! Every `unsafe` call below crosses into `src/shim.cpp`, which is sound because:
//!
//! * [`Tf2Buffer`] owns its handle: one `tft2_new`, one `tft2_free` in [`Drop`]; never null,
//!   never dangling.
//! * The C++ side catches **all** exceptions and returns codes; no unwinding crosses FFI.
//! * Every `*const c_char` is a [`CString`] outliving the call, NUL-free by validation.
//! * Every `const void*` name handle is a live `std::string` owned by a borrowed
//!   [`FrameName`], only read.
//! * Every pose buffer is `[f64; 7]` = `double[7]`, `{qw, qx, qy, qz, tx, ty, tz}` on both
//!   sides ([`Iso3::to_bits`]).
//!
//! `Tf2Buffer` is `Send + Sync`: `BufferCore` locks internally and the shim's last-error slot
//! is `thread_local`. Sharing **one** buffer across reader threads is deliberate: it is what
//! the concurrent read benchmark measures.

use std::ffi::{c_char, c_double, c_int, CStr, CString};

use tf_tree_math::Iso3;

#[allow(non_camel_case_types)]
mod ffi {
    use std::ffi::{c_char, c_double, c_int, c_void};

    extern "C" {
        pub(super) fn tft2_new(cache_secs: c_double) -> *mut c_void;
        pub(super) fn tft2_free(h: *mut c_void);
        pub(super) fn tft2_set_pre(
            h: *mut c_void,
            parent: *const c_void,
            child: *const c_void,
            stamp_ns: i64,
            pose: *const c_double,
            is_static: c_int,
        ) -> c_int;
        pub(super) fn tft2_can_transform(
            h: *mut c_void,
            target: *const c_char,
            source: *const c_char,
            stamp_ns: i64,
        ) -> c_int;
        pub(super) fn tft2_lookup_noop(
            h: *mut c_void,
            target: *const c_void,
            source: *const c_void,
            stamp_ns: i64,
            out: *mut c_double,
        ) -> c_int;
        pub(super) fn tft2_name_new(s: *const c_char) -> *mut c_void;
        pub(super) fn tft2_name_free(n: *mut c_void);
        pub(super) fn tft2_lookup_pre(
            h: *mut c_void,
            target: *const c_void,
            source: *const c_void,
            stamp_ns: i64,
            out: *mut c_double,
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
    /// A negative stamp (ROS time is unsigned); rebase the timeline to start at zero.
    NegativeStamp(i64),
    /// `setTransform` rejected the transform (tf2's own validation).
    SetRejected(String),
    /// `lookupTransform` threw; the string is tf2's own message.
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
/// Mirrors `tf_tree`'s insert and lookup so the differential harness drives both engines.
pub struct Tf2Buffer {
    handle: *mut std::ffi::c_void,
}

// SAFETY: the handle is uniquely owned; everything behind it is mutex-guarded by
// `BufferCore` or `thread_local` in the shim, so it may be moved and shared across threads.
unsafe impl Send for Tf2Buffer {}
// SAFETY: see the `Send` impl above.
unsafe impl Sync for Tf2Buffer {}

impl Tf2Buffer {
    /// Create a buffer whose cache spans `cache_secs` of history.
    ///
    /// Size it to the queried span; tf2 silently drops older transforms.
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
        Ok(Tf2Buffer { handle })
    }

    /// Insert `T_parent_child` at `stamp_ns`, taking `&str` frame names.
    ///
    /// `is_static` mirrors `/tf_static`: one entry, valid at any query time.
    ///
    /// **Not for benchmarks:** converts both names per call; use [`Self::set_transform_by_name`].
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
        self.set_transform_by_name(
            &FrameName::new(parent)?,
            &FrameName::new(child)?,
            stamp_ns,
            pose,
            is_static,
        )
    }

    /// Insert `T_parent_child` with pre-converted frame names.
    ///
    /// The allocation-free publish path; benchmarks must use it.
    ///
    /// # Errors
    ///
    /// As [`Self::set_transform`].
    pub fn set_transform_by_name(
        &self,
        parent: &FrameName,
        child: &FrameName,
        stamp_ns: i64,
        pose: &Iso3,
        is_static: bool,
    ) -> Result<(), Tf2Error> {
        if stamp_ns < 0 {
            return Err(Tf2Error::NegativeStamp(stamp_ns));
        }
        let bits = pose.to_bits();
        let vals: [f64; 7] = core::array::from_fn(|i| f64::from_bits(bits[i]));

        // SAFETY: module invariant; `vals` is exactly the `double[7]` the shim reads.
        let rc = unsafe {
            ffi::tft2_set_pre(
                self.handle,
                parent.cpp,
                child.cpp,
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

    /// Look up `T_target_source` at `stamp_ns`, taking `&str` frame names.
    ///
    /// **Not for benchmarks:** allocates per name per call; use [`Self::lookup_by_name`].
    ///
    /// # Errors
    ///
    /// [`Tf2Error::Lookup`] carrying tf2's own message.
    pub fn lookup(&self, target: &str, source: &str, stamp_ns: i64) -> Result<Iso3, Tf2Error> {
        self.lookup_by_name(&FrameName::new(target)?, &FrameName::new(source)?, stamp_ns)
    }

    /// Look up `T_target_source` at `stamp_ns` with pre-converted frame names.
    ///
    /// The allocation-free hot path; benchmarks should use only this.
    ///
    /// # Errors
    ///
    /// As [`Self::lookup`].
    pub fn lookup_by_name(
        &self,
        target: &FrameName,
        source: &FrameName,
        stamp_ns: i64,
    ) -> Result<Iso3, Tf2Error> {
        if stamp_ns < 0 {
            return Err(Tf2Error::NegativeStamp(stamp_ns));
        }
        let mut out = [0.0f64; 7];

        // SAFETY: module invariant; `out` is the `double[7]` the shim writes, only on rc == 0.
        let rc = unsafe {
            ffi::tft2_lookup_pre(
                self.handle,
                target.cpp,
                source.cpp,
                stamp_ns,
                out.as_mut_ptr().cast::<c_double>(),
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
    /// The differential compares only queries both engines can answer.
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
        // SAFETY: module invariant — a NUL-terminated pointer into the live handle's
        // `std::string`, copied before any further call.
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
        // SAFETY: module invariant — freed exactly once here.
        unsafe { ffi::tft2_free(self.handle) }
    }
}

/// A frame name prepared **once** for the FFI boundary, as a C++ `std::string`.
///
/// `lookupTransform` takes `const std::string&`; owning one avoids a per-call temporary.
/// Immutable after construction, hence `Send + Sync`.
#[derive(Debug)]
pub struct FrameName {
    /// Owning pointer to a heap `std::string` from `tft2_name_new`.
    cpp: *mut std::ffi::c_void,
    /// The Rust-side name, kept for diagnostics and for the `&str` API.
    text: String,
}

// SAFETY: `cpp` is written once at construction and only read afterwards; no interior
// mutability, so moving and sharing are sound.
unsafe impl Send for FrameName {}
// SAFETY: see the `Send` impl above.
unsafe impl Sync for FrameName {}

impl FrameName {
    /// Prepare a frame name, rejecting interior NULs.
    ///
    /// # Errors
    ///
    /// [`Tf2Error::FrameNameHasNul`] if the name cannot cross into C, or
    /// [`Tf2Error::Alloc`] if the C++ string could not be allocated.
    pub fn new(s: &str) -> Result<FrameName, Tf2Error> {
        let c = cstr(s)?;
        // SAFETY: module invariant — `c` is NUL-terminated and outlives the
        // call; `tft2_name_new` copies it into an owned `std::string`.
        let cpp = unsafe { ffi::tft2_name_new(c.as_ptr()) };
        if cpp.is_null() {
            return Err(Tf2Error::Alloc);
        }
        Ok(FrameName {
            cpp,
            text: s.to_owned(),
        })
    }

    /// The name as a `&str`, for diagnostics.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }
}

impl Drop for FrameName {
    fn drop(&mut self) {
        // SAFETY: module invariant — `cpp` is never duplicated (not `Clone`), freed once here.
        unsafe { ffi::tft2_name_free(self.cpp) }
    }
}

/// Convert a frame name for the FFI boundary, rejecting interior NULs.
fn cstr(s: &str) -> Result<CString, Tf2Error> {
    CString::new(s).map_err(|_| Tf2Error::FrameNameHasNul(s.to_owned()))
}

/// Everything [`Tf2Buffer::lookup_by_name`] does except the `BufferCore` call.
///
/// # Errors
///
/// Never in practice; the signature mirrors the real call.
pub fn lookup_overhead_probe(
    buffer: &Tf2Buffer,
    target: &FrameName,
    source: &FrameName,
    stamp_ns: i64,
) -> Result<Iso3, Tf2Error> {
    let mut out = [0.0f64; 7];
    // SAFETY: module invariant — live handle, NUL-terminated names outliving the
    // call, and a `double[7]` the shim writes exactly seven doubles into.
    let rc = unsafe {
        ffi::tft2_lookup_noop(
            buffer.handle,
            target.cpp,
            source.cpp,
            stamp_ns,
            out.as_mut_ptr().cast::<c_double>(),
        )
    };
    if rc != 0 {
        return Err(Tf2Error::Lookup("overhead probe failed".to_owned()));
    }
    let bits: [u64; 7] = core::array::from_fn(|i| out[i].to_bits());
    Ok(Iso3::from_bits(&bits))
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

    /// tf2 stores `w` last, `Iso3` first; a wrong transposition gives a plausible rotation.
    /// A single-edge round trip must return the pose bit-for-bit.
    #[test]
    fn quaternion_convention_round_trips_exactly() {
        let buf = Tf2Buffer::new(60.0).unwrap();
        for k in 1..8 {
            let p = pose(k as f64 * 0.3);
            let stamp = k as i64 * 1_000_000_000;
            buf.set_transform("map", "odom", stamp, &p, false).unwrap();
            let got = buf.lookup("map", "odom", stamp).unwrap();
            let (a, b) = (got.to_bits(), p.to_bits());
            assert_eq!(a, b, "round trip differs at k={k}: {got:?} vs {p:?}");
        }
    }

    /// tf2 must interpolate, not hold: that is what makes it comparable to `LerpSlerp`.
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

    /// A static transform answers at any stamp (`/tf_static`).
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

    /// Failures arrive as errors, never as an exception crossing FFI.
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

    /// A composed chain matches manual composition (`T_parent_child` direction).
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

    /// One buffer, many reader threads.
    #[test]
    fn buffer_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Tf2Buffer>();
    }

    /// Concurrent readers of one buffer all get the right answer; a failing lookup on one
    /// thread cannot corrupt another's error.
    #[test]
    fn concurrent_readers_share_one_buffer() {
        let buf = Tf2Buffer::new(60.0).unwrap();
        let p = pose(0.6);
        buf.set_transform("map", "odom", 0, &p, false).unwrap();
        buf.set_transform("map", "odom", 1_000_000_000, &p, false)
            .unwrap();

        std::thread::scope(|s| {
            for t in 0..8 {
                let buf = &buf;
                s.spawn(move || {
                    for _ in 0..2_000 {
                        let got = buf.lookup("map", "odom", 500_000_000).unwrap();
                        assert_eq!(got.to_bits(), p.to_bits(), "thread {t}");
                        assert!(buf.lookup("nope", "nah", 0).is_err());
                    }
                });
            }
        });
    }
}
