//! Bracket search over an edge's sample ring: locate the two published samples
//! that bracket a query stamp and interpolate.
//!
//! The search runs over **logical** indices with `& mask` applied on every probe
//! (`docs/PHASE1.md` §6.4). The window is `[head - n, head - 1]` with
//! `n = min(head,` [`SampleRing::retained`]`)`, and `retained` is `capacity - 1`:
//! logical `head - capacity` shares a slot with the sample `push` is writing.
//! The window and the trailing revalidation use the same bound; keep them in step.
//!
//! # Two hazards the trailing revalidation does **not** cover
//!
//! Both are open (closing either is a hot-path cost).
//!
//! **1.** `bracket` binary-searches `stamp_at`, a `Relaxed` load of an array the
//! writer overwrites in place. Two pushes during a search reach the window and
//! the stamps stop being monotone; the search then returns an arbitrary index, and
//! if it is still inside the window the trailing check passes and the caller gets
//! a blend of two samples that do not bracket its request.
//!
//! **2.** [`SampleRing::newest_stamp`] loads `head`, then the stamp at
//! `head - 1`; if the ring laps in between it reports a later lap's stamp. It is
//! an estimate of the frontier, not a bound, and is no baseline for judging
//! staleness.
//!
//! This module is `unsafe`-free.

use core::marker::PhantomData;

use tf_tree_math::{Interp, Iso3, ScLerp, Twist};

/// Nanoseconds per second, as the `f64` the twist scaling needs.
const NANOS_PER_SEC: f64 = 1_000_000_000.0;

use crate::buffer::SampleRing;
use crate::error::LookupError;
use crate::sync::Ordering;

/// What to do when the requested stamp is newer than every published sample.
///
/// `#[non_exhaustive]`: a caller *passes* one of these and only this crate
/// dispatches on it, so a fourth policy cannot make an existing consumer
/// silently wrong. Contrast [`crate::plan::InterpPolicy`], which downstream
/// crates must map exhaustively onto something else and therefore stays
/// exhaustive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ExtrapPolicy {
    /// Refuse: return [`LookupError::Extrapolation`]. The safe default for a
    /// control loop that must not act on invented data.
    #[default]
    Error,
    /// Hold the newest sample constant (tf2's behaviour under some settings).
    Hold,
    /// Extend the constant screw twist implied by the two newest samples. Falls
    /// back to [`ExtrapPolicy::Hold`] when fewer than two samples exist.
    ConstantTwist,
}

/// One seqlocked read of an edge's bracket, before any interpolation.
///
/// [`SampleRing::read_from`] returns this; [`SampleRing::sample_from`] folds it
/// immediately with [`Interp::eval`]. The split is `docs/decisions/0060`'s phase
/// buffering. Evaluating a bracket yields exactly the bits the per-stamp fold
/// yields (`crates/tf_tree/tests/batch_phases.rs`, by `to_bits`).
#[derive(Clone, Copy, Debug)]
pub(crate) enum Bracket {
    /// A pose that needs no interpolation: an exact stamp hit, or the answer an
    /// extrapolation policy produced on its own.
    Exact(Iso3),
    /// Interpolate between the two retained samples that bracket the query.
    Between {
        /// The older endpoint.
        a: Iso3,
        /// The newer endpoint.
        b: Iso3,
        /// Where the query falls between them, in `[0, 1)`.
        s: f64,
    },
}

impl Bracket {
    /// Fold this bracket under interpolation policy `I`, through [`Interpolated`]
    /// so the scalar and batch reads share one pair of constructors
    /// (`docs/PROJECT.md` §6).
    #[inline]
    pub(crate) fn eval<I: Interp>(&self) -> Iso3 {
        match *self {
            Bracket::Exact(p) => Interpolated::<I>::exact(p).0,
            Bracket::Between { a, b, s } => Interpolated::<I>::between(a, b, s).0,
        }
    }
}

