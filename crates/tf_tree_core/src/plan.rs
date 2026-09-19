//! Compiled lookup plans, typed time, and the evaluation `Guard`.
//!
//! A [`Plan`] resolves `lookup(target, source)` through the topology once;
//! evaluating it against a [`Guard`] is the hot path (`docs/PHASE1.md` §7).
//! `unsafe`-free, and `#[cfg(not(loom))]` because it needs [`ArenaView`].
//!
//! # Compilation direction
//!
//! `edge_of_child[c]` stores `T_parent(c)_c`, so
//! `T_target_source = (T_lca_target)⁻¹ · T_lca_source`: walking up from `target`
//! emits inverted steps in walk order, from `source` forward steps in reversed
//! walk order.

use core::marker::PhantomData;

use tf_tree_math::{log_so3, Interp, Iso3, LerpSlerp, ScLerp, Twist};

use crate::arena_view::ArenaView;
use crate::edge::EdgeKind;
use crate::error::{EdgeId, FrameId, LookupError};
use crate::layout::{write_affine32, write_mat4, write_quat, write_quat_twist, Layout};
use crate::sample::{Bracket, ExtrapPolicy};
use crate::sync::spin;
use crate::topology::TopologyView;
use crate::{MAX_DEPTH, MAX_PATH_EDGES};

/// Maximum number of knots [`Plan::at_adaptive`] may emit.
pub const MAX_KNOTS: usize = 4096;

/// Maximum bisection recursion depth in [`Plan::at_adaptive`].
pub const MAX_ADAPTIVE_DEPTH: u32 = 16;

/// How many stamps one batch-fold pass holds in flight (`docs/decisions/0060` §10.3).
const FOLD_LANES: usize = 16;

/// The batch size below which the fold stays per-stamp (`0060` §10.4).
const FOLD_MIN_BATCH: usize = 3;

// Pinned: `crates/tf_tree/tests/batch_phases.rs` copies both values.
const _: () = assert!(FOLD_LANES == 16 && FOLD_MIN_BATCH == 3);

/// Phase 2 of the batch fold: interpolate a chunk's brackets and compose each.
#[inline]
fn fold_lanes<I: Interp>(acc: &mut [Iso3], brackets: &[Bracket], inverted: bool) {
    if inverted {
        for (a, b) in acc.iter_mut().zip(brackets) {
            *a = a.mul_inv(&b.eval::<I>());
        }
    } else {
        for (a, b) in acc.iter_mut().zip(brackets) {
            *a = *a * b.eval::<I>();
        }
    }
}

/// A time domain: a compile-time marker carrying a runtime [`Domain::TAG`] byte
/// (`docs/PROJECT.md` §5 D9; `docs/PHASE1.md` §8).
pub trait Domain: Copy {
    /// The runtime tag stored on an edge's `domain` field; unique per domain.
    /// `0`–`3` are built-in; user domains take `4` up. **A tag is permanent**
    /// (`docs/API.md` §2.5, §5.2).
    const TAG: u8;
}

/// The default domain: the host system clock (`CLOCK_REALTIME`-like), tag `0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SystemDomain;
impl Domain for SystemDomain {
    const TAG: u8 = 0;
}

/// A sensor's own clock, tag `1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SensorDomain;
impl Domain for SensorDomain {
    const TAG: u8 = 1;
}

/// Simulated time (a `/clock` publisher, bag replay or physics engine), tag `2`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SimDomain;
impl Domain for SimDomain {
    const TAG: u8 = 2;
}

/// A steady, monotone clock, tag `3` (`docs/PHASE5.md` §6, `TFT019`; `docs/API.md` §5.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SteadyDomain;
impl Domain for SteadyDomain {
    const TAG: u8 = 3;
}

/// Nanoseconds in one second.
const NANOS_PER_SEC: i64 = 1_000_000_000;

/// A nanosecond timestamp in domain `D`; the phantom `D` costs nothing at runtime.
pub struct Stamp<D: Domain = SystemDomain>(i64, PhantomData<D>);

impl<D: Domain> Stamp<D> {
    /// Wrap a nanosecond count as a stamp in domain `D`.
    #[inline]
    #[must_use]
    pub const fn from_nanos(nanos: i64) -> Stamp<D> {
        Stamp(nanos, PhantomData)
    }

    /// The nanosecond count.
    #[inline]
    #[must_use]
    pub const fn nanos(self) -> i64 {
        self.0
    }

    /// Assemble a stamp from a `(seconds, nanoseconds)` pair (`builtin_interfaces/Time`,
    /// `struct timespec`), exactly (`docs/API.md` §5.1). Total: `None` for
    /// `nanos >= 1_000_000_000` (§5.2) or a result outside `i64`.
    ///
    /// # Examples
    ///
    /// ```
    /// use tf_tree_core::{Stamp, SystemDomain};
    ///
    /// let t = Stamp::<SystemDomain>::from_parts(1, 500_000_000).unwrap();
    /// assert_eq!(t.nanos(), 1_500_000_000);
    ///
    /// // Pre-epoch stamps are exact too — the seconds go negative, the
    /// // nanoseconds stay a positive remainder, exactly as `timespec` says.
    /// let before = Stamp::<SystemDomain>::from_parts(-1, 250_000_000).unwrap();
    /// assert_eq!(before.nanos(), -750_000_000);
    ///
    /// // A nanosecond field that is not a sub-second remainder is refused
    /// // rather than carried into the seconds.
    /// assert!(Stamp::<SystemDomain>::from_parts(1, 1_000_000_000).is_none());
    ///
    /// // ... and so is anything `i64` nanoseconds cannot hold.
    /// assert!(Stamp::<SystemDomain>::from_parts(i64::MAX, 0).is_none());
    /// ```
    #[inline]
    #[must_use]
    pub const fn from_parts(sec: i64, nanos: u32) -> Option<Stamp<D>> {
        if nanos as i64 >= NANOS_PER_SEC {
            return None;
        }
        // `i128`: staged `checked_*` refuses representable stamps at the negative end.
        let total = sec as i128 * NANOS_PER_SEC as i128 + nanos as i128;
        if total < i64::MIN as i128 || total > i64::MAX as i128 {
            return None;
        }
        Some(Stamp(total as i64, PhantomData))
    }

    /// Assemble a stamp from the two fields of a POSIX `struct timespec`.
    ///
    /// Refuses everything [`Self::from_parts`] does, plus a negative `tv_nsec`.
    ///
    /// # Examples
    ///
    /// ```
    /// use tf_tree_core::{SensorDomain, Stamp};
    ///
    /// // `clock_gettime(CLOCK_REALTIME, &ts)` gives exactly this pair.
    /// let t = Stamp::<SensorDomain>::from_timespec(1_700_000_000, 123_456_789).unwrap();
    /// assert_eq!(t.nanos(), 1_700_000_000_123_456_789);
    ///
    /// // A relative interval is not an absolute stamp.
    /// assert!(Stamp::<SensorDomain>::from_timespec(0, -1).is_none());
    /// ```
    #[inline]
    #[must_use]
    pub const fn from_timespec(tv_sec: i64, tv_nsec: i64) -> Option<Stamp<D>> {
        if tv_nsec < 0 || tv_nsec >= NANOS_PER_SEC {
            return None;
        }
        Self::from_parts(tv_sec, tv_nsec as u32)
    }
}

impl<D: Domain> Clone for Stamp<D> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}
impl<D: Domain> Copy for Stamp<D> {}
impl<D: Domain> PartialEq for Stamp<D> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
impl<D: Domain> Eq for Stamp<D> {}
impl<D: Domain> PartialOrd for Stamp<D> {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl<D: Domain> Ord for Stamp<D> {
    #[inline]
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.0.cmp(&other.0)
    }
}
impl<D: Domain> core::fmt::Debug for Stamp<D> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Stamp<{}>({})", D::TAG, self.0)
    }
}

/// A temporal query against a compiled [`Plan`] (`#[non_exhaustive]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Query<D: Domain = SystemDomain> {
    /// Sample every dynamic edge at exactly this stamp.
    At(Stamp<D>),
    /// Sample every dynamic edge at *its own* newest stamp; stamps may differ
    /// between edges.
    Latest,
    /// Sample every dynamic edge at the largest stamp all of them have data for
    /// — tf2's `Time(0)`, not "now".
    LatestCommon,
}

/// Selects an interpolation policy from an edge's stored discriminant
/// ([`crate::edge::EdgeRecord::interp`]). Not `#[non_exhaustive]`; an unknown
/// discriminant collapses to the default in [`InterpPolicy::from_u8`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum InterpPolicy {
    /// SE(3) screw-geodesic interpolation (the default; left- and right-invariant).
    #[default]
    ScLerp = 0,
    /// tf2-compatible translation-LERP + rotation-SLERP.
    LerpSlerp = 1,
}

