//! Bracket search over an edge's sample ring.
//!
//! The search runs over **logical** indices with `& mask` on every probe:
//! probing the physical array is wrong once the ring has wrapped
//! (`docs/PHASE1.md` §6.4). The window is `[head - n, head - 1]` with
//! `n = min(head, `[`SampleRing::retained`]`)`, and `retained` is
//! `capacity - 1`: logical `head - capacity` shares a slot with the sample
//! `push` is writing now. The window and the trailing revalidation use that
//! same bound — keep them in step. `unsafe`-free: the [`SampleRing`] atomics
//! are reached only through [`crate::buffer`]'s safe `push`/`read_slot`.
//!
//! # Two hazards the trailing revalidation does **not** cover
//!
//! Both found 2026-08-29; open, because closing either costs the hot path.
//!
//! 1. **Two concurrent pushes corrupt the search.** `push` destroys logical
//!    `head - capacity`, one below the window, so stamps stop being monotone
//!    inside it and the binary search can return an arbitrary index that still
//!    passes `head - i > retained`: a non-bracketing blend, no error. Closing
//!    it costs two `Relaxed` loads re-checking `t_i`/`t_j`.
//! 2. **[`SampleRing::newest_stamp`] can report a later lap's stamp** — it
//!    loads `head`, then `stamps[(head - 1) & mask]`, nothing between: an
//!    estimate of the frontier, not a bound, so never a staleness baseline.

use tf_tree_math::{Interp, Iso3, ScLerp, Twist};

/// Nanoseconds per second, as the `f64` the twist scaling needs.
const NANOS_PER_SEC: f64 = 1_000_000_000.0;

use crate::buffer::SampleRing;
use crate::error::LookupError;
use crate::sync::Ordering;

/// What to do when the requested stamp is newer than every published sample.
///
/// `#[non_exhaustive]`: callers only pass one and only this crate dispatches,
/// so a fourth cannot make a consumer silently wrong. Contrast
/// [`crate::plan::InterpPolicy`], which downstream crates map exhaustively.
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

