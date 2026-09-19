//! Compiled lookup plans, typed time, and the evaluation `Guard`.
//!
//! A [`Plan`] resolves `lookup(target, source)` through the topology once;
//! evaluating it against a [`Guard`] is the hot temporal sampling
//! (`docs/PHASE1.md` §7; `docs/PROJECT.md` §5 D3).
//!
//! `unsafe`-free, and `#[cfg(not(loom))]` because [`Guard`] and [`compile`] need
//! the production-only [`ArenaView`]/[`TopologyView`].
//!
//! # Compilation direction
//!
//! `edge_of_child[c]` stores `T_parent(c)_c`, so
//! `T_target_source = (T_lca_target)⁻¹ · T_lca_source`: walking up from `target`
//! emits inverted steps in walk order, from `source` forward steps in reversed
//! walk order. `lookup(base, map)` over `map → odom → base` emits
//! `[Dyn(edge_base, inv), Dyn(edge_odom, inv)]`.

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

/// How many stamps one pass of the batch fold holds in flight.
///
/// `docs/decisions/0060` §10.3: level with 64 lanes from N = 63 up and ahead
/// below, at 6 600 B of stack frame against 17 720 B.
const FOLD_LANES: usize = 16;

/// The batch size below which the fold stays per-stamp.
///
/// Chunk bookkeeping costs +82.5% at N = 1 and +14.8% at N = 2 (`0060` §10.4).
/// `at_many_small/at_many_2` and `at_many_small/at_many_3` are the bench rows
/// either side.
const FOLD_MIN_BATCH: usize = 3;

// Pinned: `crates/tf_tree/tests/batch_phases.rs` copies both values to build its
// lane shapes and cannot check them across the crate boundary.
const _: () = assert!(FOLD_LANES == 16 && FOLD_MIN_BATCH == 3);

/// Phase 2 of the batch fold: interpolate a chunk's brackets and compose each
/// into its accumulator. `inverted` is hoisted out of the loop; nothing here
/// loads an atomic or searches a ring.
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

/// A time domain: a compile-time marker carrying a runtime [`Domain::TAG`] byte.
///
/// A [`Stamp`] is parameterised by its domain, so a cross-domain lookup is a type
/// error or a [`LookupError::TimeDomainMismatch`], never a silent misread
/// (`docs/PROJECT.md` §5 D9; `docs/PHASE1.md` §8 *Time*).
pub trait Domain: Copy {
    /// The runtime tag stored on an edge's `domain` field and compared against a
    /// query's domain. Must be unique per domain.
    ///
    /// Tags `0`–`3` are the built-ins ([`SystemDomain`], [`SensorDomain`],
    /// [`SimDomain`], [`SteadyDomain`]); a user-declared domain picks a free tag
    /// from `4` up (`docs/API.md` §2.5). **A tag is permanent**: it is written
    /// into `EdgeRecord::domain`, and re-numbering re-interprets every arena and
    /// recording on disk (`docs/API.md` §5.2).
    const TAG: u8;
}

/// The default domain: the host system clock (`CLOCK_REALTIME`-like), tag `0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SystemDomain;
impl Domain for SystemDomain {
    const TAG: u8 = 0;
}

/// A sensor's own clock (e.g. a lidar or camera timestamp), tag `1`. Distinct
/// from [`SystemDomain`] so a stamp from one cannot be used to query the other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SensorDomain;
impl Domain for SensorDomain {
    const TAG: u8 = 1;
}

/// Simulated time — a `/clock` publisher, a bag replay, or a physics engine —
/// tag `2`.
///
/// Separate from [`SystemDomain`] so `TimeDomainMismatch` fires for sim versus
/// steady clocks (`docs/API.md` §2.5, §5.2), and so `docs/PHASE5.md` §6's
/// `TFT019` can tell a wall-clock step from a sim step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SimDomain;
impl Domain for SimDomain {
    const TAG: u8 = 2;
}

/// A steady, monotone clock (`CLOCK_MONOTONIC`-like), tag `3`.
///
/// It cannot step, so a run of `NonMonotonicStamp` rejections on it is a
/// publisher defect, not an NTP step (`docs/PHASE5.md` §6, `TFT019`). It carries
/// no epoch guarantee across reboots or processes (`docs/API.md` §5.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SteadyDomain;
impl Domain for SteadyDomain {
    const TAG: u8 = 3;
}

/// Nanoseconds in one second.
const NANOS_PER_SEC: i64 = 1_000_000_000;

/// A nanosecond timestamp in domain `D`.
///
/// `Copy` and `Ord`; the phantom `D` carries the domain at the type level with no
/// runtime cost (`size_of::<Stamp<D>>() == 8`). Construct with
/// [`Stamp::from_nanos`] and read with [`Stamp::nanos`].
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

    /// Assemble a stamp from a `(seconds, nanoseconds)` pair — the shape of
    /// `builtin_interfaces/Time` and POSIX `struct timespec`. Exact, never a float
    /// (`docs/API.md` §5.1, normative).
    ///
    /// Total: no panic, no wrap, no saturation. Returns `None` for
    /// `nanos >= 1_000_000_000` (the field is a sub-second remainder; carrying the
    /// excess would turn a malformed message into a plausible stamp, `docs/API.md`
    /// R4, §5.2) and for `sec * 1e9 + nanos` outside `i64`. `None` does not say
    /// which: no caller branches on it (`docs/PROJECT.md` §5 D11).
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
        // `i128`, not staged `checked_mul`/`checked_add`: the staged form refuses
        // representable stamps at the negative end (`sec = -9_223_372_037`).
        // Not `wrapping_*`: a release build must refuse what a debug build refuses.
        let total = sec as i128 * NANOS_PER_SEC as i128 + nanos as i128;
        if total < i64::MIN as i128 || total > i64::MAX as i128 {
            return None;
        }
        Some(Stamp(total as i64, PhantomData))
    }

    /// Assemble a stamp from the two fields of a POSIX `struct timespec`.
    ///
    /// Takes fields, not the struct: `tf_tree_core` is `no_std` and its dependency
    /// budget has no `libc` (`docs/PROJECT.md` §5). Refuses everything
    /// [`Self::from_parts`] does, plus a negative `tv_nsec`, which POSIX allows
    /// only in a relative interval.
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
        // The range check above makes this cast lossless.
        Self::from_parts(tv_sec, tv_nsec as u32)
    }
}

// Manual impls so `Stamp<D>` is `Copy`/`Ord` without a bound on `D` for callers.
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

