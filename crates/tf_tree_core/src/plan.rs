//! Compiled lookup plans, typed time, and the evaluation `Guard`.
//!
//! A [`Plan`] resolves a `lookup(target, source)` path through the topology
//! *once* — a fixed [`Step`] array plus the generation it compiled against — so
//! the hot path samples time and never walks topology (`docs/PHASE1.md` §7;
//! `docs/PROJECT.md` §5 D3). `unsafe`-free, via [`crate::arena_view`] and
//! [`crate::sample`]. `#[cfg(not(loom))]`: [`Guard`] and [`compile`] need the
//! production-only [`ArenaView`]/[`TopologyView`].
//!
//! **Compilation direction, verified by hand.** `edge_of_child[c]` stores
//! `T_parent(c)_c` and `T_target_source = (T_lca_target)⁻¹ · T_lca_source`, so
//! the walk up from `target` emits inverted steps in walk order and the one up
//! from `source` emits forward steps *reversed*: over `map → odom → base`,
//! `lookup(base, map)` = `[Dyn(edge_base, inv), Dyn(edge_odom, inv)]` =
//! `T_base_odom · T_odom_map = T_base_map`. Correct.

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

/// A time domain: a compile-time marker carrying a runtime [`Domain::TAG`] byte.
///
/// Keeps clocks that must not be mixed separate at the type level, so a
/// cross-domain lookup is a compile error or a
/// [`LookupError::TimeDomainMismatch`], never a silent misread. Phase 6 adds the
/// machinery that *relates* domains; the separation must exist now so adding it
/// is not a breaking change (`docs/PROJECT.md` §5 D9; `docs/PHASE1.md` §8).
pub trait Domain: Copy {
    /// The runtime tag stored on an edge's `domain` field. Must be unique.
    ///
    /// Tags `0`–`3` are the built-ins ([`SystemDomain`], [`SensorDomain`],
    /// [`SimDomain`], [`SteadyDomain`]); a user-declared domain takes `4`
    /// upwards. The trait is open so a PTP-disciplined driver declares its own
    /// rather than pretending to be one of these (`docs/API.md` §2.5).
    ///
    /// **A tag is permanent**: it is written into `EdgeRecord::domain`, so
    /// re-numbering silently re-interprets every arena and every recording
    /// already on disk (`docs/API.md` §5.2).
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
/// A built-in, not a user tag: otherwise a sim deployment defaults to
/// [`SystemDomain`] and [`LookupError::TimeDomainMismatch`] never fires for the
/// population most exposed to it (`docs/API.md` §2.5). Mixing `/clock` time with
/// a driver's steady time yields a tree wrong by however long the bag has played
/// and *well-formed* throughout, because the offset between two clock domains is
/// not recoverable from one-way stamps (`docs/API.md` §5.2; `docs/PROJECT.md` §5
/// D9). It also lets `docs/PHASE5.md` §6's `TFT019` read `NonMonotonicStamp`
/// runs on a *wall-clock* tag as a clock step without firing on sim edges, which
/// step and loop on purpose. Named `SimDomain`, not the `SimTime`
/// `docs/PHASE4.md` §5.5 and `docs/PHASE7.md` §4 J9 first used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SimDomain;
impl Domain for SimDomain {
    const TAG: u8 = 2;
}

/// A steady, monotone clock (`CLOCK_MONOTONIC`-like), tag `3` — the domain
/// `docs/API.md` §5.3 recommends for anything published at rate.
///
/// A steady clock cannot step, so `docs/PHASE5.md` §6's `TFT019` can read a run
/// of `NonMonotonicStamp` rejections here as a real publisher defect, where the
/// same run on a [`SystemDomain`] edge is most likely an NTP step or a leap
/// second. It carries no epoch guarantee — two processes' `CLOCK_MONOTONIC`
/// values are unrelated across a reboot and sometimes across processes — which
/// is why it is a *separate* domain rather than a defect of one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SteadyDomain;
impl Domain for SteadyDomain {
    const TAG: u8 = 3;
}

/// Nanoseconds in one second. Named because four literals with the same nine
/// zeros is how one of them acquires eight.
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
    /// `struct timespec` both have. Exact, never a float, and never the
    /// hand-written `sec * 10**9 + nanosec` that wraps silently at the ends of
    /// `i64` (`docs/API.md` §5.1, normative).
    ///
    /// Total over every `(i64, u32)` pair — no panic, no wrap, no saturation.
    /// Two inputs return `None`: **`nanos >= 1_000_000_000`**, because both
    /// source formats define the field as the sub-second remainder, so
    /// normalizing it would turn a malformed message into a plausible stamp
    /// (`docs/API.md` R4 and §5.2); and **`sec * 1e9 + nanos` outside `i64`**,
    /// unreachable for a real clock (±292 years) and reachable for every
    /// uninitialised one, where wrapping hands back a stamp on the other side of
    /// the epoch that compares, interpolates and prints perfectly — tested
    /// against the *sum*, not the product, see the body. `None` does not say
    /// which: a caller rejects the message either way, and the distinction would
    /// be a new error type nobody branches on (`docs/PROJECT.md` §5 D11).
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
        // `i128`, not `checked_mul` then `checked_add`: the staged form refuses
        // *representable* stamps at the negative end (at `sec = -9_223_372_037`
        // the product alone is below `i64::MIN` while `product + nanos` is
        // exactly `i64::MIN`). `i64 * 1e9 + u32` cannot overflow `i128` for any
        // input. Not `wrapping_*` or a debug-only trap either — release must
        // refuse exactly what debug refuses.
        let total = sec as i128 * NANOS_PER_SEC as i128 + nanos as i128;
        if total < i64::MIN as i128 || total > i64::MAX as i128 {
            return None;
        }
        Some(Stamp(total as i64, PhantomData))
    }

    /// Assemble a stamp from the two fields of a POSIX `struct timespec`.
    ///
    /// Fields, not a struct: `tf_tree_core` is `no_std` on a
    /// `libm` + `bytemuck` + `blake3` budget (`docs/PROJECT.md` §5), and our own
    /// `#[repr(C)]` copy would be a type the caller must convert *into*.
    /// `tv_sec`/`tv_nsec` are
    /// `i64` on every 64-bit target, so no cast. Refuses everything
    /// [`Self::from_parts`] does plus a **negative `tv_nsec`**, which POSIX
    /// allows only in a *relative* `timespec` — catching a `nanosleep` interval
    /// converted as an absolute stamp.
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
        // The range check above is what makes this cast lossless.
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

/// Selects an interpolation policy at runtime from an edge's stored discriminant.
///
/// The runtime selector for the zero-sized [`tf_tree_math::Interp`] types,
/// stored in [`crate::edge::EdgeRecord::interp`].
///
/// **Deliberately *not* `#[non_exhaustive]`.** Every consumer maps this onto
/// something else (a `tft_interp` enumerator, a `tf_tree_bridge` config name, a
/// monomorphized fold) where a catch-all arm has no honest body, so the
/// attribute would turn "teach me the new policy" into a silent wrong answer.
/// Cross-version reads are already safe — [`InterpPolicy::from_u8`] collapses an
/// unknown discriminant onto the default. Same for [`crate::edge::EdgeKind`].
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
/// Returned by [`Plan::at_extrapolating`]. **No accessor yields the pose alone**
/// ([`0039`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0039-extrapolation-you-cannot-fail-to-notice.md)):
/// the danger is a pose that looks fresh, so reading it takes a deliberate
/// `.pose` past the distance. `Copy` and allocation-free (`docs/API.md` §1 R2),
/// and not an error type — [`ExtrapPolicy::Error`] is how a caller asks for
/// extrapolation to be a failure instead.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Extrapolated {
    /// The pose.
    pub pose: Iso3,
    /// Nanoseconds past the newest stamp that *every* dynamic edge on this plan
    /// has data for.
    ///
    /// `0` means every edge bracketed the query; a positive value is the worst
    /// case over the route, since the edge that runs out of data first bounds
    /// the composed answer. It over-reports on a live arena — measured *before*
    /// the fold, so a sample arriving mid-fold can beat the label — and never
    /// under-reports, because `0` is a claim a controller acts on. See
    /// [`Plan::at_extrapolating_tagged`] on that ordering.
    pub by_ns: i64,
    /// The dynamic edge whose newest stamp is [`Self::by_ns`] behind the query.
    ///
    /// Meaningless when `by_ns == 0`. Data, not a formatted name
    /// (`docs/PROJECT.md` §5 D11); resolve it against the arena.
    pub edge: EdgeId,
}