/// What [`SampleRing::read_from`] turns a bracket into, chosen at the type level:
/// the batch instantiates [`Bracket`], the scalar path [`Interpolated<I>`].
///
/// Returning a 128-byte [`Bracket`] through the scalar path's `Result` cost
/// +10..15% on `lookup/*` under `[profile.embedder]`, and `#[inline]` only moves
/// it (`docs/decisions/0060` §10.1). Search, slot reads and lap check stay in one
/// function.
pub(crate) trait FromBracket {
    /// A pose that needs no interpolation.
    fn exact(p: Iso3) -> Self;
    /// The two retained samples that bracket the query, and where it falls.
    fn between(a: Iso3, b: Iso3, s: f64) -> Self;
}

impl FromBracket for Bracket {
    #[inline]
    fn exact(p: Iso3) -> Bracket {
        Bracket::Exact(p)
    }
    #[inline]
    fn between(a: Iso3, b: Iso3, s: f64) -> Bracket {
        Bracket::Between { a, b, s }
    }
}

/// A bracket folded at the moment it is read, under interpolation policy `I` —
/// the scalar path's [`FromBracket`].
pub(crate) struct Interpolated<I>(pub Iso3, PhantomData<I>);

impl<I: Interp> FromBracket for Interpolated<I> {
    #[inline]
    fn exact(p: Iso3) -> Self {
        Interpolated(p, PhantomData)
    }
    #[inline]
    fn between(a: Iso3, b: Iso3, s: f64) -> Self {
        Interpolated(I::eval(&a, &b, s), PhantomData)
    }
}

/// Nanoseconds from `from` to `to` as an `f64`, for a pair the caller has
/// already ordered `from <= to`.
///
/// The subtraction is in `u64` on purpose: an `i64` subtraction overflows when
/// the stamps are more than `i64::MAX` apart (panic in a checked build, a wrapped
/// negative `s` and a pose from outside the bracket in release). `wrapping_sub`
/// on the bit patterns is the exact distance for every ordered pair. `from > to`
/// yields a nonsense magnitude, not a panic.
#[inline]
fn span_ns(from: i64, to: i64) -> f64 {
    (to as u64).wrapping_sub(from as u64) as f64
}

