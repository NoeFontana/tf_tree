//! Compiled lookup plans, typed time, and the evaluation `Guard`.
//!
//! A [`Plan`] is the compiled result of resolving a `lookup(target, source)`
//! path through the topology *once*: a fixed array of [`Step`]s plus the topology
//! generation it was compiled against. Evaluating it many times against a
//! [`Guard`] separates the (rare) topology walk from the (hot) temporal sampling
//! — the single largest structural win over tf2's per-lookup topology walk
//! (`docs/PHASE1.md` §7; `docs/PROJECT.md` §5 D3).
//!
//! `unsafe`-free: it drives the arena through the safe [`crate::arena_view`] and
//! [`crate::sample`] surfaces. This module is `#[cfg(not(loom))]` because [`Guard`]
//! and [`compile`] need [`ArenaView`]/[`TopologyView`], which are themselves
//! production-only; the loom suite does not exercise the plan layer.
//!
//! # Compilation direction (verified by hand)
//!
//! Edge `edge_of_child[c]` stores `T_parent(c)_c`. For `lookup(target, source)`
//! the result is `T_target_source = (T_lca_target)⁻¹ · T_lca_source`. Walking up
//! from `target` emits inverted steps in walk order; walking up from `source`
//! emits forward steps in *reversed* walk order. Worked example
//! `map → odom → base`: `lookup(base, map)` emits
//! `[Dyn(edge_base, inv), Dyn(edge_odom, inv)]`, which folds to
//! `T_base_odom · T_odom_map = T_base_map = (T_map_base)⁻¹`. Correct.

use core::marker::PhantomData;

use tf_tree_math::{log_so3, Interp, Iso3, LerpSlerp, ScLerp, Twist};

use crate::arena_view::ArenaView;
use crate::edge::EdgeKind;
use crate::error::{EdgeId, FrameId, LookupError};
use crate::layout::{write_affine32, write_mat4, write_quat, write_quat_twist, Layout};
use crate::sample::ExtrapPolicy;
use crate::sync::spin;
use crate::topology::TopologyView;
use crate::{MAX_DEPTH, MAX_PATH_EDGES};

/// Maximum number of knots [`Plan::at_adaptive`] may emit.
pub const MAX_KNOTS: usize = 4096;

/// Maximum bisection recursion depth in [`Plan::at_adaptive`].
pub const MAX_ADAPTIVE_DEPTH: u32 = 16;

// ---------------------------------------------------------------------------
// Time: typed domains and stamps
// ---------------------------------------------------------------------------

/// A time domain: a compile-time marker carrying a runtime [`Domain::TAG`] byte.
///
/// Domains keep clocks that must not be silently mixed (the system clock, a
/// sensor's own clock) separate at the type level. A [`Stamp`] is parameterised
/// by its domain, so a cross-domain lookup is a type error at best and a
/// [`LookupError::TimeDomainMismatch`] at worst — never a silent misread. The
/// alignment machinery that *relates* domains is Phase 6; the separation must
/// exist now so adding it is not a breaking change (`docs/PROJECT.md` §5 D9;
/// `docs/PHASE1.md` §8 *Time*).
pub trait Domain: Copy {
    /// The runtime tag stored on an edge's `domain` field and compared against a
    /// query's domain. Must be unique per domain.
    ///
    /// **Tags `0`–`3` are the built-ins** ([`SystemDomain`], [`SensorDomain`],
    /// [`SimDomain`], [`SteadyDomain`]); a user-declared domain picks a free tag
    /// from `4` upwards. The trait is open on purpose — a driver with a
    /// PTP-disciplined clock declares `struct PtpDomain;` rather than pretending
    /// to be one of these (`docs/API.md` §2.5).
    ///
    /// **A tag is a permanent choice.** It is written into
    /// `EdgeRecord::domain` at declaration time and read by every consumer and
    /// every diagnostic; re-numbering one silently re-interprets every arena and
    /// every recording already on disk. `docs/API.md` §5.2 is the argument that
    /// a domain mistake is unfixable after the fact, and it applies to the
    /// numbering as much as to the choice.
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
/// # Why this exists, given that [`Domain`] is an open trait
///
/// It is open so a driver with a PTP-disciplined clock can declare its own tag,
/// and that is deliberate. But **two built-ins is close enough to a closed set
/// that everything collapses onto tag 0**: a sim deployment and a steady-clock
/// driver are both "not a sensor", so both take [`SystemDomain`] by default, and
/// [`LookupError::TimeDomainMismatch`] then never fires for the two populations
/// most exposed to the bug it exists to catch (`docs/API.md` §2.5).
///
/// The concrete failure this separates out: a node mixing `/clock`-driven sim
/// time with a driver's steady time gets a tree wrong by however long the bag
/// has been playing, and it is *well-formed* the whole time — the offset between
/// two clock domains is not recoverable from one-way stamps
/// (`docs/API.md` §5.2), so nothing downstream can notice and nothing after the
/// fact can repair it. That asymmetry is the whole reason the domain is a type
/// and not a convention (`docs/PROJECT.md` §5 D9).
///
/// **Sim time is not a wall clock**, which is a second, separable fact: it
/// steps, loops and stops, but it does so because an operator asked it to.
/// `docs/PHASE5.md` §6's `TFT019` reports a run of `NonMonotonicStamp`
/// rejections on a *wall-clock* tag as a clock step rather than a publisher
/// fault; with only tag 0 available it must fire on sim edges too, or skip
/// them by guessing. A tag of its own is what makes that check precise.
///
/// **Named `SimDomain`, not `SimTime`.** It is a domain, exactly as its three
/// siblings are, and the `-Time` spelling `docs/PHASE4.md` §5.5 and
/// `docs/PHASE7.md` §4 J9 first used was paired there with a `SystemTime` that
/// has never existed under that name — so it was never this code's convention.
/// Those documents now name this type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SimDomain;
impl Domain for SimDomain {
    const TAG: u8 = 2;
}

/// A steady, monotone clock (`CLOCK_MONOTONIC`-like), tag `3`.
///
/// The companion to [`SimDomain`], and the one `docs/API.md` §5.3 asks for by
/// name: *"a documentation line recommending a steady or PTP domain for
/// anything published at rate"* had nothing to recommend, because until now
/// there was no steady domain to name.
///
/// A steady clock cannot step, which is exactly the property
/// `docs/PHASE5.md` §6's `TFT019` needs and could not express. A run of
/// `NonMonotonicStamp` rejections on a [`SystemDomain`] edge is very likely an
/// NTP step or a leap second, and the publisher is not at fault; the same run on
/// a `SteadyDomain` edge cannot be, so it is a real publisher defect. Collapsing
/// both onto tag 0 turns the second case into the first and sends whoever meets
/// it at 3 a.m. to restart a node that was never broken.
///
/// It carries no epoch guarantee at all: two processes' `CLOCK_MONOTONIC` values
/// are unrelated across a reboot and, on some systems, across processes. That is
/// not a defect of this domain but the reason it is a *separate* one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SteadyDomain;
impl Domain for SteadyDomain {
    const TAG: u8 = 3;
}

/// Nanoseconds in one second.
///
/// Named rather than written twice: [`Stamp::from_parts`] and
/// [`Stamp::from_timespec`] both range-check against it and both multiply by it,
/// and four literals with the same nine zeros is how one of them acquires eight.
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

    /// Assemble a stamp from a `(seconds, nanoseconds)` pair — the shape
    /// `builtin_interfaces/Time` (`{int32 sec, uint32 nanosec}`) and POSIX
    /// `struct timespec` both have. Exact, and never a float
    /// (`docs/API.md` §5.1, which is normative).
    ///
    /// # Why this exists at all, when the unit was never the imposition
    ///
    /// The ecosystem already agrees with int64 nanoseconds — `rclcpp::Time`
    /// stores them, `rclpy.time.Time` is them, PTP is them. What a caller
    /// resents is writing `stamp.sec * 10**9 + stamp.nanosec` in every node, and
    /// what that hand-written line does is exactly what goes wrong here: it
    /// wraps silently at the ends of `i64` and it accepts a malformed pair
    /// without noticing. This is that line, written once, with both failures
    /// reported.
    ///
    /// # Total, and what it refuses
    ///
    /// Total: defined for every `(i64, u32)` pair, no panic, no wrap, no
    /// saturation. Two inputs have no correct answer and both return `None`:
    ///
    /// * **`nanos >= 1_000_000_000`.** Both source formats define the field as
    ///   the sub-second remainder, so a value outside `[0, 1e9)` means the pair
    ///   is not a `Time` — most often because a whole nanosecond count was put
    ///   in the wrong field. Normalizing it (carrying the excess into the
    ///   seconds) is *arithmetically* exact and is still the wrong answer:
    ///   it converts a malformed message into a plausible stamp, which is
    ///   precisely the class of silent wrongness `docs/API.md` R4 refuses in the
    ///   memory axis and §5.2 refuses in the time axis. Saturating would be
    ///   worse again.
    /// * **`sec * 1e9 + nanos` outside `i64`.** `i64` nanoseconds reach ±292
    ///   years, so this is unreachable for any real clock and reachable for
    ///   every uninitialised `i64`. Wrapping would hand back a stamp on the
    ///   other side of the epoch, which compares, interpolates and prints
    ///   perfectly. The bound is tested against the *sum*, not against the
    ///   product — see the body for the band of representable stamps a staged
    ///   `checked_mul`/`checked_add` refuses.
    ///
    /// `None` deliberately does not say *which*: a caller acts identically on
    /// both (reject the message), and a `Copy`, `String`-free error carrying the
    /// distinction would be a new error type for a fact no consumer branches on
    /// (`docs/PROJECT.md` §5 D11).
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
        // **`i128`, not `checked_mul` then `checked_add`.** The staged form
        // refuses a band of *representable* stamps at the negative end: for
        // `sec = -9_223_372_037` the product alone is below `i64::MIN` while
        // `product + nanos` is exactly `i64::MIN`, so a checked multiply
        // rejects a pair the type can hold. `i64 * 1e9 + u32` cannot overflow
        // `i128` for any input, so this branch is reached with the exact answer
        // in hand and the only question left is whether it fits.
        //
        // Not `wrapping_*` or a debug-only overflow trap either: a release build
        // must refuse exactly what a debug build refuses, or the check is a
        // test-configuration artifact rather than a property of the API.
        let total = sec as i128 * NANOS_PER_SEC as i128 + nanos as i128;
        if total < i64::MIN as i128 || total > i64::MAX as i128 {
            return None;
        }
        Some(Stamp(total as i64, PhantomData))
    }

    /// Assemble a stamp from the two fields of a POSIX `struct timespec`.
    ///
    /// # Why the fields and not the struct
    ///
    /// `tf_tree_core` is `no_std` and its whole dependency budget is
    /// `libm` + `bytemuck` + `blake3` (`docs/PROJECT.md` §5), so there is no
    /// `libc::timespec` to accept and adding `libc` to reach a two-field
    /// conversion would spend the budget on a struct definition. Declaring our
    /// own `#[repr(C)]` copy would be worse: it would be a type a caller has to
    /// convert *into*, which is the conversion this method exists to remove.
    ///
    /// `tv_sec` is `time_t` and `tv_nsec` is `long`; both are `i64` on every
    /// 64-bit target, so the call site is
    /// `Stamp::from_timespec(ts.tv_sec, ts.tv_nsec)` with no cast. On a 32-bit
    /// target they widen, which is lossless in both cases.
    ///
    /// # Total, and what it refuses
    ///
    /// Everything [`Self::from_parts`] refuses, plus a **negative `tv_nsec`**.
    /// POSIX allows one only in a *relative* `timespec` (an interval passed to
    /// `nanosleep`); an absolute time from `clock_gettime` always has
    /// `tv_nsec` in `[0, 1e9)`. A relative interval converted as if it were an
    /// absolute stamp is a whole category of wrong, and it is the one this
    /// refusal catches.
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
        // The range check above is what makes this cast lossless; it is not a
        // truncation the caller has to trust us about.
        Self::from_parts(tv_sec, tv_nsec as u32)
    }
}

// Manual auto-trait impls so `Stamp<D>` is `Copy`/`Ord` regardless of whether `D`
// itself is (it always is here, but this avoids leaking a bound onto callers).
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

/// A temporal query against a compiled [`Plan`].
///
/// Phase 1 implements [`Query::At`], [`Query::Latest`], and
/// [`Query::LatestCommon`] (`#[non_exhaustive]` so `Bracket` can arrive later
/// without a breaking change).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Query<D: Domain = SystemDomain> {
    /// Sample every dynamic edge at exactly this stamp.
    At(Stamp<D>),
    /// Sample every dynamic edge at *its own* newest stamp. The freshest possible
    /// value per edge; the timestamps used may differ between edges.
    Latest,
    /// Sample every dynamic edge at the largest stamp for which *all* of them have
    /// data — the `min` over the plan's dynamic edges of their newest stamp. This
    /// is what tf2's `Time(0)` means: "the most recent time common to the whole
    /// chain", not "now".
    LatestCommon,
}

// ---------------------------------------------------------------------------
// Interpolation selection
// ---------------------------------------------------------------------------