/// Nanoseconds from `from` to `to` as an `f64`, for a caller-ordered pair
/// (`from <= to`).
///
/// The `u64` subtraction is load-bearing: the difference need not fit an
/// `i64`. Signed subtraction panicked in a checked build and, worse, wrapped in
/// release — a negative `t_j - t_i` makes `s` negative, so `Interp::eval` runs
/// backwards past the older sample and returns a pose from outside the bracket,
/// silently, on the hot path. `wrapping_sub` on the bit patterns *is* the true
/// distance for every ordered `i64` pair. A width bound belongs upstream in
/// `push` as a `LookupError` naming the edge (R5); `from > to` is a caller bug,
/// ruled out at every call site by the preceding comparison.
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
    /// [`LookupError::NoData`] (empty ring); [`LookupError::Extrapolation`]
    /// (`t` outside the retained window, or past the newest under
    /// [`ExtrapPolicy::Error`]); [`LookupError::SlotContended`] (slot mid-write
    /// too long); [`LookupError::SlotRecycled`] (the ring lapped mid-read).
    pub fn sample<I: Interp>(&self, t: i64, policy: ExtrapPolicy) -> Result<Iso3, LookupError> {
        // Acquire: every stamp below was written before the matching `head`
        // store, so this load orders them into view.
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
            // t_i < t < t_j guaranteed here, so the denominator is non-zero.
            let s = span_ns(t_i, t) / span_ns(t_i, t_j);
            I::eval(&a, &b, s)
        };

        // A lap past `i` stales the endpoints. Error rather than loop; only
        // the caller knows whether a retry makes sense. Bound is `retained`:
        // `head - i == capacity` is already the slot `push` is overwriting.
        if self.head.load(Ordering::Acquire) - i > retained {
            return Err(LookupError::SlotRecycled { edge: self.edge });
        }
        Ok(result)
    }

    /// Hand back `v` unless the ring lapped past logical index `i` while it
    /// was being read.
    ///
    /// One spelling, because seven places need it and six lacked it: every
    /// short-circuiting arm (`Hold`, an exact hit on the newest stamp,
    /// `constant_twist`'s single sample) returned `read_slot` directly and
    /// could silently return a pose from a different stamp; a 64-slot ring
    /// laps in 64 ms at 1 kHz, one ordinary preemption. Bound is `retained`,
    /// since `head - i == capacity` is the slot `push` is overwriting.
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

    /// Sample at stamp `t` like [`Self::sample`], but resume the bracket search
    /// from the logical index in `cursor` by an exponential (galloping) search:
    /// `O(1)` amortized instead of `O(log n)` over a monotone sweep. `cursor`
    /// is updated to the lower bracket index found; seed it to `0` for the
    /// first call. Same result as [`Self::sample`]; only the search differs.
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
                ExtrapPolicy::Hold => self
                    .read_slot((newest & self.mask()) as usize)
                    .and_then(|p| self.revalidated(newest, retained, p)),
                ExtrapPolicy::ConstantTwist => self
                    .constant_twist(lo_logical, newest, t, t_new)
                    .map(|(pose, _)| pose),
            };
        }
        if t == t_new {
            *cursor = newest;
            let p = self.read_slot((newest & self.mask()) as usize)?;
            return self.revalidated(newest, retained, p);
        }

        // t_old <= t < t_new here, so `bracket_from`'s precondition holds.
        let i = self.bracket_from(lo_logical, newest, t, *cursor);
        *cursor = i;
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

    /// Load the stamp at a logical index (masked to physical).
    ///
    /// Relaxed is correct: [`Self::sample`]'s Acquire load of `head` already
    /// ordered every published stamp into view, and the arrays are atomic, so a
    /// racing overwrite of a lapped slot is not a data race. Pinned since
    /// 2026-09-08 by `head_publishes_every_stamp_below_it` (`loom_tests.rs`),
    /// which fails when `push`'s `head` store is weakened to `Relaxed`: this is
    /// the load that would then bracket the wrong pair and return a plausible,
    /// wrong pose.
    #[inline]
    fn stamp_at(&self, logical: u64) -> i64 {
        self.stamps[(logical & self.mask()) as usize].load(Ordering::Relaxed)
    }

    /// Last logical index in `[lo, hi]` whose stamp is `<= t`.
    ///
    /// Caller guarantees `stamp[lo] <= t < stamp[hi]`, so the result is `< hi`
    /// and `i + 1` is a valid upper bracket.
    ///
    /// # Not branchless — this section used to claim it was
    ///
    /// LLVM folds the mask (`base += half & (0 - cmp)`) back into a `select` on
    /// the loop-carried chain, which x86 cmov-conversion expands into control
    /// flow: every inlined copy in the shipped `--release` rlib is
    /// `cmpq`/`jle`/`xorl`, no `cmov`. So the cost depends on the stamp
    /// distribution, and on `soak --workload robot` this is the process's
    /// largest single source of mispredicts.
    /// [`0053`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0053-the-branchless-bracket-that-branches.md)
    /// measures the plain `if` as *cheaper* on instructions and mispredicts,
    /// and carries the cachegrind table (a two-level predictor model:
    /// comparable across builds, not a real CPU's count) and three unlanded
    /// alternatives. Branch versus serial dependent-load chain is untimed:
    /// `perf_event_paranoid=4` on the dev host. **Do not restore a
    /// branchlessness claim without re-running this:**
    ///
    /// ```text
    /// cargo rustc -p tf_tree_core --release --lib -- --emit asm -C debuginfo=2
    /// F=$(ls -t target/release/deps/tf_tree_core-*.s | head -1)
    /// ID=$(awk '/^\t\.file\t[0-9]+ .*sample\.rs"/ {print $2; exit}' "$F")
    /// LN=$(grep -n '^ *base = base.wrapping_add(half &' crates/tf_tree_core/src/sample.rs | cut -d: -f1)
    /// grep -c -P "\\.loc\\t$ID $LN " "$F"                       # inlined copies of this line
    /// grep -B12 -A4 -P "\\.loc\\t$ID $LN " "$F" | grep -c cmov  # 0
    /// ```
    ///
    /// An interpolated seed (`docs/design/fast-path.md` §5, exact for
    /// isochronous stamps) is falsified, not deferred: §10 gated it on real
    /// data and `cargo run --example search_seed` found it lands a **median of
    /// 11–48 indices** off — real `/tf` publishing is intermittent, 29–44 gaps
    /// over 50–71% of the timeline — more correction steps than probes saved.
    #[inline]
    fn bracket(&self, lo: u64, hi: u64, t: i64) -> u64 {
        let mut base = lo;
        let mut len = hi - lo + 1;
        while len > 1 {
            let half = len / 2;
            // Mask, not multiply — and the backend turns this one back into
            // Mask, not multiply — but the backend turns this back into
            // control flow too; do not restore a branchlessness claim here.
            let cmp = u64::from(self.stamp_at(base + half) <= t);
            base = base.wrapping_add(half & 0u64.wrapping_sub(cmp));
            len -= half;
        }
        base
    }

    /// [`Self::bracket`], but seeded from `hint` by an exponential (galloping)
    /// search instead of restarted at the window midpoint.
    ///
    /// Caller guarantees [`Self::bracket`]'s precondition,
    /// `stamp[lo_logical] <= t < stamp[newest]`. Both arms only narrow to a
    /// sub-interval that still brackets `t`, so a stale, clamped or nonsensical
    /// `hint` costs probes and can never change the answer — the property
    /// `Guard::cursor` relies on. Shared with [`Self::sample_with_twist_from`]:
    /// a copy would be a second place for the downward arm's `saturating_sub`
    /// to be wrong, and that arm is unreachable from in-tree callers (cursors
    /// seeded to `0`), so a divergence would sit untested.
    ///
    /// `inline(always)` is measured: behind a plain `#[inline]` this cost
    /// **12 %** on `examples/abi_cost.rs`'s depth-3 lookup — 188.5 -> 210 ns
    /// native, 194.6 -> 223 ns through the C ABI, three pinned runs agreeing to
    /// 1 ns — likely because `sample_from` is generic over `I: Interp` and this
    /// is not. **Do not weaken it** without re-running that example pinned.
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
        // Invariant into `bracket`: stamp[lo] <= t < stamp[hi].
        self.bracket(lo, hi, t)
    }

    /// Oldest and newest logical indices a reader may touch, for a non-empty
    /// ring. Test-only; the sampling paths reuse the `head` they already have.
    #[cfg(test)]
    pub(crate) fn window_for_test(&self) -> (u64, u64) {
        let h = self.head.load(Ordering::Acquire);
        let n = h.min(self.retained());
        (h - n, h - 1)
    }

    /// Sample at `t` **and** the body twist there, in units of 1/second —
    /// `docs/PHASE4.md` §2.3.
    ///
    /// ScLerp only, not generic over [`Interp`]: `Guard::sample_with_twist`
    /// refuses `LerpSlerp` first, and no other policy has a useful derivative.
    ///
    /// # The four bracket-less outcomes, which the spec does not cover
    ///
    /// | case | pose | twist |
    /// |---|---|---|
    /// | `t > t_new`, [`ExtrapPolicy::Hold`] | newest, held | **zero** — held *is* stationary |
    /// | `t > t_new`, [`ExtrapPolicy::ConstantTwist`] | extrapolated | the extended segment's twist |
    /// | `t == t_new`, ≥ 2 samples | newest | the *preceding* segment's twist |
    /// | one sample, or a zero-length segment | fine | [`LookupError::NoSegment`] |
    ///
    /// The last row is why [`LookupError::NoSegment`] is not
    /// [`LookupError::NoData`]: the pose is well defined, only the twist is
    /// missing.
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
    /// by the galloping search [`Self::sample_from`] uses.
    ///
    /// Without it `Plan::at_many_into(Layout::QuatTwist)` — the n = 1024 batch
    /// `docs/API.md` §3.3 is written for — is the only layout paying `O(log n)`
    /// per stamp per plan step. Only the search's *start* differs. `cursor` is
    /// updated to the lower bracket index found; seed it to `0`.
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
    /// found. `seek` is a distinct type at each call site, so the cursor
    /// variant puts no branch, pointer or spare compare into the cursor-less
    /// scalar `at_with_derivatives` path. Only the interpolating arm calls it;
    /// the others need no search and leave the cursor where it was — still a
    /// valid hint, since a wrong one cannot produce a wrong result.
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
                    // The pose is pinned, so zero is Hold's derivative rather
                    // than a fallback.
                    let p = self.read_slot((newest & self.mask()) as usize)?;
                    return self.revalidated(newest, retained, (p, Twist::ZERO));
                }
                ExtrapPolicy::ConstantTwist => {
                    // A pose to extend from but no segment to extend along:
                    // the derivative is missing while the pose is fine.
                    if newest == lo_logical {
                        return Err(LookupError::NoSegment { edge: self.edge });
                    }
                    return self.constant_twist(lo_logical, newest, t, t_new);
                }
            }
        }

        // A segment exists here unless the ring retains a single sample.
        if newest == lo_logical {
            return Err(LookupError::NoSegment { edge: self.edge });
        }
        // No forward segment at the newest stamp, and the body twist is
        // piecewise-constant per segment, so its value there is the left limit.
        // Elsewhere `bracket`'s precondition gives `i < newest`.
        let i = if t == t_new {
            newest - 1
        } else {
            seek(self, lo_logical, newest, t)
        };
        let t_i = self.stamp_at(i);
        let t_j = self.stamp_at(i + 1);
        let dt = span_ns(t_i, t_j);
        if dt == 0.0 {
            // Legal (invariant 6) but spans no time: infinite, not unknown.
            return Err(LookupError::NoSegment { edge: self.edge });
        }
        let a = self.read_slot((i & self.mask()) as usize)?;
        let b = self.read_slot(((i + 1) & self.mask()) as usize)?;
        let s = span_ns(t_i, t) / dt;
        let (pose, xi) = ScLerp::eval_with_twist(&a, &b, s);

        // Same revalidation and bound as `sample`: a lap stales both outputs.
        if self.head.load(Ordering::Acquire) - i > retained {
            return Err(LookupError::SlotRecycled { edge: self.edge });
        }
        // ξ is per unit `s`; `s` spans `dt` nanoseconds.
        Ok((pose, xi.scale(NANOS_PER_SEC / dt)))
    }

    /// [`ExtrapPolicy::ConstantTwist`] extrapolation past the newest sample,
    /// returning the extrapolated pose **and** the twist it was extended along.
    ///
    /// One [`ScLerp::eval_with_twist`] on one read of the two newest slots is
    /// what makes the read sound: an earlier split into a separate
    /// `segment_twist` read the slots twice and revalidated only the first,
    /// pairing a correctly rejected pose with a silently wrong twist. The
    /// seqlock proves a pose internally consistent, never that two came from
    /// the same era — only the trailing `head - prev > retained` check does,
    /// and it must cover every slot the result depends on.
    ///
    /// `Twist::ZERO` marks the degraded single-sample case; callers separating
    /// "held" from "extended along a real twist" test `newest == lo_logical`,
    /// as [`Self::sample_with_twist`] does. The screw route is [`ScLerp`]'s
    /// own, not the `log_se3`/`exp_se3` reference form this once took, so
    /// `sample` and `sample_with_twist` agree bit-for-bit at one screw
    /// decomposition.
    fn constant_twist(
        &self,
        lo_logical: u64,
        newest: u64,
        t: i64,
        t_new: i64,
    ) -> Result<(Iso3, Twist), LookupError> {
        if newest == lo_logical {
            // Only one sample retained: no twist to extend.
            let p = self.read_slot((newest & self.mask()) as usize)?;
            return self.revalidated(newest, self.retained(), (p, Twist::ZERO));
        }
        let prev = newest - 1;
        let t_prev = self.stamp_at(prev);
        let a = self.read_slot((prev & self.mask()) as usize)?;
        let b = self.read_slot((newest & self.mask()) as usize)?;
        let dt = span_ns(t_prev, t_new);
        let result = if dt == 0.0 {
            // Equal stamps span no time: infinite, not merely unknown.
            (b, Twist::ZERO)
        } else {
            // Constant screw twist of a->b, extended to `t`. `param > 1` walks
            // past `b` along the same screw; at `t == t_new` it is exactly 1.
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
/// [`Guard`](crate::plan::Guard) packs a search cursor and its edge tag into
/// one `u64`, so the stored cursor is the low 32 bits of a logical index.
/// `head` is monotone and never masked, so past 2^32 pushes — 49.7 days of
/// unbroken 1 kHz publishing — every hint falls below `lo_logical` and a plain
/// clamp would pin it to the *oldest* sample forever: correct, but permanently
/// worse than the midpoint restart it replaced, and past where any test would
/// notice. A cliff, not a decay.
///
/// The lift is exact, not a heuristic: the window is `retained = capacity - 1`
/// wide with `capacity: u32` ([`Capacity`](crate::edge::EdgeCfg)), strictly
/// narrower than 2^32, so it straddles at most one multiple and a truncated
/// index has exactly one preimage in it. Below 2^32 nothing changes — the lift
/// runs only when `hint < lo_logical`, where `newest`'s block base is `0` — so
/// [`SampleRing::sample_from`]'s absolute-index cursor contract is intact.
#[inline(always)]
pub(crate) fn rebase_hint(hint: u64, lo_logical: u64, newest: u64) -> u64 {
    /// One more than the largest value `Guard`'s packed cursor can represent.
    const BLOCK: u64 = 1 << 32;
    if hint >= lo_logical {
        // Already absolute and inside or ahead of the window; the caller's
        // clamp handles the `> newest` end. Hot case: one predictable compare.
        return hint;
    }
    let lifted = (newest & !(BLOCK - 1)) | (hint & (BLOCK - 1));
    if lifted > newest {
        // The window straddles a 2^32 boundary and this hint belongs to the
        // block below. `lifted - BLOCK` cannot underflow: `lifted` and `newest`
        // share a block base, so `lifted > newest` implies `newest >= BLOCK`.
        lifted - BLOCK
    } else {
        lifted
    }
}