/// A pose and its derivatives at one instant — `docs/PHASE4.md` §2.2.
///
/// Returned by [`Plan::at_with_derivatives`]. The twist is **body-frame
/// (right)**, in the plan's **source** frame — see that method for why it is
/// the source, and [`tf_tree_math::twist`] for the convention.
///
/// `#[non_exhaustive]` because the engine produces it and callers only read it,
/// so growth cannot make a consumer silently wrong — and growth is already
/// scheduled, see [`Sample::accel`].
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
    /// so acceleration is zero inside one and a delta at the knots, and
    /// `Some(ZERO)` would claim a smoothness the piecewise-geodesic path lacks.
    /// Phase 6's cumulative B-splines are the first interpolant with a real one;
    /// the field exists now so adding them is not a breaking change.
    pub accel: Option<Twist>,
}

/// One step of a compiled plan.
///
/// Not `#[non_exhaustive]`, for [`InterpPolicy`]'s reason: every consumer of
/// [`Plan::steps`] classifies each step ("which edges does this plan sample?"),
/// and a `_ =>` arm under-counts silently where a compile error names the file
/// to fix.
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
    /// Complete as it stands — `steps[..0]` composes to identity, [`compile`]'s
    /// answer for `target == source`. The *only* constructor, and [`fold_into`]
    /// takes the whole `Plan`, so no arm can leave one half-built: a plan that
    /// skipped its fold would answer `Iso3::IDENTITY` for every stamp.
    ///
    /// # Why there is one array and it lives here
    ///
    /// A `Plan::new` taking the step array by value, fed by a `fold` returning
    /// one by value, cost two of three array-sized copies per compile, none
    /// optimised away (#264, disassembled at `MAX_DEPTH = 32`): `fold`'s `out`
    /// into the caller's `sret`, 4096 B; `Plan::new`'s parameter into
    /// `self.steps`, 4096 B; `compile`'s `Plan` into `Tree::plan`'s `sret`,
    /// 4160 B — 12 352 B of `memcpy` for a usually-six-step plan. The first two
    /// were one array crossing two by-value boundaries and are gone;
    /// `fold_into` now calls no `memcpy`.
    ///
    /// **Copy 3 stays, and not for lack of trying**: the local `Plan` is
    /// address-taken across the call to `fold_into`, so LLVM declines the `sret`
    /// slot, and `#[inline]` on `fold_into` changes nothing. 2064 B today
    /// (4160 B when measured, before `0042` halved `Step`); removing it needs an
    /// out-parameter on the `pub` [`compile`], a `docs/API.md` §7 change.
    ///
    /// Worth **−55.2%** on a 6-step `Tree::plan` — 265.0 → 118.7 ns, medians of
    /// 5 rounds of 20 000 reps, `taskset -c 2`, interleaved builds, ranges
    /// [261-267] against [114-122]; refused paths barely move (−4.3%, −11.1%),
    /// returning `Err` without constructing a `Plan`. Isolated from #259 with a
    /// third build: this change −54.8% here and −0.9% refused, #259 alone −48.2%
    /// refused and +4.3% here. The −9.3% residue on `plan_refused_walk_ns` has a
    /// mechanism in neither and is not claimed.
    ///
    /// **The identity array is not waste and is not removable**: `Step` is an
    /// enum, so an invalid discriminant in `steps[len..]` is UB the moment
    /// `Copy` or `Debug` touches it, and lazy init needs `MaybeUninit` —
    /// `unsafe`, at no boundary `docs/decisions/0007` names. It is a vectorised
    /// store loop the deleted `fold` already paid for `out`.
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
    /// Test-only; here because the fields are private. The scanning half is the
    /// pre-optimisation [`Plan::has_dynamic`]/[`Plan::first_dynamic_edge`] kept
    /// verbatim, so the test compares against the replaced behaviour rather than
    /// a paraphrase of it.
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
        // On the cold side of a comparison already being made: a detached guard
        // must not report `TopologyChanged` and send the reader looking for a
        // re-plan that cannot help.
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

    /// Evaluate the plan at nanosecond stamp `t`. Assumes the caller has already
    /// validated generation and domain.
    ///
    /// **`#[inline]` here is the load-bearing one** (measured on [`Self::at`]):
    /// `at` is generic and was inlined without it, so this was the only
    /// cross-crate call a downstream caller emitted.
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
    /// **A deliberate second copy of the loop, for constant folding.**
    /// [`Self::fold_at`] passes the `ExtrapPolicy::Error` *literal*, which lets
    /// LLVM prune the `Hold` and `ConstantTwist` arms out of the inlined
    /// `SampleRing::sample_from`; a policy parameter would leave that match live
    /// on [`Self::at`]'s hot path. One path compiled twice, not two paths, so
    /// `docs/PROJECT.md` §6's second-spelling rule does not apply.
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
    /// marked alongside [`Self::fold_at`] for symmetry, and [`Self::at`]'s probe
    /// never reached it — only `at_many`, `at_many_into`, `at_many_into_f32` and
    /// [`Self::fold_batch`] do. With an `#[inline(never)]` caller doing
    /// `at_many_into(.., Layout::Mat4, ..)` over 1024 monotone stamps at depth 3,
    /// best of five, x86-64, the attribute is a pessimization in both profiles:
    /// 328 against **285 ns/elem** at `lto = false, codegen-units = 16`, and 285
    /// against **278** at `lto = "thin", codegen-units = 1`. At the default
    /// profile the ~1.9 kB body is not inlined at either call site with or
    /// without the hint, so it only codegens a second copy into the embedder's
    /// object; under thin LTO it does inline and still loses. [`Self::at`]
    /// cannot reach this function — the probe's `caller_scalar` is
    /// byte-identical across the two builds — so its numbers stand.
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
    /// `docs/API.md` §2.3 makes it normative here, on the fold, on
    /// `Guard::sample` and on the `Iso3` operators. Its stated reason (no
    /// cross-crate inlining without it) is **not** why it helps: `at` is generic
    /// and downstream already inlined it. What it buys is `inlinehint` on the
    /// *non-generic* links, and marking fewer leaves a call in the middle.
    /// Depth-3 interpolating lookup, external crate, 20 M iterations, best of
    /// five: **313 → 256 ns** at `lto = false, codegen-units = 16` (cargo's
    /// `--release` default), **217 → 207 ns** at `lto = "thin",
    /// codegen-units = 1` (this workspace's). Caller `.text` / calls left, by
    /// placement: nothing 106 B / 1 → `fold_at`; `Plan::at` alone 106 B
    /// *byte-identical* / 1 → `fold_at`; `fold_at` alone 62 B / 1 → `Plan::at`;
    /// both 1332 B / 1 → `sample_hinted`; all five 1565 B / 3. **That is the
    /// price too** — 106 B → 1565 B per embedder call site, ~15×, scalar caller
    /// only — which is why `fold_at_with_derivatives`, `fold_latest` and
    /// `fold_latest_common` are not marked.
    ///
    /// **CORRECTION (2026-09-06): the last figure was labelled *"as shipped"*
    /// and is not.** It dates from 2026-08-02; on 2026-08-29 [`Self::at_tagged`]
    /// was interposed between this and `fold_at` (`0038`) with **no attribute**,
    /// so a downstream caller emits one cross-crate call, to `at_tagged`.
    /// Re-derive rather than read it: an external `#[inline(never)]` probe
    /// consuming all seven `Iso3` components under thin LTO is 16 instructions
    /// and one indirect `call`, and `#[inline]` on `at_tagged` takes it to 1403
    /// instructions and 8 calls — **deliberately not added**, see that method
    /// for the trap in measuring it. (Five links, not six: `fold_at_cursors` was
    /// marked in the same commit, is not on this path, and has been unmarked.)
    ///
    /// `lto = "thin"` does not subsume the hint — this workspace's profile still
    /// moves ~4.5% — so `just embed-cost` and `docs/PHASE5.md` §9.2's
    /// `embedding_cross_crate` row measure the boundary against §9.2's 5%
    /// criterion: ratio **1.250–1.254** (240.0 ns out-of-crate against
    /// 191.3–191.8 in) at `codegen-units = 16`, and 0.994–0.996 (193.0–195.0
    /// against 194.2–196.2) under thin LTO; three pinned runs, paired rounds.
    /// **Refuted:** an earlier revision added "which no `#[inline]` placement
    /// closes" — dropping this attribute from `fold_at` takes the ratio to
    /// **1.001**, slowing the in-crate column 6.7% (191.5 → 204.4 ns) and the
    /// control 6.9% while the out-of-crate column gets *faster*
    /// (239.9 → 203.9 ns). A placement moves it; none measured improves every
    /// column at once. **Do not read 203.9 ns against 256 ns** — different
    /// probes; only ratios within one measurement compare.
    #[inline]
    pub fn at<D: Domain>(&self, g: &Guard, t: Stamp<D>) -> Result<Iso3, LookupError> {
        self.at_tagged(g, t.nanos(), D::TAG)
    }

    /// [`Self::at`], with the query's domain carried as a runtime tag.
    ///
    /// The binding surface: [`Domain`] is an **open trait** — a user declares
    /// their own tag from `4` upwards — so a foreign binding cannot enumerate
    /// the domains it may be asked about and carries the tag as data instead
    /// ([`0038`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0038-the-domain-a-binding-cannot-name.md)).
    /// The check is [`Self::at`]'s, unchanged; Rust callers should use
    /// [`Self::at`], where a domain mistake is a compile error.
    ///
    /// # It carries no `#[inline]`, on purpose, and that is not free
    ///
    /// Every other link on the scalar path is marked, so this one — in the
    /// middle of them — is **the one real cross-crate call a downstream
    /// `plan.at(&g, t)` emits**, leaving the caller a 16-instruction stub.
    /// Marking it is **not** obviously a win: a review probe measured **+8–10 %
    /// instructions per lookup** at depths 1, 3 and 6 on a caller consuming the
    /// whole `Iso3`, unreproduced and not settled.
    ///
    /// **The trap, which is settled: the obvious probe inverts the sign.** One
    /// returning only `iso.t.x` reads the attribute as a couple of percent
    /// *cheaper*, because LLVM dead-codes the six unused pose components across
    /// the inlined body. Consume all seven, and decide with `just embed-cost`
    /// and `just bench-ab`, not a hand probe (`docs/API.md` §2.3 item 3) —
    /// noting that **`just embed-cost` is subject to the trap today**, both its
    /// probe bodies being
    /// `match plan.at(g, s) { Ok(iso) => iso.t.x, Err(_) => f64::NAN }`.
    /// Repairing them is the first step of re-opening this, not a refinement
    /// afterwards; `just bench-ab` is clear of it.
    ///
    /// # Errors
    ///
    /// As [`Self::at`].
    pub fn at_tagged(&self, g: &Guard, nanos: i64, domain: u8) -> Result<Iso3, LookupError> {
        self.check_generation(g)?;
        self.check_domain_tag(domain)?;
        // The counters bracket the fold, not the whole function: the two checks
        // above fail on properties of the *query* and name no edge, so counting
        // them would file a caller's mistake against a working publisher
        // (`docs/PHASE5.md` §5.2's attribution argument, inverted).
        self.note(g, self.first_dynamic_edge(), self.fold_at(g, nanos))
    }

    /// Record one evaluation's outcome against the diagnostic counters.
    ///
    /// **Every entry point that folds the plan goes through here.**
    /// `docs/PHASE5.md` §5.3 makes the error counters normative and always-on
    /// and §5.2 makes them the basis of `TFT010`/`TFT011`, so a fold that does
    /// not count has failures that never reach `tf_tree top` or `doctor` — and
    /// the batch paths matter most, `at_many_into` being Python's zero-copy path
    /// and `at_with_derivatives` the C ABI's.
    ///
    /// `edge` is a parameter because [`Self::first_dynamic_edge`] is a
    /// loop-invariant O(plan length) scan a batch caller resolves once.
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
    /// `EdgeId(0)` when the plan crosses several — the sentinel no builder hands
    /// out, which [`Guard::note_ok`] folds into "credit no edge". Attributing a
    /// multi-edge plan's success to one edge would put a number in `doctor`'s
    /// table meaning something different from the rest of its column.
    ///
    /// Reads the fields [`fold_into`] derived rather than scanning. The `== 1`
    /// test is why the count is stored and not just a `has_dynamic` flag.
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
    /// `V_ac^c = Ad(T_bc⁻¹)·V_ab^b + V_bc^c` for `T_ac = T_ab·T_bc`
    /// (`docs/PHASE4.md` §2.3). Two traps: **a static step still costs an
    /// adjoint**, because its frame changes even though its twist is zero, and
    /// skipping `Ad(m⁻¹)` leaves a valid-looking vector wrong by exactly that
    /// transform; and **an inverted step folds to one adjoint, not two** — for
    /// `S = p⁻¹`, `V_S = −Ad(p)·V_p` and `Ad(S⁻¹) = Ad(p)`, so
    /// `V' = Ad(p)·(V_acc − V_p)`, subtract first and rotate once.
    ///
    /// The sampler is a parameter because the cursor is the only difference
    /// between the scalar and batch forms, and this composition — easy to get
    /// wrong, impossible to spot in a result — must exist once. `S` is a
    /// distinct type per call site, so neither form pays for the other.
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
    /// A cursor is only a *hint* — the gallop still hands the binary search an
    /// interval bracketing `t` — so a stale one costs probes and cannot change
    /// an answer, which is what lets this share the cursor-less form's
    /// assertions.
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
    /// Body-frame (right), `V^b = (T⁻¹Ṫ)^∨`, in the plan's **source** frame,
    /// because `plan(target, source)` evaluates `T_target_source` and `T⁻¹Ṫ`
    /// resolves in the frame `T` maps *from*;
    /// [`tf_tree_math::Twist::to_spatial`] gives the **target** frame. For
    /// `plan(map, base)` with `base` rotated +90° about z and moving along
    /// **map**'s +x at 1 m/s:
    ///
    /// ```text
    /// sample.twist.v              == (0, −1, 0)   // resolved in base axes
    /// sample.twist.to_spatial(&p) == (1,  0, 0)   // resolved in map axes
    /// ```
    ///
    /// Both are 1 m/s, so **`‖v‖` is identical and a magnitude check cannot tell
    /// them apart**: getting it wrong is wrong by the full `R_target_source`,
    /// silently. An earlier revision said "target", which is why this is an
    /// example and not a sentence. Costs roughly two plain lookups — the same
    /// sampling plus one adjoint per step, no transcendentals.
    ///
    /// # Errors
    ///
    /// Everything [`Self::at`] can return, plus
    /// [`LookupError::DerivativesUnavailable`] for a `LerpSlerp` edge, whose
    /// body twist is an artifact of the interpolant and is refused rather than
    /// returned (§2.4), and [`LookupError::NoSegment`] when an edge has a pose
    /// at `t` but no segment to differentiate.
    pub fn at_with_derivatives<D: Domain>(
        &self,
        g: &Guard,
        t: Stamp<D>,
    ) -> Result<Sample, LookupError> {
        self.at_with_derivatives_tagged(g, t.nanos(), D::TAG)
    }

    /// [`Self::at_with_derivatives`], with the query's domain as a runtime tag — the
    /// binding surface, for [`Self::at_tagged`]'s reason. Same check, same
    /// errors; Rust callers want [`Self::at_with_derivatives`].
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
    /// Not a temporally consistent snapshot — the stamps may differ between
    /// edges; use [`Self::latest_common`] when consistency matters.
    ///
    /// # Errors
    ///
    /// [`LookupError::TopologyChanged`], or [`LookupError::NoData`] if a dynamic
    /// edge is empty.
    pub fn latest(&self, g: &Guard) -> Result<Iso3, LookupError> {
        self.check_generation(g)?;
        self.note(g, self.first_dynamic_edge(), self.fold_latest(g))
    }

    /// [`Self::latest`]'s fold, split out so [`Self::note`]'s bracket wraps a
    /// single expression.
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
    /// Per *query*, not per edge: a `Hold` right for a 10 Hz map edge is wrong
    /// for the 1 kHz odometry edge on the same route, so the caller who bears
    /// the consequence chooses. Both [`ExtrapPolicy::ConstantTwist`] and
    /// [`ExtrapPolicy::Hold`] hand back [`Extrapolated::by_ns`], so neither
    /// reads as fresh; [`Self::at`] remains the default and refuses. `by_ns`
    /// costs one `newest_stamp` load per dynamic edge, taken **before** the fold
    /// and only here, so [`Self::at`]'s code is unmoved — and that order is a
    /// soundness guarantee, see the implementation.
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

    /// [`Self::at_extrapolating`], with the query's domain as a runtime tag —
    /// the binding surface, for [`Self::at_tagged`]'s reason. Same errors.
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
        // **Before the fold, and the order is the guarantee.** Run after, a
        // `push` landing between the fold and this walk with a stamp at or past
        // `nanos` lifts `common`, and `saturating_sub().max(0)` below then
        // reports `by_ns == 0` — "not extrapolated" — for a pose the fold
        // invented; a 100 Hz edge under a 1 kHz query crosses the stamp often
        // enough to be a race a robot runs. Measured first the error inverts
        // into the safe direction, because `SampleRing::newest_stamp` is
        // non-decreasing: `by_ns > 0` may over-report a mid-fold arrival
        // (harmless), and `by_ns == 0` means every edge held data past `nanos`
        // before the fold began.
        //
        // Not `note`d: the fold's `note` below is this query's one counter
        // event, and a second would double `lookups_ok`, the denominator
        // `doctor`'s TFT010 and TFT011 divide by.
        let common = self.newest_common(g);
        let pose = self.note(g, edge, self.fold_at_policy(g, nanos, policy))?;
        let (by_ns, which) = match common? {
            // **`saturating_sub`, not `-`** — the pattern `sample::span_ns`
            // exists to eliminate. Under `Hold` or `ConstantTwist` a query is
            // accepted whenever `t >= t_old`, so a route publishing near
            // `i64::MIN` queried near `i64::MAX` reaches this line with the
            // difference outside `i64`; wrapped negative, `.max(0)` reports
            // `by_ns == 0` — "not extrapolated" — for the most extrapolated
            // answer this type can hold. Saturating says "further than
            // representable", which is true.
            Some((common, which)) => (nanos.saturating_sub(common).max(0), which),
            // Static-only: nothing to extrapolate.
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
    /// The minimum over the plan's dynamic edges: the edge that runs out of data
    /// first bounds how invented a composed answer is. Factored out so
    /// [`Self::latest_common`] and [`Self::at_extrapolating`] share one walk and
    /// one definition of "common"
    /// ([`0039`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0039-extrapolation-you-cannot-fail-to-notice.md)).
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
    /// **Not "the interval over which this plan is answerable"**, which this
    /// used to say: it is an *intersection of outer windows* and says nothing
    /// about holes inside them. On this repository's own recording, one edge's
    /// widest bracket is 5.3 s inside a 42 s span — 105× its own median —
    /// carrying 2.57 m of unobserved motion. Outside the interval [`Self::at`]
    /// refuses; **inside it, `at` answering is not evidence that anything was
    /// observed near the stamp**, which `tf_tree doctor`'s `TFT009` detects
    /// after the fact.
    ///
    /// [`Self::latest_common`] generalised from a point to a range: lower end a
    /// `max`, upper end a `min` and `latest_common`'s own stamp. No shared
    /// helper, because folding this onto `Guard::window` would cost that lookup
    /// path a second atomic load and mask per edge for a lower end it never
    /// uses; `spans_agree_with_latest_common` pins the agreement. It lives here
    /// rather than in a binding crate because the definition of a ring's
    /// readable window **has already changed once**
    /// ([`SampleRing::retained`](crate::buffer::SampleRing::retained)).
    ///
    /// Three answers, kept distinct: `t0 <= t1` answers there and nowhere else
    /// without extrapolating; **`t0 > t1` is a real answer**, two edges with
    /// disjoint histories, and collapsing it to `None` would make it
    /// indistinguishable from the third; `None` means every step folded static,
    /// so any stamp answers. On a live arena the answer ages immediately and the
    /// two ends are not even one snapshot of one ring (`Guard::window`) —
    /// [`Self::latest`]'s contract; on a frozen `.tft` it is exact.
    ///
    /// # Errors
    ///
    /// [`LookupError::TopologyChanged`] (or [`LookupError::ChildDetached`] on a
    /// fork-poisoned guard), [`LookupError::UnknownEdge`], or
    /// [`LookupError::NoData`] naming the first edge that never published — a
    /// different fact from an empty intersection.
    pub fn span(&self, g: &Guard) -> Result<Option<(i64, i64)>, LookupError> {
        self.check_generation(g)?;
        let mut span: Option<(i64, i64)> = None;
        for step in self.steps() {
            let Step::Dyn { edge, .. } = step else {
                // A static step constrains nothing in time, inverted or not.
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
    /// edges, in milli-hertz, or `None` when no edge declares one.
    ///
    /// With [`Self::span`], the whole engine-side input to a caller's blocking
    /// wait. **There is no blocking primitive in the arena and there is not
    /// going to be one**: a shared-memory wait needs the waiter to register by
    /// *writing* a word, and D18 attaches consumers `PROT_READ`, so that store
    /// is a `SIGSEGV`.
    /// [`0018`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0018-blocking-waits-belong-in-the-shim.md)
    /// holds the trade, the shim-side loop and the escalation path (the Phase 2
    /// owner server, not a futex).
    ///
    /// **That loop must keep its `NoData` arm** — this doc had lost it until
    /// 2026-08-29. `Self::span` raises [`LookupError::NoData`] for an edge that
    /// has never published, i.e. *"the publisher has not started yet"*, so a
    /// loop written with `?` on `span` errors on its first iteration at exactly
    /// the moment a consumer is waiting for a publisher to come up; it must
    /// sleep one period and re-check, as for a shortfall.
    ///
    /// A period is `1e9 / (mhz / 1000)` ns, and it is a **prediction, not a poll
    /// interval**: one or two wakes, where a naive 1 ms poll against a 10 Hz
    /// edge wakes a hundred times. A `min`, because a plan answers only when
    /// *every* edge has reached the stamp.
    ///
    /// **`0` is *undeclared*, and is skipped rather than read as 0 Hz** — as a
    /// rate it is the minimum of every set and yields an infinite period, so one
    /// undeclared edge would silently disable the wait for the whole path (the
    /// distinction `docs/PHASE5.md` §6's `TFT007` amendment also needs). `None`
    /// is a real third answer; a caller falls back to a conservative period and
    /// **should say so once at startup**, because a mysteriously slow wait and a
    /// mysteriously busy one look identical from outside. A declared rate may
    /// also be an *observed* one — `tf_tree topology --discover` writes a
    /// measured rate here — which costs one extra wake, tolerable here and not
    /// for `TFT007`.
    ///
    /// It calls `check_generation` as [`Self::span`] does, so a stale plan or a
    /// fork-poisoned guard is reported rather than spun on until the deadline.
    /// It does **not** return [`LookupError::NoData`]: a declaration belongs to
    /// the topology, not the stream, and the caller asking how long to sleep is
    /// asking before the data exists.
    ///
    /// # Errors
    ///
    /// [`LookupError::TopologyChanged`], [`LookupError::ChildDetached`], or
    /// [`LookupError::UnknownEdge`].
    //
    // `0018`'s *Decision* section prints `-> Option<u32>`, which cannot carry
    // the errors its own implementation plan (step 1) requires. `Result` is the
    // self-consistent reading and matches `span`.
    pub fn slowest_nominal_rate_mhz(&self, g: &Guard) -> Result<Option<u32>, LookupError> {
        self.check_generation(g)?;
        let mut slowest: Option<u32> = None;
        for step in self.steps() {
            let Step::Dyn { edge, .. } = step else {
                // No publisher, so no period.
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
    /// Monotone non-decreasing `stamps` let each dynamic edge resume its bracket
    /// search by galloping — `O(1)` amortized instead of `O(log n)`; other input
    /// falls back to an independent search per stamp.
    ///
    /// # Errors
    ///
    /// As [`Self::at`], plus [`LookupError::BufferTooSmall`] when
    /// `out.len() < stamps.len()`, checked before anything is written. **Not an
    /// `assert!`**, which this was until 2026-08-29: `assert!` is unconditional,
    /// so a short buffer unwound in release or aborted under the
    /// `panic = "abort"` profile an embedder picks for a control loop, and
    /// `clippy::panic` does not lint it (`docs/API.md` R5).
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

        // Hoisted: loop-invariant, and an O(plan length) scan (see `note`).
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
    /// Over [`Self::at_many`]: an `Iso3` buffer does not alias the layout a
    /// consumer wants (4x4 `f64`, 3x4 `f32`), so writing through one costs an
    /// intermediate buffer and a second pass, where this writes once in place
    /// and allocates nothing. Since `0042` the `Quat` layout shares `Iso3`'s
    /// bytes exactly ([`crate::layout`]). `out` is a flat `f64` slice of at
    /// least `stamps.len() * layout.elems()`; use [`Self::at_many_into_f32`] for
    /// [`Layout::Affine32`]. [`Layout::QuatTwist`] folds through
    /// [`Self::at_with_derivatives`]'s exact path, so its thirteen `f64` per
    /// stamp are the scalar call's bits, refusals included.
    ///
    /// **`stamps` is raw nanoseconds, with the domain as the type parameter.**
    /// `Stamp<D>` is not `repr(transparent)`, so a caller holding `&[i64]` —
    /// every FFI caller, the NumPy path in particular — would have to allocate
    /// and copy, which is the buffer this removes. The domain is still checked,
    /// once per call.
    ///
    /// # Errors
    ///
    /// [`LookupError::BufferTooSmall`] or [`LookupError::WrongElementType`],
    /// both checked **before a single element is written** (`docs/PHASE3.md`
    /// §5.3: a half-written output is worse than none, because it looks like
    /// data). For [`Layout::QuatTwist`], additionally
    /// [`LookupError::DerivativesUnavailable`] and [`LookupError::NoSegment`],
    /// as [`Self::at_with_derivatives`] returns them.
    ///
    /// **Only those two checks are all-or-nothing.** Every other error is a
    /// property of a *stamp*, so it can fire after `k` rows are written and stop
    /// there with nothing marking the boundary — which is why the element index
    /// is worth recovering from the error. `DerivativesUnavailable` is a
    /// property of an *edge* and always fires at element 0;
    /// [`LookupError::NoSegment`] on the same layout does not. Otherwise as
    /// [`Self::at`].
    pub fn at_many_into<D: Domain>(
        &self,
        g: &Guard,
        stamps: &[i64],
        layout: Layout,
        out: &mut [f64],
    ) -> Result<(), LookupError> {
        self.at_many_into_tagged(g, stamps, D::TAG, layout, out)
    }

    /// [`Self::at_many_into`], with the query's domain as a runtime tag — the
    /// binding surface, for [`Self::at_tagged`]'s reason. Same check, same
    /// errors; Rust callers want [`Self::at_many_into`].
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
            // Needs the twist, so it folds elsewhere; see
            // [`Self::fold_batch_with_twist`] for why it is a sibling.
            Layout::QuatTwist => self.fold_batch_with_twist(g, stamps, n, out),
            // Unreachable: rejected by `is_f32` above. An error rather than a
            // panic the workspace lints forbid, and a future f32 layout added
            // without updating the check gets it instead of a wrong `f64` write.
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

    /// [`Self::at_many_into_f32`], with the query's domain as a runtime tag — the
    /// binding surface, for [`Self::at_tagged`]'s reason. Same check, same
    /// errors; Rust callers want [`Self::at_many_into_f32`].
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
    /// Generic over the element type so the `f64` and `f32` paths share one copy
    /// of the cursor logic, where the galloping search and seqlock retry live.
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
        // `chunks_exact_mut` zipped against `stamps`, so the walk is bounded by
        // the batch and an over-long buffer is untouched past the end. **Not for
        // speed**: the "one bounds check per element" argument was measured and
        // is wrong — at 1024 samples the change is within noise, because LLVM
        // elides the check and ~245 us of interpolation dwarfs it either way.
        // Hoisted: loop-invariant, and an O(plan length) scan (see `note`).
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
    /// A sibling because `fold_batch` is generic over the *emitter*, not the
    /// fold: serving both would need a no-op closure for the pose layouts or a
    /// branch inside the loop, and both put work on the one path here whose
    /// per-element cost is measured in nanoseconds ([`crate::layout`]'s rule).
    /// Ascending stamps ride a resumable cursor per step, so the search is
    /// `O(1)` amortized — this is `docs/API.md` §3.3's `n = 1024` batch, and was
    /// the one layout without a cursor. The cursor is only a *hint*, so the two
    /// branches are asserted bit-identical rather than close, and it calls the
    /// fold [`Self::at_with_derivatives`] calls: bit-identity is what makes this
    /// a *layout* and not a second implementation of derivatives, whose first
    /// symptom would be two bindings disagreeing about a velocity.
    #[inline]
    fn fold_batch_with_twist(
        &self,
        g: &Guard,
        stamps: &[i64],
        elems: usize,
        out: &mut [f64],
    ) -> Result<(), LookupError> {
        // Hoisted, as in `fold_batch`: loop-invariant O(plan length) scan.
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
    /// Recursive bisection, bounded by [`MAX_ADAPTIVE_DEPTH`] and [`MAX_KNOTS`],
    /// with all output in the caller's `scratch` and no global allocation.
    /// Returns parallel slices `(stamps, poses)`, strictly increasing in stamp,
    /// for the consumer to LERP between wherever its points live. Replaces the
    /// abandoned deskew helper.
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

    /// [`Self::at_adaptive`], with the query's domain as a runtime tag, for
    /// [`Self::at_tagged`]'s reason.
    ///
    /// **`D` here is storage and `domain` is the query** — the one shape where
    /// the two cannot be collapsed. `D` fixes the element type of `scratch` and
    /// of the returned stamps and is read by nothing in the fold; `domain` is
    /// what is checked. A binding passes any `D` it can name plus the real tag
    /// as data. **A Rust caller wants [`Self::at_adaptive`]**: a `D` that
    /// disagrees with `domain` is legal, changes no result, and yields a stamp
    /// slice whose phantom means nothing.
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
        // Counted **once per call, not once per fold**: `subdivide` evaluates
        // the plan up to `MAX_KNOTS` times for one caller-visible lookup, and
        // crediting each bisection would make this entry point dominate
        // `lookups_ok`, which means "lookups" everywhere else in `doctor`.
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
    // Splitting needs depth budget, a non-adjacent segment, and knot room; the
    // knot-room check reserves `MAX_ADAPTIVE_DEPTH` headroom because up to
    // `depth` committed ancestors each emit one knot as the DFS unwinds.
    //
    // **The width is taken in `u64`, and that is not a micro-optimisation.** The
    // difference does not fit an `i64` once the span exceeds `i64::MAX`, and
    // `at_adaptive(i64::MIN, i64::MAX)` is what `span() == Ok(None)` invites a
    // caller to ask of an all-static plan. Signed, it panicked in a checked
    // build and wrapped in release — the worse half, because the negative
    // difference fails `> 1`, so recursion stops and a path that was never
    // straight returns a two-knot straight line. Measured on a dynamic plan
    // spanning ±2^62: two knots, endpoints -0.4989 and -0.9953, true midpoint
    // -17.2030, tolerance 1e-6, no error and no panic. Through `u64` the
    // difference is exact for every ordered `i64` pair, and `wrapping_sub` is
    // the *identity* on it — `u64::MAX` for `(i64::MIN, i64::MAX)`.
    let width = (b_s as u64).wrapping_sub(a_s as u64);
    let can_split = depth < MAX_ADAPTIVE_DEPTH
        && width > 1
        && scratch.stamps.len() + (MAX_ADAPTIVE_DEPTH as usize) + 1 < MAX_KNOTS;
    if can_split {
        // `width / 2 <= 2^63 - 1` fits an `i64`, and so does the midpoint of two
        // `i64`s, so the add cannot overflow; `wrapping_add` documents that.
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
/// `#[non_exhaustive]`, so build it with [`ErrBound::new`]. A tolerance is the
/// shape that grows (a time bound, a per-axis split), the attribute is free
/// before a published tag and a major bump after it, and a new field arrives
/// with a default the constructor picks — the answer a struct literal cannot
/// give. `tf_tree::EdgeCfg` is the same pattern.
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
/// maximum knot set. Allocated once by the caller, so `at_adaptive` itself never
/// allocates.
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

/// A batch-evaluation handle: it borrows the arena and pins the topology
/// generation once, so a run of lookups validates against a single snapshot.
///
/// A mismatch against a plan's compiled generation is
/// [`LookupError::TopologyChanged`]. Make one guard per batch.
pub struct Guard<'a> {
    view: ArenaView<'a>,
    /// The pinned topology generation, or [`DETACHED`] for a guard built by
    /// [`Guard::detached`].
    generation: u64,
    /// Successful lookups so far, flushed to the arena on drop.
    ///
    /// **A plain `Cell<u32>`, not an atomic** (`docs/PHASE5.md` §5.4): `Guard`
    /// is `!Sync` by construction and per batch on one thread, so nothing else
    /// can observe it. A relaxed `fetch_add` per lookup would be a *contended*
    /// cost — sixteen readers on one edge serializing on that cache line — where
    /// accumulating turns N atomics into one. `u32` because a guard spanning
    /// four billion lookups is one nobody holds, and the flush saturates.
    #[cfg(feature = "counters")]
    ok: core::cell::Cell<u32>,
    /// Which edge's counters to credit, when every lookup in the batch went
    /// through one plan.
    ///
    /// `None` before the first lookup and **also** once two different edges have
    /// been seen: crediting whichever came last would be worse than crediting
    /// nobody, so a multi-edge plan credits the participant total only.
    #[cfg(feature = "counters")]
    ok_edge: core::cell::Cell<Option<EdgeId>>,
    /// Per-step bracket-search hints, packed `(edge << 32) | index`, so a
    /// scalar lookup resumes beside the previous answer instead of restarting at
    /// the window midpoint.
    ///
    /// Worth ~9% of a lookup. `docs/design/fast-path.md` §12 measured the
    /// bracket search at **34% of a dynamic step**, and its sweep showed the
    /// cost is not the probe count but whether the probed *stamp array* fits L1
    /// — flat to capacity 1024, stepping hard at 32 KiB of stamps, this host's
    /// L1d. A cursor does not shrink that array; it makes the access **local**.
    /// On a monotone sweep (`step_cost`): 54.58 -> 40.71 ns/sample at capacity
    /// 4096, 58.54 -> 41.37 at 16384 (a 1 kHz edge with 10 s of history), and
    /// the cliff nearly flat — a fresh search costs +7% over that step, the
    /// cursor +1.6%.
    ///
    /// **It cannot affect a result**:
    /// [`SampleRing::sample_from`](crate::buffer::SampleRing::sample_from) is
    /// tested to return exactly what
    /// [`SampleRing::sample`](crate::buffer::SampleRing::sample) returns for the
    /// same `t`, so a stale, wrong or absent cursor is a bad *hint*, never a
    /// wrong answer. That is what makes it safe in a cache nothing invalidates,
    /// and why the index may be packed into 32 bits.
    ///
    /// **The truncation cost, not correctness, was the trap.** An index past
    /// `u32::MAX` (49.7 days of unbroken 1 kHz publishing) truncates below
    /// `lo_logical` for every later query, so
    /// [`SampleRing::bracket_from`](crate::buffer::SampleRing)'s clamp pinned it
    /// to the *oldest* retained sample every call — a resumed gallop
    /// permanently reverted to a walk from the far end, worse than the midpoint
    /// restart it beats. A cliff, not a decay, and invisible to tests because a
    /// bad hint still yields the right answer. `sample::rebase_hint` lifts the
    /// truncated value back exactly, the window being narrower than 2^32.
    ///
    /// A `Cell` on the `Guard` for `ok`'s reason (`docs/PHASE5.md` §5.4). One
    /// array and one packed word, because the cost of this cache is
    /// **initialising it** — every cell is written per guard, the whole of
    /// `Guard::new`'s 1.4 -> 8.5 ns, and packing halved the stores (an
    /// inline-`const` initialiser moved nothing). The edge half is
    /// self-invalidation: one `Guard` evaluates several plans, so an untagged
    /// hint would send the gallop somewhere arbitrary — correct, but potentially
    /// costlier than a plain search.
    cursor: [core::cell::Cell<u64>; MAX_DEPTH],
    /// `(generation at creation, how to read it now)`, for the fork check.
    ///
    /// **The flush writes into the arena from a destructor**, and a shared
    /// mapping is `MADV_DONTFORK`, so in a `fork` child that arena is a hole in
    /// the address space — the trap `EdgeWriter::drop` already guards against
    /// (`docs/decisions/0005` step 9); review reproduced the segfault. A
    /// function pointer because `tf_tree_core` is `no_std` and knows nothing
    /// about processes; the facade supplies `tf_tree_ipc::fork::generation`, and
    /// a heap arena passes `None`.
    #[cfg(feature = "counters")]
    fork: Option<(u64, fn() -> u64)>,
}

/// The generation a [`Guard::detached`] guard carries.
///
/// **Not a real generation, and unreachable as one**: it starts at 0 and is
/// bumped once per mutation. Encoding the poison in a field that already exists
/// costs no bytes and no load — [`Plan::check_generation`] makes that comparison
/// anyway — where an `Option<LookupError>` field cost 32 bytes on a struct built
/// once per `at()`. (Do not quote a `Guard` size here: the "48 bytes" this used
/// to claim was stale when `0034` measured 208, and it is 336 at
/// `MAX_DEPTH = 32`.)
const DETACHED: u64 = u64::MAX;

/// Which [`EdgeCounters`] field a lookup error belongs in.
///
/// An enum rather than a closure so `counter_of` stays a pure classification
/// that borrows no arena: the caller does the lookup.
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
    /// `#[repr(C)]` records in a cross-process layout, and a trait making them
    /// interchangeable would hide a field added to one and not the other.
    /// `counters.rs` pins their shared prefix for the same reason.
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
/// **Only errors that name an edge are counted** — most of them, by D11. The
/// rest (`UnknownFrame`, `Disconnected`, `TopologyChanged`) are properties of
/// the *query*, and filing them under an edge would send an operator to inspect
/// a publisher that is working correctly.
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
            // Split, because the two mean opposite things: past the newest
            // stamp usually means a publisher stopped, before the oldest means
            // a consumer is behind or the ring is too short. `TFT010` and
            // `TFT011` key off this.
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
/// A guard spanning 1000 lookups pays one `fetch_add`, so the contention a
/// per-lookup atomic would create on a hot edge does not arise. That is the
/// whole reason this destructor exists.
#[cfg(feature = "counters")]
impl Drop for Guard<'_> {
    fn drop(&mut self) {
        let n = self.ok.get();
        // Same read-only guard as `note_err`, and this is the path that
        // actually faulted: a consumer's guard drops at the end of every batch.
        if n == 0 || !self.view.is_writable() {
            return;
        }
        // Fork guard: in a child the `MADV_DONTFORK` arena is a hole in the
        // address space and this write faults. A destructor is the worst place
        // to discover that, because it runs whether or not the child called
        // anything.
        if let Some((born, read)) = self.fork {
            if read() != born {
                return;
            }
        }
        use crate::sync::Ordering::Relaxed;
        // Credited to an edge only when the whole batch went through one: a
        // multi-edge plan credits nothing here rather than whichever edge it
        // touched last, which no operator could interpret.
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
    /// Always a stable generation: A1 removed the odd "write in progress" state,
    /// which used to make every [`Plan::at`] against a guard pinned mid-mutation
    /// fail with [`LookupError::TopologyChanged`] for no reason.
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
    /// One non-atomic increment, compiled away entirely without the `counters`
    /// feature: §5.5 wants "off" to mean *no code*, not a runtime branch.
    #[inline]
    pub(crate) fn note_ok(&self, edge: EdgeId) {
        #[cfg(feature = "counters")]
        {
            self.ok.set(self.ok.get().saturating_add(1));
            // **`EdgeId(0)` is the "no edge" sentinel and must not latch.**
            // `first_dynamic_edge` returns it for a multi-edge plan; without
            // this, `Some(0)` latched and the flush went into the reserved
            // edge-0 counter record, which `edge_counters` returns because it
            // bounds only against `max_edges`. A consumer iterating from 0 then
            // sees a phantom count, and — worse — the common `map` to
            // `base_link` query *is* multi-edge, so every reader in every
            // process funnelled its flush into one 64-byte line: the false
            // sharing `EdgeCounters`' padding exists to prevent.
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
    /// Writes straight through, unlike [`Self::note_ok`]: buffering would mean a
    /// process that dies mid-fault takes the evidence with it, which is the case
    /// §5.3 exists for.
    #[inline]
    pub(crate) fn note_err(&self, err: &LookupError) {
        #[cfg(feature = "counters")]
        {
            use crate::sync::Ordering::Relaxed;
            // **A read-only view must not write.** A consumer maps the arena
            // read-only (D18), so this faulted with SIGSEGV and killed a
            // read-only child in the multiprocess suite before the check
            // existed. §5 does not discuss it; a read-only participant keeps no
            // counters, because losing a diagnostic beats refusing to run.
            if !self.view.is_writable() {
                return;
            }
            let Some((edge, field)) = counter_of(err) else {
                return;
            };
            // The failure's own stamp, so "when" is in the arena's time domain
            // rather than a wall clock `tf_tree_core` has no access to anyway.
            // Zero reads as "never", which beats inventing a time.
            let now = match *err {
                LookupError::Extrapolation { requested, .. } => requested,
                _ => 0,
            };
            // **Both halves, not just the edge.** §5.2: per-participant
            // counters are what answer "which consumer is failing" rather than
            // "failures exist". The first version wrote only the edge side,
            // leaving the participant surface and `last_err_edge` dead.
            if let Some(slot) = self.view.interning_identity() {
                if let Some(p) = self.view.participant_counters(slot) {
                    field.bump_participant(p);
                    p.last_err_edge.store(edge.get(), Relaxed);
                    p.last_err_nanos.store(now, Relaxed);
                }
            }
            if let Some(c) = self.view.edge_counters(edge) {
                field.bump(c);
                // Declared and never written once, so "when did this last fail"
                // always read "never" — the field that turns a count into an
                // incident.
                c.last_err_nanos.store(now, Relaxed);
                if let LookupError::Extrapolation {
                    requested,
                    oldest,
                    newest,
                    ..
                } = *err
                {
                    // A high-water mark, not a total: "4 seconds past the end
                    // once" is actionable where "past the end 900 times" is not.
                    // `TFT011` reads it against the ring's span.
                    let gap = if requested > newest {
                        requested.saturating_sub(newest)
                    } else {
                        oldest.saturating_sub(requested)
                    };
                    // **`fetch_max`, not load/compare/store.** Several consumers
                    // write this concurrently, and the failure is worse than a
                    // lost update: the mark can *regress* (two threads load 0,
                    // one stores 10 s, the other then stores 500 ms), which
                    // makes `TFT011` call a ring that lapped by ten seconds
                    // fine — the check silently inverts. Reproduced by review at
                    // 154 regressions in 200 000 trials on x86-64, the
                    // *friendly* memory model. Relaxed is still the right
                    // ordering; atomicity was what was missing.
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
    /// For a facade that knows the arena is unreachable (the shared mapping went
    /// away under a `fork()`) but cannot say so: `Tree::guard` is infallible in
    /// a `let g = tree.guard();` idiom used by dozens of callers, and staying
    /// silent means the next read dereferences unmapped memory. A constructor,
    /// not a `poison` setter, because [`Self::new`] reads the topology
    /// **immediately**; `view` must still be over a *valid* arena, since
    /// [`Self::view`] hands it out. **No `poisoned(view, err)` taking an
    /// arbitrary error** — one was written and replaced, because carrying it
    /// meant a 32-byte `Option<LookupError>` field on a struct built once per
    /// `at()` call. See `DETACHED`.
    #[must_use]
    pub fn detached(view: ArenaView<'a>) -> Guard<'a> {
        Guard {
            view,
            generation: DETACHED,
            // Never counts a success and never reaches a search, but the fields
            // must exist; zero is what makes the destructor a no-op.
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
    /// `#[inline]`, like its two siblings below: they are the last non-generic
    /// links a downstream crate must inline through to reach the generic
    /// `SampleRing::sample`, whose MIR it already has. [`Plan::at`]'s table
    /// measures it — unmarked, the caller still emits one cross-crate call to
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
    /// The scalar fold's entry point, differing only in where the bracket search
    /// starts. See [`Guard::cursor`] for the measurement, for why a wrong cursor
    /// cannot produce a wrong result, and for why the cursor is trusted only
    /// when the tag says this same edge wrote it.
    #[inline]
    pub(crate) fn sample_hinted(
        &self,
        k: usize,
        edge: EdgeId,
        t: i64,
        policy: ExtrapPolicy,
    ) -> Result<Iso3, LookupError> {
        // Always in range (a plan is bounded by MAX_DEPTH); the guard keeps the
        // access provably safe rather than relying on that.
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
        // Success only: a failed search abandons `cursor` at a position no
        // successful search produced, which would poison the next query's hint.
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
    /// The refusal is checked here too, before the ring is touched: deciding it
    /// per sampler rather than per caller is what keeps the batch layout's
    /// refusal identical to the scalar call's.
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
    /// One [`SampleRing`](crate::buffer::SampleRing) handle but two independent
    /// `head` loads ([`SampleRing::oldest_stamp`](crate::buffer::SampleRing::oldest_stamp),
    /// [`SampleRing::newest_stamp`](crate::buffer::SampleRing::newest_stamp)), so
    /// a concurrent `push` between them can widen the pair past either real
    /// window — [`Plan::latest`]'s staleness, unfixable here without a seqlock
    /// over the whole ring, and documented for callers on [`Plan::span`]. On a
    /// frozen arena (§4.2's case) nothing pushes and the pair is exact.
    pub(crate) fn window(&self, edge: EdgeId) -> Result<(i64, i64), LookupError> {
        let ring = self
            .view
            .ring(edge)
            .ok_or(LookupError::UnknownEdge { edge })?;
        match (ring.oldest_stamp(), ring.newest_stamp()) {
            (Some(oldest), Some(newest)) => Ok((oldest, newest)),
            // `NoData`, never an empty interval: "nobody has published yet" and
            // "the windows do not overlap" call for different actions.
            _ => Err(LookupError::NoData { edge }),
        }
    }

    /// An edge's declared nominal publish rate, in milli-hertz, `0` meaning
    /// *undeclared*.
    ///
    /// The sentinel is passed through, not folded into an `Option`: two callers
    /// disagree about what "undeclared" means (the wait skips it,
    /// `docs/PHASE5.md` §6's `TFT007` reports it as a skip reason), so the
    /// interpretation does not belong on the read. Reads the edge *record*, not
    /// the ring — a rate is declared at construction — which is why this cannot
    /// return [`LookupError::NoData`] the way [`Self::window`] must.
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
/// seqlock, retrying so every read comes from one generation, which the plan
/// records. `edge_meta` supplies each edge's kind/domain/static-pose for
/// constant folding, and `None` for an edge this arena has no record for.
///
/// # Errors
///
/// * [`LookupError::Disconnected`], [`LookupError::FrameOutOfRange`],
///   [`LookupError::MissingEdge`].
/// * [`LookupError::TreeTooDeep`] — more than [`MAX_PATH_EDGES`] raw edges, or a
///   fold past [`MAX_DEPTH`] steps; the reported `depth` says which.
/// * [`LookupError::UnknownEdge`] / [`LookupError::MixedTimeDomains`] — from the
///   constant fold, and **before** the compiled-length refusal, so a defect on a
///   too-long path is named rather than hidden behind its length.
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

    // Retry the whole walk if a mutation lands between reads
    // (`docs/PHASE1.md` §5.2 reader protocol). Since A1 every published
    // generation is stable, so there is no parity to check and nothing to wait
    // for; the retry only discards a walk that straddled a mutation.
    'walk: loop {
        let start_gen = topo.generation();

        // Read (parent, depth, edge_of_child) for `f`, or restart.
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

        // Edges up from target (emit inverted, in order) and from source (emit
        // forward, reversed). Bounded by MAX_PATH_EDGES: 512 bytes of stack.
        let mut t_edges = [0u32; MAX_PATH_EDGES];
        let mut nt = 0usize;
        let mut s_edges = [0u32; MAX_PATH_EDGES];
        let mut ns = 0usize;

        // Record the edge from `$frame` up to its parent. Edge id `0` is the
        // "no edge" sentinel (`set_parent` accepts it when only the parent link
        // matters) but is *also* a real edge-table slot, so it must never reach
        // a `Step::Dyn` — that would silently sample an unrelated edge's ring.
        //
        // The raw bound is checked on `nt + ns`, not per side, so
        // `MAX_PATH_EDGES` means "edges walked": the per-side spelling let a
        // Y-shaped path walk twice the bound. Checked *before* the sentinel, so
        // a defect still wins over depth by position on the path. Unlike
        // `fold_into` the walk cannot count past the bound — it stops for want
        // of buffer, and a cyclic parent chain would not terminate — so it
        // reports `MAX_PATH_EDGES + 1`, the one value above the bound this field
        // takes.
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
                // Out of parents without meeting: different trees.
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
            // Depths are not compared past the lockstep phase.
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

        // `fold_into` takes the edge-id slices directly. The deleted
        // `[Step; MAX_DEPTH]` intermediate held nothing they do not (an edge id
        // plus `inverted`, true iff it came from `t_edges`), and deleting it is
        // what lets the raw bound be generous and what pays for `MAX_DEPTH`'s
        // move to 32. Folded **into the plan about to be returned**, not a
        // temporary handed back by value (#264); a refusal drops `plan`
        // half-written, unobservably, because it is a local and this returns
        // `Err`.
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
/// Writes the folded steps into `plan`, plus `len`, `domain`, `dyn_count` and
/// `first_dyn`, which are functions of them.
///
/// The `Plan` is borrowed because returning `([Step; MAX_DEPTH], usize, u8)` put
/// a 4096-byte `memcpy` between this stack and the caller's, in the shipped
/// disassembly (#264, and [`Plan::identity`]). `dyn_count`/`first_dyn` are
/// stored rather than scanned because `Plan::at` called both — through
/// `check_domain` → `has_dynamic` and for `note`'s attribution — each an
/// O(`len`) walk of a 4 KiB step array (2 KiB when measured, at
/// `MAX_DEPTH = 16`); deriving them here is free, where a second pass (the
/// short-lived `Plan::finish`) put that read straight back on the path #264
/// exists to shorten. `Plan` is a value type, **not an arena structure**, so
/// they cost no format version and no layout hash. Only writer;
/// `plan_derived_fields_match_a_fresh_scan` pins it.
///
/// **`plan` may be left partially written on `Err`** — a fact with nowhere to
/// go, the only caller borrowing a local it then drops. Every entry is still a
/// valid `Step` ([`Plan::identity`] initialises the array and this only
/// overwrites), and the four fields are published in one block past every `?`.
///
/// Takes the walk's raw buffers: `t_edges` in walk order emitted inverted, then
/// `s_edges` **reversed** emitted forward. The reversal is load-bearing — it
/// makes the composition associate `((s[n-1] * s[n-2]) * …)`, and `Iso3`
/// composition is not associative under rounding, so meeting `s[0]` first gives
/// a different bit pattern every tolerance-based test would accept.
///
/// # Running past the end of the output array
///
/// Input may be [`MAX_PATH_EDGES`] long against a `[Step; MAX_DEPTH]` output,
/// and the loop does **not** stop when it overflows: it skips the write, keeps
/// incrementing `n`, and resolves every remaining edge. So `n` is the *true*
/// compiled length [`LookupError::TreeTooDeep`] reports, and
/// [`LookupError::UnknownEdge`]/[`LookupError::MixedTimeDomains`] are still
/// raised past the bound. Returning early is cheaper and was measured (a refused
/// 64-edge dynamic chain: 994 ns against 1778 ns, two controls doing identical
/// work at +3.2 / −6.2 ns, i.e. noise) but makes a too-long path report its
/// length instead of its defect, inverting the precedence `0034` promises; only
/// refused paths pay it. The collapse arm therefore reads a tracked
/// `last_static`, not `out[n - 1]`, which would not work past the array end.
///
/// # Errors
///
/// * [`LookupError::UnknownEdge`] — a step names an edge with no record here.
/// * [`LookupError::MixedTimeDomains`] — the dynamic edges do not share one time
///   domain, so no single query stamp addresses them all.
/// * [`LookupError::TreeTooDeep`] — the exact folded step count.
fn fold_into(
    plan: &mut Plan,
    t_edges: &[u32],
    s_edges: &[u32],
    edge_meta: &impl Fn(EdgeId) -> Option<EdgeMeta>,
) -> Result<(), LookupError> {
    let out = &mut plan.steps;
    let mut n = 0usize;
    // Derived in the append arm, which already holds the step and its
    // discriminant, rather than by a second pass. Equivalent by construction:
    // the collapse arm only rewrites a `Static` in place, so it can neither add
    // nor remove a `Dyn`, and on any path returning `Ok` every append had
    // `n < MAX_DEPTH`. `plan_derived_fields_match_a_fresh_scan` is the pin.
    let mut dyn_count = 0u8;
    let mut first_dyn = EdgeId(0);
    // Whether `out[n - 1]` is a `Static` — tracked rather than read back,
    // because `n` may be past the array. `false` while `n == 0`.
    let mut last_static = false;
    // `None` until the first dynamic step fixes the domain; every later one must
    // agree. Taking the *last* one, as this used to, let a plan spanning a
    // system-clock and a sensor-clock edge pass `check_domain` and then sample
    // one with the wrong clock — the silent misread D9 exists to prevent.
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
                // Dynamic, or a tombstone treated as dynamic — sampling it
                // surfaces the real error.
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
        // Both arms guard on the array bound: past `MAX_DEPTH` the value has
        // nowhere to live and this call will refuse, but counting must continue.
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