impl InterpPolicy {
    /// The stored discriminant.
    #[inline]
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Decode a stored discriminant (`1` → `LerpSlerp`, anything else → `ScLerp`).
    #[inline]
    #[must_use]
    pub const fn from_u8(v: u8) -> InterpPolicy {
        match v {
            1 => InterpPolicy::LerpSlerp,
            _ => InterpPolicy::ScLerp,
        }
    }
}

/// A pose, and how far past the plan's newest common sample it was extrapolated
/// ([`0039`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0039-extrapolation-you-cannot-fail-to-notice.md)):
/// no accessor yields the pose alone.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Extrapolated {
    /// The pose.
    pub pose: Iso3,
    /// Nanoseconds past the newest stamp every dynamic edge has data for; `0` means
    /// every edge bracketed the query. Measured before the fold, so it may over-report.
    pub by_ns: i64,
    /// The dynamic edge whose newest stamp is [`Self::by_ns`] behind the query;
    /// meaningless when `by_ns == 0` (`docs/PROJECT.md` §5 D11).
    pub edge: EdgeId,
}

/// A pose and its derivatives at one instant (`docs/PHASE4.md` §2.2); the twist is
/// body-frame, in the plan's **source** frame ([`tf_tree_math::twist`]).
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub struct Sample {
    /// The transform at the requested stamp — bit-identical to [`Plan::at`].
    pub pose: Iso3,
    /// First derivative, body frame, rad/s and m/s.
    pub twist: Twist,
    /// Second derivative; always `None` today (ScLerp's body twist is constant
    /// across a segment).
    pub accel: Option<Twist>,
}

/// One step of a compiled plan.
///
/// Not `#[non_exhaustive]`, for [`InterpPolicy`]'s reason.
#[derive(Clone, Copy, Debug)]
pub enum Step {
    /// A constant transform (a folded static run), pre-inverted when it came from an inverted edge.
    Static(Iso3),
    /// A dynamic edge sampled at evaluation time; `inverted` composes `acc.mul_inv(p)`, else `acc * p`.
    Dyn {
        /// The edge to sample.
        edge: EdgeId,
        /// Whether to compose the sampled pose inverted.
        inverted: bool,
    },
}

/// A compiled `lookup(target, source)` path.
///
/// `Copy`, `Send`, `Sync`, heap-free. A generation mismatch is
/// [`LookupError::TopologyChanged`], never a silent stale read.
#[derive(Clone, Copy, Debug)]
pub struct Plan {
    generation: u64,
    steps: [Step; MAX_DEPTH],
    len: u8,
    domain: u8,
    /// How many of `steps[..len]` are [`Step::Dyn`]; see [`fold_into`].
    dyn_count: u8,
    /// The edge of the first [`Step::Dyn`], else [`EdgeId`]`(0)`; read via [`Plan::first_dynamic_edge`].
    first_dyn: EdgeId,
}

impl Plan {
    /// The identity plan for `generation`: zero steps, the buffer [`fold_into`] fills.
    /// The array cannot be left uninitialised (`MaybeUninit` is outside `0007`).
    fn identity(generation: u64) -> Plan {
        Plan {
            generation,
            steps: [Step::Static(Iso3::IDENTITY); MAX_DEPTH],
            len: 0,
            domain: 0,
            dyn_count: 0,
            first_dyn: EdgeId(0),
        }
    }

    /// The topology generation this plan was compiled against.
    #[inline]
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The plan's time-domain tag (`0` when all-static or empty).
    #[inline]
    #[must_use]
    pub fn domain(&self) -> u8 {
        self.domain
    }

    /// What [`fold_into`] derived next to a fresh scan of the same steps. Test-only.
    #[cfg(test)]
    pub(crate) fn derived_vs_scan_for_test(&self) -> ((bool, EdgeId), (bool, EdgeId)) {
        let scanned_has = self.steps().iter().any(|s| matches!(s, Step::Dyn { .. }));
        let scanned_first = {
            let mut found = None;
            for step in self.steps() {
                if let Step::Dyn { edge, .. } = step {
                    if found.is_some() {
                        found = Some(EdgeId(0));
                        break;
                    }
                    found = Some(*edge);
                }
            }
            found.unwrap_or(EdgeId(0))
        };
        (
            (self.has_dynamic(), self.first_dynamic_edge()),
            (scanned_has, scanned_first),
        )
    }

    /// The compiled steps (post-folding).
    #[inline]
    #[must_use]
    pub fn steps(&self) -> &[Step] {
        &self.steps[..self.len as usize]
    }

