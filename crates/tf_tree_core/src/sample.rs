//! Bracket search over an edge's sample ring (`docs/PHASE1.md` §6.4): logical
//! indices with `& mask`, window `[head - n, head - 1]`,
//! `n = min(head,` [`SampleRing::retained`]`)`, the same bound the trailing
//! revalidation uses.
//!
//! # Two hazards the trailing revalidation does **not** cover
//!
//! **1.** `bracket` searches `Relaxed` stamps the writer overwrites in place; two
//! pushes mid-search make them non-monotone, and the caller can get a blend of
//! two samples that do not bracket its request.
//!
//! **2.** [`SampleRing::newest_stamp`] can report a later lap's stamp; it is an estimate.
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
/// `#[non_exhaustive]`: only this crate dispatches on it, so a new policy cannot
/// silently break a consumer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ExtrapPolicy {
    /// Refuse with [`LookupError::Extrapolation`]; the safe default.
    #[default]
    Error,
    /// Hold the newest sample constant (tf2's behaviour under some settings).
    Hold,
    /// Extend the screw twist of the two newest samples; [`ExtrapPolicy::Hold`] if fewer than two.
    ConstantTwist,
}

/// One seqlocked read of an edge's bracket, before any interpolation
/// (`docs/decisions/0060`). Evaluating it yields the same bits as the per-stamp
/// fold (`crates/tf_tree/tests/batch_phases.rs`).
#[derive(Clone, Copy, Debug)]
pub(crate) enum Bracket {
    /// A pose that needs no interpolation.
    Exact(Iso3),
    /// Interpolate between the two retained samples that bracket the query.
    Between {
        /// The older endpoint; `b` is the newer and `s` the position in `[0, 1)`.
        a: Iso3,
        b: Iso3,
        s: f64,
    },
}

impl Bracket {
    /// Fold this bracket under interpolation policy `I` (`docs/PROJECT.md` §6).
    #[inline]
    pub(crate) fn eval<I: Interp>(&self) -> Iso3 {
        match *self {
            Bracket::Exact(p) => Interpolated::<I>::exact(p).0,
            Bracket::Between { a, b, s } => Interpolated::<I>::between(a, b, s).0,
        }
    }
}