impl SampleRing<'_> {
    /// Sample the edge at stamp `t` under interpolation policy `I` and
    /// extrapolation policy `policy`.
    ///
    /// # Errors
    ///
    /// * [`LookupError::NoData`] — the ring is empty.
    /// * [`LookupError::Extrapolation`] — `t` is older than the oldest retained
    ///   sample, or newer than the newest and `policy` is
    ///   [`ExtrapPolicy::Error`].
    /// * [`LookupError::SlotContended`] — a slot stayed mid-write too long.
    /// * [`LookupError::SlotRecycled`] — the ring lapped the reader mid-read.
    pub fn sample<I: Interp>(&self, t: i64, policy: ExtrapPolicy) -> Result<Iso3, LookupError> {
        // Acquire: pairs with `push`'s head store to publish every stamp below it.
        let h = self.head.load(Ordering::Acquire);
        if h == 0 {
            return Err(LookupError::NoData { edge: self.edge });
        }
        let retained = self.retained();
        let n = h.min(retained);
        let lo_logical = h - n; // oldest *safely readable* logical index
        let newest = h - 1;

        let t_old = self.stamp_at(lo_logical);
        let t_new = self.stamp_at(newest);

        if t < t_old {
            return Err(LookupError::Extrapolation {
                edge: self.edge,
                requested: t,
                oldest: t_old,
                newest: t_new,
            });
        }
        if t > t_new {
            return match policy {
                ExtrapPolicy::Error => Err(LookupError::Extrapolation {
                    edge: self.edge,
                    requested: t,
                    oldest: t_old,
                    newest: t_new,
                }),
                ExtrapPolicy::Hold => self
                    .read_slot((newest & self.mask()) as usize)
                    .and_then(|p| self.revalidated(newest, retained, p)),
                ExtrapPolicy::ConstantTwist => self
                    .constant_twist(lo_logical, newest, t, t_new)
                    .map(|(pose, _)| pose),
            };
        }
        if t == t_new {
            let p = self.read_slot((newest & self.mask()) as usize)?;
            return self.revalidated(newest, retained, p);
        }

        let i = self.bracket(lo_logical, newest, t);
        let t_i = self.stamp_at(i);

        let result = if t_i == t {
            // Exact hit — no interpolation.
            self.read_slot((i & self.mask()) as usize)?
        } else {
            let t_j = self.stamp_at(i + 1);
            let a = self.read_slot((i & self.mask()) as usize)?;
            let b = self.read_slot(((i + 1) & self.mask()) as usize)?;
            // t_i < t < t_j guaranteed here, so the denominator is non-zero.
            let s = span_ns(t_i, t) / span_ns(t_i, t_j);
            I::eval(&a, &b, s)
        };

        // Revalidate: a lap past `i` makes the endpoints stale; the caller decides on retry.
        if self.head.load(Ordering::Acquire) - i > retained {
            return Err(LookupError::SlotRecycled { edge: self.edge });
        }
        Ok(result)
    }

    /// Hand back `v` unless the ring lapped past logical index `i` while it was
    /// being read. Every arm that short-circuits (`Hold`, an exact hit on the
    /// newest stamp, `constant_twist`'s single sample) must use it, or a
    /// descheduled reader gets the pose of a different stamp.
    ///
    /// [`Self::revalidated`] with no payload, for the test that pins its bound.
    #[cfg(test)]
    pub(crate) fn revalidated_for_test(&self, i: u64, retained: u64) -> Result<(), LookupError> {
        self.revalidated(i, retained, ())
    }

    #[inline(always)]
    fn revalidated<T>(&self, i: u64, retained: u64, v: T) -> Result<T, LookupError> {
        if self.head.load(Ordering::Acquire) - i > retained {
            return Err(LookupError::SlotRecycled { edge: self.edge });
        }
        Ok(v)
    }

    /// Read the bracket at stamp `t`, resuming the search from `cursor`, and
    /// return it **without interpolating**.
    ///
    /// For monotone stamps the galloping resume is `O(1)` amortized. `cursor` is
    /// updated to the lower bracket index found; seed it to `0`.
    ///
    /// This is the one read body: [`Self::sample_from`] adds one [`Interp::eval`],
    /// and `Plan`'s batch fold runs it per chunk (`docs/decisions/0060` step 2).
    /// The trailing `head - i > retained` check runs after the slot reads, so for
    /// `B = Bracket` it precedes the arithmetic; it judges only values already
    /// copied out, so that loses nothing.
    ///
    /// `#[inline(always)]` is load-bearing: LLVM declines plain `#[inline]` at the
    /// batch's per-lane call site (`at_many/into_mat4_1024` −7.8% vs −17.1%).
    ///
    /// # Errors
    ///
    /// Identical to [`Self::sample`].
    #[inline(always)]
    pub(crate) fn read_from<B: FromBracket>(
        &self,
        t: i64,
        policy: ExtrapPolicy,
        cursor: &mut u64,
    ) -> Result<B, LookupError> {
        let h = self.head.load(Ordering::Acquire);
        if h == 0 {
            return Err(LookupError::NoData { edge: self.edge });
        }
        let retained = self.retained();
        let n = h.min(retained);
        let lo_logical = h - n;
        let newest = h - 1;

        let t_old = self.stamp_at(lo_logical);
        let t_new = self.stamp_at(newest);

        if t < t_old {
            return Err(LookupError::Extrapolation {
                edge: self.edge,
                requested: t,
                oldest: t_old,
                newest: t_new,
            });
        }
        if t > t_new {
            *cursor = newest;
            return match policy {
                ExtrapPolicy::Error => Err(LookupError::Extrapolation {
                    edge: self.edge,
                    requested: t,
                    oldest: t_old,
                    newest: t_new,
                }),
                ExtrapPolicy::Hold => self
                    .read_slot((newest & self.mask()) as usize)
                    .and_then(|p| self.revalidated(newest, retained, p))
                    .map(B::exact),
                ExtrapPolicy::ConstantTwist => self
                    .constant_twist(lo_logical, newest, t, t_new)
                    .map(|(pose, _)| B::exact(pose)),
            };
        }
        if t == t_new {
            *cursor = newest;
            let p = self.read_slot((newest & self.mask()) as usize)?;
            return self.revalidated(newest, retained, p).map(B::exact);
        }

        // Here t_old <= t < t_new, so the window endpoints already bracket `t`
        // and `bracket_from`'s precondition holds.
        let i = self.bracket_from(lo_logical, newest, t, *cursor);
        *cursor = i;
        let t_i = self.stamp_at(i);

        let result = if t_i == t {
            B::exact(self.read_slot((i & self.mask()) as usize)?)
        } else {
            let t_j = self.stamp_at(i + 1);
            let a = self.read_slot((i & self.mask()) as usize)?;
            let b = self.read_slot(((i + 1) & self.mask()) as usize)?;
            let s = span_ns(t_i, t) / span_ns(t_i, t_j);
            B::between(a, b, s)
        };

        if self.head.load(Ordering::Acquire) - i > retained {
            return Err(LookupError::SlotRecycled { edge: self.edge });
        }
        Ok(result)
    }

    /// [`Self::sample`], resuming the search from the logical index in `cursor`
    /// by an exponential (galloping) search. The result is identical; only the
    /// search path differs.
    ///
    /// # Errors
    ///
    /// Identical to [`Self::sample`].
    pub fn sample_from<I: Interp>(
        &self,
        t: i64,
        policy: ExtrapPolicy,
        cursor: &mut u64,
    ) -> Result<Iso3, LookupError> {
        Ok(self.read_from::<Interpolated<I>>(t, policy, cursor)?.0)
    }

    /// Load the stamp at a logical index (masked to physical). `Relaxed` is
    /// correct because the `head` Acquire load already ordered every stamp of a
    /// published sample into view; the edge is pinned by
    /// `head_publishes_every_stamp_below_it` (`loom_tests.rs`).
    #[inline]
    fn stamp_at(&self, logical: u64) -> i64 {
        self.stamps[(logical & self.mask()) as usize].load(Ordering::Relaxed)
    }

    /// Last logical index in `[lo, hi]` whose stamp is `<= t`.
    ///
    /// Caller guarantees `stamp[lo] <= t < stamp[hi]`, so the result is `< hi`
    /// and `i + 1` is a valid upper bracket.
    ///
    /// The mask update (`base += half & (0 - cmp)`) is **not** branchless after
    /// codegen: LLVM folds it back to a `select` and the cmov-conversion pass
    /// expands it to control flow. `docs/decisions/0053` owns the measurement and
    /// the rejected spellings; the interpolated seed of `docs/design/fast-path.md`
    /// §5 is falsified by `examples/search_seed`.
    #[inline]
    fn bracket(&self, lo: u64, hi: u64, t: i64) -> u64 {
        let mut base = lo;
        let mut len = hi - lo + 1;
        while len > 1 {
            let half = len / 2;
            // The backend turns this mask back into control flow (0053); do not claim branchlessness.
            let cmp = u64::from(self.stamp_at(base + half) <= t);
            base = base.wrapping_add(half & 0u64.wrapping_sub(cmp));
            len -= half;
        }
        base
    }

    /// [`Self::bracket`], seeded from `hint` by a galloping search.
    ///
    /// Caller guarantees `stamp[lo_logical] <= t < stamp[newest]`; a stale,
    /// clamped or nonsensical `hint` then costs probes and never changes the
    /// answer (`Guard::cursor` relies on this). Both twist and pose samplers
    /// share it.
    ///
    /// `inline(always)` is load-bearing: plain `#[inline]` cost 12% on
    /// `examples/abi_cost.rs`'s depth-3 lookup.
    #[inline(always)]
    fn bracket_from(&self, lo_logical: u64, newest: u64, t: i64, hint: u64) -> u64 {
        let hint = rebase_hint(hint, lo_logical, newest).clamp(lo_logical, newest);
        let (lo, hi) = if self.stamp_at(hint) <= t {
            // Gallop upward while the probe stays <= t.
            let mut step = 1u64;
            while hint + step < newest && self.stamp_at(hint + step) <= t {
                step *= 2;
            }
            (hint + step / 2, (hint + step).min(newest))
        } else {
            // Gallop downward while the probe stays > t.
            let mut step = 1u64;
            while hint.saturating_sub(step) > lo_logical && self.stamp_at(hint - step) > t {
                step *= 2;
            }
            (hint.saturating_sub(step).max(lo_logical), hint - step / 2)
        };
        // Binary search within the galloped bracket.
        // Invariant: stamp[lo] <= t < stamp[hi].
        self.bracket(lo, hi, t)
    }

    /// Oldest and newest readable logical index, for a non-empty ring. Test-only.
    #[cfg(test)]
    pub(crate) fn window_for_test(&self) -> (u64, u64) {
        let h = self.head.load(Ordering::Acquire);
        let n = h.min(self.retained());
        (h - n, h - 1)
    }

    /// Sample at `t` **and** the body twist there, in units of 1/second
    /// (`docs/PHASE4.md` §2.3). ScLerp only; `Guard::sample_with_twist` refuses
    /// `LerpSlerp` before reaching here.
    ///
    /// # Bracket-less outcomes
    ///
    /// | case | pose | twist |
    /// |---|---|---|
    /// | `t > t_new`, [`ExtrapPolicy::Hold`] | newest, held | **zero** |
    /// | `t > t_new`, [`ExtrapPolicy::ConstantTwist`] | extrapolated | the extended segment's twist |
    /// | `t == t_new`, ≥ 2 samples | newest | the *preceding* segment's twist |
    /// | one sample, or a zero-length segment | fine | [`LookupError::NoSegment`] |
    ///
    /// # Errors
    ///
    /// As [`Self::sample`], plus [`LookupError::NoSegment`].
    pub fn sample_with_twist(
        &self,
        t: i64,
        policy: ExtrapPolicy,
    ) -> Result<(Iso3, Twist), LookupError> {
        self.sample_with_twist_seeking(t, policy, |s, lo, hi, t| s.bracket(lo, hi, t))
    }

    /// [`Self::sample_with_twist`], resuming the search from `cursor` as
    /// [`Self::sample_from`] does (for `Plan::at_many_into(Layout::QuatTwist)`).
    /// `cursor` is updated to the lower bracket index; seed it to `0`.
    ///
    /// # Errors
    ///
    /// Identical to [`Self::sample_with_twist`].
    pub fn sample_with_twist_from(
        &self,
        t: i64,
        policy: ExtrapPolicy,
        cursor: &mut u64,
    ) -> Result<(Iso3, Twist), LookupError> {
        self.sample_with_twist_seeking(t, policy, |s, lo, hi, t| {
            let i = s.bracket_from(lo, hi, t, *cursor);
            *cursor = i;
            i
        })
    }

    /// Body of [`Self::sample_with_twist`] and [`Self::sample_with_twist_from`],
    /// generic over `seek` so the cursor-less path gains no branch. `seek` runs
    /// only in the interpolating arm; other arms leave a cursor where it was.
    #[inline]
    fn sample_with_twist_seeking<F>(
        &self,
        t: i64,
        policy: ExtrapPolicy,
        seek: F,
    ) -> Result<(Iso3, Twist), LookupError>
    where
        F: FnOnce(&Self, u64, u64, i64) -> u64,
    {
        let h = self.head.load(Ordering::Acquire);
        if h == 0 {
            return Err(LookupError::NoData { edge: self.edge });
        }
        let retained = self.retained();
        let n = h.min(retained);
        let lo_logical = h - n;
        let newest = h - 1;

        let t_old = self.stamp_at(lo_logical);
        let t_new = self.stamp_at(newest);

        if t < t_old {
            return Err(LookupError::Extrapolation {
                edge: self.edge,
                requested: t,
                oldest: t_old,
                newest: t_new,
            });
        }
        if t > t_new {
            match policy {
                ExtrapPolicy::Error => {
                    return Err(LookupError::Extrapolation {
                        edge: self.edge,
                        requested: t,
                        oldest: t_old,
                        newest: t_new,
                    })
                }
                ExtrapPolicy::Hold => {
                    // Held is stationary: zero is the derivative, not a fallback.
                    let p = self.read_slot((newest & self.mask()) as usize)?;
                    return self.revalidated(newest, retained, (p, Twist::ZERO));
                }
                ExtrapPolicy::ConstantTwist => {
                    // One sample: a pose to extend from, no segment to extend along.
                    if newest == lo_logical {
                        return Err(LookupError::NoSegment { edge: self.edge });
                    }
                    return self.constant_twist(lo_logical, newest, t, t_new);
                }
            }
        }

        // Inside `[t_old, t_new]` a segment exists unless one sample is retained.
        if newest == lo_logical {
            return Err(LookupError::NoSegment { edge: self.edge });
        }
        // At the newest stamp the twist is the left limit: the segment ending there.
        let i = if t == t_new {
            newest - 1
        } else {
            seek(self, lo_logical, newest, t)
        };
        let t_i = self.stamp_at(i);
        let t_j = self.stamp_at(i + 1);
        let dt = span_ns(t_i, t_j);
        if dt == 0.0 {
            // Equal stamps are legal (invariant 6) but span no time.
            return Err(LookupError::NoSegment { edge: self.edge });
        }
        let a = self.read_slot((i & self.mask()) as usize)?;
        let b = self.read_slot(((i + 1) & self.mask()) as usize)?;
        let s = span_ns(t_i, t) / dt;
        let (pose, xi) = ScLerp::eval_with_twist(&a, &b, s);

        // A lap past `i` makes both pose and twist stale.
        if self.head.load(Ordering::Acquire) - i > retained {
            return Err(LookupError::SlotRecycled { edge: self.edge });
        }
        // ξ is per unit `s`; `s` spans `dt` nanoseconds.
        Ok((pose, xi.scale(NANOS_PER_SEC / dt)))
    }

    /// [`ExtrapPolicy::ConstantTwist`] extrapolation past the newest sample,
    /// returning the pose **and** the twist it was extended along.
    ///
    /// Both come from one [`ScLerp::eval_with_twist`] on one read of the two
    /// newest slots, covered by one trailing `head - prev > retained` check;
    /// `read_slot`'s seqlock proves each pose consistent, never that two poses
    /// share an era. `Twist::ZERO` accompanies the single-sample case; callers
    /// distinguish it via `newest == lo_logical`.
    ///
    fn constant_twist(
        &self,
        lo_logical: u64,
        newest: u64,
        t: i64,
        t_new: i64,
    ) -> Result<(Iso3, Twist), LookupError> {
        if newest == lo_logical {
            let p = self.read_slot((newest & self.mask()) as usize)?;
            return self.revalidated(newest, self.retained(), (p, Twist::ZERO));
        }
        let prev = newest - 1;
        let t_prev = self.stamp_at(prev);
        let a = self.read_slot((prev & self.mask()) as usize)?;
        let b = self.read_slot((newest & self.mask()) as usize)?;
        let dt = span_ns(t_prev, t_new);
        let result = if dt == 0.0 {
            // Equal stamps span no time: nothing to extend along.
            (b, Twist::ZERO)
        } else {
            // Extend a->b's screw twist to `t`; `param == 1` at `t_new` reproduces `b`.
            let param = span_ns(t_prev, t) / dt;
            let (pose, xi) = ScLerp::eval_with_twist(&a, &b, param);
            (pose, xi.scale(NANOS_PER_SEC / dt))
        };
        if self.head.load(Ordering::Acquire) - prev > self.retained() {
            return Err(LookupError::SlotRecycled { edge: self.edge });
        }
        Ok(result)
    }
}