    /// The number of compiled steps.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.len as usize
    }

    /// Whether the plan is empty (a `lookup(x, x)` identity plan).
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    fn has_dynamic(&self) -> bool {
        self.dyn_count > 0
    }

    #[inline]
    fn check_generation(&self, g: &Guard) -> Result<(), LookupError> {
        let cur = g.generation();
        if cur == self.generation {
            return Ok(());
        }
        // A detached guard reports `ChildDetached`: no re-plan helps.
        if cur == DETACHED {
            return Err(LookupError::ChildDetached);
        }
        Err(LookupError::TopologyChanged {
            plan: self.generation,
            current: cur,
        })
    }

    #[inline]
    fn check_domain_tag(&self, domain: u8) -> Result<(), LookupError> {
        if self.has_dynamic() && domain != self.domain {
            return Err(LookupError::TimeDomainMismatch {
                expected: self.domain,
                got: domain,
            });
        }
        Ok(())
    }

    /// Evaluate the plan at `t`; generation and domain are already validated.
    /// `#[inline]` is load-bearing (`docs/API.md` §2.3).
    #[inline]
    fn fold_at(&self, g: &Guard, t: i64) -> Result<Iso3, LookupError> {
        let mut acc = Iso3::IDENTITY;
        for (k, step) in self.steps().iter().enumerate() {
            acc = match step {
                Step::Static(m) => acc * *m,
                Step::Dyn { edge, inverted } => {
                    let p = g.sample_hinted(k, *edge, t, ExtrapPolicy::Error)?;
                    if *inverted {
                        acc.mul_inv(&p)
                    } else {
                        acc * p
                    }
                }
            };
        }
        Ok(acc)
    }

    /// [`Self::fold_at`] under a caller-chosen policy. A deliberate second copy: `fold_at`
    /// passes the `Error` literal so LLVM prunes the other arms on [`Self::at`]'s hot
    /// path (`0039` §4).
    fn fold_at_policy(&self, g: &Guard, t: i64, policy: ExtrapPolicy) -> Result<Iso3, LookupError> {
        let mut acc = Iso3::IDENTITY;
        for (k, step) in self.steps().iter().enumerate() {
            acc = match step {
                Step::Static(m) => acc * *m,
                Step::Dyn { edge, inverted } => {
                    let p = g.sample_hinted(k, *edge, t, policy)?;
                    if *inverted {
                        acc.mul_inv(&p)
                    } else {
                        acc * p
                    }
                }
            };
        }
        Ok(acc)
    }

    /// Like [`Self::fold_at`] but each dynamic step gallops from its own cursor.
    /// Not `#[inline]` (`docs/API.md` §2.3); reached only from [`Self::fold_batch`]'s
    /// sub-chunk bypass.
    fn fold_at_cursors(
        &self,
        g: &Guard,
        t: i64,
        cursors: &mut [u64; MAX_DEPTH],
    ) -> Result<Iso3, LookupError> {
        let mut acc = Iso3::IDENTITY;
        for (k, step) in self.steps().iter().enumerate() {
            acc = match step {
                Step::Static(m) => acc * *m,
                Step::Dyn { edge, inverted } => {
                    let p = g.sample_from(*edge, t, ExtrapPolicy::Error, &mut cursors[k])?;
                    if *inverted {
                        acc.mul_inv(&p)
                    } else {
                        acc * p
                    }
                }
            };
        }
        Ok(acc)
    }

    /// Evaluate the plan at stamp `t` (an `At(t)` query). `#[inline]`: `docs/API.md` §2.3.
    ///
    /// # Errors
    ///
    /// * [`LookupError::TopologyChanged`] — the topology changed since compilation.
    /// * [`LookupError::TimeDomainMismatch`] — `D` does not match the plan's edges.
    /// * Any sampling error from an edge ([`LookupError::NoData`], …).
    #[inline]
    pub fn at<D: Domain>(&self, g: &Guard, t: Stamp<D>) -> Result<Iso3, LookupError> {
        self.at_tagged(g, t.nanos(), D::TAG)
    }

    /// [`Self::at`], with the query's domain as a runtime tag, for bindings that cannot
    /// name a [`Domain`] type ([`0038`]).
    ///
    /// [`0038`]: https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0038-the-domain-a-binding-cannot-name.md
    ///
    /// # No `#[inline]`, on purpose
    ///
    /// The one cross-crate call `plan.at(&g, t)` emits (`docs/API.md` §2.3); re-measure
    /// with `just bench-ab`.
    ///
    /// # Errors
    ///
    /// As [`Self::at`].
    pub fn at_tagged(&self, g: &Guard, nanos: i64, domain: u8) -> Result<Iso3, LookupError> {
        self.check_generation(g)?;
        self.check_domain_tag(domain)?;
        self.note(g, self.first_dynamic_edge(), self.fold_at(g, nanos))
    }

    /// Record one evaluation's outcome against the diagnostic counters
    /// (`docs/PHASE5.md` §5.3; the basis of `TFT010`/`TFT011`).
    #[inline]
    fn note<T>(
        &self,
        g: &Guard,
        edge: EdgeId,
        r: Result<T, LookupError>,
    ) -> Result<T, LookupError> {
        match r {
            Ok(v) => {
                g.note_ok(edge);
                Ok(v)
            }
            Err(e) => {
                g.note_err(&e);
                Err(e)
            }
        }
    }

    /// The single dynamic edge this plan traverses; `EdgeId(0)` when several.
    #[inline]
    fn first_dynamic_edge(&self) -> EdgeId {
        if self.dyn_count == 1 {
            self.first_dyn
        } else {
            EdgeId(0)
        }
    }

    /// Fold the plan at `t`, accumulating the body twist with the pose:
    /// `V_ac^c = Ad(T_bc⁻¹)·V_ab^b + V_bc^c` (`docs/PHASE4.md` §2.3). The sampler is a
    /// parameter so the composition lives in one place.
    #[inline]
    fn fold_with_derivatives<S>(&self, mut sample: S) -> Result<(Iso3, Twist), LookupError>
    where
        S: FnMut(usize, EdgeId) -> Result<(Iso3, Twist), LookupError>,
    {
        let mut acc = Iso3::IDENTITY;
        let mut vel = Twist::ZERO;
        for (k, step) in self.steps().iter().enumerate() {
            match step {
                Step::Static(m) => {
                    vel = m.adjoint_inv(&vel);
                    acc = acc * *m;
                }
                Step::Dyn { edge, inverted } => {
                    let (p, vp) = sample(k, *edge)?;
                    if *inverted {
                        vel = p.adjoint(&vel.sub(vp));
                        acc = acc.mul_inv(&p);
                    } else {
                        vel = p.adjoint_inv(&vel).add(vp);
                        acc = acc * p;
                    }
                }
            }
        }
        Ok((acc, vel))
    }

    /// [`Self::fold_with_derivatives`] restarting each search at the window midpoint.
    #[inline]
    fn fold_at_with_derivatives(&self, g: &Guard, t: i64) -> Result<(Iso3, Twist), LookupError> {
        self.fold_with_derivatives(|_, edge| g.sample_with_twist(edge, t, ExtrapPolicy::Error))
    }

    /// [`Self::fold_with_derivatives`] resuming each search from its own cursor.
    #[inline]
    fn fold_at_with_derivatives_cursors(
        &self,
        g: &Guard,
        t: i64,
        cursors: &mut [u64; MAX_DEPTH],
    ) -> Result<(Iso3, Twist), LookupError> {
        self.fold_with_derivatives(|k, edge| {
            g.sample_with_twist_from(edge, t, ExtrapPolicy::Error, &mut cursors[k])
        })
    }

    /// Evaluate the plan at `t`, returning the pose **and its derivatives**
    /// (`docs/PHASE4.md` §2.2). The twist is body-frame, `V^b = (T⁻¹Ṫ)^∨`, in the plan's
    /// **source** frame; use [`tf_tree_math::Twist::to_spatial`] for the target frame.
    /// For `base` rotated +90° about z moving along **map**'s +x at 1 m/s:
    ///
    /// ```text
    /// sample.twist.v              == (0, −1, 0)   // resolved in base axes
    /// sample.twist.to_spatial(&p) == (1,  0, 0)   // resolved in map axes
    /// ```
    ///
    /// # Errors
    ///
    /// Everything [`Self::at`] can return, plus [`LookupError::DerivativesUnavailable`]
    /// (an edge is `LerpSlerp`, §2.4) and [`LookupError::NoSegment`].
    pub fn at_with_derivatives<D: Domain>(
        &self,
        g: &Guard,
        t: Stamp<D>,
    ) -> Result<Sample, LookupError> {
        self.at_with_derivatives_tagged(g, t.nanos(), D::TAG)
    }

    /// [`Self::at_with_derivatives`], with the query's domain as a runtime tag ([`0038`]).
    ///
    /// [`0038`]: https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0038-the-domain-a-binding-cannot-name.md
    ///
    /// # Errors
    ///
    /// As [`Self::at_with_derivatives`].
    pub fn at_with_derivatives_tagged(
        &self,
        g: &Guard,
        nanos: i64,
        domain: u8,
    ) -> Result<Sample, LookupError> {
        self.check_generation(g)?;
        self.check_domain_tag(domain)?;
        let (pose, twist) = self.note(
            g,
            self.first_dynamic_edge(),
            self.fold_at_with_derivatives(g, nanos),
        )?;
        Ok(Sample {
            pose,
            twist,
            accel: None,
        })
    }

    /// Dispatch a [`Query`] against the plan.
    ///
    /// # Errors
    ///
    /// As [`Self::at`] / [`Self::latest`] / [`Self::latest_common`].
    pub fn query<D: Domain>(&self, g: &Guard, q: Query<D>) -> Result<Iso3, LookupError> {
        match q {
            Query::At(t) => self.at(g, t),
            Query::Latest => self.latest(g),
            Query::LatestCommon => self.latest_common(g),
        }
    }

    /// Sample every dynamic edge at *its own* newest stamp; see [`Self::latest_common`].
    ///
    /// # Errors
    ///
    /// [`LookupError::TopologyChanged`], or [`LookupError::NoData`] if a dynamic
    /// edge is empty.
    pub fn latest(&self, g: &Guard) -> Result<Iso3, LookupError> {
        self.check_generation(g)?;
        self.note(g, self.first_dynamic_edge(), self.fold_latest(g))
    }

    /// [`Self::latest`]'s fold, split out so [`Self::note`] wraps one expression.
    fn fold_latest(&self, g: &Guard) -> Result<Iso3, LookupError> {
        let mut acc = Iso3::IDENTITY;
        for step in self.steps() {
            acc = match step {
                Step::Static(m) => acc * *m,
                Step::Dyn { edge, inverted } => {
                    let t = g.newest_stamp(*edge)?;
                    let p = g.sample(*edge, t, ExtrapPolicy::Error)?;
                    if *inverted {
                        acc.mul_inv(&p)
                    } else {
                        acc * p
                    }
                }
            };
        }
        Ok(acc)
    }

    /// [`Self::at`], permitting extrapolation under `policy` and reporting how far
    /// ([`0039`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0039-extrapolation-you-cannot-fail-to-notice.md)).
    /// `by_ns` costs one `newest_stamp` load per dynamic edge, taken **before** the fold.
    ///
    /// # Errors
    ///
    /// As [`Self::at`]; under [`ExtrapPolicy::Error`] a query past the newest sample is
    /// [`LookupError::Extrapolation`].
    pub fn at_extrapolating<D: Domain>(
        &self,
        g: &Guard,
        t: Stamp<D>,
        policy: ExtrapPolicy,
    ) -> Result<Extrapolated, LookupError> {
        self.at_extrapolating_tagged(g, t.nanos(), D::TAG, policy)
    }

    /// [`Self::at_extrapolating`], with the query's domain as a runtime tag ([`0038`]).
    ///
    /// [`0038`]: https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0038-the-domain-a-binding-cannot-name.md
    ///
    /// # Errors
    ///
    /// As [`Self::at_extrapolating`].
    pub fn at_extrapolating_tagged(
        &self,
        g: &Guard,
        nanos: i64,
        domain: u8,
        policy: ExtrapPolicy,
    ) -> Result<Extrapolated, LookupError> {
        self.check_generation(g)?;
        self.check_domain_tag(domain)?;
        let edge = self.first_dynamic_edge();
        // Measured before the fold, so `by_ns == 0` is sound: measuring after let a mid-fold `push` report 0 for an invented pose.
        let common = self.newest_common(g);
        let pose = self.note(g, edge, self.fold_at_policy(g, nanos, policy))?;
        let (by_ns, which) = match common? {
            // `saturating_sub`: a wrapped negative would report 0 (see `sample::span_ns`).
            Some((common, which)) => (nanos.saturating_sub(common).max(0), which),
            None => (0, EdgeId(0)),
        };
        Ok(Extrapolated {
            pose,
            by_ns,
            edge: which,
        })
    }

    /// Sample every dynamic edge at the newest stamp common to all of them (tf2's
    /// `Time(0)`).
    ///
    /// # Errors
    pub fn latest_common(&self, g: &Guard) -> Result<Iso3, LookupError> {
        self.check_generation(g)?;
        self.note(g, self.first_dynamic_edge(), self.fold_latest_common(g))
    }

    /// [`Self::latest_common`]'s fold, split out like [`Self::fold_latest`].
    fn fold_latest_common(&self, g: &Guard) -> Result<Iso3, LookupError> {
        let Some((common, _)) = self.newest_common(g)? else {
            return Ok(self.static_only());
        };
        self.fold_at(g, common)
    }

    /// The newest stamp every dynamic edge has data for, and its edge; `None` when static-only.
    #[cfg(test)]
    pub(crate) fn newest_common_for_test(
        &self,
        g: &Guard,
    ) -> Result<Option<(i64, EdgeId)>, LookupError> {
        self.newest_common(g)
    }

    fn newest_common(&self, g: &Guard) -> Result<Option<(i64, EdgeId)>, LookupError> {
        let mut common = i64::MAX;
        let mut which = EdgeId(0);
        let mut any = false;
        for step in self.steps() {
            if let Step::Dyn { edge, .. } = step {
                let newest = g.newest_stamp(*edge)?;
                if !any || newest < common {
                    common = newest;
                    which = *edge;
                }
                any = true;
            }
        }
        Ok(any.then_some((common, which)))
    }

    /// The **outer bound outside which this plan certainly cannot answer**, or `None`
    /// when unbounded (`docs/PHASE5.md` §4.2); holes inside it are `TFT009`'s business.
    /// `t0 > t1` is an empty intersection, a real answer. On a live arena the answer
    /// ages as it is returned; on a frozen `.tft` it is exact.
    ///
    /// # Errors
    ///
    /// [`LookupError::TopologyChanged`] (or [`LookupError::ChildDetached`]),
    /// [`LookupError::UnknownEdge`], or [`LookupError::NoData`] for a never-published edge.
    pub fn span(&self, g: &Guard) -> Result<Option<(i64, i64)>, LookupError> {
        self.check_generation(g)?;
        let mut span: Option<(i64, i64)> = None;
        for step in self.steps() {
            let Step::Dyn { edge, .. } = step else {
                continue;
            };
            let (oldest, newest) = g.window(*edge)?;
            span = Some(match span {
                None => (oldest, newest),
                Some((lo, hi)) => (lo.max(oldest), hi.min(newest)),
            });
        }
        Ok(span)
    }

    /// The **slowest** declared nominal publish rate among this plan's dynamic edges,
    /// in milli-hertz, or `None` when none declares one (`docs/decisions/0018`).
    ///
    /// With [`Self::span`], the engine-side input to a caller's blocking wait; the wait
    /// loop is `0018`'s. `EdgeRecord::nominal_rate_mhz == 0` means *undeclared* and is
    /// skipped (`docs/PHASE5.md` §6, `TFT007`). Unlike `span`, it does **not** return
    /// [`LookupError::NoData`] for a never-published edge.
    ///
    /// # Errors
    ///
    /// [`LookupError::TopologyChanged`], [`LookupError::ChildDetached`], or
    /// [`LookupError::UnknownEdge`] if a step names an edge with no record.
    pub fn slowest_nominal_rate_mhz(&self, g: &Guard) -> Result<Option<u32>, LookupError> {
        self.check_generation(g)?;
        let mut slowest: Option<u32> = None;
        for step in self.steps() {
            let Step::Dyn { edge, .. } = step else {
                continue;
            };
            let mhz = g.nominal_rate_mhz(*edge)?;
            if mhz == 0 {
                continue;
            }
            slowest = Some(slowest.map_or(mhz, |current| current.min(mhz)));
        }
        Ok(slowest)
    }

    /// Fold an all-static plan (no `Guard` sampling needed).
    fn static_only(&self) -> Iso3 {
        let mut acc = Iso3::IDENTITY;
        for step in self.steps() {
            if let Step::Static(m) = step {
                acc = acc * *m;
            }
        }
        acc
    }

    /// Evaluate the plan at each stamp in `stamps`, writing results into `out`;
    /// monotone input resumes each search from the previous stamp.
    ///
    /// # Errors
    ///
    /// As [`Self::at`], plus [`LookupError::BufferTooSmall`] when `out.len() < stamps.len()`,
    /// checked before anything is written.
    pub fn at_many<D: Domain>(
        &self,
        g: &Guard,
        stamps: &[Stamp<D>],
        out: &mut [Iso3],
    ) -> Result<(), LookupError> {
        if out.len() < stamps.len() {
            return Err(LookupError::BufferTooSmall {
                need: stamps.len(),
                got: out.len(),
            });
        }
        self.check_generation(g)?;
        self.check_domain_tag(D::TAG)?;

        self.fold_batch(
            g,
            stamps,
            |s: Stamp<D>| s.nanos(),
            |iso, dst| dst[0] = *iso,
            1,
            out,
        )
    }

    /// Evaluate a batch **directly into a caller's buffer**, in `layout`. `out` is a flat
    /// `f64` slice of at least `stamps.len() * layout.elems()`; use
    /// [`Self::at_many_into_f32`] for [`Layout::Affine32`]. [`Layout::QuatTwist`] is
    /// bit-identical to [`Self::at_with_derivatives`]. `stamps` is raw nanoseconds, so
    /// `&[i64]` callers (FFI, NumPy) need not copy.
    ///
    /// # Errors
    ///
    /// [`LookupError::BufferTooSmall`] or [`LookupError::WrongElementType`], both checked
    /// **before any element is written** (`docs/PHASE3.md` §5.3); for
    /// [`Layout::QuatTwist`] also [`LookupError::DerivativesUnavailable`] and
    /// [`LookupError::NoSegment`]. Every other error is per *stamp*: `k` rows may already
    /// be written.
    pub fn at_many_into<D: Domain>(
        &self,
        g: &Guard,
        stamps: &[i64],
        layout: Layout,
        out: &mut [f64],
    ) -> Result<(), LookupError> {
        self.at_many_into_tagged(g, stamps, D::TAG, layout, out)
    }

    /// [`Self::at_many_into`], with the query's domain as a runtime tag ([`0038`]).
    ///
    /// [`0038`]: https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0038-the-domain-a-binding-cannot-name.md
    ///
    /// # Errors
    ///
    /// As [`Self::at_many_into`].
    pub fn at_many_into_tagged(
        &self,
        g: &Guard,
        stamps: &[i64],
        domain: u8,
        layout: Layout,
        out: &mut [f64],
    ) -> Result<(), LookupError> {
        if layout.is_f32() {
            return Err(LookupError::WrongElementType);
        }
        let need = stamps.len().saturating_mul(layout.elems());
        if out.len() < need {
            return Err(LookupError::BufferTooSmall {
                need,
                got: out.len(),
            });
        }
        self.check_generation(g)?;
        self.check_domain_tag(domain)?;

        let n = layout.elems();
        match layout {
            Layout::Mat4 => self.fold_batch(g, stamps, |s| s, write_mat4, n, out),
            Layout::Quat => self.fold_batch(g, stamps, |s| s, write_quat, n, out),
            Layout::QuatTwist => self.fold_batch_with_twist(g, stamps, n, out),
            Layout::Affine32 => Err(LookupError::WrongElementType),
        }
    }

    /// [`Self::at_many_into`] for `f32` layouts ([`Layout::Affine32`]).
    ///
    /// # Errors
    ///
    /// As [`Self::at_many_into`], with [`LookupError::WrongElementType`] for a
    /// layout that is not `f32`.
    pub fn at_many_into_f32<D: Domain>(
        &self,
        g: &Guard,
        stamps: &[i64],
        layout: Layout,
        out: &mut [f32],
    ) -> Result<(), LookupError> {
        self.at_many_into_f32_tagged(g, stamps, D::TAG, layout, out)
    }

    /// [`Self::at_many_into_f32`], with the query's domain as a runtime tag ([`0038`]).
    ///
    /// [`0038`]: https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0038-the-domain-a-binding-cannot-name.md
    ///
    /// # Errors
    ///
    /// As [`Self::at_many_into_f32`].
    pub fn at_many_into_f32_tagged(
        &self,
        g: &Guard,
        stamps: &[i64],
        domain: u8,
        layout: Layout,
        out: &mut [f32],
    ) -> Result<(), LookupError> {
        if !layout.is_f32() {
            return Err(LookupError::WrongElementType);
        }
        let need = stamps.len().saturating_mul(layout.elems());
        if out.len() < need {
            return Err(LookupError::BufferTooSmall {
                need,
                got: out.len(),
            });
        }
        self.check_generation(g)?;
        self.check_domain_tag(domain)?;

        let n = layout.elems();
        self.fold_batch(g, stamps, |s| s, write_affine32, n, out)
    }

    /// The shared batch loop, generic over element and stamp type
    /// (`docs/decisions/0060` Decision A): a monotone batch of at least
    /// [`FOLD_MIN_BATCH`] stamps is walked in chunks of [`FOLD_LANES`], each dynamic step
    /// reading every lane's bracket before interpolating. Rows are bit-identical to
    /// [`Self::at`] on a quiescent ring (`crates/tf_tree/tests/batch_phases.rs`); on
    /// failure rows before the failing stamp are written.
    #[inline]
    fn fold_batch<S, T, W, N>(
        &self,
        g: &Guard,
        stamps: &[S],
        nanos: N,
        write: W,
        elems: usize,
        out: &mut [T],
    ) -> Result<(), LookupError>
    where
        S: Copy,
        W: Fn(&Iso3, &mut [T]),
        N: Fn(S) -> i64,
    {
        let edge = self.first_dynamic_edge();

        // `chunks_exact_mut` zipped with `stamps` leaves an over-long buffer untouched.
        if !stamps.windows(2).all(|w| nanos(w[0]) <= nanos(w[1])) {
            for (s, dst) in stamps.iter().zip(out.chunks_exact_mut(elems)) {
                let iso = self.note(g, edge, self.fold_at(g, nanos(*s)))?;
                write(&iso, dst);
            }
            return Ok(());
        }
        if stamps.len() < FOLD_MIN_BATCH {
            let mut cursors = [0u64; MAX_DEPTH];
            for (s, dst) in stamps.iter().zip(out.chunks_exact_mut(elems)) {
                let iso = self.note(g, edge, self.fold_at_cursors(g, nanos(*s), &mut cursors))?;
                write(&iso, dst);
            }
            return Ok(());
        }

        self.fold_chunked(g, edge, stamps, nanos, write, elems, out)
    }

    /// [`Self::fold_batch`]'s chunked pass, `#[inline(never)]` so its ~4 kB lane buffers
    /// charge only this frame (`0060` §10.4).
    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    fn fold_chunked<S, T, W, N>(
        &self,
        g: &Guard,
        edge: EdgeId,
        stamps: &[S],
        nanos: N,
        write: W,
        elems: usize,
        out: &mut [T],
    ) -> Result<(), LookupError>
    where
        S: Copy,
        W: Fn(&Iso3, &mut [T]),
        N: Fn(S) -> i64,
    {
        let mut cursors = [0u64; MAX_DEPTH];
        let mut acc = [Iso3::IDENTITY; FOLD_LANES];
        let mut brackets = [Bracket::Exact(Iso3::IDENTITY); FOLD_LANES];

        for (chunk, dsts) in stamps
            .chunks(FOLD_LANES)
            .zip(out.chunks_mut(FOLD_LANES * elems))
        {
            let mut live = chunk.len();
            let mut failure: Option<LookupError> = None;
            acc[..live].fill(Iso3::IDENTITY);

            for (k, step) in self.steps().iter().enumerate() {
                match step {
                    Step::Static(m) => {
                        for a in &mut acc[..live] {
                            *a = *a * *m;
                        }
                    }
                    Step::Dyn { edge: e, inverted } => {
                        let Some((interp, ring)) = g.view().sampler(*e) else {
                            failure = Some(LookupError::UnknownEdge { edge: *e });
                            live = 0;
                            break;
                        };
                        let cursor = &mut cursors[k];

                        let mut read = live;
                        for (lane, s) in chunk[..live].iter().enumerate() {
                            match ring.read_from::<Bracket>(nanos(*s), ExtrapPolicy::Error, cursor)
                            {
                                Ok(b) => brackets[lane] = b,
                                Err(err) => {
                                    read = lane;
                                    failure = Some(err);
                                    break;
                                }
                            }
                        }
                        live = read;

                        match InterpPolicy::from_u8(interp) {
                            InterpPolicy::LerpSlerp => {
                                fold_lanes::<LerpSlerp>(
                                    &mut acc[..live],
                                    &brackets[..live],
                                    *inverted,
                                );
                            }
                            InterpPolicy::ScLerp => {
                                fold_lanes::<ScLerp>(
                                    &mut acc[..live],
                                    &brackets[..live],
                                    *inverted,
                                );
                            }
                        }
                        if live == 0 {
                            break;
                        }
                    }
                }
            }

            for (a, dst) in acc[..live].iter().zip(dsts.chunks_exact_mut(elems)) {
                write(a, dst);
            }
            for _ in 0..live {
                g.note_ok(edge);
            }
            if let Some(err) = failure {
                g.note_err(&err);
                return Err(err);
            }
        }
        Ok(())
    }

    /// [`Layout::QuatTwist`]'s batch loop, [`Self::fold_batch`]'s sibling for the
    /// `(Iso3, Twist)` fold; the cursor is a hint, so both branches are bit-identical.
    #[inline]
    fn fold_batch_with_twist(
        &self,
        g: &Guard,
        stamps: &[i64],
        elems: usize,
        out: &mut [f64],
    ) -> Result<(), LookupError> {
        let edge = self.first_dynamic_edge();
        if stamps.windows(2).all(|w| w[0] <= w[1]) {
            let mut cursors = [0u64; MAX_DEPTH];
            for (s, dst) in stamps.iter().zip(out.chunks_exact_mut(elems)) {
                let r = self.fold_at_with_derivatives_cursors(g, *s, &mut cursors);
                let (pose, twist) = self.note(g, edge, r)?;
                write_quat_twist(&pose, &twist, dst);
            }
        } else {
            for (s, dst) in stamps.iter().zip(out.chunks_exact_mut(elems)) {
                let (pose, twist) = self.note(g, edge, self.fold_at_with_derivatives(g, *s))?;
                write_quat_twist(&pose, &twist, dst);
            }
        }
        Ok(())
    }

    /// Emit the minimum set of knots such that [`LerpSlerp`] between adjacent knots stays
    /// within `tol` of the exact evaluation across `span`, bounded by
    /// [`MAX_ADAPTIVE_DEPTH`] and [`MAX_KNOTS`]. Returns parallel slices `(stamps, poses)`.
    ///
    /// # Errors
    ///
    /// As [`Self::at`]. An empty span (`start >= end`) yields the two endpoints.
    pub fn at_adaptive<'s, D: Domain>(
        &self,
        g: &Guard,
        span: (Stamp<D>, Stamp<D>),
        tol: ErrBound,
        scratch: &'s mut AdaptiveScratch<D>,
    ) -> Result<(&'s [Stamp<D>], &'s [Iso3]), LookupError> {
        self.at_adaptive_tagged(g, span, D::TAG, tol, scratch)
    }

    /// [`Self::at_adaptive`], with the query's domain as a runtime tag
    /// (`docs/decisions/0038-the-domain-a-binding-cannot-name.md`).
    ///
    /// # Errors
    ///
    /// As [`Self::at_adaptive`].
    pub fn at_adaptive_tagged<'s, D: Domain>(
        &self,
        g: &Guard,
        span: (Stamp<D>, Stamp<D>),
        domain: u8,
        tol: ErrBound,
        scratch: &'s mut AdaptiveScratch<D>,
    ) -> Result<(&'s [Stamp<D>], &'s [Iso3]), LookupError> {
        self.check_generation(g)?;
        self.check_domain_tag(domain)?;
        self.note(
            g,
            self.first_dynamic_edge(),
            self.fold_adaptive(g, span, tol, scratch),
        )
    }

    /// [`Self::at_adaptive`]'s body, split out so [`Self::note`] wraps one expression.
    fn fold_adaptive<'s, D: Domain>(
        &self,
        g: &Guard,
        span: (Stamp<D>, Stamp<D>),
        tol: ErrBound,
        scratch: &'s mut AdaptiveScratch<D>,
    ) -> Result<(&'s [Stamp<D>], &'s [Iso3]), LookupError> {
        scratch.stamps.clear();
        scratch.poses.clear();

        let (a_s, b_s) = (span.0.nanos(), span.1.nanos());
        let a_p = self.fold_at(g, a_s)?;
        scratch.stamps.push(Stamp::from_nanos(a_s));
        scratch.poses.push(a_p);

        if b_s <= a_s {
            return Ok((&scratch.stamps[..], &scratch.poses[..]));
        }
        let b_p = self.fold_at(g, b_s)?;
        subdivide(self, g, a_s, a_p, b_s, b_p, 0, tol, scratch)?;
        Ok((&scratch.stamps[..], &scratch.poses[..]))
    }
}