/// Selects an interpolation policy at runtime from an edge's stored discriminant.
///
/// The math crate models policies as zero-sized types implementing
/// [`tf_tree_math::Interp`]; this enum is the runtime selector stored in
/// [`crate::edge::EdgeRecord::interp`] and dispatched when a [`Guard`] samples an
/// edge.
///
/// # Deliberately *not* `#[non_exhaustive]`
///
/// Every other growable enum here carries it; this one does not, and the reason
/// is that exhaustive matching is the point. Each consumer that names this type
/// maps it onto something else — a `tft_interp` enumerator in the C ABI, a
/// `domain`-style name in `tf_tree_bridge`'s config parser, a monomorphized
/// [`tf_tree_math::Interp`] impl in the fold — and a catch-all arm in any of
/// those has no honest body. `#[non_exhaustive]` would convert a compile error
/// that says "teach me the new policy" into a silent wrong answer, which is a
/// worse trade than a major version bump.
///
/// Forward compatibility in the direction that actually matters — an older
/// binary reading a newer arena, where the two cannot be rebuilt together — is
/// already handled: [`InterpPolicy::from_u8`] collapses an unknown discriminant
/// onto the default rather than faulting. The same argument covers
/// [`crate::edge::EdgeKind`].
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

// ---------------------------------------------------------------------------
// Sampled pose plus derivatives
// ---------------------------------------------------------------------------

/// A pose, and how far past the plan's newest common sample it was extrapolated.
///
/// Returned by [`Plan::at_extrapolating`]. **There is no accessor that yields the
/// pose alone, and that is the design rather than an oversight**
/// ([`0039`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0039-extrapolation-you-cannot-fail-to-notice.md)):
/// the danger in extrapolation is a pose that looks fresh, so the distance is
/// handed over in the same value. Reading the pose without the distance takes a
/// deliberate `.pose`, not a forgotten check.
///
/// `Copy` and allocation-free, like every other value on this path
/// (`docs/API.md` §1 R2). It is not an error type: extrapolation under an
/// explicit policy is a requested outcome, and [`ExtrapPolicy::Error`] is how a
/// caller asks for it to be a failure instead.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Extrapolated {
    /// The pose.
    pub pose: Iso3,
    /// Nanoseconds past the newest stamp that *every* dynamic edge on this plan
    /// has data for.
    ///
    /// `0` means every edge bracketed the query — the answer is interpolated
    /// between published samples, not invented past them. A positive value is
    /// the worst case over the route, because the edge that runs out of data
    /// first is what bounds the composed answer.
    ///
    /// # It errs toward over-reporting, and that direction is deliberate
    ///
    /// On a live arena the distance is measured just *before* the fold, so a
    /// sample arriving mid-fold can make the answer better than the label: a
    /// query that was in fact bracketed may still report a positive `by_ns`. The
    /// reverse never happens, and that asymmetry is the point — `0` is a claim a
    /// controller acts on, so it is only ever made when the data was already
    /// there. Measuring after the fold made `0` reachable for an invented pose,
    /// which is what `at_extrapolating_tagged`'s comment on the ordering is
    /// about.
    pub by_ns: i64,
    /// The dynamic edge whose newest stamp is [`Self::by_ns`] behind the query.
    ///
    /// Meaningless when `by_ns == 0`. Named as data rather than formatted,
    /// exactly as the errors are (`docs/PROJECT.md` §5 D11): a caller that wants
    /// to log which sensor stopped publishing resolves it against the arena.
    pub edge: EdgeId,
}

/// A pose and its derivatives at one instant — `docs/PHASE4.md` §2.2.
///
/// Returned by [`Plan::at_with_derivatives`]. The twist is **body-frame
/// (right)**, expressed in the plan's **source** frame — see
/// [`Plan::at_with_derivatives`] for why that is the source and not the target,
/// and [`tf_tree_math::twist`] for the convention generally.
///
/// `#[non_exhaustive]` because this struct is *produced by the engine and only
/// read by a caller*, so growing it cannot make an existing consumer silently
/// wrong — it can only stop one from writing a literal nothing outside this
/// crate writes. [`Sample::accel`]'s own note already schedules the growth:
/// Phase 6's cumulative B-splines are the first interpolant with a real second
/// derivative, and a third derivative after them would otherwise be a major
/// bump for a field nobody had to read.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub struct Sample {
    /// The transform at the requested stamp — bit-identical to [`Plan::at`].
    pub pose: Iso3,
    /// First derivative, body frame, rad/s and m/s.
    pub twist: Twist,
    /// Second derivative, when the interpolant has one.
    ///
    /// Always `None` today: ScLerp's body twist is *constant* across a segment,
    /// so the acceleration is identically zero within a segment and undefined
    /// (a delta) at the knots. Reporting `Some(ZERO)` would claim a smoothness
    /// the piecewise-geodesic path does not have. Phase 6's cumulative B-splines
    /// are the first interpolant with a genuine second derivative, and this
    /// field exists now so that adding them is not a breaking change.
    pub accel: Option<Twist>,
}

// ---------------------------------------------------------------------------
// Steps and the compiled plan
// ---------------------------------------------------------------------------

/// One step of a compiled plan.
///
/// Not `#[non_exhaustive]`, for [`InterpPolicy`]'s reason rather than by
/// oversight. The only thing a consumer does with [`Plan::steps`] is classify
/// each step — "which edges does this plan sample?" is what
/// `tf_tree_bench::workload`, `tf_tree_py::offline` and the derivative tests all
/// ask — and a `_ =>` arm answers *no* for a step kind it has never seen. That
/// under-counts silently, where a compile error names the file to fix.
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
    /// How many of `steps[..len]` are [`Step::Dyn`], accumulated as the steps
    /// are appended. See [`fold_into`] for why this is stored rather than
    /// counted.
    dyn_count: u8,
    /// The edge of the *first* [`Step::Dyn`], or [`EdgeId`]`(0)` when there is
    /// none. Only meaningful together with `dyn_count`; read it through
    /// [`Plan::first_dynamic_edge`], never directly.
    first_dyn: EdgeId,
}

impl Plan {
    /// The identity plan for `generation`: zero steps, and **the buffer
    /// [`fold_into`] fills**.
    ///
    /// It is a complete, correct `Plan` as it stands — `steps[..0]` is empty and
    /// [`Plan::at`] composes nothing over it, which is exactly the answer
    /// [`compile`] owes for `target == source`. That is why this is the *only*
    /// constructor and why [`fold_into`] takes the whole `Plan` rather than its
    /// step array: there is no half-built state a second call has to complete,
    /// and so none a future arm can forget to complete. A plan that skipped its
    /// fold would otherwise answer `Iso3::IDENTITY` for every stamp — a wrong
    /// answer where a refusal belongs.
    ///
    /// # Why there is one array and it lives here
    ///
    /// This used to be `Plan::new(generation, steps, len, domain)` taking the
    /// step array **by value**, fed by a `fold` that returned one by value. That
    /// was two of the three array-sized copies `Tree::plan` paid per compile,
    /// neither of them proportional to the path (#264). Disassembled at
    /// `MAX_DEPTH = 32`, none of the three had been optimised away:
    ///
    /// | copy | site | bytes |
    /// |---|---|---|
    /// | 1 | `fold` returning its `out` array into the caller's `sret` buffer | 4096 |
    /// | 2 | `Plan::new` copying that parameter into `self.steps` | 4096 |
    /// | 3 | `compile`'s `Plan` into `Tree::plan`'s `sret` slot | 4160 |
    ///
    /// 12 352 bytes of `memcpy` to compile a plan that is usually six steps
    /// long. **Re-takeable in one command**, unlike the `LD_PRELOAD` session
    /// behind `tf_tree::cache`'s copy-count table:
    ///
    /// ```text
    /// cargo rustc --release -p tf_tree --lib -- --emit=asm -C codegen-units=1 -o /tmp/t.s
    /// ```
    ///
    /// then read the `movl $N, %edx` before each `callq *memcpy@GOTPCREL` in the
    /// `Tree::plan` and `fold_into` symbols (v0 mangling: grep `4Tree4plan` and
    /// `9fold_into`). `compile` has no symbol of its own — it inlines into
    /// `Tree::plan`.
    ///
    /// Copies 1 and 2 are the *same array crossing two by-value boundaries*, and
    /// both are gone once there is one array — this one, inside the `Plan` that
    /// will be returned — written once, in place. Re-disassembled after:
    /// `fold_into` calls no `memcpy` at all, and the only one left on the path is
    /// copy 3 — **2064 bytes today**, and 4160 when the table above was taken,
    /// because `0042` halved `Step`. The three figures in the table are the
    /// measurement as it stood; the surviving copy is the one that moved.
    ///
    /// **Copy 3 stays, and was not left alone for lack of trying.** It is
    /// `compile` returning by value into its caller's `sret` slot, not a
    /// temporary; the local `Plan` is address-taken across a real call to
    /// `fold_into`, and LLVM declines to place it in the `sret` memory. Marking
    /// `fold_into` `#[inline]` was measured and changes nothing — the same
    /// `memcpy` (4160 bytes when measured, 2064 since `0042`), the same
    /// instruction count in both functions. Removing it needs
    /// an out-parameter on [`compile`], which is `pub`: a `docs/API.md` §7
    /// change for the remaining third, and not one this took.
    ///
    /// Worth **−55.2%** on a 6-step `Tree::plan` — 265.0 ns → 118.7 ns, medians
    /// of 5 rounds of 20 000 reps, `taskset -c 2`, builds interleaved, ranges
    /// [261-267] against [114-122] and so non-overlapping. The refused paths
    /// barely move (−4.3% fold-bound, −11.1% shallow), because a refusal returns
    /// `Err` and never constructs a `Plan`, so it never paid copies 2 and 3 in
    /// the first place.
    ///
    /// **Attribution was isolated rather than assumed**, with a third build
    /// carrying only this change and not #259's: it moved a 6-step `Tree::plan`
    /// −54.8% and the refused-lookup metric −0.9%, while #259 alone moved the
    /// refused lookup −48.2% and this metric +4.3% — i.e. each change owns one
    /// number and contributes noise to the other's. A residue on
    /// `plan_refused_walk_ns` (−9.3%) has a mechanism in neither: that path
    /// refuses inside the walk, before either `Plan::identity` or `fold_into` is
    /// reached. It is recorded as unattributed code layout, not claimed.
    ///
    /// The identity array is not waste and is not removable: `Step` is an enum,
    /// so `steps[len..]` holding an invalid discriminant would be UB the moment
    /// the `Copy` or `Debug` derive touched it, and initialising it lazily needs
    /// `MaybeUninit` — `unsafe`, at no boundary the unsafe budget names
    /// (`docs/decisions/0007`). It is a vectorised store loop, not a `memcpy`,
    /// and the deleted `fold` was already paying it for `out`.
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
    ///
    /// Test-only, and it lives here because the fields are private to this
    /// module. The scanning half is the pre-optimisation implementation of
    /// [`Plan::has_dynamic`] and [`Plan::first_dynamic_edge`], kept verbatim so
    /// the test compares against the behaviour that was replaced rather than
    /// against a paraphrase of it.
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
        // Only now, on the cold side of a comparison that was already being
        // made. A detached guard must not report `TopologyChanged`, which would
        // send the reader looking for a re-plan that cannot help.
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
    /// `t`. Assumes the caller has already validated generation and domain.
    ///
    /// **`#[inline]` here is the load-bearing one** — see [`Self::at`] for the
    /// measurement. This is the only cross-crate call a downstream caller
    /// emitted before the attribute existed, because `at` is generic and was
    /// already being inlined without it.
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
    /// **A deliberate second copy of a fifteen-line loop, and the reason is
    /// constant folding.** [`Self::fold_at`] passes the `ExtrapPolicy::Error`
    /// *literal*, which lets LLVM prune the `Hold` and `ConstantTwist` arms out
    /// of the inlined `SampleRing::sample_from` on the path every existing
    /// caller takes. Giving `fold_at` a policy parameter would replace that
    /// literal with a variable and leave the match live on
    /// [`Self::at`]'s hot path — paying, in the default case, for a capability
    /// the default case does not use.
    ///
    /// So the specialisation is the point rather than an oversight, and
    /// `docs/PROJECT.md` §6's rule against a second spelling of an existing path
    /// does not apply: these are one path compiled twice, not two paths.
    /// [`0039`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0039-extrapolation-you-cannot-fail-to-notice.md)
    /// §4 is the commitment this keeps.
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
    /// **Deliberately not `#[inline]`, and that is a measurement.** It was
    /// marked alongside [`Self::fold_at`] for symmetry and the probe behind
    /// [`Self::at`]'s table never executed it — the measured path is
    /// `at → fold_at`, while this one is reached only from `at_many`,
    /// `at_many_into`, `at_many_into_f32` and [`Self::fold_batch`]. Extending
    /// that probe with an `#[inline(never)]` caller doing
    /// `at_many_into(.., Layout::Mat4, ..)` over 1024 monotone stamps at depth
    /// 3, best of five, x86-64, isolating this one attribute:
    ///
    /// | downstream profile | with `#[inline]` | without |
    /// | --- | --- | --- |
    /// | `lto = false`, `codegen-units = 16` | 328 ns/elem | **285 ns/elem** |
    /// | `lto = "thin"`, `codegen-units = 1` | 285 ns/elem | **278 ns/elem** |
    ///
    /// It is a pessimization in both, and `objdump` says why. At the default
    /// profile the body is ~1.9 kB and LLVM **declines to inline it at either
    /// `fold_batch` call site with or without the hint** — both builds leave a
    /// real call — so all the attribute does is codegen a second copy of it into
    /// the embedder's object instead of calling the one in this crate. Under
    /// thin LTO it does inline, and still loses. The scalar caller is untouched
    /// either way: [`Self::at`] cannot reach this function, and that probe's
    /// `caller_scalar` is byte-identical (`0x9ca`, same disassembly) across the
    /// two builds — so [`Self::at`]'s tables stand as measured.
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
    /// # Why `#[inline]` — and what was actually measured
    ///
    /// `docs/API.md` §2.3 makes the attribute normative on this method, on the
    /// fold, on `Guard::sample` and on the `Iso3` operators (the last already
    /// carried it). Its stated reason — "Rust does not inline across crates
    /// without `#[inline]` or LTO" — **is not why it helps here**, and the
    /// generated code says so:
    ///
    /// | downstream profile | before | after |
    /// | --- | --- | --- |
    /// | `lto = false`, `codegen-units = 16` (cargo's `--release` default) | 313 ns | 256 ns |
    /// | `lto = "thin"`, `codegen-units = 1` (this workspace's own) | 217 ns | 207 ns |
    ///
    /// Depth-3 interpolating lookup, external crate, 20 M iterations, best of
    /// five. The `objdump` of that caller explains the shape:
    ///
    /// | attribute placed on | caller `.text` | calls left in the caller |
    /// | --- | --- | --- |
    /// | nothing (the state before) | 106 B | 1 → `Plan::fold_at` |
    /// | `Plan::at` **alone** | 106 B — *byte-identical* | 1 → `Plan::fold_at` |
    /// | `fold_at` alone | 62 B | 1 → `Plan::at` |
    /// | `fold_at` + `Plan::at` | 1332 B | 1 → `Guard::sample_hinted` |
    /// | all five on this path, as shipped | 1565 B | 3 → `sampler`, 2× `SampleRing::sample_from` |
    ///
    /// **Five, not six.** `Self::fold_at_cursors` was marked in the same
    /// commit and is not on this path at all — no row above ever moved because
    /// of it. It was measured separately, on the batch entry point that does
    /// reach it, and removed: see its own doc comment for the numbers. Marking
    /// it had been symmetry, not measurement.
    ///
    /// **`at` is generic, so its MIR crossed the crate boundary anyway and a
    /// downstream caller was already inlining it.** On its own the attribute
    /// changes nothing. What it buys is LLVM's `inlinehint` on the *non-generic*
    /// links: `fold_at` first — the one real cross-crate call there ever
    /// was — then this method again, to stop the cost model halting at a
    /// now-larger `at`, then `Guard::sample*` to remove the last one. Marking
    /// fewer leaves a call in the middle; that is the claim the third and fourth
    /// rows above test rather than assert.
    ///
    /// **The price is the caller's code size: 106 B → 1565 B at every embedder
    /// call site**, ~15×, and that is the *scalar* caller only. It is why
    /// `fold_at_with_derivatives`, `fold_latest` and `fold_latest_common` are
    /// deliberately not marked — and why `Self::fold_at_cursors`, whose price
    /// on the batch path went unmeasured for a round, no longer is either.
    ///
    /// Note the second row of the first table: `lto = "thin"` does **not**
    /// subsume the hint. This workspace's own profile still moves ~4.5%, so the
    /// benchmark gate is not indifferent to this change — `just bench-ab` is
    /// workspace-wide and was not run. `docs/API.md` §2.3 item 3 (a gated
    /// cross-crate row) has since landed and does measure it continuously:
    /// `just embed-cost`, and the `embedding_cross_crate` row of
    /// `docs/PHASE5.md` §9.2's artifact.
    ///
    /// **That row reports a ratio over §9.2's 5% criterion, and it also reports
    /// the control that says what does close it.** Both columns are the same
    /// three lines behind `#[inline(never)]`, one compiled in `tf_tree_bench`
    /// and one here; three consecutive runs, `taskset`-pinned, paired rounds:
    ///
    /// | downstream profile | out-of-crate | in-crate | ratio |
    /// | --- | --- | --- | --- |
    /// | `lto = false`, `codegen-units = 16` | 240.0–240.1 ns | 191.3–191.8 ns | **1.250–1.254** |
    /// | `lto = "thin"`, `codegen-units = 1` | 193.0–195.0 ns | 194.2–196.2 ns | 0.994–0.996 |
    ///
    /// So the boundary costs about a quarter of a depth-3 lookup at cargo's
    /// `--release` defaults, and `lto = "thin"` in the embedder's own profile
    /// erases it.
    ///
    /// **An earlier revision of this paragraph added "which no `#[inline]`
    /// placement closes". That is removed, because the row's own toggle refutes
    /// it.** Dropping this attribute from `Plan::fold_at` and re-running the
    /// recipe takes the ratio from 1.253 to **1.001** — inside the gate — by
    /// making the in-crate column 6.7% slower (191.5 → 204.4 ns) and the
    /// `lto = "thin"` control 6.9% slower (193.2 → 206.6 ns), while the
    /// out-of-crate embedder column gets *faster* (239.9 → 203.9 ns). A
    /// placement moves it; what no placement measured here does is improve
    /// every column at once. Nothing is claimed about whether some other one
    /// would.
    ///
    /// **Do not read 203.9 ns against the 256 ns in the first table.** Those are
    /// different probes — the first is a throwaway 20 M-iteration loop, this one
    /// is `just embed-cost`'s sweep over 1024 off-grid stamps — and only ratios
    /// within one table are comparable. What both tables agree on is direction,
    /// and the second is the one that runs on every change.
    #[inline]
    pub fn at<D: Domain>(&self, g: &Guard, t: Stamp<D>) -> Result<Iso3, LookupError> {
        self.at_tagged(g, t.nanos(), D::TAG)
    }

