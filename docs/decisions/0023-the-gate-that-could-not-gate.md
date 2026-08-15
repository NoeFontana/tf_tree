# 0023: §7 gate criterion 1 — three rungs at a real boundary, not one quotient at a fake one

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** the measurement half has landed (`just abi-cost`); the §7
gate table is unchanged and stays that way until this record is `ready`.

## Context

`docs/PHASE4.md` §7 gate criterion 1 reads: **"C ABI within 5% of native for
depth-3 lookup."** One quotient, one threshold. It has never gated anything, for
two compounding reasons that were found separately and are the same defect.

**The profile erased the boundary.** `crates/tf_tree_c/examples/abi_cost.rs` was
built at the workspace `release` profile, which is `lto = "thin"`. That inlines
`tft_plan_at` into a Rust caller, so the C boundary the criterion exists to price
was not present in the binary pricing it. `report.rs`'s PHASE5 §9.2 embedding row
had already recorded that exact trap in those words — thin LTO "is exactly what
erases the boundary" — and `[profile.embedder]` (`lto = false`) exists *because*
of it. Nothing had applied either to §7. Measured, the erasure is worth about
half the answer: the same ABI prices at **1.016–1.019×** with the boundary gone
and **1.025–1.038×** with it present.

**The denominator was at LLVM's discretion.** Adding a second, wholly unrelated
`Tree::guard()` call site to `abi_cost.rs` moved the native baseline from
**133 ns to 190 ns (43%)** and flipped the verdict FAIL → PASS. The ABI arm never
moved: 194–196 ns in every variant. With `tft_plan_at` inlined, both arms
collapse into one optimisable blob and the ratio turns on how well LLVM
specialises the comparand. Neither the old FAIL nor the subsequent PASS was a
statement about the C ABI.

**And this host cannot escape into absolute nanoseconds.** `report.rs`'s
`Fitness` refuses `Sensitivity::AbsoluteTiming` rows here — SMT on, unreadable
governor, four physical cores — and `fair_for_ratios` is the deliberately weaker
axis that survives, because common-mode drift lands on both arms of an
interleaved pair. During the measurements below the absolute baseline wandered
**217–248 ns** (a neighbouring project was building) while the quotients moved by
under one percentage point. Absolute is not available; the criterion has to stay
a ratio, which means the denominator must be made non-optimisable **on purpose**.

## Decision

**Replace §7 gate criterion 1's single quotient with three, measured at
`[profile.embedder]`, plus a control that fails if the instrument stops working.**

§7's gate list becomes, for criterion 1:

> 1. C ABI, measured at `[profile.embedder]` (`lto = false`) by `just abi-cost`:
>    **R1** `tft_plan_at` over a native lookup in the shape the C signature
>    forces < **1.10**; **R2** the panic guard < **1.05**; **R3** a guard per
>    lookup over one hoisted out of the loop < **1.25**; and **C**, the control,
>    within **±0.02** of 1.

The ladder each is taken from — five arms, all interleaved within every round so
drift is common-mode, each rung differing from the one below by exactly one
thing:

| rung | arm | what it adds |
|---|---|---|
| 0 | native, guard hoisted | the shape a Rust embedder writes |
| 1 | native, guard per call | the shape the **C signature** forces |
| C | (control: rung 1's twin) | nothing; it must agree with rung 1 |
| 2 | the ABI, no panic guard | the boundary, minus `catch_unwind` |
| 3 | `tft_plan_at` | the shipped call |

R1 = rung 3 / rung 1. R2 = rung 3 / rung 2. R3 = rung 1 / rung 0. C = control /
rung 1.

**The comparands are pinned.** Every native arm is an `#[inline(never)]` function
with `black_box` on the stamp going in and the scalar coming out, so no call site
can hoist, vectorise or partially evaluate it, and all call sites share one
machine-code body.

**The control is permanent and measured every run.** It is a structural twin of
rung 1: same work, separate symbol, separate call site, reading `buf[15]` instead
of `buf[0]` purely so identical-code folding cannot merge the two and make the
control vacuous. If the compiler ever specialises a comparand per call site
again, these two disagree and the run goes red before any rung is believed.

**`just abi-cost` builds and runs both profiles and gates only the `embedder`
one.** The `release` run is kept for the contrast, and its verdicts are printed
with a banner saying they are not the gate.

## Rationale

**Why three rungs rather than one quotient.** R1 is the C *ABI*: handle
validation, layout dispatch, the output slice, the `catch_unwind` landing pad.
R3 is the C *signature*: `tft_plan_at` takes a plan and a stamp and has nowhere
to keep a guard between calls, so it builds one every time. Both are real costs a
C caller pays and both belong in §7 — but they have different owners (R1 is
`tf_tree_c`'s, R3 is `0022`'s) and different futures (`0022` intends to *lower*
R3 by giving the C tier a guard handle; nothing intends to lower R1). Rolled into
one number they move together and neither is diagnosable: a single ratio only
says *something* changed. This is the same reasoning that made `abi_cost` a
ladder in the first place; this record only finishes the job by gating the rungs
rather than the sum.

**Why not gate absolute nanoseconds instead.** That was the other candidate fix
and this host forbids it — see `report.rs`'s `Sensitivity` and the `Fitness`
split. Gating what the machine cannot measure would produce a criterion that is
either always red or permanently forced.

**Why not keep 1.05.** Because 1.05 was a figure for a quotient nobody could
reproduce, and reusing it would smuggle an unmeasured threshold into a measured
gate. Each allowance below is set from what this host actually shows, with its
falsifier stated. The §7-as-written quotient (rung 3 over rung 0, i.e. R1 × R3)
is still *printed* — it measures **1.098–1.108×** at `embedder` — so nobody has
to take on faith that the re-cut is not hiding a regression behind a
decomposition.

**How each threshold was chosen, and what falsifies it.**

| gate | measured (embedder, 12 runs, `taskset -c 2`) | allowance | why that number |
|---|---|---|---|
| R1 | 1.025–1.038 | **1.10** | ~2.5× the largest measured excess over 1. Loose on purpose: a row that goes red for noise becomes a row people re-run until green. Still catches a doubling of any single check the boundary performs |
| R2 | 0.999–1.006 | **1.05** | §3.4 predicts ~0 and that is what it measures. Fails if the landing pads stop being free on this target |
| R3 | 1.059–1.075 | **1.25** | a *regression* detector, not a target: if `Guard` acquires new per-construction work this is the row that moves. `0022` aims to lower it, and lowering it is the win |
| C | 0.992–1.002 | **±0.02** | more than twice the 0.8% widest excursion, and deliberately far tighter than the rungs it protects — the failure it hunts moved the comparand **43%** |

All four are falsified in the useful direction by a *quiet* host: if the spread
there is much smaller than what a contended machine showed, R1 in particular
should be tightened. This record invites that; it does not pretend 1.10 is a
performance target.

## Consequences

- **`just abi-cost` becomes red-on-failure**, at the `embedder` profile only.
  The recipe previously said "wire the exit status in the commit that fixes the
  regression"; the regression turned out to be in the instrument, and that is
  the commit.
- **Two builds, two target directories.** `abi-cost` now compiles the example at
  `release` and at `embedder`, as `just embed-cost` already does. The
  `embedder` directory is shared with `embed-cost` (measured at 166 MiB there).
- **§7's criterion 1 becomes four numbers instead of one.** A reader wanting "the
  ABI's cost" reads R1. A reader wanting the old shape reads the printed
  reference quotient. The delta table in `docs/API.md` §6 is untouched: nothing
  about the *surface* changes.
- **The control is load-bearing.** If it fails, no rung on that run means
  anything, and it is included in the exit status for that reason.
- **What is now measurable that was not:** whether a change to the C ABI's
  validation costs anything. The old gate could not have detected a 2× regression
  in `tft_plan_at`'s handle checks, because the noise in its denominator was an
  order of magnitude larger.

## Implementation plan

1. **Pin the comparands and interleave the arms** in `abi_cost.rs`; add the
   control arm — verified by `just abi-cost` printing row C within ±0.02 on a
   run at both profiles. *Landed.*
2. **Build and run at `[profile.embedder]`** from `just abi-cost`, gating only
   that arm; keep the `release` run as a labelled contrast — verified by the two
   banners in the recipe's output and by R1 differing between them (1.016 vs
   1.038). *Landed.*
3. **Demonstrate the pin holds** by re-applying the edit that broke the old
   gate — an unrelated extra `Tree::guard()` call site — measuring, and
   reverting. *Landed*: the ratios moved ≤0.4 pp (R1 1.028→1.029, R3
   1.064→1.059) while the host moved the absolute baseline 14% in the same
   window; reverting the edit did **not** restore the old absolute number, which
   is what says the 14% was the host and not the edit. Recorded in
   `docs/PHASE4.md` §7.
4. **On acceptance**, edit `docs/PHASE4.md` §7's gate list to the wording under
   *Decision* — verified by the gate table naming a profile, which no row in it
   does today. **Not done: this record is `draft`.**
5. **Re-measure on a quiet host** and tighten R1 if the spread justifies it —
   verified by twelve runs with the absolute baseline stable to a few percent.
   Blocked on a machine this repository does not have.

## Open questions

1. **Is rung 1 the right denominator for R1?** It charges the ABI only for what
   the boundary does, and charges the per-call guard to R3. The alternative —
   keep rung 0 as the denominator, as §7's current wording implies — makes R1
   include a cost the C *signature* forces rather than the ABI's implementation,
   which is `0022`'s subject and not `tf_tree_c`'s to fix. Both are reported; the
   decision is which one carries the pass/fail. This is the one question that has
   to be settled before `ready`.
2. **Should R3 exist at all in §7**, or move to `0022` as that record's own
   gate? It is here because it is the larger of the two costs and because a §7
   that reports only R1 would let the total drift upward with nothing red.
3. **Does the heap-tree fixture understate R3?** `just abi-attached` measures the
   per-call guard at ~45 ns on an arena attached read-only across a process
   boundary; this file measures ~16 ns on a three-edge heap tree. Both are at
   `[profile.embedder]`. The difference is unexplained and belongs to `0022`
   amendment 4's open list, but if §7 gates R3 it should probably gate the
   dearer configuration.