/// Recursive bisection helper for [`Plan::at_adaptive`]. Emits the right endpoint
/// of each accepted sub-segment, in increasing stamp order.
#[allow(clippy::too_many_arguments)]
fn subdivide<D: Domain>(
    plan: &Plan,
    g: &Guard,
    a_s: i64,
    a_p: Iso3,
    b_s: i64,
    b_p: Iso3,
    depth: u32,
    tol: ErrBound,
    scratch: &mut AdaptiveScratch<D>,
) -> Result<(), LookupError> {
    // Splittable: depth budget, a non-adjacent segment, and room for this knot plus
    // the ancestors that still emit one on unwind. The width is taken in `u64`: a
    // signed subtraction on `(i64::MIN, i64::MAX)` panics or wraps.
    let width = (b_s as u64).wrapping_sub(a_s as u64);
    let can_split = depth < MAX_ADAPTIVE_DEPTH
        && width > 1
        && scratch.stamps.len() + (MAX_ADAPTIVE_DEPTH as usize) + 1 < MAX_KNOTS;
    if can_split {
        let m_s = a_s.wrapping_add((width / 2) as i64);
        let m_p = plan.fold_at(g, m_s)?;
        let s = (m_s as u64).wrapping_sub(a_s as u64) as f64 / width as f64;
        let approx = <LerpSlerp as Interp>::eval(&a_p, &b_p, s);
        if !within(tol, &approx, &m_p) {
            subdivide(plan, g, a_s, a_p, m_s, m_p, depth + 1, tol, scratch)?;
            subdivide(plan, g, m_s, m_p, b_s, b_p, depth + 1, tol, scratch)?;
            return Ok(());
        }
    }
    scratch.stamps.push(Stamp::from_nanos(b_s));
    scratch.poses.push(b_p);
    Ok(())
}