/// Selects an interpolation policy at runtime from an edge's stored discriminant.
///
/// The runtime selector stored in [`crate::edge::EdgeRecord::interp`] and
/// dispatched when a [`Guard`] samples an edge.
///
/// Deliberately not `#[non_exhaustive]`: every consumer maps it onto something
/// else and a catch-all arm has no honest body. An older binary reading a newer
/// arena is handled by [`InterpPolicy::from_u8`] collapsing an unknown
/// discriminant onto the default; the same holds for [`crate::edge::EdgeKind`].
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

/// A pose, and how far past the plan's newest common sample it was extrapolated.
///
/// Returned by [`Plan::at_extrapolating`]. There is deliberately no accessor that
/// yields the pose alone ([`0039`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0039-extrapolation-you-cannot-fail-to-notice.md)):
/// the distance travels with the pose. Not an error type;
/// [`ExtrapPolicy::Error`] is how a caller asks for a failure.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Extrapolated {
    /// The pose.
    pub pose: Iso3,
    /// Nanoseconds past the newest stamp that *every* dynamic edge on this plan
    /// has data for; `0` means every edge bracketed the query.
    ///
    /// Errs toward over-reporting: it is measured just *before* the fold, so a
    /// sample arriving mid-fold may leave a bracketed query reporting a positive
    /// value, but `0` is never reported for an invented pose.
    pub by_ns: i64,
    /// The dynamic edge whose newest stamp is [`Self::by_ns`] behind the query;
    /// meaningless when `by_ns == 0`. Data, not formatted
    /// (`docs/PROJECT.md` §5 D11).
    pub edge: EdgeId,
}

/// A pose and its derivatives at one instant — `docs/PHASE4.md` §2.2.
///
/// Returned by [`Plan::at_with_derivatives`]. The twist is body-frame (right),
/// expressed in the plan's **source** frame; see that method and
/// [`tf_tree_math::twist`]. `#[non_exhaustive]`: engine-produced, read-only to
/// callers, so growth cannot make a consumer wrong.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub struct Sample {
    /// The transform at the requested stamp — bit-identical to [`Plan::at`].
    pub pose: Iso3,
    /// First derivative, body frame, rad/s and m/s.
    pub twist: Twist,
    /// Second derivative, when the interpolant has one. Always `None` today:
    /// ScLerp's body twist is constant across a segment, so `Some(ZERO)` would
    /// claim smoothness the path lacks.
    pub accel: Option<Twist>,
}

/// One step of a compiled plan.
///
/// Not `#[non_exhaustive]`, for [`InterpPolicy`]'s reason: consumers classify
/// steps and a `_ =>` arm would silently under-count.
#[derive(Clone, Copy, Debug)]
pub enum Step {
    /// A constant transform composed directly (a folded static edge or a run of
    /// them). Pre-inverted at compile time when it came from an inverted edge.
    Static(Iso3),
    /// A dynamic edge to be sampled at evaluation time. `inverted` composes the
    /// inverse of the sampled pose (`acc.mul_inv(p)`); otherwise `acc * p`.
    Dyn {
        /// The edge to sample.
        edge: EdgeId,
        /// Whether to compose the sampled pose inverted.
        inverted: bool,
    },
}

/// A compiled `lookup(target, source)` path.
///
/// `Copy`, `Send`, `Sync`, heap-free, dependency-free: a fixed `[Step; MAX_DEPTH]`
/// array plus the topology generation it was compiled against. Evaluate it with
/// [`Plan::at`] and friends against a [`Guard`]; a generation mismatch is
/// [`LookupError::TopologyChanged`] ("re-plan"), never a silent stale read.
#[derive(Clone, Copy, Debug)]
pub struct Plan {
    generation: u64,
    steps: [Step; MAX_DEPTH],
    len: u8,
    domain: u8,
    /// How many of `steps[..len]` are [`Step::Dyn`]; see [`fold_into`].
    dyn_count: u8,
    /// The edge of the first [`Step::Dyn`], or [`EdgeId`]`(0)` when none; read it
    /// through [`Plan::first_dynamic_edge`].
    first_dyn: EdgeId,
}

impl Plan {
    /// The identity plan for `generation`: zero steps, and the buffer
    /// [`fold_into`] fills.
    ///
    /// The only constructor, so there is no half-built state a future arm can
    /// forget to complete. One array written in place removed two by-value
    /// copies from `Tree::plan` (#264). The identity array cannot be left
    /// uninitialised: an invalid `Step` discriminant is UB under `Copy`/`Debug`,
    /// and `MaybeUninit` is outside the unsafe budget (`docs/decisions/0007`).
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

    /// The plan's time-domain tag (the domain of its dynamic edges; `0` when the
    /// plan is all-static or empty).
    #[inline]
    #[must_use]
    pub fn domain(&self) -> u8 {
        self.domain
    }

    /// What [`fold_into`] derived, next to what a fresh scan of the same steps
    /// produces — `((stored_has_dynamic, stored_edge), (scanned, scanned))`.
    /// Test-only; the fields are private to this module.
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
        // A detached guard must not report `TopologyChanged`: no re-plan helps.
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

    /// Evaluate the plan at nanosecond stamp `t`, sampling every dynamic edge at
    /// `t`. Assumes generation and domain are already validated. `#[inline]` is
    /// load-bearing (`docs/API.md` §2.3).
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

    /// [`Self::fold_at`] under a caller-chosen extrapolation policy.
    ///
    /// A deliberate second copy: `fold_at` passes the `ExtrapPolicy::Error`
    /// literal so LLVM prunes the `Hold`/`ConstantTwist` arms on [`Self::at`]'s hot
    /// path, and a policy parameter would keep that match live. One path compiled
    /// twice, not a second spelling (`docs/PROJECT.md` §6).
    /// [`0039`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0039-extrapolation-you-cannot-fail-to-notice.md)
    /// §4.
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

    /// Like [`Self::fold_at`] but each dynamic step gallops from its own resumable
    /// cursor (`cursors[step_index]`), for a monotone stamp sweep.
    ///
    /// Deliberately not `#[inline]` (`docs/API.md` §2.3). Reached only from
    /// [`Self::fold_batch`]'s sub-chunk bypass (`docs/decisions/0060` step 2).
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

    /// Evaluate the plan at stamp `t` (an `At(t)` query).
    ///
    /// # Errors
    ///
    /// * [`LookupError::TopologyChanged`] — the topology changed since compilation.
    /// * [`LookupError::TimeDomainMismatch`] — `D` does not match the plan's edges.
    /// * Any sampling error from an edge ([`LookupError::NoData`],
    ///   [`LookupError::Extrapolation`], …).
    ///
    /// `#[inline]` is deliberate: `docs/API.md` §2.3.
    #[inline]
    pub fn at<D: Domain>(&self, g: &Guard, t: Stamp<D>) -> Result<Iso3, LookupError> {
        self.at_tagged(g, t.nanos(), D::TAG)
    }