/// Lift a hint whose high bits were discarded back onto the live window.
///
/// [`Guard`](crate::plan::Guard) stores the cursor as the low 32 bits of a
/// logical index. `head` is never masked, so past 2^32 pushes (49.7 days at
/// 1 kHz) every stored hint is below `lo_logical` and a plain clamp pins it to
/// the oldest sample forever: correct but permanently slow.
///
/// The lift is exact: the window is `capacity - 1 < 2^32` wide, so a truncated
/// index has exactly one preimage in it, in `newest`'s 2^32 block or the one
/// below. Below 2^32 `lifted == hint` and behaviour is unchanged.
#[inline(always)]
pub(crate) fn rebase_hint(hint: u64, lo_logical: u64, newest: u64) -> u64 {
    /// One more than the largest value `Guard`'s packed cursor can represent.
    const BLOCK: u64 = 1 << 32;
    if hint >= lo_logical {
        // Already absolute (the hot, warm-cursor case); the caller clamps `> newest`.
        return hint;
    }
    let lifted = (newest & !(BLOCK - 1)) | (hint & (BLOCK - 1));
    if lifted > newest {
        // The hint belongs to the block below the straddled 2^32 boundary.
        // `newest >= lifted - BLOCK` cannot underflow: `lifted` and `newest`
        // share a block base, so `lifted > newest` implies `newest >= BLOCK`.
        lifted - BLOCK
    } else {
        lifted
    }
}