/// Whether `approx` is within `tol` of `exact` (rotation angle + translation).
fn within(tol: ErrBound, approx: &Iso3, exact: &Iso3) -> bool {
    let dq = approx.q.conjugate() * exact.q;
    let rot = log_so3(dq).norm();
    let trans = approx.t.sub(exact.t).norm();
    rot <= tol.rot_rad && trans <= tol.trans
}

/// The per-component error tolerance for [`Plan::at_adaptive`]; build with [`ErrBound::new`].
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub struct ErrBound {
    /// Maximum allowed rotation error, in radians.
    pub rot_rad: f64,
    /// Maximum allowed translation error, in the pose's length units.
    pub trans: f64,
}

impl ErrBound {
    /// A tolerance of `rot_rad` radians and `trans` length units.
    #[inline]
    #[must_use]
    pub const fn new(rot_rad: f64, trans: f64) -> ErrBound {
        ErrBound { rot_rad, trans }
    }
}

/// Caller-provided scratch for [`Plan::at_adaptive`], sized for the maximum knot set.
pub struct AdaptiveScratch<D: Domain = SystemDomain> {
    stamps: alloc::vec::Vec<Stamp<D>>,
    poses: alloc::vec::Vec<Iso3>,
}

impl<D: Domain> AdaptiveScratch<D> {
    /// Allocate scratch with capacity for [`MAX_KNOTS`] knots; reusable.
    #[must_use]
    pub fn new() -> AdaptiveScratch<D> {
        AdaptiveScratch {
            stamps: alloc::vec::Vec::with_capacity(MAX_KNOTS),
            poses: alloc::vec::Vec::with_capacity(MAX_KNOTS),
        }
    }
}

