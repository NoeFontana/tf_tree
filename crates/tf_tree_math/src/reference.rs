//! Obvious, slow reference implementations kept forever as the definition of
//! correct. Every fast routine in this crate is proptested against its
//! reference twin (decision `0003`, D13).

use crate::iso3::{exp_se3, log_se3, Iso3};

/// Reference SE(3) screw interpolation: `a · exp_se3(s · log_se3(a⁻¹·b))`.
///
/// This is the definition of `ScLerp`. It is slower than
/// [`crate::dualquat::screw_pow`] (two transcendental pairs plus the full
/// `V`/`V⁻¹` series) but maximally transparent; the fast version is tested
/// against it to `1e-14`.
#[inline]
#[must_use]
pub fn sclerp(a: &Iso3, b: &Iso3, s: f64) -> Iso3 {
    let rel = a.inverse() * *b;
    let xi = log_se3(rel);
    let scaled = [
        s * xi[0],
        s * xi[1],
        s * xi[2],
        s * xi[3],
        s * xi[4],
        s * xi[5],
    ];
    *a * exp_se3(scaled)
}
