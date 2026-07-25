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

use tf_tree_math::{log_so3, Interp, Iso3, LerpSlerp, ScLerp};

use crate::arena_view::ArenaView;
use crate::edge::EdgeKind;
use crate::error::{EdgeId, FrameId, LookupError};
use crate::sample::ExtrapPolicy;
use crate::sync::spin;
use crate::topology::TopologyView;
use crate::MAX_DEPTH;

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
// Steps and the compiled plan
// ---------------------------------------------------------------------------

/// One step of a compiled plan.
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
}

impl Plan {
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
        self.steps().iter().any(|s| matches!(s, Step::Dyn { .. }))
    }

    #[inline]
    fn check_generation(&self, g: &Guard) -> Result<(), LookupError> {
        let cur = g.generation();
        if cur == self.generation {
            Ok(())
        } else {
            Err(LookupError::TopologyChanged {
                plan: self.generation,
                current: cur,
            })
        }
    }

    #[inline]
    fn check_domain<D: Domain>(&self) -> Result<(), LookupError> {
        if self.has_dynamic() && D::TAG != self.domain {
            return Err(LookupError::TimeDomainMismatch {
                expected: self.domain,
                got: D::TAG,
            });
        }
        Ok(())
    }

    /// Evaluate the plan at nanosecond stamp `t`, sampling every dynamic edge at
    /// `t`. Assumes the caller has already validated generation and domain.
    fn fold_at(&self, g: &Guard, t: i64) -> Result<Iso3, LookupError> {
        let mut acc = Iso3::IDENTITY;
        for step in self.steps() {
            acc = match step {
                Step::Static(m) => acc * *m,
                Step::Dyn { edge, inverted } => {
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

    /// Like [`Self::fold_at`] but each dynamic step gallops from its own resumable
    /// cursor (`cursors[step_index]`), for a monotone stamp sweep.
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
    pub fn at<D: Domain>(&self, g: &Guard, t: Stamp<D>) -> Result<Iso3, LookupError> {
        self.check_generation(g)?;
        self.check_domain::<D>()?;
        self.fold_at(g, t.nanos())
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
        let mut common = i64::MAX;
        let mut any = false;
        for step in self.steps() {
            if let Step::Dyn { edge, .. } = step {
                common = common.min(g.newest_stamp(*edge)?);
                any = true;
            }
        }
        if !any {
            return Ok(self.static_only());
        }
        self.fold_at(g, common)
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
    /// As [`Self::at`], plus a debug-time length check that `out.len() >=
    /// stamps.len()` (extra `out` slots are left untouched).
    ///
    /// # Panics
    ///
    /// If `out.len() < stamps.len()` (there is nowhere to write a result).
    pub fn at_many<D: Domain>(
        &self,
        g: &Guard,
        stamps: &[Stamp<D>],
        out: &mut [Iso3],
    ) -> Result<(), LookupError> {
        assert!(out.len() >= stamps.len(), "out too short for stamps");
        self.check_generation(g)?;
        self.check_domain::<D>()?;

        let monotone = stamps.windows(2).all(|w| w[0].nanos() <= w[1].nanos());
        if monotone {
            let mut cursors = [0u64; MAX_DEPTH];
            for (s, o) in stamps.iter().zip(out.iter_mut()) {
                *o = self.fold_at_cursors(g, s.nanos(), &mut cursors)?;
            }
        } else {
            for (s, o) in stamps.iter().zip(out.iter_mut()) {
                *o = self.fold_at(g, s.nanos())?;
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
        self.check_generation(g)?;
        self.check_domain::<D>()?;
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
    let can_split = depth < MAX_ADAPTIVE_DEPTH
        && (b_s - a_s) > 1
        && scratch.stamps.len() + (MAX_ADAPTIVE_DEPTH as usize) + 1 < MAX_KNOTS;
    if can_split {
        let m_s = a_s + (b_s - a_s) / 2;
        let m_p = plan.fold_at(g, m_s)?;
        let s = (m_s - a_s) as f64 / (b_s - a_s) as f64;
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
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ErrBound {
    /// Maximum allowed rotation error, in radians.
    pub rot_rad: f64,
    /// Maximum allowed translation error, in the pose's length units.
    pub trans: f64,
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
    generation: u64,
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
        Guard { view, generation }
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

    /// Galloping variant of [`Self::sample`] resuming from `cursor`.
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
/// * [`LookupError::TreeTooDeep`] — the combined path exceeds [`MAX_DEPTH`].
/// * [`LookupError::FrameOutOfRange`] — a frame id is out of range for `topo`.
/// * [`LookupError::MissingEdge`] — a parent link on the path records no edge.
/// * [`LookupError::UnknownEdge`] / [`LookupError::MixedTimeDomains`] — as
///   [`fold`].
pub fn compile(
    topo: &TopologyView,
    edge_meta: impl Fn(EdgeId) -> Option<EdgeMeta>,
    target: FrameId,
    source: FrameId,
) -> Result<Plan, LookupError> {
    if target == source {
        // Identity plan; still stamp it with a consistent generation.
        let generation = topo.stable_generation();
        return Ok(Plan {
            generation,
            steps: [Step::Static(Iso3::IDENTITY); MAX_DEPTH],
            len: 0,
            domain: 0,
        });
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

        // Record the edge on the link from `$frame` up to its parent. Edge id `0`
        // is the "no edge" sentinel (`set_parent` accepts it when only the parent
        // link matters) but is *also* a real edge-table slot, so it must never be
        // emitted as a `Step::Dyn` — that would silently sample an unrelated
        // edge's ring.
        macro_rules! push_edge {
            ($buf:expr, $n:expr, $edge:expr, $frame:expr) => {{
                if $edge == 0 {
                    return Err(LookupError::MissingEdge { child: $frame });
                }
                $buf[$n] = $edge;
                $n += 1;
            }};
        }

        let mut a = target;
        let mut b = source;
        let (mut pa, mut da, mut ea) = read!(a);
        let (mut pb, mut db, mut eb) = read!(b);

        // Edges collected walking up from target (emit inverted, in order) and from
        // source (emit forward, in reverse). Bounded by MAX_DEPTH.
        let mut t_edges = [0u32; MAX_DEPTH];
        let mut nt = 0usize;
        let mut s_edges = [0u32; MAX_DEPTH];
        let mut ns = 0usize;

        // Bring the deeper frame up until depths match.
        while da > db {
            if nt >= MAX_DEPTH {
                return Err(LookupError::TreeTooDeep {
                    depth: (nt + ns) as u16,
                });
            }
            push_edge!(t_edges, nt, ea, a);
            a = frame_or_disconnect(pa, target, source, a)?;
            let (p, d, e) = read!(a);
            pa = p;
            da = d;
            ea = e;
        }
        while db > da {
            if ns >= MAX_DEPTH {
                return Err(LookupError::TreeTooDeep {
                    depth: (nt + ns) as u16,
                });
            }
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
            if nt >= MAX_DEPTH || ns >= MAX_DEPTH {
                return Err(LookupError::TreeTooDeep {
                    depth: (nt + ns) as u16,
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

        if nt + ns > MAX_DEPTH {
            return Err(LookupError::TreeTooDeep {
                depth: (nt + ns) as u16,
            });
        }

        // Build the raw step list: target side inverted in order, source side
        // forward in reverse.
        let mut steps = [Step::Static(Iso3::IDENTITY); MAX_DEPTH];
        let mut len = 0usize;
        for &e in t_edges.iter().take(nt) {
            steps[len] = Step::Dyn {
                edge: EdgeId(e),
                inverted: true,
            };
            len += 1;
        }
        for k in 0..ns {
            let e = s_edges[ns - 1 - k];
            steps[len] = Step::Dyn {
                edge: EdgeId(e),
                inverted: false,
            };
            len += 1;
        }

        // Confirm the whole walk observed one generation before folding.
        if topo.generation() != start_gen {
            spin();
            continue 'walk;
        }

        let (steps, len, domain) = fold(&steps, len, &edge_meta)?;
        return Ok(Plan {
            generation: start_gen,
            steps,
            len: len as u8,
            domain,
        });
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
/// Returns the folded step array, its length, and the plan's domain tag.
///
/// # Errors
///
/// * [`LookupError::UnknownEdge`] — a step names an edge with no record in this
///   arena.
/// * [`LookupError::MixedTimeDomains`] — the path's dynamic edges do not all
///   share one time domain, so no single query stamp addresses them all.
fn fold(
    raw: &[Step; MAX_DEPTH],
    len: usize,
    edge_meta: &impl Fn(EdgeId) -> Option<EdgeMeta>,
) -> Result<([Step; MAX_DEPTH], usize, u8), LookupError> {
    let mut out = [Step::Static(Iso3::IDENTITY); MAX_DEPTH];
    let mut n = 0usize;
    // `None` until the first dynamic step fixes the plan's domain. Every later
    // dynamic step must agree: taking the last one (as this used to) let a plan
    // spanning a system-clock edge and a sensor-clock edge pass `check_domain`
    // and then sample one of them with the wrong clock — exactly the silent
    // misread D9 exists to prevent.
    let mut domain: Option<u8> = None;

    for step in raw.iter().take(len) {
        // Resolve the step to either a constant or a (still dynamic) sample.
        let resolved = match *step {
            Step::Static(m) => Step::Static(m),
            Step::Dyn { edge, inverted } => {
                let meta = edge_meta(edge).ok_or(LookupError::UnknownEdge { edge })?;
                match meta.kind {
                    EdgeKind::Static => {
                        let m = if inverted {
                            meta.static_pose.inverse()
                        } else {
                            meta.static_pose
                        };
                        Step::Static(m)
                    }
                    _ => {
                        // Dynamic (or tombstone — treated as dynamic; sampling it
                        // will surface the real error). Pin/verify the domain.
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
                }
            }
        };

        // Collapse into the previous step if both are Static.
        match (n, resolved) {
            (m, Step::Static(cur)) if m > 0 => {
                if let Step::Static(prev) = out[n - 1] {
                    out[n - 1] = Step::Static(prev * cur);
                } else {
                    out[n] = Step::Static(cur);
                    n += 1;
                }
            }
            (_, s) => {
                out[n] = s;
                n += 1;
            }
        }
    }

    Ok((out, n, domain.unwrap_or(0)))
}