impl<D: Domain> Default for AdaptiveScratch<D> {
    fn default() -> Self {
        AdaptiveScratch::new()
    }
}

/// Static metadata about an edge, supplied to [`compile`] for constant folding.
#[derive(Clone, Copy, Debug)]
pub struct EdgeMeta {
    /// The edge kind (static edges are folded into constant steps).
    pub kind: EdgeKind,
    /// The edge's time-domain tag.
    pub domain: u8,
    /// For a static edge, its inline pose `T_parent_child`; ignored otherwise.
    pub static_pose: Iso3,
}

/// A batch-evaluation handle: it borrows the arena and pins the topology
/// generation once, so a run of lookups validates against one snapshot.
pub struct Guard<'a> {
    view: ArenaView<'a>,
    /// The pinned topology generation, or [`DETACHED`] for a guard built by
    /// [`Guard::detached`].
    generation: u64,
    /// Successful lookups so far, flushed to the arena on drop. A plain `Cell`
    /// (`docs/PHASE5.md` §5.4): `Guard` is `!Sync`. The flush saturates.
    #[cfg(feature = "counters")]
    ok: core::cell::Cell<u32>,
    /// Which edge's counters to credit when every lookup went through one plan;
    /// `None` before any lookup and once two edges were seen.
    #[cfg(feature = "counters")]
    ok_edge: core::cell::Cell<Option<EdgeId>>,
    /// Per-step bracket-search hints, packed `(edge << 32) | index`
    /// (`docs/design/fast-path.md` §12). A hint never changes a result, so a stale cursor
    /// needs no invalidation; an index truncated past `u32::MAX` is lifted back by
    /// `sample::rebase_hint`.
    cursor: [core::cell::Cell<u64>; MAX_DEPTH],
    /// `(generation at creation, how to read it now)`, for the fork check: the flush
    /// writes from a destructor and a shared mapping is `MADV_DONTFORK`
    /// (`docs/decisions/0005` step 9). `None` for a heap arena.
    #[cfg(feature = "counters")]
    fork: Option<(u64, fn() -> u64)>,
}

/// The generation a [`Guard::detached`] guard carries: unreachable as a real one,
/// and [`Plan::check_generation`] already compares it, so it adds no hot-path load.
const DETACHED: u64 = u64::MAX;

/// Which [`crate::counters::EdgeCounters`] field a lookup error belongs in.
#[cfg(feature = "counters")]
#[derive(Clone, Copy)]
enum CounterField {
    ExtrapBefore,
    ExtrapAfter,
    NoData,
    SlotRecycled,
    SlotContended,
}

#[cfg(feature = "counters")]
impl CounterField {
    #[inline]
    fn bump(self, c: &crate::counters::EdgeCounters) {
        use crate::sync::Ordering::Relaxed;
        let f = match self {
            CounterField::ExtrapBefore => &c.err_extrap_before,
            CounterField::ExtrapAfter => &c.err_extrap_after,
            CounterField::NoData => &c.err_no_data,
            CounterField::SlotRecycled => &c.err_slot_recycled,
            CounterField::SlotContended => &c.err_slot_contended,
        };
        f.fetch_add(1, Relaxed);
    }

    /// The same classification against the participant mirror; the two `#[repr(C)]`
    /// records must not become interchangeable (`counters.rs`).
    #[inline]
    fn bump_participant(self, p: &crate::counters::ParticipantCounters) {
        use crate::sync::Ordering::Relaxed;
        let f = match self {
            CounterField::ExtrapBefore => &p.err_extrap_before,
            CounterField::ExtrapAfter => &p.err_extrap_after,
            CounterField::NoData => &p.err_no_data,
            CounterField::SlotRecycled => &p.err_slot_recycled,
            CounterField::SlotContended => &p.err_slot_contended,
        };
        f.fetch_add(1, Relaxed);
    }
}

/// Classify a lookup error into `(edge, field)`, or `None` when it names no edge:
/// query-describing errors (D11) must not blame a working publisher.
#[cfg(feature = "counters")]
#[inline]
fn counter_of(err: &LookupError) -> Option<(EdgeId, CounterField)> {
    Some(match *err {
        LookupError::Extrapolation {
            edge,
            requested,
            newest,
            ..
        } => (
            edge,
            // Past the newest usually means a publisher stopped, before the oldest a consumer behind (`TFT010`/`TFT011`).
            if requested > newest {
                CounterField::ExtrapAfter
            } else {
                CounterField::ExtrapBefore
            },
        ),
        LookupError::NoData { edge } => (edge, CounterField::NoData),
        LookupError::SlotRecycled { edge } => (edge, CounterField::SlotRecycled),
        LookupError::SlotContended { edge } => (edge, CounterField::SlotContended),
        _ => return None,
    })
}

/// [`DETACHED`], for the test that pins it.
#[cfg(test)]
pub(crate) const DETACHED_FOR_TEST: u64 = DETACHED;

/// Flush the batch's success count into the arena: one relaxed atomic per guard
/// (`docs/PHASE5.md` §5.4).
#[cfg(feature = "counters")]
impl Drop for Guard<'_> {
    fn drop(&mut self) {
        let n = self.ok.get();
        // Read-only guards drop every batch and must not write.
        if n == 0 || !self.view.is_writable() {
            return;
        }
        if let Some((born, read)) = self.fork {
            if read() != born {
                return;
            }
        }
        use crate::sync::Ordering::Relaxed;
        if let Some(edge) = self.ok_edge.get() {
            if let Some(c) = self.view.edge_counters(edge) {
                c.lookups_ok.fetch_add(u64::from(n), Relaxed);
            }
        }
        if let Some(slot) = self.view.interning_identity() {
            if let Some(p) = self.view.participant_counters(slot) {
                p.lookups_ok.fetch_add(u64::from(n), Relaxed);
            }
        }
    }
}

impl<'a> Guard<'a> {
    /// Pin the current topology generation and wrap the arena view for a batch of
    /// lookups. The pinned value is always stable (A1: no odd state).
    #[must_use]
    pub fn new(view: ArenaView<'a>) -> Guard<'a> {
        let generation = view.topology().stable_generation();
        Guard {
            view,
            generation,
            cursor: [const { core::cell::Cell::new(0) }; MAX_DEPTH],
            #[cfg(feature = "counters")]
            ok: core::cell::Cell::new(0),
            #[cfg(feature = "counters")]
            ok_edge: core::cell::Cell::new(None),
            #[cfg(feature = "counters")]
            fork: None,
        }
    }

    /// Attach a fork-generation check to this guard's counter flush. `read` must
    /// return a value that changes when the process forks.
    #[must_use]
    pub fn with_fork_check(self, read: fn() -> u64) -> Guard<'a> {
        #[cfg(feature = "counters")]
        {
            let mut g = self;
            g.fork = Some((read(), read));
            g
        }
        #[cfg(not(feature = "counters"))]
        {
            let _ = read;
            self
        }
    }

