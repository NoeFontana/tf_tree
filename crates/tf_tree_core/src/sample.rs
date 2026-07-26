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

use tf_tree_math::{exp_se3, log_se3, Interp, Iso3, ScLerp, Twist};

/// Nanoseconds per second, as the `f64` the twist scaling needs.
const NANOS_PER_SEC: f64 = 1_000_000_000.0;

use crate::buffer::SampleRing;
use crate::error::LookupError;
use crate::sync::Ordering;

/// What to do when the requested stamp is newer than every published sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
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
                ExtrapPolicy::ConstantTwist => self.constant_twist(lo_logical, newest, t, t_new),
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
            let s = (t - t_i) as f64 / (t_j - t_i) as f64;
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
                ExtrapPolicy::ConstantTwist => self.constant_twist(lo_logical, newest, t, t_new),
            };
        }
        if t == t_new {
            *cursor = newest;
            return self.read_slot((newest & self.mask) as usize);
        }

        // Exponential (galloping) search for the last logical index whose stamp is
        // <= t, resuming from `cursor`. Here t_old <= t < t_new, so the window
        // endpoints already bracket `t`: stamp[lo_logical] <= t < stamp[newest].
        let hint = (*cursor).clamp(lo_logical, newest);
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
        let i = self.bracket(lo, hi, t);
        *cursor = i;
        let t_i = self.stamp_at(i);

        let result = if t_i == t {
            self.read_slot((i & self.mask) as usize)?
        } else {
            let t_j = self.stamp_at(i + 1);
            let a = self.read_slot((i & self.mask) as usize)?;
            let b = self.read_slot(((i + 1) & self.mask) as usize)?;
            let s = (t - t_i) as f64 / (t_j - t_i) as f64;
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
                    let pose = self.constant_twist(lo_logical, newest, t, t_new)?;
                    if newest == lo_logical {
                        // ConstantTwist degraded to Hold: one sample, no twist.
                        return Err(LookupError::NoSegment { edge: self.edge });
                    }
                    let tw = self.segment_twist(newest - 1)?;
                    return Ok((pose, tw));
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
            self.bracket(lo_logical, newest, t)
        };
        let t_i = self.stamp_at(i);
        let t_j = self.stamp_at(i + 1);
        let dt = (t_j - t_i) as f64;
        if dt == 0.0 {
            // Equal stamps are legal (invariant 6) but span no time, so the
            // velocity would be infinite rather than merely unknown.
            return Err(LookupError::NoSegment { edge: self.edge });
        }
        let a = self.read_slot((i & self.mask) as usize)?;
        let b = self.read_slot(((i + 1) & self.mask) as usize)?;
        let s = (t - t_i) as f64 / dt;
        let (pose, xi) = ScLerp::eval_with_twist(&a, &b, s);

        // Same revalidation and the same bound as `sample`: if the ring lapped
        // past `i` while we read, both the pose and the twist are stale.
        if self.head.load(Ordering::Acquire) - i > retained {
            return Err(LookupError::SlotRecycled { edge: self.edge });
        }
        // ξ is per unit `s`; `s` spans `dt` nanoseconds.
        Ok((pose, xi.scale(NANOS_PER_SEC / dt)))
    }

    /// The body twist of the segment `[i, i+1]`, in units of 1/second.
    ///
    /// Shared by the `ConstantTwist` arm, which needs the twist of a segment it
    /// has already located.
    fn segment_twist(&self, i: u64) -> Result<Twist, LookupError> {
        let t_i = self.stamp_at(i);
        let t_j = self.stamp_at(i + 1);
        let dt = (t_j - t_i) as f64;
        if dt == 0.0 {
            return Err(LookupError::NoSegment { edge: self.edge });
        }
        let a = self.read_slot((i & self.mask) as usize)?;
        let b = self.read_slot(((i + 1) & self.mask) as usize)?;
        let (_, xi) = ScLerp::eval_with_twist(&a, &b, 0.0);
        Ok(xi.scale(NANOS_PER_SEC / dt))
    }

    /// [`ExtrapPolicy::ConstantTwist`] extrapolation past the newest sample.
    fn constant_twist(
        &self,
        lo_logical: u64,
        newest: u64,
        t: i64,
        t_new: i64,
    ) -> Result<Iso3, LookupError> {
        if newest == lo_logical {
            // Only one sample retained: no twist to extend.
            return self.read_slot((newest & self.mask) as usize);
        }
        let prev = newest - 1;
        let t_prev = self.stamp_at(prev);
        let a = self.read_slot((prev & self.mask) as usize)?;
        let b = self.read_slot((newest & self.mask) as usize)?;
        let dt = (t_new - t_prev) as f64;
        let result = if dt == 0.0 {
            b
        } else {
            // Constant screw twist of a->b, extended to `t`. At t == t_new this
            // reproduces `b` exactly (param == 1).
            let twist = log_se3(a.inverse() * b);
            let param = (t - t_prev) as f64 / dt;
            let scaled = [
                twist[0] * param,
                twist[1] * param,
                twist[2] * param,
                twist[3] * param,
                twist[4] * param,
                twist[5] * param,
            ];
            a * exp_se3(scaled)
        };
        if self.head.load(Ordering::Acquire) - prev > self.retained() {
            return Err(LookupError::SlotRecycled { edge: self.edge });
        }
        Ok(result)
    }
}
