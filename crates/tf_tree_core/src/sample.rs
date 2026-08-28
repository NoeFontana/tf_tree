//! Bracket search over an edge's sample ring.
//!
//! Given a query stamp `t`, locate the two published samples that bracket it and
//! interpolate. The search is over **logical** indices with `& mask` applied on
//! every probe: searching the physical array directly is wrong once the ring has
//! wrapped, and is a classic off-by-one source (`docs/PHASE1.md` §6.4). The trailing
//! revalidation makes the read wait-free in practice — it fails only if the ring
//! lapped the reader mid-read, which cannot happen within the buffer's time-slack
//! under any sane configuration.
//!
//! The searched window is `[head - n, head - 1]` where `n = min(head,
//! `[`SampleRing::retained`]`)` — and `retained` is `capacity - 1`, **not**
//! `capacity`. Logical index `head - capacity` shares a physical slot with the
//! sample `push` is writing right now, so including it means reading a slot
//! mid-overwrite. Both the window and the trailing revalidation use the same
//! bound; keep them in step.
//!
//! This module is `unsafe`-free: it drives the [`SampleRing`] atomics through the
//! safe `push`/`read_slot` surface exposed by [`crate::buffer`].

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

/// Nanoseconds from `from` to `to` as an `f64`, for a pair the caller has
/// already ordered so that `from <= to`.
///
/// **The subtraction is done in `u64`, and that is load-bearing rather than a
/// micro-optimisation.** Every caller here has established its ordering — the
/// bracket precondition `t_i <= t < t_j`, or two adjacent ring samples, or the
/// forward-extrapolation branch's `t > t_new > t_prev` — so the difference is
/// mathematically non-negative in all of them. It still does not *fit* an `i64`
/// once the two stamps are more than `i64::MAX` apart, and the signed
/// subtraction was reached with exactly that:
///
/// * in a checked build it panicked — `attempt to subtract with overflow`;
/// * in a release build it wrapped, and the wrap is the worse half. A negative
///   `t_j - t_i` makes `s` negative, so `Interp::eval` runs *backwards past* the
///   older sample and returns a pose from outside the bracket. No error, no
///   panic, wrong answer, on the hot path.
///
/// Casting through `u64` first makes the result exact for every ordered `i64`
/// pair: `wrapping_sub` on the bit patterns *is* the true distance, not a
/// truncation of it, and for `(i64::MIN, i64::MAX)` it yields `u64::MAX`.
///
/// Two samples 292 years apart is malformed data, and this is not an endorsement
/// of it. But the defensible answers to malformed data are an error naming the
/// edge or a correct interpolation of it; a silently wrong pose is neither, and
/// removing that was the point. If a bound on segment width is wanted it belongs
/// upstream in `push`, as a `LookupError` that names the edge (R5), not here as
/// an accident of two's complement.
///
/// `from > to` is a caller bug and yields a nonsense magnitude rather than a
/// panic; every call site in this module is directly preceded by the comparison
/// that rules it out.
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
        // Publishes the sample set; every stamp below was written before the
        // matching head store, so this Acquire load orders them into view.
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
                ExtrapPolicy::Hold => self.read_slot((newest & self.mask) as usize),
                ExtrapPolicy::ConstantTwist => self
                    .constant_twist(lo_logical, newest, t, t_new)
                    .map(|(pose, _)| pose),
            };
        }
        if t == t_new {
            return self.read_slot((newest & self.mask) as usize);
        }

        let i = self.bracket(lo_logical, newest, t);
        let t_i = self.stamp_at(i);

        let result = if t_i == t {
            // Exact hit — no interpolation.
            self.read_slot((i & self.mask) as usize)?
        } else {
            let t_j = self.stamp_at(i + 1);
            let a = self.read_slot((i & self.mask) as usize)?;
            let b = self.read_slot(((i + 1) & self.mask) as usize)?;
            // t_i < t < t_j guaranteed here, so the denominator is non-zero.
            let s = span_ns(t_i, t) / span_ns(t_i, t_j);
            I::eval(&a, &b, s)
        };

        // Revalidate: if the ring lapped past `i` while we read, the endpoints we
        // used may be stale. Return the error rather than looping; the caller
        // knows whether a retry makes sense. The bound is `retained`, not
        // `capacity`: `head - i == capacity` already means slot `i` is the one
        // `push` is overwriting.
        if self.head.load(Ordering::Acquire) - i > retained {
            return Err(LookupError::SlotRecycled { edge: self.edge });
        }
        Ok(result)
    }

    /// Sample at stamp `t` like [`Self::sample`], but resume the bracket search
    /// from the logical index in `cursor` using an exponential (galloping) search.
    ///
    /// For a monotone non-decreasing sweep of stamps this turns the per-query
    /// `O(log n)` binary search into `O(1)` amortized: each call gallops from the
    /// previous result rather than restarting at the window midpoint. `cursor` is
    /// updated to the lower bracket index found, so the next call resumes there.
    /// Pass a `cursor` seeded to `0` for the first call.
    ///
    /// The result is identical to [`Self::sample`] for the same `t`; only the
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
                ExtrapPolicy::Hold => self.read_slot((newest & self.mask) as usize),
                ExtrapPolicy::ConstantTwist => self
                    .constant_twist(lo_logical, newest, t, t_new)
                    .map(|(pose, _)| pose),
            };
        }
        if t == t_new {
            *cursor = newest;
            return self.read_slot((newest & self.mask) as usize);
        }

        // Here t_old <= t < t_new, so the window endpoints already bracket `t`
        // and `bracket_from`'s precondition holds.
        let i = self.bracket_from(lo_logical, newest, t, *cursor);
        *cursor = i;
        let t_i = self.stamp_at(i);

        let result = if t_i == t {
            self.read_slot((i & self.mask) as usize)?
        } else {
            let t_j = self.stamp_at(i + 1);
            let a = self.read_slot((i & self.mask) as usize)?;
            let b = self.read_slot(((i + 1) & self.mask) as usize)?;
            let s = span_ns(t_i, t) / span_ns(t_i, t_j);
            I::eval(&a, &b, s)
        };

        if self.head.load(Ordering::Acquire) - i > retained {
            return Err(LookupError::SlotRecycled { edge: self.edge });
        }
        Ok(result)
    }

    /// Load the stamp at a logical index (masked to physical). Relaxed is correct:
    /// the `head` Acquire load in [`Self::sample`] already ordered every stamp of
    /// a published sample into view, and the stamp arrays are atomic so even a
    /// racing overwrite of a since-lapped slot is not a data race.
    #[inline]
    fn stamp_at(&self, logical: u64) -> i64 {
        self.stamps[(logical & self.mask) as usize].load(Ordering::Relaxed)
    }

    /// Last logical index in `[lo, hi]` whose stamp is `<= t`, **branchlessly**.
    ///
    /// Caller guarantees `stamp[lo] <= t < stamp[hi]`, which `sample` and
    /// `sample_from` establish before calling. Under that precondition the
    /// result is always `< hi`, so `i + 1` is a valid index for the upper
    /// bracket.
    ///
    /// # Why branchless, and how much it is actually worth
    ///
    /// The textbook form is `if stamp <= t { lo = mid } else { hi = mid }`,
    /// whose branch is by construction a coin flip: a binary search able to
    /// predict its own comparisons would not need to make them. This form
    /// instead does `base += half * (cmp as u64)` — a multiply by 0 or 1 — and
    /// shrinks `len` unconditionally, so the trip count depends only on the
    /// window size.
    ///
    /// **The measured gain is small, and the reason is worth recording so nobody
    /// re-derives the same wrong expectation.** The hypothesis was that the
    /// branchy form was paying most of the fixture's 8.16 mispredicted branches
    /// per depth-3 lookup. It was not: LLVM already compiled that `if` to a
    /// `cmov`, so the two forms were nearly the same machine code. Rewriting it
    /// bought **1.2% fewer instructions** and, on the pinned `cost_model`,
    /// 219.2 -> 217.0 ns at capacity 4096 and 237.0 -> 231.2 ns at 16384 — real,
    /// largest where the search is deepest, and about a tenth of what was
    /// expected.
    ///
    /// It is kept because it is the same amount of code, it does not depend on
    /// the optimizer continuing to choose `cmov` across future edits, and its
    /// cost is independent of the stamp distribution. It is *not* kept on the
    /// strength of a branch-prediction argument, which did not survive contact
    /// with the measurement.
    ///
    /// The serial dependent-load chain remains — each probe's address depends on
    /// the previous result — and dominates: at ~1.7 ns/probe the search is
    /// latency-bound, not branch-bound.
    ///
    /// (The 8.16 figure comes from cachegrind, whose branch predictor is a
    /// simple two-level model, not a Zen 3 TAGE. It is sound for comparing two
    /// engines under the *same* model, which is how
    /// `docs/benchmarks/tf2.md` uses it, and should not be read as the count a
    /// real CPU incurs.)
    ///
    /// # Why not an interpolated seed
    ///
    /// `docs/design/fast-path.md` §5 proposed seeding from
    /// `lo + (t − t_lo)(hi − lo)/(t_hi − t_lo)` instead, which is exact for
    /// isochronous stamps. Its §10 made that conditional on measuring real data
    /// first, and `cargo run --example search_seed` did: on the recorded stream
    /// the seed lands a **median of 11–48 indices** from the answer, because a
    /// real robot's `/tf` publishing is intermittent — 29–44 gaps covering
    /// 50–71% of the timeline — so sample density is nowhere near uniform. That
    /// is more correction steps than binary search needs probes in total. The
    /// lever is falsified; this one subsumes its intent without assuming
    /// anything about the stamp distribution.
    #[inline]
    fn bracket(&self, lo: u64, hi: u64, t: i64) -> u64 {
        let mut base = lo;
        let mut len = hi - lo + 1;
        while len > 1 {
            let half = len / 2;
            // Mask, not multiply. `half * cmp` reads as branchless and is not:
            // measured at 1.38 mispredicts per call, LLVM emits a branch for it.
            // `0 - cmp` is 0 or all-ones, and `half & mask` is an AND the
            // backend cannot turn back into control flow.
            let cmp = u64::from(self.stamp_at(base + half) <= t);
            base = base.wrapping_add(half & 0u64.wrapping_sub(cmp));
            len -= half;
        }
        base
    }

    /// [`Self::bracket`], but seeded from `hint` by an exponential (galloping)
    /// search instead of restarted at the window midpoint.
    ///
    /// Caller guarantees the same precondition [`Self::bracket`] wants,
    /// `stamp[lo_logical] <= t < stamp[newest]`, and it is what makes the
    /// gallop safe to seed with anything: both arms exist only to hand
    /// `bracket` a sub-interval that still brackets `t`, so a stale, clamped or
    /// nonsensical `hint` costs probes and can never change the answer. That is
    /// the property `Guard::cursor` relies on.
    ///
    /// # One implementation, two samplers
    ///
    /// [`Self::sample_from`] and [`Self::sample_with_twist_from`] both come
    /// here. They differ in what they do with the bracket — one interpolates a
    /// pose, the other also differentiates the segment — never in how they find
    /// it. A second copy of this would be a second place for the
    /// `saturating_sub` arithmetic in the downward arm to be wrong, and that
    /// arm is unreachable from the in-tree callers (they seed cursors to `0`),
    /// so a divergence would sit untested until somebody's stamps descended.
    ///
    /// # Why `inline(always)` and not `inline`
    ///
    /// **Measured, and the difference is not small.** This code used to be
    /// written out inside `sample_from`, which is where `Plan::at`'s scalar
    /// lookup reaches it through `Guard::sample_hinted`. Hoisting it behind a
    /// plain `#[inline]` cost **12 %** on `examples/abi_cost.rs`'s depth-3
    /// lookup — 188.5 -> 210 ns native, 194.6 -> 223 ns through the C ABI, over
    /// three pinned runs each that agreed to 1 ns. `inline(always)` returns it
    /// to 188.5 / 194.6, i.e. to the byte-for-byte inline form.
    ///
    /// The likely reason is that `sample_from` is generic over `I: Interp` and
    /// this is not, so one shared body faces two monomorphized call sites and
    /// the inliner declines. Whatever the mechanism, the attribute is load
    /// bearing: **do not weaken it to `#[inline]`** without re-running that
    /// example pinned, because nothing else in the suite will notice.
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
        // Binary search within the galloped bracket, branchless — see
        // [`Self::bracket`]. Invariant: stamp[lo] <= t < stamp[hi].
        self.bracket(lo, hi, t)
    }

    /// The oldest logical index a reader may still touch, and the newest, for a
    /// ring known to be non-empty. Test-only; the sampling paths compute both
    /// from the `head` they already loaded rather than loading it twice.
    #[cfg(test)]
    pub(crate) fn window_for_test(&self) -> (u64, u64) {
        let h = self.head.load(Ordering::Acquire);
        let n = h.min(self.retained());
        (h - n, h - 1)
    }

    /// Sample at `t` **and** the body twist there, in units of 1/second —
    /// `docs/PHASE4.md` §2.3.
    ///
    /// ScLerp only. The caller (`Guard::sample_with_twist`) refuses `LerpSlerp`
    /// before reaching here, so this is not generic over [`Interp`]: there is
    /// exactly one policy with a derivative worth reporting, and making the
    /// signature pretend otherwise would invite someone to add the wrong one.
    ///
    /// # The four bracket-less outcomes, which the spec does not cover
    ///
    /// `Plan::at` has cases that produce a pose without a segment, and each
    /// needs an answer here rather than a plausible-looking zero:
    ///
    /// | case | pose | twist |
    /// |---|---|---|
    /// | `t > t_new`, [`ExtrapPolicy::Hold`] | newest, held | **zero** — held *is* stationary |
    /// | `t > t_new`, [`ExtrapPolicy::ConstantTwist`] | extrapolated | the extended segment's twist |
    /// | `t == t_new`, ≥ 2 samples | newest | the *preceding* segment's twist |
    /// | one sample, or a zero-length segment | fine | [`LookupError::NoSegment`] |
    ///
    /// The last row is why [`LookupError::NoSegment`] exists rather than reusing
    /// [`LookupError::NoData`]: the pose is well defined and only the derivative
    /// is missing, and telling a caller "no data" when there is data sends them
    /// to the wrong problem.
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

    /// [`Self::sample_with_twist`], resuming the bracket search from `cursor`
    /// by the same galloping search [`Self::sample_from`] uses.
    ///
    /// This is [`Self::sample_from`]'s counterpart for the derivative path, and
    /// it exists for the same reason: `Plan::at_many_into(Layout::QuatTwist)`
    /// is the n = 1024 batch `docs/API.md` §3.3 is written for, and without it
    /// that layout is the only one paying an `O(log n)` binary search per stamp
    /// per plan step while every pose layout pays `O(1)` amortized.
    ///
    /// Only the *start* of the search differs; `Self::bracket_from` is shared
    /// with `sample_from` and cannot return a different index than
    /// `Self::bracket` would. `cursor` is updated to the lower bracket index
    /// found, so the next call resumes there; seed it to `0` for the first.
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

    /// The body of [`Self::sample_with_twist`] and
    /// [`Self::sample_with_twist_from`], parameterized on how the bracket is
    /// found.
    ///
    /// `seek` is a distinct type at each of the two call sites, so each gets
    /// exactly the code it had — the cursor variant does not put a branch, a
    /// pointer or a spare compare into the cursor-less one, which is the scalar
    /// `at_with_derivatives` path.
    ///
    /// It is called only from the interpolating arm. The extrapolation arms and
    /// the `t == t_new` left-limit arm reach their index without a search at
    /// all, so a cursor passed through them is simply left where it was — still
    /// a valid hint, since a wrong one cannot produce a wrong result.
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
                    // The pose is pinned, so the velocity really is zero. This is
                    // not a fallback — it is the derivative of what Hold does.
                    return Ok((self.read_slot((newest & self.mask) as usize)?, Twist::ZERO));
                }
                ExtrapPolicy::ConstantTwist => {
                    // Degraded to Hold: there is a pose to extend from but no
                    // segment to extend *along*, so the derivative is missing
                    // while the pose is fine — which is exactly `NoSegment`.
                    if newest == lo_logical {
                        return Err(LookupError::NoSegment { edge: self.edge });
                    }
                    return self.constant_twist(lo_logical, newest, t, t_new);
                }
            }
        }

        // `t` is inside `[t_old, t_new]`, so a segment exists unless the ring
        // retains a single sample.
        if newest == lo_logical {
            return Err(LookupError::NoSegment { edge: self.edge });
        }
        // At the newest stamp there is no forward segment; the body twist is
        // piecewise-constant per segment, so the value there is the left limit —
        // the segment that *ends* at that knot. Everywhere else `bracket`'s
        // precondition (`stamp[lo] <= t < stamp[hi]`) holds and it returns
        // `i < newest`, so `i + 1` is in range.
        let i = if t == t_new {
            newest - 1
        } else {
            seek(self, lo_logical, newest, t)
        };
        let t_i = self.stamp_at(i);
        let t_j = self.stamp_at(i + 1);
        let dt = span_ns(t_i, t_j);
        if dt == 0.0 {
            // Equal stamps are legal (invariant 6) but span no time, so the
            // velocity would be infinite rather than merely unknown.
            return Err(LookupError::NoSegment { edge: self.edge });
        }
        let a = self.read_slot((i & self.mask) as usize)?;
        let b = self.read_slot(((i + 1) & self.mask) as usize)?;
        let s = span_ns(t_i, t) / dt;
        let (pose, xi) = ScLerp::eval_with_twist(&a, &b, s);

        // Same revalidation and the same bound as `sample`: if the ring lapped
        // past `i` while we read, both the pose and the twist are stale.
        if self.head.load(Ordering::Acquire) - i > retained {
            return Err(LookupError::SlotRecycled { edge: self.edge });
        }
        // ξ is per unit `s`; `s` spans `dt` nanoseconds.
        Ok((pose, xi.scale(NANOS_PER_SEC / dt)))
    }

    /// [`ExtrapPolicy::ConstantTwist`] extrapolation past the newest sample,
    /// returning the extrapolated pose **and** the twist it was extended along.
    ///
    /// # One decomposition, one revalidation
    ///
    /// Both outputs come from a single [`ScLerp::eval_with_twist`] on a single
    /// read of the two newest slots. That is not a micro-optimization; it is what
    /// makes the read sound. An earlier split — `constant_twist` for the pose and
    /// a separate `segment_twist` for the twist — read the same two slots twice
    /// and revalidated only the first pair, so a writer that lapped the ring
    /// between them produced a pose that was correctly rejected beside a twist
    /// that was silently wrong. `read_slot`'s seqlock proves each pose is
    /// internally consistent, never that two poses came from the same era; only
    /// the trailing `head - prev > retained` check does that, and it has to cover
    /// every slot the result depends on.
    ///
    /// `Twist::ZERO` accompanies the degraded single-sample case: there is a pose
    /// to hold but no segment to differentiate. Callers that need to distinguish
    /// "held, so stationary" from "extended along a real twist" check
    /// `newest == lo_logical` themselves, as [`Self::sample_with_twist`] does.
    ///
    /// The screw route here is the same one [`ScLerp`] uses everywhere else,
    /// rather than the `log_se3`/`exp_se3` reference form this used to take —
    /// so `sample` and `sample_with_twist` agree bit-for-bit under this policy,
    /// and the extrapolation now costs one screw decomposition instead of a full
    /// log and exp.
    fn constant_twist(
        &self,
        lo_logical: u64,
        newest: u64,
        t: i64,
        t_new: i64,
    ) -> Result<(Iso3, Twist), LookupError> {
        if newest == lo_logical {
            // Only one sample retained: no twist to extend.
            return Ok((self.read_slot((newest & self.mask) as usize)?, Twist::ZERO));
        }
        let prev = newest - 1;
        let t_prev = self.stamp_at(prev);
        let a = self.read_slot((prev & self.mask) as usize)?;
        let b = self.read_slot((newest & self.mask) as usize)?;
        let dt = span_ns(t_prev, t_new);
        let result = if dt == 0.0 {
            // Equal stamps span no time: nothing to extend along, and the
            // velocity would be infinite rather than unknown.
            (b, Twist::ZERO)
        } else {
            // Constant screw twist of a->b, extended to `t`. `param > 1` walks
            // past `b` along the same screw; at `t == t_new` it is exactly 1 and
            // reproduces `b`.
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

/// Lift a hint whose high bits may have been discarded back onto the live
/// window.
///
/// # Why a hint arrives truncated
///
/// [`Guard`](crate::plan::Guard) packs a per-step search cursor and its edge tag
/// into one `u64` cell, so the cursor it stores is the low 32 bits of a logical
/// index. `head` is monotone for the life of the arena and is never masked, so
/// once an edge passes 2^32 pushes — 49.7 days of unbroken 1 kHz publishing —
/// every stored hint is smaller than `lo_logical`, and a plain
/// `clamp(lo_logical, newest)` pins it to the *oldest* retained sample on every
/// call thereafter. The resumed gallop then walks the whole window from the far
/// end: still correct (a hint can never change a result), but permanently worse
/// than the midpoint restart it was added to beat, and permanently past the
/// point where any test would notice. That is a cliff, not a decay — before
/// 2^32 the cursor is exact and after it, it is inert forever.
///
/// # Why the lift is exact rather than a heuristic
///
/// The readable window is `retained` wide and `retained = capacity - 1` with
/// `capacity: u32` ([`Capacity`](crate::edge::EdgeCfg)), so the window is
/// *strictly* narrower than 2^32 and can straddle at most one multiple of it.
/// A truncated index therefore has exactly one preimage in the window: the one
/// in `newest`'s 2^32 block, or — when the window straddles the boundary and the
/// hint's low bits sit above `newest`'s — the one in the block below. Both are
/// recovered here with two arithmetic operations and no load.
///
/// # Why this is not a behaviour change below 2^32
///
/// The lift is reached only when `hint < lo_logical`, and while `head < 2^32`
/// the block base of `newest` is `0`, so `lifted == hint` and the subsequent
/// clamp pins it to `lo_logical` exactly as before. Every existing caller —
/// including [`SampleRing::sample_from`]'s public cursor contract, which is an
/// absolute logical index — sees identical behaviour until the regime where the
/// old behaviour was already useless.
#[inline(always)]
pub(crate) fn rebase_hint(hint: u64, lo_logical: u64, newest: u64) -> u64 {
    /// One more than the largest value `Guard`'s packed cursor can represent.
    const BLOCK: u64 = 1 << 32;
    if hint >= lo_logical {
        // Already absolute and inside (or ahead of) the window; the caller's
        // clamp handles the `> newest` end. This is the hot case — a warm
        // cursor from the previous query on the same edge — so it costs one
        // predictable compare.
        return hint;
    }
    let lifted = (newest & !(BLOCK - 1)) | (hint & (BLOCK - 1));
    if lifted > newest {
        // The window straddles a 2^32 boundary and this hint belongs to the
        // block below it. `newest >= lifted - BLOCK` cannot underflow: `lifted`
        // and `newest` share a block base, so `lifted > newest` implies
        // `newest >= BLOCK`.
        lifted - BLOCK
    } else {
        lifted
    }
}