/// What [`SampleRing::read_from`] turns a bracket into: the batch instantiates
/// [`Bracket`], the scalar path [`Interpolated<I>`]. The split keeps the 128-byte
/// [`Bracket`] out of the scalar `Result` (`docs/decisions/0060` §10.1).
pub(crate) trait FromBracket {
    fn exact(p: Iso3) -> Self;
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

/// A bracket folded as it is read, under interpolation policy `I`.
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

/// Nanoseconds from `from` to `to`, for `from <= to`.
///
/// Subtracts in `u64`: an `i64` subtraction overflows for stamps more than
/// `i64::MAX` apart. `from > to` yields a nonsense magnitude, not a panic.
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
            self.read_slot((i & self.mask()) as usize)?
        } else {
            let t_j = self.stamp_at(i + 1);
            let a = self.read_slot((i & self.mask()) as usize)?;
            let b = self.read_slot(((i + 1) & self.mask()) as usize)?;
            let s = span_ns(t_i, t) / span_ns(t_i, t_j);
            I::eval(&a, &b, s)
        };

        if self.head.load(Ordering::Acquire) - i > retained {
            return Err(LookupError::SlotRecycled { edge: self.edge });
        }
        Ok(result)
    }

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

    /// Read the bracket at stamp `t`, resuming from `cursor`, **without
    /// interpolating**.
    ///
    /// `cursor` is updated to the lower bracket index; seed it to `0`. This is
    /// the one read body (`docs/decisions/0060` step 2). `#[inline(always)]` is
    /// load-bearing: LLVM declines plain `#[inline]` at the batch's call site.
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

    /// [`Self::sample`], resuming the search from `cursor` by galloping.
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

    /// Load the stamp at a logical index; `Relaxed` is correct because the `head`
    /// Acquire load orders it (`head_publishes_every_stamp_below_it`, `loom_tests.rs`).
    #[inline]
    fn stamp_at(&self, logical: u64) -> i64 {
        self.stamps[(logical & self.mask()) as usize].load(Ordering::Relaxed)
    }

    /// Last logical index in `[lo, hi]` whose stamp is `<= t`.
    ///
    /// Caller guarantees `stamp[lo] <= t < stamp[hi]`. The mask update is not
    /// branchless after codegen (`docs/decisions/0053`).
    #[inline]
    fn bracket(&self, lo: u64, hi: u64, t: i64) -> u64 {
        let mut base = lo;
        let mut len = hi - lo + 1;
        while len > 1 {
            let half = len / 2;
            // 0053: the backend turns this back into control flow.
            let cmp = u64::from(self.stamp_at(base + half) <= t);
            base = base.wrapping_add(half & 0u64.wrapping_sub(cmp));
            len -= half;
        }
        base
    }

    /// [`Self::bracket`], seeded from `hint` by a galloping search.
    ///
    /// Caller guarantees `stamp[lo_logical] <= t < stamp[newest]`; a bad `hint`
    /// costs probes, never the answer. `inline(always)` is load-bearing.
    #[inline(always)]
    fn bracket_from(&self, lo_logical: u64, newest: u64, t: i64, hint: u64) -> u64 {
        let hint = rebase_hint(hint, lo_logical, newest).clamp(lo_logical, newest);
        let (lo, hi) = if self.stamp_at(hint) <= t {
            let mut step = 1u64;
            while hint + step < newest && self.stamp_at(hint + step) <= t {
                step *= 2;
            }
            (hint + step / 2, (hint + step).min(newest))
        } else {
            let mut step = 1u64;
            while hint.saturating_sub(step) > lo_logical && self.stamp_at(hint - step) > t {
                step *= 2;
            }
            (hint.saturating_sub(step).max(lo_logical), hint - step / 2)
        };
        self.bracket(lo, hi, t)
    }

    /// Oldest and newest readable logical index, for a non-empty ring. Test-only.
    #[cfg(test)]
    pub(crate) fn window_for_test(&self) -> (u64, u64) {
        let h = self.head.load(Ordering::Acquire);
        let n = h.min(self.retained());
        (h - n, h - 1)
    }

    /// Sample at `t` **and** the body twist there, in 1/second
    /// (`docs/PHASE4.md` §2.3). ScLerp only.
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

    /// [`Self::sample_with_twist`], resuming from `cursor` as
    /// [`Self::sample_from`] does.
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

    /// Body of both `sample_with_twist*`; `seek` runs only in the interpolating arm.
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
                    let p = self.read_slot((newest & self.mask()) as usize)?;
                    return self.revalidated(newest, retained, (p, Twist::ZERO));
                }
                ExtrapPolicy::ConstantTwist => {
                    if newest == lo_logical {
                        return Err(LookupError::NoSegment { edge: self.edge });
                    }
                    return self.constant_twist(lo_logical, newest, t, t_new);
                }
            }
        }

        if newest == lo_logical {
            return Err(LookupError::NoSegment { edge: self.edge });
        }
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

        if self.head.load(Ordering::Acquire) - i > retained {
            return Err(LookupError::SlotRecycled { edge: self.edge });
        }
        Ok((pose, xi.scale(NANOS_PER_SEC / dt)))
    }

    /// [`ExtrapPolicy::ConstantTwist`] extrapolation: the pose **and** the twist
    /// it was extended along, from one read covered by one revalidation.
    /// `Twist::ZERO` accompanies the single-sample case (`newest == lo_logical`).
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
            (b, Twist::ZERO)
        } else {
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
/// logical index; without the lift a hint past 2^32 pushes pins to the oldest
/// sample. Exact: the window is `< 2^32` wide, so the index has one preimage.
#[inline(always)]
pub(crate) fn rebase_hint(hint: u64, lo_logical: u64, newest: u64) -> u64 {
    const BLOCK: u64 = 1 << 32;
    if hint >= lo_logical {
        return hint;
    }
    let lifted = (newest & !(BLOCK - 1)) | (hint & (BLOCK - 1));
    if lifted > newest {
        lifted - BLOCK
    } else {
        lifted
    }
}