    /// [`Self::at`], with the query's domain carried as a runtime tag.
    ///
    /// [`Domain`] is an open trait, so a foreign binding cannot dispatch to the
    /// typed form and carries the tag as data ([`0038`]). Same check, same
    /// [`LookupError::TimeDomainMismatch`]; Rust callers should use
    /// [`Self::at`].
    ///
    /// [`0038`]: https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0038-the-domain-a-binding-cannot-name.md
    ///
    /// # No `#[inline]`, on purpose
    ///
    /// Every other link on the scalar path is inlined, so this is the one
    /// cross-crate call `plan.at(&g, t)` emits (`docs/API.md` §2.3). Marking it
    /// is not obviously a win, and a probe consuming only `iso.t.x` inverts the
    /// sign; a re-measurement must consume all seven components, and
    /// `just embed-cost`'s probes (`tf_tree_bench::embed::one`,
    /// `bench_probe::depth3_lookup`) do not yet, so use `just bench-ab`.
    ///
    /// # Errors
    ///
    /// As [`Self::at`].
    pub fn at_tagged(&self, g: &Guard, nanos: i64, domain: u8) -> Result<Iso3, LookupError> {
        self.check_generation(g)?;
        self.check_domain_tag(domain)?;
        // The counter brackets the fold only: the two checks above fail on the
        // query, name no edge, and would blame a working publisher
        // (`docs/PHASE5.md` §5.2).
        self.note(g, self.first_dynamic_edge(), self.fold_at(g, nanos))
    }

    /// Record one evaluation's outcome against the diagnostic counters.
    ///
    /// Every entry point that folds the plan goes through here:
    /// `docs/PHASE5.md` §5.3 makes the error counters normative and they are the
    /// basis of `TFT010`/`TFT011`. `edge` is [`Self::first_dynamic_edge`], passed
    /// in because a batch caller resolves it once.
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

    /// The single dynamic edge this plan traverses, for counter attribution.
    ///
    /// `EdgeId(0)` when the plan crosses several, which [`Guard::note_ok`] folds
    /// into "credit no edge". Hence the stored count, not just a flag.
    #[inline]
    fn first_dynamic_edge(&self) -> EdgeId {
        if self.dyn_count == 1 {
            self.first_dyn
        } else {
            EdgeId(0)
        }
    }

    /// Fold the plan at `t`, accumulating the body twist alongside the pose.
    ///
    /// For `T_ac = T_ab·T_bc` (`docs/PHASE4.md` §2.3):
    ///
    /// ```text
    /// V_ac^c = Ad(T_bc⁻¹)·V_ab^b + V_bc^c
    /// ```
    ///
    /// * A static step still costs an adjoint: its twist is zero but the frame
    ///   changes.
    /// * An inverted step folds to one adjoint: `V' = Ad(p)·(V_acc − V_p)`.
    ///
    /// The sampler is a parameter so the composition lives in one place; the
    /// batch form differs only by resuming from a cursor.
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
                    // No twist of its own, but the body frame moves.
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

    /// [`Self::fold_with_derivatives`] restarting every bracket search at the
    /// window midpoint — the scalar path.
    #[inline]
    fn fold_at_with_derivatives(&self, g: &Guard, t: i64) -> Result<(Iso3, Twist), LookupError> {
        self.fold_with_derivatives(|_, edge| g.sample_with_twist(edge, t, ExtrapPolicy::Error))
    }

    /// [`Self::fold_with_derivatives`] resuming each step's bracket search from
    /// its own cursor, a hint only (see [`Guard::cursor`]): a monotone
    /// [`Layout::QuatTwist`] batch is `O(1)` amortized per stamp.
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

    /// Evaluate the plan at `t`, returning the pose **and its derivatives** —
    /// `docs/PHASE4.md` §2.2.
    ///
    /// The twist is body-frame (right), `V^b = (T⁻¹Ṫ)^∨`, expressed in the plan's
    /// **source** frame, because `T_target_source` maps *from* the source. Use
    /// [`tf_tree_math::Twist::to_spatial`] with the returned pose for the target
    /// frame. For `plan(map, base)` with `base` rotated +90° about z moving along
    /// **map**'s +x at 1 m/s:
    ///
    /// ```text
    /// sample.twist.v              == (0, −1, 0)   // resolved in base axes
    /// sample.twist.to_spatial(&p) == (1,  0, 0)   // resolved in map axes
    /// ```
    ///
    /// `‖v‖` is identical in both, so a magnitude check cannot catch a mix-up.
    /// Costs roughly two plain lookups (see [`tf_tree_math::twist`]).
    ///
    /// # Errors
    ///
    /// Everything [`Self::at`] can return, plus:
    ///
    /// * [`LookupError::DerivativesUnavailable`] — some edge on the path is
    ///   `LerpSlerp`, whose body twist is an artifact of the interpolant rather
    ///   than of the motion. Refused rather than returned (§2.4).
    /// * [`LookupError::NoSegment`] — an edge has a pose at `t` but no segment to
    ///   differentiate (one retained sample, or two with equal stamps).
    pub fn at_with_derivatives<D: Domain>(
        &self,
        g: &Guard,
        t: Stamp<D>,
    ) -> Result<Sample, LookupError> {
        self.at_with_derivatives_tagged(g, t.nanos(), D::TAG)
    }

    /// [`Self::at_with_derivatives`], with the query's domain as a runtime tag
    /// ([`0038`]).
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

    /// Sample every dynamic edge at *its own* newest stamp. Stamps may differ
    /// between edges; use [`Self::latest_common`] for a consistent snapshot.
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

    /// [`Self::at`], permitting extrapolation past the newest sample under
    /// `policy`, and reporting how far the answer was extrapolated
    /// ([`0039`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0039-extrapolation-you-cannot-fail-to-notice.md)).
    ///
    /// The policy is per query, not per edge: the caller who bears the
    /// consequence chooses. [`ExtrapPolicy::Error`] is [`Self::at`] with a
    /// distance attached on success.
    ///
    /// `by_ns` costs one `newest_stamp` load per dynamic edge, taken **before**
    /// the fold and only here, so [`Self::at`]'s generated code is unmoved. The
    /// order is a soundness guarantee; see the body.
    /// # Errors
    ///
    /// As [`Self::at`]. Under [`ExtrapPolicy::Error`] a query past the newest
    /// sample is [`LookupError::Extrapolation`]; under the other two it is not.
    pub fn at_extrapolating<D: Domain>(
        &self,
        g: &Guard,
        t: Stamp<D>,
        policy: ExtrapPolicy,
    ) -> Result<Extrapolated, LookupError> {
        self.at_extrapolating_tagged(g, t.nanos(), D::TAG, policy)
    }