    /// [`Self::at`], with the query's domain carried as a runtime tag.
    ///
    /// The domain arrives as a runtime tag instead of a type parameter.
    /// [`Domain`] is an **open trait** — a user declares their own tag from `4`
    /// upwards — so a foreign binding cannot enumerate the domains it may be
    /// asked about and cannot dispatch to the typed form. It carries the tag as
    /// data instead ([`0038`]).
    ///
    /// The check is [`Self::at`]'s, unchanged: same condition, same
    /// [`LookupError::TimeDomainMismatch`]. Only where the tag comes from
    /// differs. Rust callers should use [`Self::at`], where a domain
    /// mistake is a compile error.
    ///
    /// [`0038`]: https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0038-the-domain-a-binding-cannot-name.md
    ///
    /// # Errors
    ///
    /// As [`Self::at`].
    pub fn at_tagged(&self, g: &Guard, nanos: i64, domain: u8) -> Result<Iso3, LookupError> {
        self.check_generation(g)?;
        self.check_domain_tag(domain)?;
        // **The counter calls bracket the fold, not the whole function.**
        //
        // The two checks above fail on properties of the *query* — a plan
        // compiled against an old topology, or a domain mismatch — and neither
        // names an edge. Counting them would file a caller's mistake against a
        // publisher that is working correctly, which is worse than not counting
        // them at all (`docs/PHASE5.md` §5.2's attribution argument, applied in
        // the other direction).
        self.note(g, self.first_dynamic_edge(), self.fold_at(g, nanos))
    }

    /// Record one evaluation's outcome against the diagnostic counters.
    ///
    /// Every entry point that folds the plan goes through here, not just
    /// [`Self::at`]. `docs/PHASE5.md` §5.3 makes the error-path counters
    /// normative and always-on, and §5.2 makes them the whole basis of `TFT010`
    /// and `TFT011` — so a path that folds without counting is a path whose
    /// failures never reach `tf_tree top` or `doctor`. The batch entry points
    /// are exactly the ones that must not be missed: `at_many_into` is the
    /// Python zero-copy path and `at_with_derivatives` is the C ABI's, so
    /// leaving them silent meant the two flagship consumers contributed nothing
    /// to the operator's view of who is failing.
    ///
    /// `edge` is [`Self::first_dynamic_edge`]. It is a parameter rather than a
    /// call inside this function because it is an O(plan length) scan over the
    /// steps and it is loop-invariant: a batch caller resolves it once and
    /// passes the same value for every element, which is the difference between
    /// a per-element cost of a few nanoseconds and one proportional to depth.
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
    /// `EdgeId(0)` when the plan crosses several — the sentinel id no builder
    /// hands out, which [`Guard::note_ok`] then folds into "credit no edge".
    /// Attributing a multi-edge plan's success to one of its edges would put a
    /// number in `doctor`'s table that means something different from every
    /// other number in the same column.
    ///
    /// Reads the fields [`Plan::new`] derived; it no longer scans. The `== 1`
    /// test is what preserves the "several edges credit nobody" rule, which is
    /// why the count is stored and not just a `has_dynamic` flag.
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
    /// The composition identity (`docs/PHASE4.md` §2.3), for `T_ac = T_ab·T_bc`:
    ///
    /// ```text
    /// V_ac^c = Ad(T_bc⁻¹)·V_ab^b + V_bc^c
    /// ```
    ///
    /// so each step re-expresses the accumulated twist in the *new* body frame
    /// and adds the step's own. Two consequences worth naming, because both are
    /// easy to get subtly wrong:
    ///
    /// * **A static step still costs an adjoint.** Its own twist is zero, but the
    ///   frame changes, so the accumulator must still be mapped through
    ///   `Ad(m⁻¹)`. Skipping it leaves the twist expressed in an ancestor's
    ///   frame — a valid-looking vector, wrong by exactly that transform.
    /// * **An inverted step folds to one adjoint, not two.** For `S = p⁻¹`,
    ///   `V_S = −Ad(p)·V_p` and `Ad(S⁻¹) = Ad(p)`, so
    ///   `V' = Ad(p)·V_acc − Ad(p)·V_p = Ad(p)·(V_acc − V_p)`. Subtract first,
    ///   then rotate once.
    ///
    /// # Why the sampler is a parameter
    ///
    /// The batch form of this fold resumes each edge's bracket search from a
    /// cursor and the scalar form does not, and that is the *only* difference
    /// between them. Passing the sampler in keeps the composition above — the
    /// part that is easy to get subtly wrong and impossible to spot in a
    /// result — in one place, rather than in two copies that could drift into
    /// disagreeing about a velocity. `S` is a distinct type per call site, so
    /// neither form pays for the other's existence.
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
                    // Constant transform: no twist of its own, but the body
                    // frame moves, so the accumulator must follow it.
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
    /// window midpoint — the scalar `at_with_derivatives` path.
    #[inline]
    fn fold_at_with_derivatives(&self, g: &Guard, t: i64) -> Result<(Iso3, Twist), LookupError> {
        self.fold_with_derivatives(|_, edge| g.sample_with_twist(edge, t, ExtrapPolicy::Error))
    }

    /// [`Self::fold_with_derivatives`] resuming each step's bracket search from
    /// its own cursor — [`Self::fold_at_cursors`]'s counterpart, and the reason
    /// a monotone [`Layout::QuatTwist`] batch costs `O(1)` amortized per stamp
    /// instead of `O(log n)`.
    ///
    /// A cursor is a *hint*: the galloping search still hands the binary search
    /// an interval that brackets `t`, so a stale one costs probes and cannot
    /// change an answer. That is what lets this share every assertion the
    /// cursor-less form has.
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
    /// # Which frame the twist is in — read this once
    ///
    /// The twist is body-frame (right), `V^b = (T⁻¹Ṫ)^∨`, expressed in the
    /// plan's **source** frame. Use [`tf_tree_math::Twist::to_spatial`] with the
    /// returned pose to get it in the **target** frame.
    ///
    /// It is the source frame because `plan(target, source)` evaluates
    /// `T_target_source`, and `T⁻¹Ṫ` is by construction resolved in the frame `T`
    /// maps *from*. Concretely, for `plan(map, base)` where `base` is rotated
    /// +90° about z and moves along **map**'s +x at 1 m/s:
    ///
    /// ```text
    /// sample.twist.v              == (0, −1, 0)   // resolved in base axes
    /// sample.twist.to_spatial(&p) == (1,  0, 0)   // resolved in map axes
    /// ```
    ///
    /// Both are 1 m/s, so **`‖v‖` is identical and a magnitude check cannot tell
    /// them apart**. Getting this wrong is wrong by the full rotation
    /// `R_target_source`, silently. An earlier revision of this doc comment said
    /// "target", which is why the example is here rather than a sentence.
    ///
    /// Costs roughly two plain lookups: the same sampling work, plus one adjoint
    /// application per plan step (two quaternion rotations and a cross product,
    /// no transcendentals — see [`tf_tree_math::twist`]).
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

