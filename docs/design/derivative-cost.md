# Design: what `at_with_derivatives` actually costs

**Status:** implemented; one tuning lever **falsified by measurement** (§4).
**Measured on:** AMD EPYC-Milan, 4 physical cores, idle, `taskset -c 2`
**Reproduce:**
`cargo run --release -p tf_tree_bench --example deriv_cost`
`cargo run --release -p tf_tree_bench --example adjoint_cost`

---

## 1. The spec's cost model was wrong twice, in opposite directions

`docs/PHASE4.md` §2 made two quantitative claims about
`at_with_derivatives`. Both were checked against the shipped code and both were
wrong — one optimistic, one pessimistic:

| Claim | Reality |
|---|---|
| §2.3: "ξ is *already computed* … the first derivative costs one scalar multiply" | **False.** `ScLerp::eval` never materializes ξ. Measured cost of recovering it: **+1.6 ns on a 43 ns interpolation, 1.04×.** |
| §2.3 / §7: "roughly 2× a plain lookup" | **Pessimistic.** Measured **1.22× at depth 3.** |

Both amendments are recorded inline in `PHASE4.md` §2.3 rather than here, so a
reader of the spec meets them where the wrong number was.

## 2. ξ-recovery is nearly free, and the branch is why

`ScLerp::eval_with_twist` vs `ScLerp::eval`, on identical pose pairs:

| regime | rel. angle | `eval` ns | `eval_with_twist` ns | ratio |
|---|---|---|---|---|
| adjacent 1 kHz | 3.0e-3 | 42.95 | 44.51 | **1.04** |
| 200 Hz | 1.6e-2 | 42.97 | 44.57 | 1.04 |
| 50 Hz | 6.3e-2 | 42.89 | 44.50 | 1.04 |
| slow 10 Hz | 3.14e-1 | 51.85 | 55.04 | 1.06 |
| large arc | 1.5 | 58.31 | 61.82 | 1.06 |
| near-identity | 1e-9 | 42.99 | 44.49 | 1.03 |

**The ratio is flat**, which is the point worth reading. `ScrewParts::twist`
mirrors `pow`'s branch: on the small-angle path — everything up to 50 Hz above,
i.e. essentially all real traffic — `φ` comes from a `sqrt` of the
`theta_sq_from_chord` value the power already computes, so **no transcendental is
added at all**. Calling `atan2` unconditionally would be correct and would show
up as a step in this column at the top three rows.

## 2a. The endpoints skip the power, and it is worth 60%

`ScLerp::eval_with_twist` tests `s == 0.0` / `s == 1.0` **before** raising `rel`
to a power, not after. `ScrewParts::pow` is the half of the decomposition that
carries the transcendental on the large-arc branch, and at the endpoints its
result is discarded; LLVM does not sink the call out of the untaken branch. The
twist is still computed — it is a property of the segment, not of `s` — so only
the power is skipped.

| regime | interior ns | endpoint ns | saved |
|---|---|---|---|
| adjacent 1 kHz | 54.60 | 22.62 | **58.6%** |
| large arc | 69.02 | 27.59 | **60.0%** |

This is not a rare path. `sample_with_twist` produces `s == 0.0` exactly on an
**exact hit on a published sample**, and `s == 1.0` exactly at **`t == t_new`** —
which is where every `latest`-style query lands. Raised in review; the original
code computed and threw away the power at both.

## 3. At plan level, the adjoint is the cost — not ξ

| depth | `at` ns | `at_with_derivatives` ns | ratio | delta/step ns |
|---|---|---|---|---|
| 1 | 97.8 | 118.1 | 1.21 | 20.30 |
| 3 | 281.1 | 343.7 | 1.22 | **20.87** |
| 8 | 738.1 | 907.9 | 1.23 | 21.23 |

The per-step delta is **flat at ~21 ns**, which is what a per-step adjoint should
look like and is the strongest evidence the fold is doing what §2.3 describes
rather than something depth-dependent.

Note the split: ξ-recovery is 1.6 ns of that 21. **The adjoint and the
`sample` → `sample_with_twist` path change are the other ~19 ns**, so any tuning
belongs there and not in the screw math.

**§7 gate row: 1.22× at depth 3, against a stated expectation of ~2×.** Passes
with room.

## 4. Falsified: replacing the quaternion adjoint with a matrix form

`Iso3::adjoint_inv` is two `Quat::rotate` calls plus a cross — about 40 flops.
At ~10 ns sequential that is ~4 flops/ns, well off this core's throughput, which
says the chain is **latency**-bound. `Quat::rotate` is already the optimized
Rodrigues form, but its chain is deep (`cross → scale → scale → add → cross →
add`), and in `adjoint_inv` the second rotation's input depends on a further
cross.

Hypothesis: build the rotation matrix once (12 mutually independent flops), then
two mat-vecs whose nine products are mutually independent. Same arithmetic, far
more ILP.

Measured, after confirming the two forms agree to 7.5e-16:

| regime | quaternion ns | matrix ns | speedup |
|---|---|---|---|
| independent inputs | 1.45 | 1.62 | **0.90** |
| sequential (as the fold runs) | 10.62 | 9.60 | **1.11** |

**Rejected.** Two reasons, and the threshold was set before the measurement:

1. **1.11× is below the 1.15× bar** for introducing a second code path. It would
   buy ~1 ns per plan step on an opt-in API already inside its budget by a factor
   of 1.6.
2. **The quaternion form is *faster* on independent inputs** (0.90×). A fold that
   ever became parallel across steps, or a batch `at_many_with_derivatives`, would
   regress. Trading a win in the general case for 1 ns in the sequential one is
   the wrong direction.

The probe is kept as `examples/adjoint_cost.rs` so the next person with this
idea can re-run it in one command rather than re-deriving it.

## 5. What is still worth trying, if this ever needs to be faster

Not attempted, in rough order of expected return:

- **The ~19 ns that is not ξ and not the adjoint.** `sample_with_twist`
  duplicates `sample`'s bracket-and-revalidate structure with extra branches for
  the four bracket-less cases. Merging them behind a const-generic
  `WANT_TWIST: bool` would let the compiler delete the twist work entirely from
  the plain path while sharing one search. Measure before writing it: `sample`
  is the hottest function in the system and a regression there is far more
  expensive than this API is worth.
- **Batching.** `at_many_with_derivatives` over the galloping cursor path would
  amortize the bracket search exactly as `at_many` already does, and the
  independent-input row above says the quaternion adjoint would benefit rather
  than suffer.

Neither has a requester. Recorded so that "is the derivative fast enough" has an
answer that is not a guess.
