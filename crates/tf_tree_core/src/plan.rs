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
use crate::layout::{write_affine32, write_mat4, write_quat, Layout};
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
// Sampled pose plus derivatives
// ---------------------------------------------------------------------------

/// A pose and its derivatives at one instant — `docs/PHASE4.md` §2.2.
///
/// Returned by [`Plan::at_with_derivatives`]. The twist is **body-frame
/// (right)**, expressed in the plan's **source** frame — see
/// [`Plan::at_with_derivatives`] for why that is the source and not the target,
/// and [`tf_tree_math::twist`] for the convention generally.
#[derive(Clone, Copy, Debug, PartialEq)]
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
    /// How many of `steps[..len]` are [`Step::Dyn`], computed once at compile
    /// time. See [`Plan::new`] for why this is stored rather than counted.
    dyn_count: u8,
    /// The edge of the *first* [`Step::Dyn`], or [`EdgeId`]`(0)` when there is
    /// none. Only meaningful together with `dyn_count`; read it through
    /// [`Plan::first_dynamic_edge`], never directly.
    first_dyn: EdgeId,
}

impl Plan {
    /// Assemble a plan and derive everything that is a pure function of its
    /// steps.
    ///
    /// # Why these two are stored and not computed on demand
    ///
    /// They used to be computed on demand, and `Plan::at` called *both* — once
    /// through `check_domain` → `has_dynamic`, and once for `note`'s
    /// attribution. Each is an O(`len`) scan over a 2 KiB `[Step; MAX_DEPTH]`
    /// array, so a depth-14 lookup walked 28 steps before folding anything.
    /// `first_dynamic_edge`'s own doc comment already said it was
    /// "loop-invariant" and hoisted it for the *batch* path; the scalar path
    /// kept paying it per call.
    ///
    /// Both are functions of the compiled steps alone, so compile time is the
    /// right place. `Plan` is a value type — **not an arena structure** — so
    /// this costs no format version and no layout hash; the two fields land in
    /// padding the struct already had.
    ///
    /// Every construction goes through here so the derivation cannot drift from
    /// the steps it describes; `plan_derived_fields_match_a_fresh_scan` pins it.
    fn new(generation: u64, steps: [Step; MAX_DEPTH], len: usize, domain: u8) -> Plan {
        let mut dyn_count = 0u8;
        let mut first_dyn = EdgeId(0);
        for step in &steps[..len] {
            if let Step::Dyn { edge, .. } = step {
                if dyn_count == 0 {
                    first_dyn = *edge;
                }
                dyn_count = dyn_count.saturating_add(1);
            }
        }
        Plan {
            generation,
            steps,
            len: len as u8,
            domain,
            dyn_count,
            first_dyn,
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

    /// What [`Plan::new`] derived, next to what a fresh scan of the same steps
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
        // **The counter calls bracket the fold, not the whole function.**
        //
        // The two checks above fail on properties of the *query* — a plan
        // compiled against an old topology, or a domain mismatch — and neither
        // names an edge. Counting them would file a caller's mistake against a
        // publisher that is working correctly, which is worse than not counting
        // them at all (`docs/PHASE5.md` §5.2's attribution argument, applied in
        // the other direction).
        self.note(g, self.first_dynamic_edge(), self.fold_at(g, t.nanos()))
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
    fn fold_at_with_derivatives(&self, g: &Guard, t: i64) -> Result<(Iso3, Twist), LookupError> {
        let mut acc = Iso3::IDENTITY;
        let mut vel = Twist::ZERO;
        for step in self.steps() {
            match step {
                Step::Static(m) => {
                    // Constant transform: no twist of its own, but the body
                    // frame moves, so the accumulator must follow it.
                    vel = m.adjoint_inv(&vel);
                    acc = acc * *m;
                }
                Step::Dyn { edge, inverted } => {
                    let (p, vp) = g.sample_with_twist(*edge, t, ExtrapPolicy::Error)?;
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
        self.check_generation(g)?;
        self.check_domain::<D>()?;
        let (pose, twist) = self.note(
            g,
            self.first_dynamic_edge(),
            self.fold_at_with_derivatives(g, t.nanos()),
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

    /// The interval over which this plan is answerable, or `None` when it is
    /// unbounded (`docs/PHASE5.md` §4.2).
    ///
    /// [`Self::latest_common`] generalised from a point to a range: the
    /// **intersection** of every dynamic step's retained window, so the lower end
    /// is a `max` and the upper end a `min`. The upper end is `latest_common`'s
    /// own stamp — both are `min` over [`SampleRing::newest_stamp`](crate::buffer::SampleRing::newest_stamp).
    ///
    /// The two do *not* share a helper, on purpose: `latest_common` is a lookup
    /// path and folding it onto [`Guard::window`] would cost it a second atomic
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
    /// ends are not even one snapshot of one ring — see [`Guard::window`]. That
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
    /// The point of this over [`Self::at_many`] is that `Iso3` is
    /// `repr(C, align(64))` and so cannot alias any layout a consumer wants
    /// (see [`crate::layout`]). Writing through it costs an intermediate buffer
    /// and a second pass; this writes once, in place, and allocates nothing.
    ///
    /// `out` is a flat `f64` slice of at least `stamps.len() * layout.elems()`.
    /// Use [`Self::at_many_into_f32`] for [`Layout::Affine32`].
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
    /// Otherwise as [`Self::at`].
    pub fn at_many_into<D: Domain>(
        &self,
        g: &Guard,
        stamps: &[i64],
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
        self.check_domain::<D>()?;

        let n = layout.elems();
        // The layout is matched once, here. Putting it inside the loop would
        // add an unpredictable branch between every element and the next, in
        // the one API whose whole purpose is a per-element cost of nanoseconds.
        match layout {
            Layout::Mat4 => self.fold_batch(g, stamps, write_mat4, n, out),
            Layout::Quat => self.fold_batch(g, stamps, write_quat, n, out),
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
        self.check_domain::<D>()?;

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
    /// Per-step bracket-search hints, so a scalar lookup resumes beside the
    /// previous answer instead of restarting at the window midpoint.
    ///
    /// # Why this is worth 25-29% of a sample
    ///
    /// `docs/design/fast-path.md` §12 measured the bracket search at **34% of a
    /// dynamic step**, and §12's capacity sweep showed the cost is not the probe
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
    /// safe to keep in a cache that nothing invalidates.
    ///
    /// # Why a `Cell` per element, and why on the `Guard`
    ///
    /// `Guard` is `!Sync` by construction and created per batch on one thread —
    /// the same argument `ok` above carries from `docs/PHASE5.md` §5.4 — so a
    /// non-atomic cell is sound and is what the hot path should pay. A
    /// `Cell<[u64; MAX_DEPTH]>` would copy the whole array on every `get`/`set`;
    /// per-element cells load and store one word.
    ///
    /// `cursor_edge` is the self-invalidation. One `Guard` can evaluate several
    /// plans, and step `k` of one plan is a different edge from step `k` of
    /// another; without the tag the hint would send the gallop somewhere
    /// arbitrary, which is still correct but can cost up to twice a plain
    /// search. Storing the edge and using the hint only on a match makes a
    /// mismatched cursor cost one comparison instead.
    /// Packed `(edge << 32) | index`, one word per step.
    ///
    /// Two arrays became one because the cost of this cache is **initialising
    /// it**: a guard is built per batch and every cell must be written, so 32
    /// stores became 16 and `Guard::new` went from 8.9 ns back toward its
    /// original 1.4. Packing is safe for the index precisely because the whole
    /// cache is: a logical index past `u32::MAX` — 49 days of unbroken 1 kHz
    /// publishing — truncates to a wrong *hint*, and a wrong hint is corrected
    /// by the gallop, never returned.
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
/// keeps `Guard` at 48 bytes and adds **no load at all** to the hot path:
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
    /// call, to express a generality nothing needed. See [`DETACHED`].
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
///   `fold`.
pub fn compile(
    topo: &TopologyView,
    edge_meta: impl Fn(EdgeId) -> Option<EdgeMeta>,
    target: FrameId,
    source: FrameId,
) -> Result<Plan, LookupError> {
    if target == source {
        // Identity plan; still stamp it with a consistent generation.
        let generation = topo.stable_generation();
        return Ok(Plan::new(
            generation,
            [Step::Static(Iso3::IDENTITY); MAX_DEPTH],
            0,
            0,
        ));
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
        return Ok(Plan::new(start_gen, steps, len, domain));
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