    /// [`Self::at_extrapolating`], with the query's domain carried as a runtime
    /// tag ([`0038`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0038-the-domain-a-binding-cannot-name.md)).
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
        // Measure before the fold: `newest_stamp` is non-decreasing, so
        // `common_before <= common_during`. `by_ns > 0` may over-report;
        // `by_ns == 0` means every edge held data past `nanos` before the fold
        // began. Measuring after let a mid-fold `push` report 0 for an invented
        // pose. Not `note`d: the fold's `note` is this query's one counter event.
        let common = self.newest_common(g);
        let pose = self.note(g, edge, self.fold_at_policy(g, nanos, policy))?;
        let (by_ns, which) = match common? {
            // `saturating_sub`: a plain subtraction wraps in release, and a wrapped
            // negative would report `by_ns == 0` for the most extrapolated answer
            // (see `sample::span_ns`).
            Some((common, which)) => (nanos.saturating_sub(common).max(0), which),
            // Static-only: nothing can be extrapolated, so nothing was.
            None => (0, EdgeId(0)),
        };
        Ok(Extrapolated {
            pose,
            by_ns,
            edge: which,
        })
    }

    /// Sample every dynamic edge at the newest stamp common to all of them —
    /// tf2's `Time(0)` semantics.
    /// # Errors
    ///
    /// [`LookupError::TopologyChanged`], [`LookupError::NoData`] if an edge is
    /// empty, or [`LookupError::Extrapolation`] if an edge's retained window does
    /// not reach the common stamp.
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

    /// The newest stamp every dynamic edge has data for, and the edge that
    /// produced it — `None` when static-only. Shared by [`Self::latest_common`]
    /// and [`Self::at_extrapolating`] ([`0039`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0039-extrapolation-you-cannot-fail-to-notice.md)).
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

    /// The **outer bound outside which this plan certainly cannot answer**, or
    /// `None` when it is unbounded (`docs/PHASE5.md` §4.2).
    ///
    /// It is an intersection of outer windows — the lower end a `max`, the upper
    /// end a `min` (`latest_common`'s own stamp) — and says nothing about holes
    /// inside them. Inside it, `at` answering is not evidence anything was
    /// observed near the stamp; `tf_tree doctor`'s `TFT009` detects that.
    ///
    /// Not a shared helper with `latest_common`, which would pay a second atomic
    /// load per edge; `span_answers_exactly_at_the_ends_it_reports`
    /// (`tf_tree`'s `tests/behavior.rs`) pins the agreement. It lives beside
    /// [`SampleRing::retained`](crate::buffer::SampleRing::retained) so the
    /// window definition has one home.
    ///
    /// * `Some((t0, t1))`, `t0 <= t1` — answerable there, and nowhere else
    ///   without extrapolating.
    /// * `Some((t0, t1))`, `t0 > t1` — an empty intersection: a real answer, not
    ///   an error.
    /// * `None` — every step is static, so any stamp is answerable.
    ///
    /// On a live arena the answer ages as it is returned (the contract of
    /// [`Self::latest`]); on a frozen `.tft` it is exact.
    ///
    /// # Errors
    ///
    /// [`LookupError::TopologyChanged`] (or [`LookupError::ChildDetached`] on a
    /// fork-poisoned guard), [`LookupError::UnknownEdge`], or
    /// [`LookupError::NoData`] naming the first edge that has never published.
    pub fn span(&self, g: &Guard) -> Result<Option<(i64, i64)>, LookupError> {
        self.check_generation(g)?;
        let mut span: Option<(i64, i64)> = None;
        for step in self.steps() {
            let Step::Dyn { edge, .. } = step else {
                // A static step constrains nothing in time.
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

    /// The **slowest** declared nominal publish rate among this plan's dynamic
    /// edges, in milli-hertz, or `None` when none declares one
    /// (`docs/decisions/0018`).
    ///
    /// With [`Self::span`], the whole engine-side input to a caller's blocking
    /// wait; there is no blocking primitive in the arena (`0018`). The shim's wait:
    ///
    /// ```text
    /// loop {
    ///     let g = tree.guard();
    ///     match plan.span(&g) {
    ///         Ok(None)                                  => return plan.at(&g, wanted),
    ///         Ok(Some((_, newest))) if newest >= wanted  => return plan.at(&g, wanted),
    ///         Ok(Some((_, newest))) => sleep(min(deadline_remaining,
    ///                                           (wanted - newest) + one_period)),
    ///         // An edge that has never published raises NoData: "not started yet".
    ///         Err(NoData { .. }) => sleep(min(deadline_remaining, one_period)),
    ///         Err(e) => return Err(e),
    ///     }
    ///     if now >= deadline { return Err(Timeout) }
    /// }
    /// ```
    ///
    /// where `one_period` is `1e9 / (mhz / 1000)` nanoseconds from this method,
    /// a prediction and not a poll interval.
    ///
    /// The answer is the slowest edge because a plan is answerable only when
    /// every dynamic edge has reached the stamp. `EdgeRecord::nominal_rate_mhz == 0`
    /// means *undeclared* and is skipped, not read as 0 Hz (`docs/PHASE5.md` §6,
    /// `TFT007`). `None` is a real third answer: fall back to a conservative
    /// period and say so once at startup (`0018` *Consequences*). A declared rate
    /// may be an observed one (`tf_tree topology --discover`); here that costs one
    /// extra wake.
    ///
    /// Generation-checked like [`Self::span`], so a waiter never spins against a
    /// plan that cannot be satisfied; but it does **not** return
    /// [`LookupError::NoData`] for a never-published edge, since a declaration
    /// is a property of the topology and the caller asks before data exists.
    ///
    /// # Errors
    ///
    /// [`LookupError::TopologyChanged`], [`LookupError::ChildDetached`], or
    /// [`LookupError::UnknownEdge`] if a step names an edge this arena has no
    /// record for.
    /// # Errors
    ///
    /// [`LookupError::TopologyChanged`], [`LookupError::ChildDetached`], or
    /// [`LookupError::UnknownEdge`] if a step names an edge this arena has no
    /// record for.
    pub fn slowest_nominal_rate_mhz(&self, g: &Guard) -> Result<Option<u32>, LookupError> {
        self.check_generation(g)?;
        let mut slowest: Option<u32> = None;
        for step in self.steps() {
            let Step::Dyn { edge, .. } = step else {
                // A static edge has no publisher and no period.
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

    /// Evaluate the plan at each stamp in `stamps`, writing results into `out`.
    ///
    /// When `stamps` is monotone non-decreasing, each dynamic edge resumes its
    /// bracket search from the previous stamp via an exponential (galloping) search
    /// — `O(1)` amortized per stamp instead of `O(log n)`. Non-monotone input
    /// falls back to an independent search per stamp.
    ///
    /// # Errors
    ///
    /// As [`Self::at`], plus [`LookupError::BufferTooSmall`] when
    /// `out.len() < stamps.len()` — checked before anything is written, so a
    /// refusal leaves `out` untouched. Extra `out` slots are left untouched on
    /// success too.
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

        // Through [`Self::fold_batch`] (`docs/decisions/0060` step 2), with
        // `elems == 1` and a move as the emitter.
        self.fold_batch(
            g,
            stamps,
            |s: Stamp<D>| s.nanos(),
            |iso, dst| dst[0] = *iso,
            1,
            out,
        )
    }

    /// Evaluate a batch **directly into a caller's buffer**, in `layout`.
    ///
    /// Unlike [`Self::at_many`], writes the layout a consumer wants (a 4x4 `f64`
    /// matrix, say) once, in place, with no intermediate buffer. `Quat` shares
    /// `Iso3`'s bytes exactly (see [`crate::layout`]). `out` is a flat `f64`
    /// slice of at least `stamps.len() * layout.elems()`; use
    /// [`Self::at_many_into_f32`] for [`Layout::Affine32`].
    ///
    /// [`Layout::QuatTwist`] folds through [`Self::at_with_derivatives`]'s path,
    /// so its thirteen `f64` per stamp are bit-identical to the scalar call,
    /// refusals included.
    ///
    /// `stamps` is raw nanoseconds with the domain as the type parameter:
    /// `Stamp<D>` is not `repr(transparent)`, so `&[i64]` callers (FFI, NumPy)
    /// would otherwise have to copy.
    ///
    /// # Errors
    ///
    /// [`LookupError::BufferTooSmall`] if `out` cannot hold the batch, or
    /// [`LookupError::WrongElementType`] for an `f32` layout. Both are checked
    /// **before any element is written** (`docs/PHASE3.md` §5.3).
    ///
    /// For [`Layout::QuatTwist`], additionally
    /// [`LookupError::DerivativesUnavailable`] and [`LookupError::NoSegment`], as
    /// [`Self::at_with_derivatives`].
    ///
    /// Only those two checks are all-or-nothing: every other error is a property
    /// of a *stamp*, so `k` rows may already be written with nothing marking the
    /// boundary. `DerivativesUnavailable` is per-edge and fires at element 0;
    /// `NoSegment` is not.
    ///
    /// Otherwise as [`Self::at`].
    pub fn at_many_into<D: Domain>(
        &self,
        g: &Guard,
        stamps: &[i64],
        layout: Layout,
        out: &mut [f64],
    ) -> Result<(), LookupError> {
        self.at_many_into_tagged(g, stamps, D::TAG, layout, out)
    }

    /// [`Self::at_many_into`], with the query's domain as a runtime tag
    /// ([`0038`]).
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
        // Matched once: inside the loop it would add a branch per element.
        match layout {
            Layout::Mat4 => self.fold_batch(g, stamps, |s| s, write_mat4, n, out),
            Layout::Quat => self.fold_batch(g, stamps, |s| s, write_quat, n, out),
            // Needs the twist, so it folds through `fold_batch_with_twist`.
            Layout::QuatTwist => self.fold_batch_with_twist(g, stamps, n, out),
            // Unreachable (rejected above); an error, not a panic, keeps the
            // panic-free lint posture.
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

    /// [`Self::at_many_into_f32`], with the query's domain as a runtime tag
    /// ([`0038`]).
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

    /// The shared batch loop: a chunk's brackets are read first, then folded.
    ///
    /// Generic over the element type so `f64` and `f32` share one copy of the
    /// cursor logic (galloping search, seqlock retry), and over the stamp type so
    /// [`Self::at_many`] shares the body.
    ///
    /// `docs/decisions/0060` Decision A: a monotone batch of at least
    /// [`FOLD_MIN_BATCH`] stamps is walked in chunks of [`FOLD_LANES`], each
    /// folded step by step. For every dynamic step, phase 1 reads every lane's
    /// bracket through
    /// [`SampleRing::read_from`](crate::buffer::SampleRing::read_from) and phase 2
    /// calls `Interp::eval` per lane with no atomic load between elements; the
    /// phase buffering carries the win (§10.1, §10.5). Every row is bit-identical
    /// to [`Self::at`] on a quiescent ring
    /// (`crates/tf_tree/tests/batch_phases.rs`).
    ///
    /// A non-monotone batch or one below [`FOLD_MIN_BATCH`] stays per-stamp
    /// (§1, §10.4).
    ///
    /// On failure the rows before the first failing stamp are written, its error
    /// is returned, and later rows are untouched. A chunk may have read brackets
    /// past the failure, which only moves ring cursors: hints, never results
    /// (see [`Guard::cursor`]).
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
        // Hoisted: loop-invariant.
        let edge = self.first_dynamic_edge();

        // `chunks_exact_mut` zipped with `stamps` bounds the walk by the batch and
        // leaves a caller's over-long buffer untouched; not a speed choice.
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

    /// [`Self::fold_batch`]'s chunked pass, in **its own stack frame**.
    ///
    /// `#[inline(never)]` is for the frame: the ~4 kB lane buffers are reserved
    /// in the prologue before any branch, so inlining would charge every entry
    /// point, bypassing calls included. Entry frames stay at `sub $0x378` /
    /// `sub $0x158`; `fold_chunked` takes 4 056–4 088 B. It is not what fixed the
    /// small-N rows; that was
    /// [`SampleRing::read_from`](crate::buffer::SampleRing::read_from)'s
    /// `#[inline(always)]`.
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
        // Declared once per batch: per chunk is what made §10.4's small-N rows bad.
        let mut cursors = [0u64; MAX_DEPTH];
        let mut acc = [Iso3::IDENTITY; FOLD_LANES];
        let mut brackets = [Bracket::Exact(Iso3::IDENTITY); FOLD_LANES];

        for (chunk, dsts) in stamps
            .chunks(FOLD_LANES)
            .zip(out.chunks_mut(FOLD_LANES * elems))
        {
            // Shrinks to the lowest-numbered failed stamp: the one the per-stamp
            // fold would have stopped at.
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
                        // One bounds check and policy dispatch per chunk per step.
                        let Some((interp, ring)) = g.view().sampler(*e) else {
                            failure = Some(LookupError::UnknownEdge { edge: *e });
                            live = 0;
                            break;
                        };
                        let cursor = &mut cursors[k];

                        // Phase 1: every bracket, no arithmetic.
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

                        // Phase 2: every fold, no atomics.
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
            // Per stamp: the counters count lookups, not chunks.
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

    /// [`Layout::QuatTwist`]'s batch loop — [`Self::fold_batch`]'s sibling.
    ///
    /// A sibling, not a parameter: it needs [`Self::fold_at_with_derivatives`]'s
    /// `(Iso3, Twist)` fold, and generalising `fold_batch` would put a closure or
    /// a branch into the scalar batch path (`crate::layout` fixes the rule).
    /// Ascending stamps ride a per-step cursor (`docs/API.md` §3.3); the cursor is
    /// a hint, so both branches are bit-identical. It calls the same fold as
    /// `at_with_derivatives` so the two can never disagree about a velocity.
    #[inline]
    fn fold_batch_with_twist(
        &self,
        g: &Guard,
        stamps: &[i64],
        elems: usize,
        out: &mut [f64],
    ) -> Result<(), LookupError> {
        // Hoisted, as in `fold_batch`.
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

    /// Emit the minimum set of knots such that [`LerpSlerp`] between adjacent knots
    /// stays within `tol` of the exact plan evaluation across `span`.
    ///
    /// Recursive bisection, bounded by [`MAX_ADAPTIVE_DEPTH`] and [`MAX_KNOTS`];
    /// all output lives in the caller's `scratch`. Returns parallel slices
    /// `(stamps, poses)`, strictly increasing in stamp.
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

    /// [`Self::at_adaptive`], with the query's domain carried as a runtime tag
    /// (`docs/decisions/0038-the-domain-a-binding-cannot-name.md`).
    ///
    /// `D` is storage only, fixing the element type of `scratch` and the returned
    /// slice; `domain` is the query and is what is checked. A Rust caller wants
    /// [`Self::at_adaptive`].
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
        // Counted once per call: `subdivide` folds up to `MAX_KNOTS` times and
        // per-fold credit would swamp `lookups_ok`.
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
            // Degenerate span: a single knot.
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
    // Splittable: depth budget, a non-adjacent segment, and room for one more
    // knot plus the up-to-`depth` ancestors that still emit one each on unwind.
    // The width is taken in `u64`: `at_adaptive(i64::MIN, i64::MAX)` is a
    // legitimate request, and a signed subtraction panics in a checked build and
    // wraps in release, silently returning a two-knot line. `wrapping_sub` on the
    // `u64` casts is the exact width for every ordered `i64` pair.
    let width = (b_s as u64).wrapping_sub(a_s as u64);
    let can_split = depth < MAX_ADAPTIVE_DEPTH
        && width > 1
        && scratch.stamps.len() + (MAX_ADAPTIVE_DEPTH as usize) + 1 < MAX_KNOTS;
    if can_split {
        // `width / 2` fits an `i64` and so does the midpoint; `wrapping_add`
        // documents that the add cannot overflow.
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
    // Accept segment a..b: emit its right endpoint.
    scratch.stamps.push(Stamp::from_nanos(b_s));
    scratch.poses.push(b_p);
    Ok(())
}

/// Whether `approx` is within `tol` of `exact` (rotation angle + translation).
fn within(tol: ErrBound, approx: &Iso3, exact: &Iso3) -> bool {
    // ‖log_so3(q_approx* · q_exact)‖
    let dq = approx.q.conjugate() * exact.q;
    let rot = log_so3(dq).norm();
    let trans = approx.t.sub(exact.t).norm();
    rot <= tol.rot_rad && trans <= tol.trans
}

/// The per-component error tolerance for [`Plan::at_adaptive`].
///
/// `#[non_exhaustive]`: build it with [`ErrBound::new`], like `tf_tree::EdgeCfg`;
/// a tolerance is a shape that grows.
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

/// Caller-provided scratch storage for [`Plan::at_adaptive`], sized for the
/// maximum knot set; its allocation is not counted against `at_adaptive`.
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
/// generation once, so a run of lookups validates against one snapshot. Make one
/// guard per batch of lookups.
pub struct Guard<'a> {
    view: ArenaView<'a>,
    /// The pinned topology generation, or [`DETACHED`] for a guard built by
    /// [`Guard::detached`].
    generation: u64,
    /// Successful lookups so far, flushed to the arena on drop.
    ///
    /// A plain `Cell<u32>`, not an atomic (`docs/PHASE5.md` §5.4): `Guard` is
    /// `!Sync`, and a per-lookup `fetch_add` would be contended across readers.
    /// The flush saturates.
    #[cfg(feature = "counters")]
    ok: core::cell::Cell<u32>,
    /// Which edge's counters to credit, when every lookup in the batch went
    /// Which edge's counters to credit, when every lookup in the batch went
    /// through one plan. `None` before any lookup, and once two different edges
    /// were seen: a multi-edge plan credits the participant total and no edge.
    #[cfg(feature = "counters")]
    ok_edge: core::cell::Cell<Option<EdgeId>>,
    /// Per-step bracket-search hints, packed `(edge << 32) | index`, so a scalar
    /// lookup resumes beside the previous answer instead of restarting at the
    /// window midpoint (`docs/design/fast-path.md` §12; `step_cost`: 54.58 ->
    /// 40.71 ns/sample at capacity 4096).
    ///
    /// A hint never changes a result:
    /// [`SampleRing::sample_from`](crate::buffer::SampleRing::sample_from) returns
    /// exactly what [`SampleRing::sample`](crate::buffer::SampleRing::sample)
    /// does, so a stale or absent cursor is safe with no invalidation. An index
    /// truncated past `u32::MAX` is lifted back onto the live window by
    /// `sample::rebase_hint`; otherwise the clamp would pin it to the oldest
    /// sample forever.
    ///
    /// A `Cell` because `Guard` is `!Sync`. One packed word per step halves the
    /// stores in `Guard::new`. The edge tag self-invalidates the hint when one
    /// guard evaluates several plans: a mismatch costs one comparison.
    cursor: [core::cell::Cell<u64>; MAX_DEPTH],
    /// `(generation at creation, how to read it now)`, for the fork check.
    ///
    /// The flush writes into the arena from a destructor, and a shared mapping is
    /// `MADV_DONTFORK`, so in a `fork` child it would fault (as `EdgeWriter::drop`
    /// guards, `docs/decisions/0005` step 9). A function pointer because the core
    /// is `no_std`; the facade supplies `tf_tree_ipc::fork::generation`. `None`
    /// for a heap arena.
    #[cfg(feature = "counters")]
    fork: Option<(u64, fn() -> u64)>,
}

/// The generation a [`Guard::detached`] guard carries.
///
/// Unreachable as a real generation: it starts at 0 and bumps once per
/// mutation. Encoding the poison in an existing field adds no load to the hot
/// path, since [`Plan::check_generation`] already compares it.
const DETACHED: u64 = u64::MAX;

/// Which [`crate::counters::EdgeCounters`] field a lookup error belongs in.
/// readable as a table.
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

    /// The same classification against the participant mirror. Two `match`es,
    /// not a generic: the two `#[repr(C)]` records must not be made
    /// interchangeable (`counters.rs` pins their shared prefix).
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

/// Classify a lookup error into `(edge, field)`, or `None` when it names no
/// edge: `UnknownFrame`, `Disconnected` and `TopologyChanged` describe the query
/// (D11), and filing them under an edge would blame a working publisher.
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
            // Split: past the newest usually means a publisher stopped, before
            // the oldest a consumer running behind (`TFT010`/`TFT011`).
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

/// Flush the batch's success count into the arena — one relaxed atomic per
/// guard, not per lookup (`docs/PHASE5.md` §5.4).
#[cfg(feature = "counters")]
impl Drop for Guard<'_> {
    fn drop(&mut self) {
        let n = self.ok.get();
        // Read-only guard, as in `note_err`: a consumer's guard drops every batch.
        if n == 0 || !self.view.is_writable() {
            return;
        }
        // Fork guard: in a child the arena is a hole and this write faults.
        if let Some((born, read)) = self.fork {
            if read() != born {
                return;
            }
        }
        use crate::sync::Ordering::Relaxed;
        // Credited to an edge only when the whole batch went through one.
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
    /// lookups. The pinned value is always stable: A1 removed the odd
    /// "write in progress" state.
    #[must_use]
    pub fn new(view: ArenaView<'a>) -> Guard<'a> {
        let generation = view.topology().stable_generation();
        Guard {
            view,
            generation,
            // `EdgeId(0)` is the sentinel: a fresh guard matches no edge.
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
    /// return a value that changes when the process forks; a heap arena passes
    /// nothing.
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

    /// Record a successful lookup through `edge` (`docs/PHASE5.md` §5.4): one
    /// non-atomic increment, compiled away without the `counters` feature.
    #[inline]
    pub(crate) fn note_ok(&self, edge: EdgeId) {
        #[cfg(feature = "counters")]
        {
            self.ok.set(self.ok.get().saturating_add(1));
            // `EdgeId(0)` is "no edge" (a multi-edge plan) and must not latch:
            // it would funnel every reader's flush into the reserved edge-0 record,
            // a phantom count and false sharing on one line.
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

    /// Record a failed lookup. Unlike [`Self::note_ok`] this writes straight
    /// through: failures are rare, and buffering would lose the evidence of a
    /// process that dies mid-fault (`docs/PHASE5.md` §5.3).
    #[inline]
    pub(crate) fn note_err(&self, err: &LookupError) {
        #[cfg(feature = "counters")]
        {
            use crate::sync::Ordering::Relaxed;
            // A read-only view (D18) must not write: it would SIGSEGV. A
            // read-only participant keeps no counters.
            if !self.view.is_writable() {
                return;
            }
            let Some((edge, field)) = counter_of(err) else {
                return;
            };
            // The failure's own stamp when it has one (`no_std` has no clock);
            // zero reads as "never".
            let now = match *err {
                LookupError::Extrapolation { requested, .. } => requested,
                _ => 0,
            };
            // Both halves: per-participant counters are what make a diagnostic
            // actionable (`docs/PHASE5.md` §5.2).
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
                    // High-water mark: `TFT011` reads it against the ring's span.
                    let gap = if requested > newest {
                        requested.saturating_sub(newest)
                    } else {
                        oldest.saturating_sub(requested)
                    };
                    // `fetch_max`, not load/compare/store: the latter lets the mark
                    // regress under concurrent writers and inverts `TFT011`.
                    c.worst_extrap_gap_ns.fetch_max(gap, Relaxed);
                }
            }
        }
        #[cfg(not(feature = "counters"))]
        let _ = err;
    }

    /// A guard that fails every evaluation with [`LookupError::ChildDetached`],
    /// without reading `view`.
    ///
    /// For a facade that knows the arena is unreachable (a shared mapping lost
    /// under `fork()`) where its API is infallible (`Tree::guard`). [`Self::new`]
    /// reads the topology immediately, so it cannot serve. `view` must still be
    /// over a valid arena, since [`Self::view`] hands it out; supply a throwaway.
    /// There is no `poisoned(view, err)`: it would cost a 32-byte field on a
    /// struct built per `at()` call. See `DETACHED`.
    #[must_use]
    pub fn detached(view: ArenaView<'a>) -> Guard<'a> {
        Guard {
            view,
            generation: DETACHED,
            // Never counts a success or reaches a search, so its destructor is a
            // no-op; zeroed fields make that true.
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
    /// `#[inline]`, like its two siblings below (`docs/API.md` §2.3).
    #[inline]
    pub(crate) fn sample(
        &self,
        edge: EdgeId,
        t: i64,
        policy: ExtrapPolicy,
    ) -> Result<Iso3, LookupError> {
        // One bounds check resolves both the interp discriminant and the ring.
        let (interp, ring) = self
            .view
            .sampler(edge)
            .ok_or(LookupError::UnknownEdge { edge })?;
        match InterpPolicy::from_u8(interp) {
            InterpPolicy::LerpSlerp => ring.sample::<LerpSlerp>(t, policy),
            InterpPolicy::ScLerp => ring.sample::<ScLerp>(t, policy),
        }
    }

    /// [`Self::sample`], resuming from this guard's cursor for step `k` — the
    /// scalar fold's entry point. See [`Guard::cursor`].
    #[inline]
    pub(crate) fn sample_hinted(
        &self,
        k: usize,
        edge: EdgeId,
        t: i64,
        policy: ExtrapPolicy,
    ) -> Result<Iso3, LookupError> {
        // Always in range (plans are bounded by MAX_DEPTH); `get` keeps it provable.
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
        // Success only: a failed search leaves `cursor` at a position no
        // successful search produced.
        if out.is_ok() {
            slot.set((u64::from(edge.0) << 32) | (cursor & 0xFFFF_FFFF));
        }
        out
    }

    /// Sample edge `edge` at `t` and also return its body twist, in 1/second.
    /// Refuses `LerpSlerp` ([`LookupError::DerivativesUnavailable`]).
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

    /// [`Self::sample_with_twist`], resuming from `cursor`. The refusal is
    /// checked before the ring is touched, so the batch layout's refusal matches
    /// the scalar call's.
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

    /// Both ends of a dynamic edge's retained window, `(oldest, newest)`.
    ///
    /// Two independent `head` loads, so a concurrent `push` on a live ring can
    /// widen the pair: the staleness [`Plan::latest`] has (see [`Plan::span`]).
    pub(crate) fn window(&self, edge: EdgeId) -> Result<(i64, i64), LookupError> {
        let ring = self
            .view
            .ring(edge)
            .ok_or(LookupError::UnknownEdge { edge })?;
        match (ring.oldest_stamp(), ring.newest_stamp()) {
            (Some(oldest), Some(newest)) => Ok((oldest, newest)),
            // Empty is `NoData`, never an empty interval.
            _ => Err(LookupError::NoData { edge }),
        }
    }

    /// An edge's declared nominal publish rate, in milli-hertz, `0` meaning
    /// *undeclared*. The sentinel is passed through: what it means is decided by
    /// [`Plan::slowest_nominal_rate_mhz`] and `docs/PHASE5.md` §6's `TFT007`.
    /// Reads the edge record, so it cannot return [`LookupError::NoData`].
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
/// Walks up from both frames to their lowest common ancestor under the topology
/// seqlock, retrying if a mutation lands mid-walk; the plan records that
/// generation. `edge_meta` supplies each edge's kind/domain/static-pose for
/// constant folding, and returns `None` for an edge id with no record.
///
/// # Errors
///
/// * [`LookupError::Disconnected`] — different connected components.
/// * [`LookupError::TreeTooDeep`] — more than [`MAX_PATH_EDGES`] raw edges, or
///   more than [`MAX_DEPTH`] folded steps (readable off `depth`).
/// * [`LookupError::FrameOutOfRange`] — a frame id is out of range for `topo`.
/// * [`LookupError::MissingEdge`] — a parent link on the path records no edge.
/// * [`LookupError::UnknownEdge`] / [`LookupError::MixedTimeDomains`] — raised
///   by the constant fold, before the length refusal, so a defect on a too-long
///   path is named.
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

    // Retry the whole walk if a mutation lands between reads (`docs/PHASE1.md`
    // §5.2 reader protocol).
    'walk: loop {
        // Every published generation is stable (A1); the retry only discards a
        // walk that straddled a mutation.
        let start_gen = topo.generation();

        // Read (parent, depth, edge_of_child) for `f`, restarting on a generation
        // change.
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

        // Edges walking up from target (emitted inverted, in order) and from
        // source (emitted forward, reversed); 512 bytes of stack together.
        let mut t_edges = [0u32; MAX_PATH_EDGES];
        let mut nt = 0usize;
        let mut s_edges = [0u32; MAX_PATH_EDGES];
        let mut ns = 0usize;

        // Record the edge on the link from `$frame` up to its parent. Edge id `0`
        // is the "no edge" sentinel and also a real slot, so it must never become
        // a `Step::Dyn`. The bound is on `nt + ns` ("edges walked"), checked
        // before the sentinel so a defect wins by position on the path. The walk
        // stops at the bound (a cyclic parent chain would not terminate), so the
        // reported depth is `MAX_PATH_EDGES + 1`: "more than the bound".
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

        // Bring the deeper frame up until depths match.
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

        // Walk both up in lockstep until they meet at the LCA.
        while a != b {
            if pa == 0 || pb == 0 {
                // Ran out of parents on one side without meeting: different trees.
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
            // Only parent and edge_of_child are needed past the lockstep phase.
            let (p, _d, e) = read!(a);
            pa = p;
            ea = e;
            let (p, _d, e) = read!(b);
            pb = p;
            eb = e;
        }

        // Confirm the whole walk observed one generation.
        if topo.generation() != start_gen {
            spin();
            continue 'walk;
        }

        // Folded into the plan about to be returned, not a by-value temporary
        // (#264); a refusal drops the half-written local.
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

/// Constant folding: replace static edges with constant steps (pre-inverting
/// when the step is inverted), then collapse adjacent `Static` runs by composing
/// them. Writes the folded steps into `plan` along with `len`, `domain`,
/// `dyn_count` and `first_dyn`.
///
/// `dyn_count` and `first_dyn` are derived here, where the step is already in
/// hand, so `Plan::at` does no O(`len`) scan. `Plan` is not an arena structure:
/// no format version or layout hash is touched. This is the only writer;
/// `plan_derived_fields_match_a_fresh_scan` pins it against a fresh scan.
///
/// `plan` may be left partially written on `Err`: every entry is a valid `Step`
/// and the four fields are published at the end past every `?`, so it is still
/// the identity plan.
///
/// `t_edges` are in walk order, emitted inverted, then `s_edges` **reversed**,
/// emitted forward. The reversal is load-bearing: `Iso3` composition is not
/// associative under rounding, and a different order gives different bits that
/// every tolerance-based test would accept.
///
/// # Running past the end of the output array
///
/// A path can fold to more than `MAX_DEPTH` steps. The loop skips the write,
/// keeps incrementing `n`, and keeps resolving every edge through `edge_meta`,
/// so `n` is the true compiled length that [`LookupError::TreeTooDeep`] reports
/// and a defect past the bound is still named (`0034`'s precedence). Returning
/// early is cheaper (994 ns against 1778 ns on a refused 64-edge chain) but
/// reports length instead of defect. The collapse decision reads a tracked
/// `last_static`, not `out[n - 1]`.
/// The collapse decision therefore reads a tracked `last_static` rather than
/// `out[n - 1]`, which is the one thing that would not work past the array end.
///
/// # Errors
///
/// * [`LookupError::UnknownEdge`] — a step names an edge with no record.
/// * [`LookupError::MixedTimeDomains`] — the dynamic edges do not share one
///   time domain.
/// * [`LookupError::TreeTooDeep`] — more than [`MAX_DEPTH`] steps; reports the
///   exact folded count.
fn fold_into(
    plan: &mut Plan,
    t_edges: &[u32],
    s_edges: &[u32],
    edge_meta: &impl Fn(EdgeId) -> Option<EdgeMeta>,
) -> Result<(), LookupError> {
    let out = &mut plan.steps;
    let mut n = 0usize;
    // Derived in the append arm; the collapse arm only rewrites a `Static`, so
    // it cannot add or remove a `Dyn`.
    let mut dyn_count = 0u8;
    let mut first_dyn = EdgeId(0);
    // Tracked rather than read back, because `n` may be past the array.
    let mut last_static = false;
    // `None` until the first dynamic step fixes the domain; every later one
    // must agree, or one edge would be sampled with the wrong clock (D9).
    let mut domain: Option<u8> = None;

    let path = t_edges
        .iter()
        .map(|&e| (e, true))
        .chain(s_edges.iter().rev().map(|&e| (e, false)));

    for (edge, inverted) in path {
        let edge = EdgeId(edge);
        // Resolve the edge to either a constant or a (still dynamic) sample.
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

        // Collapse into the previous step if both are Static, otherwise append.
        // Past `MAX_DEPTH` the value has nowhere to live but counting continues.
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