    /// [`Self::at_with_derivatives`], with the query's domain as a runtime tag.
    ///
    /// The domain arrives as a runtime tag instead of a type parameter.
    /// [`Domain`] is an **open trait** — a user declares their own tag from `4`
    /// upwards — so a foreign binding cannot enumerate the domains it may be
    /// asked about and cannot dispatch to the typed form. It carries the tag as
    /// data instead ([`0038`]).
    ///
    /// The check is [`Self::at_with_derivatives`]'s, unchanged: same condition, same
    /// [`LookupError::TimeDomainMismatch`]. Only where the tag comes from
    /// differs. Rust callers should use [`Self::at_with_derivatives`], where a domain
    /// mistake is a compile error.
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

    /// Sample every dynamic edge at *its own* newest stamp (freshest per edge).
    ///
    /// The timestamps used may differ between edges — this is the freshest value
    /// each edge can provide, not a temporally consistent snapshot. Use
    /// [`Self::latest_common`] when consistency matters.
    ///
    /// # Errors
    ///
    /// [`LookupError::TopologyChanged`], or [`LookupError::NoData`] if a dynamic
    /// edge is empty.
    pub fn latest(&self, g: &Guard) -> Result<Iso3, LookupError> {
        self.check_generation(g)?;
        self.note(g, self.first_dynamic_edge(), self.fold_latest(g))
    }