    /// Record a successful lookup through `edge` (`docs/PHASE5.md` §5.4); compiled
    /// away without the `counters` feature.
    #[inline]
    pub(crate) fn note_ok(&self, edge: EdgeId) {
        #[cfg(feature = "counters")]
        {
            self.ok.set(self.ok.get().saturating_add(1));
            // `EdgeId(0)` is "no edge" (a multi-edge plan) and must not latch: it would funnel every flush into edge 0's record.
            if edge == EdgeId(0) {
                self.ok_edge.set(None);
                return;
            }
            match self.ok_edge.get() {
                None if self.ok.get() == 1 => self.ok_edge.set(Some(edge)),
                Some(e) if e != edge => self.ok_edge.set(None),
                _ => {}
            }
        }
        #[cfg(not(feature = "counters"))]
        let _ = edge;
    }

    /// Record a failed lookup, writing straight through so a process dying mid-fault
    /// leaves evidence (`docs/PHASE5.md` §5.3).
    #[inline]
    pub(crate) fn note_err(&self, err: &LookupError) {
        #[cfg(feature = "counters")]
        {
            use crate::sync::Ordering::Relaxed;
            // A read-only view (D18) must not write; it keeps no counters.
            if !self.view.is_writable() {
                return;
            }
            let Some((edge, field)) = counter_of(err) else {
                return;
            };
            // The failure's own stamp when it has one; zero reads as "never".
            let now = match *err {
                LookupError::Extrapolation { requested, .. } => requested,
                _ => 0,
            };
            // Both halves (`docs/PHASE5.md` §5.2).
            if let Some(slot) = self.view.interning_identity() {
                if let Some(p) = self.view.participant_counters(slot) {
                    field.bump_participant(p);
                    p.last_err_edge.store(edge.get(), Relaxed);
                    p.last_err_nanos.store(now, Relaxed);
                }
            }
            if let Some(c) = self.view.edge_counters(edge) {
                field.bump(c);
                c.last_err_nanos.store(now, Relaxed);
                if let LookupError::Extrapolation {
                    requested,
                    oldest,
                    newest,
                    ..
                } = *err
                {
                    // High-water mark, read by `TFT011`.
                    let gap = if requested > newest {
                        requested.saturating_sub(newest)
                    } else {
                        oldest.saturating_sub(requested)
                    };
                    // `fetch_max`: load/compare/store lets the mark regress under concurrent writers.
                    c.worst_extrap_gap_ns.fetch_max(gap, Relaxed);
                }
            }
        }
        #[cfg(not(feature = "counters"))]
        let _ = err;
    }

    /// A guard that fails every evaluation with [`LookupError::ChildDetached`], for a
    /// facade whose arena is unreachable (a mapping lost under `fork()`); `view` must
    /// still be valid. See `DETACHED`.
    #[must_use]
    pub fn detached(view: ArenaView<'a>) -> Guard<'a> {
        Guard {
            view,
            generation: DETACHED,
            cursor: [const { core::cell::Cell::new(0) }; MAX_DEPTH],
            #[cfg(feature = "counters")]
            ok: core::cell::Cell::new(0),
            #[cfg(feature = "counters")]
            ok_edge: core::cell::Cell::new(None),
            #[cfg(feature = "counters")]
            fork: None,
        }
    }

    /// The failure this guard refuses every evaluation with, if it does.
    #[inline]
    #[must_use]
    pub fn poison(&self) -> Option<LookupError> {
        (self.generation == DETACHED).then_some(LookupError::ChildDetached)
    }

    /// The pinned topology generation.
    #[inline]
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The underlying arena view (for diagnostics / name resolution).
    #[inline]
    #[must_use]
    pub fn view(&self) -> &ArenaView<'a> {
        &self.view
    }

    /// Sample edge `edge` at stamp `t`, dispatching on the edge's interp policy.
    /// `#[inline]` (`docs/API.md` §2.3).
    #[inline]
    pub(crate) fn sample(
        &self,
        edge: EdgeId,
        t: i64,
        policy: ExtrapPolicy,
    ) -> Result<Iso3, LookupError> {
        let (interp, ring) = self
            .view
            .sampler(edge)
            .ok_or(LookupError::UnknownEdge { edge })?;
        match InterpPolicy::from_u8(interp) {
            InterpPolicy::LerpSlerp => ring.sample::<LerpSlerp>(t, policy),
            InterpPolicy::ScLerp => ring.sample::<ScLerp>(t, policy),
        }
    }

    /// [`Self::sample`], resuming from this guard's cursor for step `k`. See
    /// [`Guard::cursor`].
    #[inline]
    pub(crate) fn sample_hinted(
        &self,
        k: usize,
        edge: EdgeId,
        t: i64,
        policy: ExtrapPolicy,
    ) -> Result<Iso3, LookupError> {
        let Some(slot) = self.cursor.get(k) else {
            return self.sample(edge, t, policy);
        };
        let packed = slot.get();
        let mut cursor = if (packed >> 32) as u32 == edge.0 {
            packed & 0xFFFF_FFFF
        } else {
            0
        };
        let out = self.sample_from(edge, t, policy, &mut cursor);
        // Success only: a failed search leaves `cursor` at a position no success produced.
        if out.is_ok() {
            slot.set((u64::from(edge.0) << 32) | (cursor & 0xFFFF_FFFF));
        }
        out
    }

    /// Sample edge `edge` at `t` and also return its body twist. Refuses `LerpSlerp`
    /// ([`LookupError::DerivativesUnavailable`]).
    pub(crate) fn sample_with_twist(
        &self,
        edge: EdgeId,
        t: i64,
        policy: ExtrapPolicy,
    ) -> Result<(Iso3, Twist), LookupError> {
        let (interp, ring) = self
            .view
            .sampler(edge)
            .ok_or(LookupError::UnknownEdge { edge })?;
        match InterpPolicy::from_u8(interp) {
            InterpPolicy::ScLerp => ring.sample_with_twist(t, policy),
            InterpPolicy::LerpSlerp => Err(LookupError::DerivativesUnavailable { edge, interp }),
        }
    }

    /// [`Self::sample_with_twist`], resuming from `cursor`. The refusal precedes any
    /// ring access, so the batch refusal matches the scalar call's.
    pub(crate) fn sample_with_twist_from(
        &self,
        edge: EdgeId,
        t: i64,
        policy: ExtrapPolicy,
        cursor: &mut u64,
    ) -> Result<(Iso3, Twist), LookupError> {
        let (interp, ring) = self
            .view
            .sampler(edge)
            .ok_or(LookupError::UnknownEdge { edge })?;
        match InterpPolicy::from_u8(interp) {
            InterpPolicy::ScLerp => ring.sample_with_twist_from(t, policy, cursor),
            InterpPolicy::LerpSlerp => Err(LookupError::DerivativesUnavailable { edge, interp }),
        }
    }

    /// Galloping variant of [`Self::sample`] resuming from `cursor`.
    #[inline]
    pub(crate) fn sample_from(
        &self,
        edge: EdgeId,
        t: i64,
        policy: ExtrapPolicy,
        cursor: &mut u64,
    ) -> Result<Iso3, LookupError> {
        let (interp, ring) = self
            .view
            .sampler(edge)
            .ok_or(LookupError::UnknownEdge { edge })?;
        match InterpPolicy::from_u8(interp) {
            InterpPolicy::LerpSlerp => ring.sample_from::<LerpSlerp>(t, policy, cursor),
            InterpPolicy::ScLerp => ring.sample_from::<ScLerp>(t, policy, cursor),
        }
    }

    /// The newest published stamp of a dynamic edge.
    pub(crate) fn newest_stamp(&self, edge: EdgeId) -> Result<i64, LookupError> {
        self.view
            .ring(edge)
            .ok_or(LookupError::UnknownEdge { edge })?
            .newest_stamp()
            .ok_or(LookupError::NoData { edge })
    }

    /// Both ends of a dynamic edge's retained window, `(oldest, newest)`. Two
    /// independent `head` loads, so a concurrent `push` can widen the pair (see
    /// [`Plan::span`]).
    pub(crate) fn window(&self, edge: EdgeId) -> Result<(i64, i64), LookupError> {
        let ring = self
            .view
            .ring(edge)
            .ok_or(LookupError::UnknownEdge { edge })?;
        match (ring.oldest_stamp(), ring.newest_stamp()) {
            (Some(oldest), Some(newest)) => Ok((oldest, newest)),
            _ => Err(LookupError::NoData { edge }),
        }
    }