    /// [`Self::latest`]'s fold, split out so the counter bracket in
    /// [`Self::note`] wraps a single expression.
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
    /// `policy`, and reporting how far the answer was extrapolated.
    ///
    /// The capability existed in the sampler from the beginning and was
    /// reachable from no shipped surface until
    /// [`0039`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0039-extrapolation-you-cannot-fail-to-notice.md).
    /// It is per *query*, not per edge, because a `Hold` that is right for a
    /// 10 Hz map edge is wrong for the 1 kHz odometry edge on the same route —
    /// so the caller who bears the consequence chooses, not whoever published.
    ///
    /// A controller running faster than its state estimate is *always* asking
    /// for a stamp past the newest sample. [`ExtrapPolicy::ConstantTwist`] is
    /// the honest answer there and [`ExtrapPolicy::Hold`] the honest one for a
    /// latched or displayed value; both hand back
    /// [`Extrapolated::by_ns`] so neither can be mistaken for fresh data.
    ///
    /// [`ExtrapPolicy::Error`] here is [`Self::at`] with a distance attached on
    /// success. [`Self::at`] itself is unchanged and remains the default: it
    /// refuses, and refusing is right for a caller that must not act on invented
    /// data.
    ///
    /// # The distance is not free, and is not on [`Self::at`]'s path
    ///
    /// `by_ns` costs one `newest_stamp` load per dynamic edge, taken *after* the
    /// fold and only here. Nothing is threaded through `fold_at` or the seqlock
    /// read, so [`Self::at`]'s generated code is unmoved.
    ///
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
    /// tag — the binding surface, for
    /// [`0038`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0038-the-domain-a-binding-cannot-name.md)'s
    /// reason.
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
        // **Before the fold, and the order is the guarantee.** `newest_common`
        // is the same walk `latest_common` folds at, so the two agree by
        // construction rather than by two definitions of "common" staying in
        // step — but *when* it runs decides whether `by_ns == 0` is sound.
        //
        // It used to run after. A `push` landing between the fold and this walk,
        // with a stamp at or past `nanos`, lifts `common` to `>= nanos` — and
        // the `saturating_sub().max(0)` below then reports **`by_ns == 0`, "not
        // extrapolated", for a pose the fold genuinely invented**. That is the
        // one claim this type exists to make unmissable, and a 100 Hz edge under
        // a 1 kHz query crosses the stamp regularly enough that it is a race a
        // robot runs, not a thought experiment.
        //
        // Measuring first inverts the error into the safe direction, because
        // `SampleRing::newest_stamp` is non-decreasing (`head` only advances and
        // `push` refuses a stamp strictly older than the newest). So
        // `common_before <= common_during`, and:
        //
        // * `by_ns > 0` may over-report — a sample that arrived mid-fold made the
        //   answer better than the label. Harmless: the caller treats a real
        //   answer as extrapolated.
        // * `by_ns == 0` means every edge already held data past `nanos`
        //   *before* the fold began, so the fold certainly bracketed. Sound.
        //
        // Not `note`d, exactly as it was not before: the fold's `note` below is
        // this query's one counter event, and a second would double `lookups_ok`
        // — the denominator `doctor`'s TFT010 and TFT011 divide by.
        let common = self.newest_common(g);
        let pose = self.note(g, edge, self.fold_at_policy(g, nanos, policy))?;
        let (by_ns, which) = match common? {
            // **`saturating_sub`, not `-`.** This is the pattern
            // `sample::span_ns` was written to eliminate, and its doc comment is
            // the argument: a plain subtraction of two stamps panicked in a
            // checked build and *wrapped* in a release one, and the wrap is the
            // worse half. Here it would be worse still. Under `Hold` or
            // `ConstantTwist` a query is accepted whenever `t >= t_old`, so a
            // route publishing near `i64::MIN` queried near `i64::MAX` reaches
            // this line with the difference outside `i64`. Wrapped negative, the
            // `.max(0)` below then reports `by_ns == 0` — *"not extrapolated"*
            // for the most extrapolated answer this type can hold, which is
            // exactly the confusion `Extrapolated` exists to make impossible.
            // Saturating says "further than representable", which is true.
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

    /// Sample every dynamic edge at the newest stamp common to all of them (the
    /// `min` of their newest stamps) — tf2's `Time(0)` semantics.
    ///
    /// # Errors
    ///
    /// [`LookupError::TopologyChanged`], [`LookupError::NoData`] if an edge is
    /// empty, or [`LookupError::Extrapolation`] if an edge's retained window does
    /// not reach the common stamp.
    pub fn latest_common(&self, g: &Guard) -> Result<Iso3, LookupError> {
        self.check_generation(g)?;
        self.note(g, self.first_dynamic_edge(), self.fold_latest_common(g))
    }

    /// [`Self::latest_common`]'s fold, split out for the same reason as
    /// [`Self::fold_latest`].
    fn fold_latest_common(&self, g: &Guard) -> Result<Iso3, LookupError> {
        let Some((common, _)) = self.newest_common(g)? else {
            return Ok(self.static_only());
        };
        self.fold_at(g, common)
    }

    /// The newest stamp every dynamic edge on this plan has data for, and the
    /// edge that produced it — `None` when the plan is static-only.
    ///
    /// The minimum over the plan's dynamic edges, which is what
    /// [`Self::latest_common`] folds at and what
    /// [`Self::at_extrapolating`] measures its distance from: the edge that runs
    /// out of data first is the one that bounds how invented a composed answer
    /// is. Factored out so the two callers share one walk and one definition of
    /// "common" ([`0039`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0039-extrapolation-you-cannot-fail-to-notice.md)).
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
    /// **It is not "the interval over which this plan is answerable", which is
    /// what this sentence used to say.** The value is an *intersection of outer
    /// windows* and carries no information about holes inside them: a publisher
    /// that died for 30 s in the middle of a 100 Hz edge moves neither end, so a
    /// span can be almost entirely gap and still look healthy. Measured on this
    /// repository's own recording, one edge's widest bracket is 5.3 s inside a
    /// 42 s span — 105× its own median — carrying 2.57 m of unobserved motion.
    ///
    /// Outside the returned interval [`Self::at`] refuses; **inside it, `at`
    /// answering is not evidence that anything was observed near the stamp** —
    /// it interpolates across whatever bracket it finds. `tf_tree doctor`'s
    /// `TFT009` is what detects that today, after the fact and from the same
    /// bytes.
    ///
    /// [`Self::latest_common`] generalised from a point to a range: the
    /// **intersection** of every dynamic step's retained window, so the lower end
    /// is a `max` and the upper end a `min`. The upper end is `latest_common`'s
    /// own stamp — both are `min` over [`SampleRing::newest_stamp`](crate::buffer::SampleRing::newest_stamp).
    ///
    /// The two do *not* share a helper, on purpose: `latest_common` is a lookup
    /// path and folding it onto `Guard::window` would cost it a second atomic
    /// load and a second mask per edge to compute a lower end it never uses.
    /// `spans_agree_with_latest_common` in `tests.rs` pins the agreement instead.
    ///
    /// It lives here, next to that method and to
    /// [`SampleRing::retained`](crate::buffer::SampleRing::retained), because the
    /// definition of a ring's readable window **has already changed once** — a
    /// copy of this arithmetic in a binding crate would not have moved with it,
    /// and would be provable only through that binding's own test runner.
    ///
    /// Three answers, kept distinct on purpose:
    ///
    /// * `Some((t0, t1))` with `t0 <= t1` — the plan answers there, and nowhere
    ///   else without extrapolating.
    /// * `Some((t0, t1))` with `t0 > t1` — **an empty intersection is a real
    ///   answer**, not an error: two edges on the path have disjoint histories
    ///   and the caller's `t0 <= t <= t1` is correctly false everywhere.
    ///   Collapsing it to `None` would make it indistinguishable from the
    ///   unbounded case below.
    /// * `None` — every step folded to a static transform (or the plan is the
    ///   empty `lookup(x, x)`), so it is answerable at *any* stamp and there is
    ///   no finite interval to report.
    ///
    /// # Staleness
    ///
    /// On a live arena the answer ages the moment it is returned, and the two
    /// ends are not even one snapshot of one ring — see `Guard::window`. That
    /// is the same contract [`Self::latest`] has, and refusing to answer would be
    /// worse than answering the question that was asked. On a frozen `.tft`, the
    /// case §4.2 is about, nothing pushes and the interval is exact.
    ///
    /// # Errors
    ///
    /// [`LookupError::TopologyChanged`] (or [`LookupError::ChildDetached`] on a
    /// fork-poisoned guard), [`LookupError::UnknownEdge`], or
    /// [`LookupError::NoData`] naming the first edge on the path that has never
    /// published — which is a different fact from an empty intersection and is
    /// reported differently.
    pub fn span(&self, g: &Guard) -> Result<Option<(i64, i64)>, LookupError> {
        self.check_generation(g)?;
        let mut span: Option<(i64, i64)> = None;
        for step in self.steps() {
            let Step::Dyn { edge, .. } = step else {
                // A static step carries its transform in the plan and constrains
                // nothing in time. `inverted` does not matter either: inverting a
                // pose does not move the stamp it was published at.
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
    /// edges, in milli-hertz, or `None` when no edge on the plan declares one
    /// (`docs/decisions/0018`).
    ///
    /// Together with [`Self::span`] this is the whole engine-side input to a
    /// caller's blocking wait. **There is no blocking primitive in the arena and
    /// there is not going to be one**: every shared-memory wait requires the
    /// waiter to register by *writing* a word the waker can see, and D18 makes
    /// consumers attach `PROT_READ`, so that store is a `SIGSEGV` rather than an
    /// error. Buying back ~1 ms of startup latency by spending an MMU-enforced
    /// safety boundary is the wrong trade; `0018` records it, and records the
    /// escalation path (the Phase 2 owner server, not a futex) so it is not
    /// reinvented.
    ///
    /// The wait the shim writes, once, out of these two:
    ///
    /// ```text
    /// loop {
    ///     let g = tree.guard();
    ///     match plan.span(&g)? {
    ///         None                                  => return plan.at(&g, wanted),
    ///         Some((_, newest)) if newest >= wanted  => return plan.at(&g, wanted),
    ///         Some((_, newest)) => sleep(min(deadline_remaining,
    ///                                        (wanted - newest) + one_period)),
    ///     }
    ///     if now >= deadline { return Err(Timeout) }
    /// }
    /// ```
    ///
    /// where `one_period` is `1e9 / (mhz / 1000)` nanoseconds from *this*
    /// method. It is a **prediction, not a poll interval**: the shortfall is
    /// known and the period is declared, so the typical wake count is one or
    /// two. A naive 1 ms poll against a 10 Hz edge wakes a hundred times.
    ///
    /// # Slowest, and why the answer is a `min`
    ///
    /// A plan is answerable only when *every* dynamic edge on it has reached the
    /// stamp, so the edge that decides when the wait ends is the one that
    /// publishes least often. Sleeping a period of the fastest edge on a path
    /// that also carries a 10 Hz map update wakes a hundred times per useful
    /// answer — the poll this design exists to avoid, arrived at from inside the
    /// prediction.
    ///
    /// # `0` is *undeclared*, and is skipped rather than treated as 0 Hz
    ///
    /// `EdgeRecord::nominal_rate_mhz` uses `0` as "not declared" — an edge whose
    /// ring was sized by an explicit slot count states no rate. Reading the
    /// sentinel as a rate makes it the minimum of every set it appears in and
    /// yields an infinite period, so one undeclared edge would silently disable
    /// the wait for the whole path. `docs/PHASE5.md` §6's `TFT007` amendment
    /// makes the same distinction load-bearing for the rate check, and for the
    /// same reason: comparing against zero fabricates a finding on every edge of
    /// a correct arena.
    ///
    /// `None` — nobody declared — is therefore a real third answer and not a
    /// degenerate minimum. A caller falls back to a conservative period and
    /// **should say so once at startup** rather than silently, because a
    /// mysteriously slow wait and a mysteriously busy one look identical from
    /// outside (`0018` *Consequences*).
    ///
    /// # A declared rate may be an observed one
    ///
    /// `tf_tree topology --discover` measures a rate and writes it into the same
    /// `rate_hz` this reads as a declaration, so a recording of a degraded
    /// publisher declares the fault as nominal. Here the consequence is one
    /// extra wake, which is why the ambiguity is tolerable in a wait and was not
    /// tolerable for `TFT007` (`docs/PHASE5.md` §6).
    ///
    /// # Why it is generation-checked, and why it is not `NoData`
    ///
    /// It calls `check_generation` exactly as [`Self::span`] does, so a stale
    /// plan reports [`LookupError::TopologyChanged`] and a fork-poisoned guard
    /// reports [`LookupError::ChildDetached`]. A waiter that missed either would
    /// spin until its deadline against a plan that can never be satisfied — the
    /// one failure mode a *timeout* API hides best.
    ///
    /// It does **not** return [`LookupError::NoData`] for an edge that has never
    /// published, which is where it deliberately parts company with
    /// [`Self::span`]. A declaration is a property of the topology, not of the
    /// stream: the caller asking how long to sleep is by definition asking
    /// *before* the data exists, and that is the startup case this whole loop
    /// was built for.
    ///
    /// # Errors
    ///
    /// [`LookupError::TopologyChanged`], [`LookupError::ChildDetached`], or
    /// [`LookupError::UnknownEdge`] if a step names an edge this arena has no
    /// record for.
    //
    // The signature `0018` prints in its *Decision* section is
    // `-> Option<u32>`, which cannot carry the errors the same record's
    // implementation plan (step 1) requires it to raise — "calling
    // `check_generation` as `span` does" is only meaningful if the result
    // reaches the caller. The `Result` wrapper is the self-consistent reading
    // and matches `span`, the method it is documented beside and used with.
    pub fn slowest_nominal_rate_mhz(&self, g: &Guard) -> Result<Option<u32>, LookupError> {
        self.check_generation(g)?;
        let mut slowest: Option<u32> = None;
        for step in self.steps() {
            let Step::Dyn { edge, .. } = step else {
                // A static edge has no publisher and therefore no period; it
                // constrains a wait exactly as much as it constrains `span`.
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
    ///
    /// **This was an `assert!` and a `# Panics` section until 2026-08-29**, and
    /// the `# Errors` section above it called the check "debug-time" — which it
    /// never was: `assert!` is unconditional, so a short buffer unwound in
    /// release, or aborted outright under the `panic = "abort"` profile an
    /// embedder picks for a control loop. Every sibling on this path already
    /// returned the `Copy` identifier `docs/API.md` R5 requires
    /// ([`Self::at_many_into`], [`Self::at_many_into_f32`]); this was the one
    /// batch entry point that did not, and `clippy::panic` — which the workspace
    /// denies precisely to keep this out of the engine — does not lint `assert!`.
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

        // Hoisted: loop-invariant, and an O(plan length) scan (see [`Self::note`]).
        let edge = self.first_dynamic_edge();
        let monotone = stamps.windows(2).all(|w| w[0].nanos() <= w[1].nanos());
        if monotone {
            let mut cursors = [0u64; MAX_DEPTH];
            for (s, o) in stamps.iter().zip(out.iter_mut()) {
                *o = self.note(g, edge, self.fold_at_cursors(g, s.nanos(), &mut cursors))?;
            }
        } else {
            for (s, o) in stamps.iter().zip(out.iter_mut()) {
                *o = self.note(g, edge, self.fold_at(g, s.nanos()))?;
            }
        }
        Ok(())
    }

    /// Evaluate a batch **directly into a caller's buffer**, in `layout`.
    ///
    /// The point of this over [`Self::at_many`] is that an `Iso3` buffer does
    /// not alias the layout a consumer wants — a 4x4 `f64` matrix or a 3x4 `f32`
    /// affine — so writing through one costs an intermediate buffer and a second
    /// pass; this writes once, in place, and allocates nothing. Since `0042` the
    /// `Quat` layout is the exception and shares `Iso3`'s bytes exactly; see
    /// [`crate::layout`], which carries what that does and does not change.
    ///
    /// `out` is a flat `f64` slice of at least `stamps.len() * layout.elems()`.
    /// Use [`Self::at_many_into_f32`] for [`Layout::Affine32`].
    ///
    /// [`Layout::QuatTwist`] is the batch form of [`Self::at_with_derivatives`]
    /// and folds through exactly that path, so the thirteen `f64` it writes per
    /// stamp are the same bits the scalar call would produce — including its
    /// refusals. It is the only layout here that can fail for a reason the pose
    /// layouts cannot.
    ///
    /// **`stamps` is raw nanoseconds, with the domain as the type parameter.**
    /// `Stamp<D>` is a newtype and not `repr(transparent)`, so a caller holding
    /// `&[i64]` — every FFI caller, and the NumPy path in particular — would
    /// have to *allocate and copy* to produce `&[Stamp<D>]`, which is exactly
    /// the intermediate buffer this method exists to remove. Nothing is lost:
    /// the domain is still checked, once per call rather than carried in eight
    /// bytes of `PhantomData` per element.
    ///
    /// # Errors
    ///
    /// [`LookupError::BufferTooSmall`] if `out` cannot hold the batch, or
    /// [`LookupError::WrongElementType`] for an `f32` layout. Both are checked
    /// **before a single element is written**, so a rejected call leaves the
    /// caller's buffer untouched — `docs/PHASE3.md` §5.3 requires that, because
    /// a half-written output is worse than none: it looks like data.
    ///
    /// For [`Layout::QuatTwist`], additionally
    /// [`LookupError::DerivativesUnavailable`] and [`LookupError::NoSegment`],
    /// exactly as [`Self::at_with_derivatives`] returns them — a `LerpSlerp`
    /// edge is refused rather than finite-differenced.
    ///
    /// **Only the two checks above are all-or-nothing.** Every other error is a
    /// property of a *stamp*, so it can fire after `k` rows are already
    /// written and the batch stops there, leaving `k` live rows and the rest as
    /// the caller left them, with nothing in the buffer marking the boundary —
    /// which is why the element index is worth recovering from the error
    /// (`NoData`, `Extrapolation` and `SlotRecycled` name the edge and the
    /// window). The distinction is not academic:
    /// `DerivativesUnavailable` is a property of an *edge*, so it always fires
    /// at element 0 and the buffer really is untouched, while
    /// [`LookupError::NoSegment`] on the same layout depends on which segment
    /// the stamp brackets and is not.
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

    /// [`Self::at_many_into`], with the query's domain as a runtime tag.
    ///
    /// The domain arrives as a runtime tag instead of a type parameter.
    /// [`Domain`] is an **open trait** — a user declares their own tag from `4`
    /// upwards — so a foreign binding cannot enumerate the domains it may be
    /// asked about and cannot dispatch to the typed form. It carries the tag as
    /// data instead ([`0038`]).
    ///
    /// The check is [`Self::at_many_into`]'s, unchanged: same condition, same
    /// [`LookupError::TimeDomainMismatch`]. Only where the tag comes from
    /// differs. Rust callers should use [`Self::at_many_into`], where a domain
    /// mistake is a compile error.
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
        // The layout is matched once, here. Putting it inside the loop would
        // add an unpredictable branch between every element and the next, in
        // the one API whose whole purpose is a per-element cost of nanoseconds.
        match layout {
            Layout::Mat4 => self.fold_batch(g, stamps, write_mat4, n, out),
            Layout::Quat => self.fold_batch(g, stamps, write_quat, n, out),
            // The one arm that does not go through `fold_batch`: it needs the
            // twist, so it folds through `fold_at_with_derivatives` instead.
            // See [`Self::fold_batch_with_twist`] for why that is a sibling and
            // not a generic parameter on the existing loop.
            Layout::QuatTwist => self.fold_batch_with_twist(g, stamps, n, out),
            // Unreachable: rejected by the `is_f32` check above. Returning the
            // same error rather than panicking keeps this crate free of a
            // panic path the workspace lints forbid, and a future f32 layout
            // added without updating the check gets a clear error rather than
            // a silently wrong `f64` write.
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

    /// [`Self::at_many_into_f32`], with the query's domain as a runtime tag.
    ///
    /// The domain arrives as a runtime tag instead of a type parameter.
    /// [`Domain`] is an **open trait** — a user declares their own tag from `4`
    /// upwards — so a foreign binding cannot enumerate the domains it may be
    /// asked about and cannot dispatch to the typed form. It carries the tag as
    /// data instead ([`0038`]).
    ///
    /// The check is [`Self::at_many_into_f32`]'s, unchanged: same condition, same
    /// [`LookupError::TimeDomainMismatch`]. Only where the tag comes from
    /// differs. Rust callers should use [`Self::at_many_into_f32`], where a domain
    /// mistake is a compile error.
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
        self.fold_batch(g, stamps, write_affine32, n, out)
    }

    /// The shared batch loop: monotone stamps ride resumable cursors, and each
    /// result is emitted straight into its slot.
    ///
    /// Generic over the element type so the `f64` and `f32` paths share one
    /// copy of the cursor logic — which is the part that must not be duplicated,
    /// because it is where the galloping search and the seqlock retry live.
    #[inline]
    fn fold_batch<T, W>(
        &self,
        g: &Guard,
        stamps: &[i64],
        write: W,
        elems: usize,
        out: &mut [T],
    ) -> Result<(), LookupError>
    where
        W: Fn(&Iso3, &mut [T]),
    {
        // `chunks_exact_mut` rather than `out[i * elems..(i + 1) * elems]`,
        // and zipped against `stamps` so the walk is bounded by the batch and a
        // caller's over-long buffer is left untouched past the end.
        //
        // **Not for speed.** The obvious argument — that the indexed form is a
        // bounds check per element, hence a branch between every sample and the
        // next — was measured and is wrong: at 1024 samples the change is
        // within noise (`No change in performance detected`), because LLVM
        // already elides the check and because ~245 us of interpolation dwarfs
        // it either way. It stays because it says what it means and drops the
        // manual index arithmetic, not because it is faster.
        // Hoisted: loop-invariant, and an O(plan length) scan (see [`Self::note`]).
        let edge = self.first_dynamic_edge();
        let monotone = stamps.windows(2).all(|w| w[0] <= w[1]);
        if monotone {
            let mut cursors = [0u64; MAX_DEPTH];
            for (s, dst) in stamps.iter().zip(out.chunks_exact_mut(elems)) {
                let iso = self.note(g, edge, self.fold_at_cursors(g, *s, &mut cursors))?;
                write(&iso, dst);
            }
        } else {
            for (s, dst) in stamps.iter().zip(out.chunks_exact_mut(elems)) {
                let iso = self.note(g, edge, self.fold_at(g, *s))?;
                write(&iso, dst);
            }
        }
        Ok(())
    }

    /// [`Layout::QuatTwist`]'s batch loop — [`Self::fold_batch`]'s sibling.
    ///
    /// # Why a sibling rather than a parameter on `fold_batch`
    ///
    /// `fold_batch` is generic over the *emitter*, not over the fold. This one
    /// needs a different fold — [`Self::fold_at_with_derivatives`], which
    /// returns `(Iso3, Twist)` — so making the existing loop serve both would
    /// mean either a second closure the pose layouts pass as a no-op or a
    /// branch on the layout inside the loop. Both put work into the scalar
    /// batch path, which is the one path in this file whose per-element cost is
    /// measured in nanoseconds. Duplicating twelve lines is the cheaper trade,
    /// and the module doc for [`crate::layout`] already fixes it as the rule.
    ///
    /// # The monotone cursor, which this pays for exactly as `fold_batch` does
    ///
    /// Ascending stamps ride a resumable cursor per plan step, through
    /// [`Self::fold_at_with_derivatives_cursors`], so the bracket search is
    /// `O(1)` amortized rather than `O(log n)` per stamp per step. This layout
    /// is the `n = 1024` ML/perception batch `docs/API.md` §3.3 exists for, and
    /// leaving it as the one layout without a cursor made the flagship path the
    /// slowest one.
    ///
    /// The cursor is only a *hint*: the galloping search still hands the binary
    /// search an interval bracketing `t`, so the two branches below cannot
    /// disagree about a number — which is why the monotone and non-monotone
    /// paths are asserted bit-identical rather than merely close.
    ///
    /// # Why it calls the same fold `at_with_derivatives` does
    ///
    /// Bit-identity with the scalar call is the property that makes this a
    /// *layout* and not a second implementation of derivatives. Anything else
    /// here — a finite difference, a re-derived adjoint chain — would be a
    /// second answer to the same question, and the first symptom would be two
    /// bindings disagreeing about a velocity.
    #[inline]
    fn fold_batch_with_twist(
        &self,
        g: &Guard,
        stamps: &[i64],
        elems: usize,
        out: &mut [f64],
    ) -> Result<(), LookupError> {
        // Hoisted for the same reason as in `fold_batch`: loop-invariant, and
        // an O(plan length) scan (see [`Self::note`]).
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
    /// Recursive bisection: the midpoint of each segment is evaluated exactly and
    /// compared against the LERP of the segment endpoints; the segment is
    /// subdivided only if the error exceeds `tol`. Recursion depth is bounded at
    /// [`MAX_ADAPTIVE_DEPTH`] and the knot count at [`MAX_KNOTS`]. All output lives
    /// in the caller-provided `scratch`; there is no global allocation.
    ///
    /// Returns parallel slices `(stamps, poses)` of the emitted knots (strictly
    /// increasing in stamp). The consumer LERPs between them on whatever device the
    /// points live on, with the reconstruction error bounded by construction. This
    /// is the API that replaces the abandoned deskew helper.
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

    /// [`Self::at_adaptive`], with the query's domain carried as a runtime tag.
    ///
    /// The tagged sibling of the adaptive shape, for the same reason as
    /// [`Self::at_tagged`]: [`Domain`] is an open trait, so a foreign binding
    /// cannot name the type it would have to instantiate
    /// (`docs/decisions/0038-the-domain-a-binding-cannot-name.md`).
    ///
    /// # `D` here is storage, and `domain` is the query
    ///
    /// This is the one shape where the two cannot be collapsed. `D` fixes the
    /// element type of the caller's `scratch` and of the returned stamp slice —
    /// it is a phantom over the buffer and is read by nothing in the fold, which
    /// never consults `D::TAG`. The *query's* domain is `domain`, and `domain`
    /// is what is checked. A binding therefore passes any `D` it can name
    /// (`SystemDomain`) and the real tag as data, then converts the returned
    /// stamps to integers immediately.
    ///
    /// **A Rust caller wants [`Self::at_adaptive`]**, where the two are the same
    /// value by construction and a domain mistake is a compile error. Passing a
    /// `D` here whose tag disagrees with `domain` is legal, does not affect the
    /// result, and produces a stamp slice whose phantom means nothing — which is
    /// exactly why the typed form exists and is the default.
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
        // Counted **once per call, not once per fold.** `subdivide` evaluates
        // the plan up to `MAX_KNOTS` times for a single caller-visible lookup,
        // and crediting each bisection separately would make this one entry
        // point dominate `lookups_ok` — a number that means "lookups" everywhere
        // else in `doctor`'s table. One call, one outcome.
        self.note(
            g,
            self.first_dynamic_edge(),
            self.fold_adaptive(g, span, tol, scratch),
        )
    }

    /// [`Self::at_adaptive`]'s body, split out so the counter bracket wraps a
    /// single expression.
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
        // First knot.
        scratch.stamps.push(Stamp::from_nanos(a_s));
        scratch.poses.push(a_p);

        if b_s <= a_s {
            // Degenerate span: a single knot is sufficient. Return what we have.
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
    // Can we still subdivide? Need depth budget, a non-adjacent segment, and room
    // for at least one more knot. The knot-room check reserves `MAX_ADAPTIVE_DEPTH`
    // headroom: once splitting stops, up to `depth` already-committed ancestors
    // still emit one knot each as the DFS unwinds, so this keeps the final count
    // at or below `MAX_KNOTS`.
    // **The width is taken in `u64`, and that is not a micro-optimisation.**
    // `a_s < b_s` is guaranteed by `at_adaptive`'s degenerate-span early return,
    // so `b_s - a_s` is mathematically non-negative — but it does not fit in an
    // `i64` once the span exceeds `i64::MAX`, and `at_adaptive(i64::MIN, i64::MAX)`
    // is a *legitimate* request rather than a pathological one: it is precisely
    // what `span() == Ok(None)` — "answerable at any stamp" — invites a caller to
    // ask of an all-static plan. The signed subtraction panicked on it in a checked
    // build and wrapped in a release one, and the release wrap is the worse half:
    // the negative difference fails the `> 1` test, so the recursion stops
    // immediately and returns a two-knot straight line for a path that was never
    // straight. Measured on a dynamic plan spanning ±2^62: two knots, endpoints
    // -0.4989 and -0.9953, true midpoint -17.2030, against a requested tolerance
    // of 1e-6. No error, no panic, no knot in the middle.
    //
    // Casting through `u64` makes the difference exact for every ordered pair an
    // `i64` can hold, and `wrapping_sub` is the *identity* on that difference
    // rather than a truncation of it — for `(i64::MIN, i64::MAX)` it is `u64::MAX`,
    // the true width.
    let width = (b_s as u64).wrapping_sub(a_s as u64);
    let can_split = depth < MAX_ADAPTIVE_DEPTH
        && width > 1
        && scratch.stamps.len() + (MAX_ADAPTIVE_DEPTH as usize) + 1 < MAX_KNOTS;
    if can_split {
        // `width / 2 <= 2^63 - 1` fits an `i64`, and `a_s + width / 2` is the
        // midpoint of two `i64`s, which always does too — so the add cannot
        // overflow either, and `wrapping_add` documents that rather than risking it.
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
    // Relative rotation angle: ‖log_so3(q_approx* · q_exact)‖.
    let dq = approx.q.conjugate() * exact.q;
    let rot = log_so3(dq).norm();
    let trans = approx.t.sub(exact.t).norm();
    rot <= tol.rot_rad && trans <= tol.trans
}

/// The per-component error tolerance for [`Plan::at_adaptive`].
///
/// `#[non_exhaustive]`, so build it with [`ErrBound::new`] rather than a struct
/// literal. This is a caller-supplied *configuration* struct, and the crate
/// already has one — `tf_tree::EdgeCfg` — that is `#[non_exhaustive]` with a
/// constructor for exactly this reason: a tolerance is the shape that grows
/// (a time bound, a per-axis split), and the attribute is free before a
/// published tag and a major bump after it. A new field arrives with a default
/// [`ErrBound::new`] picks, which is the answer a struct literal cannot give.
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

/// Caller-provided scratch storage for [`Plan::at_adaptive`], sized to hold the
/// maximum knot set. Allocated once by the caller (its allocation is *not*
/// counted against `at_adaptive`, which never allocates globally).
pub struct AdaptiveScratch<D: Domain = SystemDomain> {
    stamps: alloc::vec::Vec<Stamp<D>>,
    poses: alloc::vec::Vec<Iso3>,
}

impl<D: Domain> AdaptiveScratch<D> {
    /// Allocate scratch with capacity for [`MAX_KNOTS`] knots. Reusable across
    /// many `at_adaptive` calls.
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

// ---------------------------------------------------------------------------
// The evaluation guard
// ---------------------------------------------------------------------------

/// A batch-evaluation handle: it borrows the arena and pins the topology
/// generation once, so a run of lookups validates against a single snapshot.
///
/// [`Plan::at`] compares the plan's compiled generation against the guard's pinned
/// generation; a mismatch is [`LookupError::TopologyChanged`]. Make one guard per
/// batch of lookups.
pub struct Guard<'a> {
    view: ArenaView<'a>,
    /// The pinned topology generation, or [`DETACHED`] for a guard built by
    /// [`Guard::detached`].
    generation: u64,
    /// Successful lookups so far, flushed to the arena on drop.
    ///
    /// **A plain `Cell<u32>`, not an atomic, and that is the whole design**
    /// (`docs/PHASE5.md` §5.4). `Guard` is `!Sync` by construction — it holds an
    /// `ArenaView` and is created per batch on one thread — so nothing else can
    /// observe this, and a non-atomic increment on a line already in L1 is what
    /// the hot path pays.
    ///
    /// The alternative is a relaxed `fetch_add` per lookup, which is both a
    /// per-lookup cost and, worse, a *contended* one: sixteen readers on one
    /// edge would serialize on that cache line. Accumulating and flushing once
    /// turns N atomics into one.
    ///
    /// `u32` rather than `u64`: a guard spanning four billion lookups is a
    /// guard nobody holds, and the flush saturates rather than wrapping.
    #[cfg(feature = "counters")]
    ok: core::cell::Cell<u32>,
    /// Which edge's counters to credit, when every lookup in the batch went
    /// through one plan.
    ///
    /// `None` while the guard has seen no lookup, and **also** once it has seen
    /// lookups on two different edges — because a single count cannot be
    /// attributed to both, and attributing it to whichever came last would be
    /// worse than not attributing it. A plan crossing several edges credits the
    /// participant total and no edge, which is the honest answer.
    #[cfg(feature = "counters")]
    ok_edge: core::cell::Cell<Option<EdgeId>>,
    /// Per-step bracket-search hints, packed `(edge << 32) | index`, so a
    /// scalar lookup resumes beside the previous answer instead of restarting at
    /// the window midpoint.
    ///
    /// # Why this is worth ~9% of a lookup
    ///
    /// `docs/design/fast-path.md` §12 measured the bracket search at **34% of a
    /// dynamic step**, and its capacity sweep showed the cost is not the probe
    /// count but whether the probed *stamp array* fits L1 — flat to capacity
    /// 1024, then stepping hard at 32 KiB of stamps, this host's L1d. A cursor
    /// does not shrink that array; it makes the access **local**, so the probes
    /// land in a line the previous query already pulled in.
    ///
    /// Measured on a monotone sweep (`step_cost`): 54.58 -> 40.71 ns/sample at
    /// capacity 4096, and 58.54 -> 41.37 at capacity 16384 — which is what a
    /// 1 kHz edge with 10 s of history actually gets. It also nearly **flattens
    /// the cliff**: a fresh search costs +7% going 4096 -> 16384, the cursor
    /// +1.6%.
    ///
    /// # Why this cannot affect a result
    ///
    /// [`SampleRing::sample_from`](crate::buffer::SampleRing::sample_from) is
    /// documented and tested to return exactly what
    /// [`SampleRing::sample`](crate::buffer::SampleRing::sample) returns for the
    /// same `t`; only the search path differs. So a stale, wrong or absent
    /// cursor is a bad *hint* and never a wrong answer. That is what makes this
    /// safe to keep in a cache that nothing invalidates, and it is also why the
    /// index may be packed into 32 bits: a logical index past `u32::MAX` — 49.7
    /// days of unbroken 1 kHz publishing — truncates, and the gallop corrects it.
    ///
    /// **Correctness was never the issue there, and cost was.** A truncated hint
    /// is smaller than `lo_logical` for every subsequent query, so the clamp in
    /// [`SampleRing::bracket_from`](crate::buffer::SampleRing) used to pin it to
    /// the *oldest* retained sample on every call — permanently reverting the
    /// resumed gallop to a walk from the far end of the window, which is worse
    /// than the midpoint restart it exists to beat. A cliff rather than a decay:
    /// before 2^32 exact, after it inert, and no test could see the difference
    /// because a bad hint still yields the right answer.
    /// `sample::rebase_hint` lifts the truncated value back onto the live
    /// window; the window is strictly narrower than 2^32, so the lift is exact
    /// rather than a heuristic.
    ///
    /// # Why a `Cell`, and why on the `Guard`
    ///
    /// `Guard` is `!Sync` by construction and created per batch on one thread —
    /// the same argument `ok` above carries from `docs/PHASE5.md` §5.4 — so a
    /// non-atomic cell is sound and is what the hot path should pay.
    ///
    /// One array rather than two, and one word rather than two, because the cost
    /// of this cache is **initialising it**: every cell is written when a guard
    /// is built, and that is the whole of `Guard::new`'s 1.4 -> 8.5 ns. Packing
    /// halved the stores. (An inline-`const` initialiser was also tried and
    /// moved nothing.)
    ///
    /// The edge half is the self-invalidation. One `Guard` can evaluate several
    /// plans, and step `k` of one plan is a different edge from step `k` of
    /// another; without the tag the hint would send the gallop somewhere
    /// arbitrary — still correct, but potentially costing more than a plain
    /// search. Using the hint only on a tag match makes a mismatched cursor cost
    /// one comparison instead.
    cursor: [core::cell::Cell<u64>; MAX_DEPTH],
    /// `(generation at creation, how to read it now)`, for the fork check.
    ///
    /// **The flush is a write into the arena from a destructor**, and a shared
    /// mapping is `MADV_DONTFORK` — so in a `fork` child that arena is a hole in
    /// the address space and the destructor faults. That is the same trap
    /// `EdgeWriter::drop` already carries a guard for (`docs/decisions/0005`
    /// step 9), and `Guard` walked straight into it: a guard created before a
    /// fork and dropped in the child segfaults, which review reproduced.
    ///
    /// A function pointer rather than a direct call because `tf_tree_core` is
    /// `no_std` and knows nothing about processes; the facade supplies
    /// `tf_tree_ipc::fork::generation`. `None` for a heap arena, which cannot
    /// have the problem.
    #[cfg(feature = "counters")]
    fork: Option<(u64, fn() -> u64)>,
}

/// The generation a [`Guard::detached`] guard carries.
///
/// **Not a real generation, and unreachable as one.** The topology generation
/// starts at 0 and is bumped once per mutation; reaching `u64::MAX` would take
/// longer than the age of the universe at any rate a robot can mutate a tree, so
/// there is no value here to collide with.
///
/// Encoding the poison in a field that already exists — rather than adding one —
/// costs `Guard` nothing and adds **no load at all** to the hot path:
/// (the "48 bytes" this sentence used to quote was already stale when `0034`
/// measured it at 208; it is 336 at `MAX_DEPTH = 32`, since `Guard` carries
/// `[Cell<u64>; MAX_DEPTH]`. The argument was never about the absolute size —
/// it is that a field that already exists costs no bytes at all.)
/// [`Plan::check_generation`] already reads `generation`, and the poison check
/// is the comparison it was already making. An `Option<LookupError>` field cost
/// 32 bytes on a struct built once per `at()` call.
const DETACHED: u64 = u64::MAX;

/// Which [`EdgeCounters`] field a lookup error belongs in.
///
/// A tiny enum rather than a closure so `counter_of` stays a pure classification
/// with no borrow of the arena — the caller does the lookup, and the mapping is
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

    /// The same classification against the participant mirror.
    ///
    /// Two `match`es rather than a generic over the two structs: they are
    /// `#[repr(C)]` records in a cross-process layout, and a trait that made
    /// them interchangeable would make it easy to add a field to one and not
    /// the other without noticing. `counters.rs` has a test pinning their
    /// shared prefix for the same reason.
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
/// edge.
///
/// **Only errors that name an edge are counted**, which is most of them by
/// design (D11: every variant that *can* name an edge does). The ones that
/// cannot — `UnknownFrame`, `Disconnected`, `TopologyChanged` — are properties
/// of the *query*, not of an edge, and filing them under one would send an
/// operator to inspect a publisher that is working correctly.
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
            // Split, because the two mean opposite things. Past the newest
            // stamp usually means a publisher stopped; before the oldest means
            // a consumer is running behind, or the ring is too short. `TFT010`
            // and `TFT011` key off exactly this distinction.
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

/// [`DETACHED`], for the test that pins it. Any other value is a generation some
/// tree can reach.
#[cfg(test)]
pub(crate) const DETACHED_FOR_TEST: u64 = DETACHED;

/// Flush the batch's success count into the arena — **one relaxed atomic per
/// guard, not per lookup** (`docs/PHASE5.md` §5.4).
///
/// A guard spanning 1000 lookups pays one `fetch_add` per 1000, per thread, so
/// the contention a per-lookup atomic would create on a hot edge simply does not
/// arise. That is the entire argument for buffering, and it is why the
/// destructor exists at all.
#[cfg(feature = "counters")]
impl Drop for Guard<'_> {
    fn drop(&mut self) {
        let n = self.ok.get();
        // Same read-only guard as `note_err`, and this is the path that
        // actually faulted: a consumer's guard drops at the end of every batch.
        if n == 0 || !self.view.is_writable() {
            return;
        }
        // And the fork guard. A shared mapping is `MADV_DONTFORK`, so in a child
        // the arena is a hole in the address space and this write faults —
        // exactly the failure `EdgeWriter::drop` already guards against. A
        // destructor is the worst place to discover it, because it runs whether
        // or not the child ever called anything.
        if let Some((born, read)) = self.fork {
            if read() != born {
                return;
            }
        }
        use crate::sync::Ordering::Relaxed;
        // Credited to an edge only when the whole batch went through one. A
        // multi-edge plan credits nothing here rather than crediting the last
        // edge it happened to touch, which would be a number no operator could
        // interpret.
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
    /// lookups.
    ///
    /// The pinned value is always a stable generation — A1 removed the odd
    /// "write in progress" state, so there is no torn value to pin. Formerly the
    /// mutation publishes mid-flight would make every [`Plan::at`] against this
    /// guard fail with [`LookupError::TopologyChanged`] for no reason.
    #[must_use]
    pub fn new(view: ArenaView<'a>) -> Guard<'a> {
        let generation = view.topology().stable_generation();
        Guard {
            view,
            generation,
            // `EdgeId(0)` is the sentinel no builder hands out, so a fresh guard
            // matches no edge and every step takes the cold path once.
            cursor: [const { core::cell::Cell::new(0) }; MAX_DEPTH],
            #[cfg(feature = "counters")]
            ok: core::cell::Cell::new(0),
            #[cfg(feature = "counters")]
            ok_edge: core::cell::Cell::new(None),
            #[cfg(feature = "counters")]
            fork: None,
        }
    }

    /// Attach a fork-generation check to this guard's counter flush.
    ///
    /// `read` must return a value that changes when the process forks. The
    /// facade passes `tf_tree_ipc::fork::generation`; a heap arena passes
    /// nothing, because it has no mapping to lose.
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

    /// Record a successful lookup through `edge` (`docs/PHASE5.md` §5.4).
    ///
    /// One non-atomic increment. Compiled away entirely without the `counters`
    /// feature — §5.5's point being that "off" should mean *no code*, not a
    /// runtime branch.
    #[inline]
    pub(crate) fn note_ok(&self, edge: EdgeId) {
        #[cfg(feature = "counters")]
        {
            self.ok.set(self.ok.get().saturating_add(1));
            // **`EdgeId(0)` is the "no edge" sentinel and must not latch.**
            // `first_dynamic_edge` returns it for a multi-edge plan, and the
            // comment there claimed this folded it into "credit no edge". It
            // did not: `Some(0)` latched, and the flush went into the reserved
            // edge-0 counter record — which `edge_counters` happily returns,
            // since it bounds only against `max_edges`.
            //
            // Two consequences, and the second is the worse one. A consumer
            // iterating edges from 0 rather than 1 sees a phantom count (the
            // same off-by-one `error.rs` records `tf_tree_c` having already
            // shipped once). And on a real robot the common query — `map` to
            // `base_link` — *is* multi-edge, so every reader in every process
            // would have funnelled its flush into one 64-byte line: precisely
            // the false sharing `EdgeCounters`' padding exists to prevent.
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

    /// Record a failed lookup, on the error path where cost is irrelevant.
    ///
    /// Unlike [`Self::note_ok`] this writes straight through to the arena: a
    /// failure is rare by construction, and buffering it would mean a process
    /// that dies mid-fault takes the evidence with it — which is precisely the
    /// case §5.3 exists for.
    #[inline]
    pub(crate) fn note_err(&self, err: &LookupError) {
        #[cfg(feature = "counters")]
        {
            use crate::sync::Ordering::Relaxed;
            // **A read-only view must not write.** A consumer maps the arena
            // read-only (D18), so this would fault with SIGSEGV — which it did,
            // killing a read-only child in the multiprocess suite before the
            // check existed. §5 does not discuss the interaction; the resolution
            // is that a read-only participant keeps no counters, because it
            // cannot, and refusing to run would be far worse than losing a
            // diagnostic.
            if !self.view.is_writable() {
                return;
            }
            let Some((edge, field)) = counter_of(err) else {
                return;
            };
            // The failure's own stamp when it has one, so "when" is in the
            // arena's time domain rather than the reader's wall clock — which
            // `tf_tree_core` has no access to anyway (`no_std`, no clock).
            // Zero when the error carries no stamp, which reads as "never" and
            // is honest: the alternative is inventing a time.
            let now = match *err {
                LookupError::Extrapolation { requested, .. } => requested,
                _ => 0,
            };
            // **Both halves, not just the edge.** §5.2 is explicit that
            // per-participant counters are what make a diagnostic actionable —
            // "which consumer is failing", not merely "failures exist" — and
            // the first version wrote only the edge side, leaving the entire
            // participant failure surface, and `last_err_edge` with it, dead.
            if let Some(slot) = self.view.interning_identity() {
                if let Some(p) = self.view.participant_counters(slot) {
                    field.bump_participant(p);
                    p.last_err_edge.store(edge.get(), Relaxed);
                    p.last_err_nanos.store(now, Relaxed);
                }
            }
            if let Some(c) = self.view.edge_counters(edge) {
                field.bump(c);
                // `last_err_nanos` was declared and never written, so "when did
                // this last fail" always read "never" — the field that turns a
                // count into an incident, per its own doc comment.
                c.last_err_nanos.store(now, Relaxed);
                if let LookupError::Extrapolation {
                    requested,
                    oldest,
                    newest,
                    ..
                } = *err
                {
                    // The high-water mark, not a total: "we were 4 seconds past
                    // the end once" is actionable and "we were past the end 900
                    // times" is not. `TFT011` reads this against the ring's
                    // span to answer whether the buffer is simply too short.
                    let gap = if requested > newest {
                        requested.saturating_sub(newest)
                    } else {
                        oldest.saturating_sub(requested)
                    };
                    // **`fetch_max`, not load/compare/store.** The latter is a
                    // non-atomic read-modify-write on a location several
                    // consumers write concurrently, and the failure is worse
                    // than a lost update: the mark can *regress*. Two threads
                    // load 0; one stores 10 s; the other then stores 500 ms, and
                    // a value that had already been published as 10 s reads
                    // 500 ms afterwards. `TFT011` compares this against the
                    // ring's span to decide whether the buffer is too short, so
                    // a regression makes it conclude the ring is fine for a ring
                    // that lapped by ten seconds — the check silently inverts.
                    //
                    // Reproduced by review at 154 regressions in 200 000 trials
                    // on x86-64, which is the *friendly* memory model. Relaxed
                    // is still the right ordering; what was missing is
                    // atomicity, not ordering.
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
    /// # Why this is not the same as returning an error from the constructor
    ///
    /// A facade may know the arena has become unreachable — the shared mapping
    /// went away under a `fork()` is the case this was built for — at a point
    /// where its own API cannot report it. `Tree::guard` is infallible and used
    /// in a `let g = tree.guard();` idiom by dozens of callers; making it
    /// fallible to carry a condition none of them will ever hit is the wrong
    /// trade, and *not* reporting it means the next read dereferences memory
    /// that is no longer mapped.
    ///
    /// This is the third option: hand back a guard that is safe to hold, safe to
    /// pass to [`Plan::at`], and answers `err` to everything. Note that
    /// [`Self::new`] reads the topology **immediately**, so a caller in this
    /// position cannot use it even to build a guard it intends to discard —
    /// which is exactly why this constructor exists rather than a `poison`
    /// setter.
    ///
    /// `view` must still be a view over a *valid* arena, because [`Self::view`]
    /// hands it out; supply a small throwaway arena rather than the unreachable
    /// one.
    ///
    /// There is deliberately no `poisoned(view, err)` taking an arbitrary error.
    /// One was written and replaced: carrying the error meant a 32-byte
    /// `Option<LookupError>` field on a struct that is built once per `at()`
    /// call, to express a generality nothing needed. See `DETACHED`.
    #[must_use]
    pub fn detached(view: ArenaView<'a>) -> Guard<'a> {
        Guard {
            view,
            generation: DETACHED,
            // A detached guard fails every evaluation, so it never counts a
            // success, never reaches a search, and its destructor is a no-op —
            // but the fields must exist, and starting them at zero is what makes
            // that true.
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
    ///
    /// `#[inline]`, and so are its two siblings below: they are the last
    /// non-generic links in the chain a downstream crate has to inline through
    /// to reach the generic `SampleRing::sample`, whose MIR is available to it
    /// anyway. [`Plan::at`]'s table measures what that is worth — without these
    /// three the caller still emits one cross-crate call, to
    /// [`Self::sample_hinted`].
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

    /// [`Self::sample`], resuming from this guard's cursor for step `k`.
    ///
    /// The scalar fold's entry point. It differs from [`Self::sample`] only in
    /// *where the bracket search starts*: `sample` restarts at the window
    /// midpoint every call, this resumes beside the previous answer. See
    /// [`Guard::cursor`] for the measurement and for why a wrong cursor
    /// cannot produce a wrong result.
    ///
    /// The tag check is what keeps a mismatched hint cheap. `k` indexes the
    /// plan's step, and one guard may evaluate several plans, so the cursor is
    /// only trusted when it was last written by this same edge.
    #[inline]
    pub(crate) fn sample_hinted(
        &self,
        k: usize,
        edge: EdgeId,
        t: i64,
        policy: ExtrapPolicy,
    ) -> Result<Iso3, LookupError> {
        // A plan is bounded by MAX_DEPTH, so this is always in range; the guard
        // keeps the array access provably safe rather than relying on it.
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
        // Written on success only. A failed sample leaves `cursor` wherever the
        // search abandoned it, and storing that would poison the next query's
        // hint with a position no successful search produced.
        if out.is_ok() {
            slot.set((u64::from(edge.0) << 32) | (cursor & 0xFFFF_FFFF));
        }
        out
    }

    /// Sample edge `edge` at `t` and also return its body twist, in 1/second.
    ///
    /// Refuses `LerpSlerp` rather than dispatching to it —
    /// [`LookupError::DerivativesUnavailable`] carries the reasoning.
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

    /// [`Self::sample_with_twist`], resuming from `cursor` — the derivative
    /// path's counterpart to [`Self::sample_from`].
    ///
    /// The refusal is checked here too, and before the ring is touched: a
    /// `LerpSlerp` edge has no exact body twist whatever the search does, and
    /// deciding that once per sampler rather than once per caller is what keeps
    /// the batch layout's refusal identical to the scalar call's.
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
    /// The two ends come from **one** [`SampleRing`](crate::buffer::SampleRing)
    /// handle but from two
    /// independent `head` loads (see [`SampleRing::oldest_stamp`](crate::buffer::SampleRing::oldest_stamp) and
    /// [`SampleRing::newest_stamp`](crate::buffer::SampleRing::newest_stamp)), so on a live ring a concurrent `push`
    /// between them can widen the pair past either real window. That is the same
    /// staleness [`Plan::latest`] has and is not fixable here without a seqlock
    /// over the whole ring; [`Plan::span`] documents what it means for a caller.
    /// On a frozen arena — the case §4.2 is about — no push exists and the pair
    /// is exact.
    pub(crate) fn window(&self, edge: EdgeId) -> Result<(i64, i64), LookupError> {
        let ring = self
            .view
            .ring(edge)
            .ok_or(LookupError::UnknownEdge { edge })?;
        match (ring.oldest_stamp(), ring.newest_stamp()) {
            (Some(oldest), Some(newest)) => Ok((oldest, newest)),
            // An empty ring is `NoData`, never an empty interval: the caller
            // acts differently on "nobody has published this yet" than on "the
            // windows do not overlap".
            _ => Err(LookupError::NoData { edge }),
        }
    }

    /// An edge's declared nominal publish rate, in milli-hertz, `0` meaning
    /// *undeclared*.
    ///
    /// The sentinel is passed through rather than folded into an `Option` here,
    /// because this is the layer that reads the record and
    /// [`Plan::slowest_nominal_rate_mhz`] is the layer that decides what
    /// "undeclared" means for a wait. Two callers already disagree about that —
    /// the wait skips it, `docs/PHASE5.md` §6's `TFT007` reports it as a skip
    /// reason — so the interpretation does not belong on the read.
    ///
    /// Reads the edge *record*, not the ring: a rate is declared at
    /// construction and does not depend on anything having been published, which
    /// is why this cannot return [`LookupError::NoData`] the way
    /// [`Self::window`] must.
    pub(crate) fn nominal_rate_mhz(&self, edge: EdgeId) -> Result<u32, LookupError> {
        Ok(self
            .view
            .edge(edge)
            .ok_or(LookupError::UnknownEdge { edge })?
            .nominal_rate_mhz)
    }
}

// ---------------------------------------------------------------------------
// Compilation
// ---------------------------------------------------------------------------

/// Compile a `lookup(target, source)` path into a [`Plan`].
///
/// Reads parent/depth/`edge_of_child` from `topo` under the topology seqlock,
/// walking up from both frames to their lowest common ancestor. All reads are
/// taken from a single consistent generation (retried if a mutation lands
/// mid-walk); the returned plan records that generation. `edge_meta` supplies the
/// kind/domain/static-pose of each edge for constant folding, and returns `None`
/// for an edge id this arena has no record for.
///
/// # Errors
///
/// * [`LookupError::Disconnected`] — `target` and `source` are in different
///   connected components.
/// * [`LookupError::TreeTooDeep`] — the walk needed more than
///   [`MAX_PATH_EDGES`] raw edges, or the path folded to more than
///   [`MAX_DEPTH`] steps. Which one is readable off the reported `depth`; see
///   the variant's own documentation.
/// * [`LookupError::FrameOutOfRange`] — a frame id is out of range for `topo`.
/// * [`LookupError::MissingEdge`] — a parent link on the path records no edge.
/// * [`LookupError::UnknownEdge`] / [`LookupError::MixedTimeDomains`] — raised
///   by the constant fold, and **before** the compiled-length refusal: the fold
///   resolves every edge on the path, including the ones past the step array, so
///   a defect anywhere on a too-long path is named rather than hidden behind its
///   length.
pub fn compile(
    topo: &TopologyView,
    edge_meta: impl Fn(EdgeId) -> Option<EdgeMeta>,
    target: FrameId,
    source: FrameId,
) -> Result<Plan, LookupError> {
    if target == source {
        // Identity plan; still stamp it with a consistent generation.
        return Ok(Plan::identity(topo.stable_generation()));
    }

    // Retry the whole walk if a topology mutation lands between reads, so every
    // read is from one consistent generation (`docs/PHASE1.md` §5.2 reader protocol).
    'walk: loop {
        // Every published generation is stable since A1 removed the odd state,
        // so there is no parity to check and nothing to wait for. The retry
        // below exists only to discard a walk that straddled a mutation.
        let start_gen = topo.generation();

        // Read (parent, depth, edge_of_child) for `f`, or signal a restart if the
        // read observed a different generation.
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

        // Edges collected walking up from target (emit inverted, in order) and from
        // source (emit forward, in reverse). Bounded by MAX_PATH_EDGES: two
        // `[u32; 64]` buffers, 512 bytes of stack together.
        let mut t_edges = [0u32; MAX_PATH_EDGES];
        let mut nt = 0usize;
        let mut s_edges = [0u32; MAX_PATH_EDGES];
        let mut ns = 0usize;

        // Record the edge on the link from `$frame` up to its parent. Edge id `0`
        // is the "no edge" sentinel (`set_parent` accepts it when only the parent
        // link matters) but is *also* a real edge-table slot, so it must never be
        // emitted as a `Step::Dyn` — that would silently sample an unrelated
        // edge's ring.
        //
        // The raw bound lives here, checked on `nt + ns` rather than per side,
        // so `MAX_PATH_EDGES` means "edges walked" and not "edges walked on one
        // side": the per-side spelling this replaces let a Y-shaped path walk up
        // to twice the bound before refusing. It is checked *before* the
        // sentinel, which keeps the precedence the old code had — a defect wins
        // over depth by its position on the path, not by the path's length.
        //
        // The walk does not keep counting past the bound the way `fold_into`
        // does.
        // It cannot: it stops precisely because it has no buffer left, and a
        // corrupt parent chain with a cycle in it would not terminate. So the
        // number reported is `MAX_PATH_EDGES + 1` — "more than the bound", the
        // one value above it this field ever takes — rather than a raw length
        // nothing measured.
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
            // Depths are no longer compared past the lockstep phase, so only the
            // parent and edge_of_child are needed here.
            let (p, _d, e) = read!(a);
            pa = p;
            ea = e;
            let (p, _d, e) = read!(b);
            pb = p;
            eb = e;
        }

        // Confirm the whole walk observed one generation before folding.
        if topo.generation() != start_gen {
            spin();
            continue 'walk;
        }

        // `fold_into` takes the two edge-id slices directly. There used to be a
        // `[Step; MAX_DEPTH]` intermediate here, built purely to hand to `fold`;
        // it held nothing the slices do not — an edge id, plus an `inverted`
        // flag that is `true` iff the edge came from `t_edges`. Deleting it is
        // what lets the raw bound be generous, and it is also what pays for
        // `MAX_DEPTH`'s move to 32: `compile` used to materialise two arrays of
        // it per call and now materialises one.
        // Folded **into the plan that is about to be returned**, not into a
        // temporary handed back by value (#264). A refusal drops `plan` with
        // its steps half-written, which is unobservable for the same reason it
        // always was: it is a local, and this returns `Err`.
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
/// the step is inverted), then collapse adjacent `Static` runs by composing them.
/// Writes the folded steps into `plan`, along with the four fields that are a
/// function of them: `len`, `domain`, `dyn_count` and `first_dyn`.
///
/// # The `Plan` is borrowed, and the array used to be returned
///
/// This returned `([Step; MAX_DEPTH], usize, u8)`, which put a 4096-byte
/// `memcpy` between its own stack and the caller's — measured in the shipped
/// disassembly, not assumed (#264, and the table on [`Plan::identity`]). It now
/// writes into the `Plan` `compile` is about to return, so the array is written
/// exactly once and there is one construction step rather than two to remember.
/// Nothing else changed: the loop below, its bound handling and its error
/// precedence are as they were.
///
/// # Why `dyn_count` and `first_dyn` are stored and not computed on demand
///
/// They used to be computed on demand, and `Plan::at` called *both* — once
/// through `check_domain` → `has_dynamic`, and once for `note`'s attribution.
/// Each is an O(`len`) scan over a 4 KiB `[Step; MAX_DEPTH]` array (2 KiB when
/// this was measured, at `MAX_DEPTH = 16`), so a depth-14 lookup walked 28 steps
/// before folding anything. `first_dynamic_edge`'s own doc comment already said
/// it was "loop-invariant" and hoisted it for the *batch* path; the scalar path
/// kept paying it per call.
///
/// Both are functions of the compiled steps alone, so compile time is the right
/// place — and this loop is the right point *within* compile time, because it
/// already holds the step and its discriminant. Deriving them from a second pass
/// over the array (which is what the short-lived `Plan::finish` did) put an
/// O(`len`) read back on the path #264 exists to shorten, immediately after
/// writing the thing it read. `Plan` is a value type — **not an arena
/// structure** — so storing them costs no format version and no layout hash; the
/// two fields land in padding the struct already had.
///
/// This is the only writer, so the derivation cannot drift from the steps it
/// describes; `plan_derived_fields_match_a_fresh_scan` pins it against a fresh
/// scan.
///
/// **`plan` may be left partially written when this returns `Err`.** That is not
/// a caller obligation so much as a fact with nowhere to go: the only caller
/// borrows a local `Plan` it then drops. Every entry is a valid `Step` — the
/// array arrives fully initialised from [`Plan::identity`] and this only ever
/// overwrites entries — and `len`/`domain`/`dyn_count`/`first_dyn` are published
/// in one block at the end, past every `?`. So a partial fold is not a
/// half-formed value; it is still the identity plan it started as.
///
/// Takes the walk's two raw buffers rather than a step array: `t_edges` in walk
/// order, emitted inverted, then `s_edges` **reversed**, emitted forward. That
/// reversal is load-bearing — it is why the composition associates
/// `((s[n-1] * s[n-2]) * …)`, and `Iso3` composition is not associative under
/// rounding, so an accumulator meeting `s[0]` first would produce a different
/// bit pattern that every tolerance-based test in the suite would accept.
///
/// # Running past the end of the output array
///
/// The output is `[Step; MAX_DEPTH]` and the input may be up to
/// [`MAX_PATH_EDGES`] long, so a path can fold to more steps than fit. The loop
/// does **not** stop when that happens: it skips the write, keeps incrementing
/// `n`, and goes on resolving every remaining edge through `edge_meta`. Two
/// things fall out of that, and both are the reason it is written this way:
///
/// * `n` past the loop is the *true* compiled length, which is what
///   [`LookupError::TreeTooDeep`] reports — not the bound, and not the raw walk
///   length.
/// * [`LookupError::UnknownEdge`] and [`LookupError::MixedTimeDomains`] are
///   still raised for a defect that sits past the bound. Returning early
///   instead is cheaper and was measured (a refused 64-edge dynamic chain: 994 ns
///   early-return against 1778 ns here, with two controls where the two do
///   identical work reading +3.2 / −6.2 ns, i.e. noise) — but it makes a
///   too-long path report its length instead of its defect, which is the
///   opposite of the precedence `0034` promises. The cost is paid only by paths
///   that are refused anyway.
///
/// The collapse decision therefore reads a tracked `last_static` rather than
/// `out[n - 1]`, which is the one thing that would not work past the array end.
///
/// # Errors
///
/// * [`LookupError::UnknownEdge`] — a step names an edge with no record in this
///   arena.
/// * [`LookupError::MixedTimeDomains`] — the path's dynamic edges do not all
///   share one time domain, so no single query stamp addresses them all.
/// * [`LookupError::TreeTooDeep`] — the folded path needs more than
///   [`MAX_DEPTH`] steps. Reported as the exact folded step count.
fn fold_into(
    plan: &mut Plan,
    t_edges: &[u32],
    s_edges: &[u32],
    edge_meta: &impl Fn(EdgeId) -> Option<EdgeMeta>,
) -> Result<(), LookupError> {
    let out = &mut plan.steps;
    let mut n = 0usize;
    // Derived here rather than by a second pass over `out`: the append arm below
    // already holds the step and its discriminant, so both fall out for free,
    // where a second pass would put an O(`len`) read back on the path #264
    // exists to shorten, immediately after writing the thing it reads.
    // Equivalent by construction — the
    // collapse arm only ever rewrites a `Static` in place, so it can neither add
    // nor remove a `Dyn`, and every append that is *written* is an append with
    // `n < MAX_DEPTH`, which on any path that returns `Ok` is every append.
    // `plan_derived_fields_match_a_fresh_scan` is the pin.
    let mut dyn_count = 0u8;
    let mut first_dyn = EdgeId(0);
    // Whether `out[n - 1]` is a `Static` — tracked rather than read back,
    // because `n` may be past the array. `false` while `n == 0`.
    let mut last_static = false;
    // `None` until the first dynamic step fixes the plan's domain. Every later
    // dynamic step must agree: taking the last one (as this used to) let a plan
    // spanning a system-clock edge and a sensor-clock edge pass `check_domain`
    // and then sample one of them with the wrong clock — exactly the silent
    // misread D9 exists to prevent.
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
                // Dynamic (or tombstone — treated as dynamic; sampling it will
                // surface the real error). Pin/verify the domain.
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
        // Both arms are guarded on the array bound rather than on `n` alone:
        // past `MAX_DEPTH` the composed value has nowhere to live and this call
        // is going to refuse, but the counting has to continue.
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