    /// An edge's declared nominal publish rate in milli-hertz, `0` meaning
    /// *undeclared* and passed through (see [`Plan::slowest_nominal_rate_mhz`]). Reads
    /// the edge record, so it cannot return [`LookupError::NoData`].
    pub(crate) fn nominal_rate_mhz(&self, edge: EdgeId) -> Result<u32, LookupError> {
        Ok(self
            .view
            .edge(edge)
            .ok_or(LookupError::UnknownEdge { edge })?
            .nominal_rate_mhz)
    }
}

/// Compile a `lookup(target, source)` path into a [`Plan`].
///
/// Walks both frames up to their lowest common ancestor under the topology seqlock,
/// retrying if a mutation lands mid-walk. `edge_meta` supplies each edge's
/// kind/domain/static-pose for constant folding, `None` for an id with no record.
///
/// # Errors
///
/// * [`LookupError::Disconnected`] — different connected components.
/// * [`LookupError::TreeTooDeep`] — more than [`MAX_PATH_EDGES`] raw edges, or more
///   than [`MAX_DEPTH`] folded steps (readable off `depth`).
/// * [`LookupError::FrameOutOfRange`] — a frame id is out of range for `topo`.
/// * [`LookupError::MissingEdge`] — a parent link on the path records no edge.
/// * [`LookupError::UnknownEdge`] / [`LookupError::MixedTimeDomains`] — raised by the
///   constant fold, before the length refusal.
pub fn compile(
    topo: &TopologyView,
    edge_meta: impl Fn(EdgeId) -> Option<EdgeMeta>,
    target: FrameId,
    source: FrameId,
) -> Result<Plan, LookupError> {
    if target == source {
        // Identity plan, stamped with a consistent generation.
        return Ok(Plan::identity(topo.stable_generation()));
    }

    'walk: loop {
        // Every published generation is stable (A1); the retry discards a straddling walk.
        let start_gen = topo.generation();

        // Read (parent, depth, edge_of_child) for `f`, restarting on a generation change.
        macro_rules! read {
            ($f:expr) => {{
                let (parent, depth, edge, gen) = topo
                    .read_frame($f)
                    .ok_or(LookupError::FrameOutOfRange { frame: $f })?;
                if gen != start_gen {
                    spin();
                    continue 'walk;
                }
                (parent, depth, edge)
            }};
        }

        let mut a = target;
        let mut b = source;
        let (mut pa, mut da, mut ea) = read!(a);
        let (mut pb, mut db, mut eb) = read!(b);

        // Edges walking up from target (emitted inverted) and from source (forward, reversed).
        let mut t_edges = [0u32; MAX_PATH_EDGES];
        let mut nt = 0usize;
        let mut s_edges = [0u32; MAX_PATH_EDGES];
        let mut ns = 0usize;

        // Record the edge from `$frame` up to its parent. Edge id `0` is the "no edge"
        // sentinel and must never become a `Step::Dyn`. The bound is checked before the
        // sentinel so a defect wins by position; the walk stops there (a cyclic parent
        // chain would not terminate), reporting depth `MAX_PATH_EDGES + 1`.
        macro_rules! push_edge {
            ($buf:expr, $n:expr, $edge:expr, $frame:expr) => {{
                if nt + ns == MAX_PATH_EDGES {
                    return Err(LookupError::TreeTooDeep {
                        depth: (MAX_PATH_EDGES + 1) as u16,
                    });
                }
                if $edge == 0 {
                    return Err(LookupError::MissingEdge { child: $frame });
                }
                $buf[$n] = $edge;
                $n += 1;
            }};
        }

        while da > db {
            push_edge!(t_edges, nt, ea, a);
            a = frame_or_disconnect(pa, target, source, a)?;
            let (p, d, e) = read!(a);
            pa = p;
            da = d;
            ea = e;
        }
        while db > da {
            push_edge!(s_edges, ns, eb, b);
            b = frame_or_disconnect(pb, target, source, b)?;
            let (p, d, e) = read!(b);
            pb = p;
            db = d;
            eb = e;
        }

        while a != b {
            if pa == 0 || pb == 0 {
                return Err(LookupError::Disconnected {
                    target,
                    source,
                    cut_at: a,
                });
            }
            push_edge!(t_edges, nt, ea, a);
            push_edge!(s_edges, ns, eb, b);
            a = frame_or_disconnect(pa, target, source, a)?;
            b = frame_or_disconnect(pb, target, source, b)?;
            let (p, _d, e) = read!(a);
            pa = p;
            ea = e;
            let (p, _d, e) = read!(b);
            pb = p;
            eb = e;
        }

        if topo.generation() != start_gen {
            spin();
            continue 'walk;
        }

        // Folded in place, not via a by-value temporary (#264).
        let mut plan = Plan::identity(start_gen);
        fold_into(&mut plan, &t_edges[..nt], &s_edges[..ns], &edge_meta)?;
        return Ok(plan);
    }
}

/// Advance to `parent`, or fail with `Disconnected` if it is the root sentinel.
#[inline]
fn frame_or_disconnect(
    parent: u32,
    target: FrameId,
    source: FrameId,
    cut_at: FrameId,
) -> Result<FrameId, LookupError> {
    FrameId::new(parent).ok_or(LookupError::Disconnected {
        target,
        source,
        cut_at,
    })
}

/// Constant folding: replace static edges with constant steps (pre-inverting when
/// inverted), then collapse adjacent `Static` runs, writing `len`, `domain`,
/// `dyn_count` and `first_dyn` into `plan` (so `Plan::at` does no O(`len`) scan;
/// `plan_derived_fields_match_a_fresh_scan` pins this only writer). On `Err`, `plan`
/// is still the identity plan.
///
/// `t_edges` are in walk order, emitted inverted, then `s_edges` **reversed**,
/// emitted forward. The reversal is load-bearing: `Iso3` composition is not
/// associative under rounding, and tolerance-based tests would accept other bits.
///
/// # Running past the end of the output array
///
/// A path can fold to more than `MAX_DEPTH` steps: the write is skipped but `n` and
/// edge resolution continue, so [`LookupError::TreeTooDeep`] reports the true length
/// and a later defect is still named (`0034`). The collapse reads a tracked
/// `last_static`, not `out[n - 1]`.
fn fold_into(
    plan: &mut Plan,
    t_edges: &[u32],
    s_edges: &[u32],
    edge_meta: &impl Fn(EdgeId) -> Option<EdgeMeta>,
) -> Result<(), LookupError> {
    let out = &mut plan.steps;
    let mut n = 0usize;
    // Derived in the append arm; the collapse arm cannot add or remove a `Dyn`.
    let mut dyn_count = 0u8;
    let mut first_dyn = EdgeId(0);
    // Tracked rather than read back, because `n` may be past the array.
    let mut last_static = false;
    // `None` until the first dynamic step fixes the domain; a mismatch would sample with the wrong clock (D9).
    let mut domain: Option<u8> = None;

    let path = t_edges
        .iter()
        .map(|&e| (e, true))
        .chain(s_edges.iter().rev().map(|&e| (e, false)));

    for (edge, inverted) in path {
        let edge = EdgeId(edge);
        let meta = edge_meta(edge).ok_or(LookupError::UnknownEdge { edge })?;
        let resolved = match meta.kind {
            EdgeKind::Static => {
                let m = if inverted {
                    meta.static_pose.inverse()
                } else {
                    meta.static_pose
                };
                Step::Static(m)
            }
            _ => {
                // Dynamic, or a tombstone (sampling surfaces the real error).
                match domain {
                    None => domain = Some(meta.domain),
                    Some(d) if d != meta.domain => {
                        return Err(LookupError::MixedTimeDomains {
                            edge,
                            expected: d,
                            got: meta.domain,
                        })
                    }
                    Some(_) => {}
                }
                Step::Dyn { edge, inverted }
            }
        };

        // Collapse into a previous Static, else append; past `MAX_DEPTH` only count.
        match resolved {
            Step::Static(cur) if last_static => {
                if n <= MAX_DEPTH {
                    if let Step::Static(prev) = out[n - 1] {
                        out[n - 1] = Step::Static(prev * cur);
                    }
                }
            }
            s => {
                if n < MAX_DEPTH {
                    out[n] = s;
                }
                if let Step::Dyn { edge, .. } = s {
                    if dyn_count == 0 {
                        first_dyn = edge;
                    }
                    dyn_count = dyn_count.saturating_add(1);
                }
                last_static = matches!(s, Step::Static(_));
                n += 1;
            }
        }
    }

    if n > MAX_DEPTH {
        return Err(LookupError::TreeTooDeep { depth: n as u16 });
    }

    plan.len = n as u8;
    plan.domain = domain.unwrap_or(0);
    plan.dyn_count = dyn_count;
    plan.first_dyn = first_dyn;
    Ok(())
}
