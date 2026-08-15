# 0023: §7 gate criterion 1 — three rungs at a real boundary, not one quotient at a fake one

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** the measurement half has landed (`just abi-cost`); the §7
gate table is unchanged and stays that way until this record is `ready`.

**All four open questions now carry a written recommendation with its argument**
(§*Open questions*), so ratifying this record is a judgement call and not a
research task. Two of them change what the *Decision* above would mean and are
therefore called out here rather than left at the bottom: **R3 should become the
primary criterion and should be measured on the §11.1 fixture rather than the
three-edge one** (question 3 — the ~16-versus-~45 ns difference turned out to be
the fixture's working set, and the backing is worth ~1.4 ns of it), and **R1's
1.10 allowance is provisional, set on a host that was not quiet** (question 4).
The record stays `draft`: a human ratifies, and the two changed thresholds are
proposals until then.

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
`tf_tree_c`'s, R3 is `0022`'s) and different futures — **as first written, that
read "`0022` intends to *lower* R3 by giving the C tier a guard handle";
`0022` has since gone `ready` declining the handle, so neither row is expected to
move and both are regression detectors.** Rolled into
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
| R1 | 1.025–1.038 | **1.10** | ~2.5× the largest measured excess over 1. Loose on purpose: a row that goes red for noise becomes a row people re-run until green. Still catches a doubling of any single check the boundary performs. **Provisional — the twelve runs were taken on a contended host; see open question 4** |
| R2 | 0.999–1.006 | **1.05** | §3.4 predicts ~0 and that is what it measures. Fails if the landing pads stop being free on this target |
| R3 | 1.059–1.075 | **1.25** | a *regression* detector, not a target: if `Guard` acquires new per-construction work this is the row that moves. **The clause that used to end this row — "`0022` aims to lower it, and lowering it is the win" — is now wrong**: `0022` is `ready` and declines the `tft_guard` handle, so R3 is a permanent regression detector rather than a number somebody intends to move. It is also measured on the wrong fixture; see open question 3 |
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
5. **Re-measure on a quiet host** and re-derive R1 by open question 4's rule —
   verified by twelve runs each recording busy ≤ `mp::QUIET_ENOUGH`, which
   requires `abi_cost.rs` to measure and print a busy fraction (it does not
   today). Blocked on a machine this repository does not have; the printing half
   is not blocked on anything.

**Steps 6 and 7 are what the open questions' recommendations would add, listed
here so a ratifier sees the whole cost. They are proposals, like everything else
under *Decision*.**

6. **Pair the two fixtures in one binary before moving R3.** Add a three-edge arm
   beside `arena_backing`'s guard-cost pair so the 16-versus-34.4 ns comparison
   becomes one interleaved measurement instead of two binaries' medians —
   verified by the paired difference reproducing ~18 ns, and falsified if it does
   not, in which case open question 3's recommendation is withdrawn rather than
   argued.
7. **Move R3 onto the §11.1 fixture and re-derive its allowance there** —
   verified by `just abi-cost` reporting R3 against a numerator ~2.5× larger and
   by the new allowance being derived by the same rule as R1's, not carried
   across from 1.25.

## Open questions

Each carries a **recommendation** written in below. They are recommendations and
not decisions because this record is `draft`: a human ratifies by merging, and
until then `docs/PHASE4.md` §7's normative gate list is untouched.

1. **Is rung 1 the right denominator for R1?** It charges the ABI only for what
   the boundary does, and charges the per-call guard to R3. The alternative —
   keep rung 0 as the denominator, as §7's current wording implies — makes R1
   include a cost the C *signature* forces rather than the ABI's implementation,
   which is `0022`'s subject and not `tf_tree_c`'s to fix. Both are reported; the
   decision is which one carries the pass/fail. This is the one question that has
   to be settled before `ready`.

   **Recommendation: keep rung 1 as R1's denominator, and make R3 the primary
   criterion — the row a reader is pointed at, and the row whose movement is
   expected to be the interesting one.** R1 stays gated and stays as specified;
   what changes is which of the two §7 calls *the* criterion.

   The argument is where the money is. R1 measures 1.025–1.038 — about **6–9 ns**
   on a ~245 ns lookup — and is a health check on handle validation, layout
   dispatch and the output slice. R3 measures the per-call guard, which
   [`0022`](./0022-the-per-call-guard-and-the-unwatched-gate.md) amendment 3
   prices at **~48 of the ~56 ns** a C++ caller pays over native Rust, i.e. ~85%
   of the whole thing. A §7 whose headline is R1 puts the reader's eye on the
   smaller term. That is also the *asymmetry* argument for keeping rung 1 as the
   denominator: with rung 0 underneath, R1 would be R1 × R3 and would move
   whenever the guard moved, which is exactly the non-diagnosability this record
   exists to remove.

   **The objection this has to answer** is that R3 is not `tf_tree_c`'s to fix,
   so gating it makes §7 red for a cost another crate owns. Two replies. First,
   §7 is Phase 4's gate on *what a C caller pays*, not on `tf_tree_c`'s source;
   a C caller pays the guard. Second, `0022` is now `ready` with the decision
   **not** to lower R3 — the answer there is `tft_plan_at_many`, not a
   `tft_guard` handle — so R3 is no longer a number waiting to be improved by
   somebody else's work. It is a **regression detector on `Guard`'s
   construction**, permanently, and that is a stabler thing to gate than a
   quotient somebody intends to move. `0023`'s own R3 doc comment in
   `abi_cost.rs` says "it is the row `0022` intends to *lower*, and lowering it
   is the win"; **that sentence is now false** and should be corrected in the
   commit that ratifies this record.

2. **Should R3 exist at all in §7**, or move to `0022` as that record's own
   gate? It is here because it is the larger of the two costs and because a §7
   that reports only R1 would let the total drift upward with nothing red.

   **Recommendation: R3 stays in §7, and question 1's answer is why.** `0022` is
   `ready` and builds nothing, so it has no implementation to hang a gate on and
   no recipe of its own; a gate parked in a record that ships no code is a gate
   nobody runs — which is the *original* defect this record was written to fix
   (`abi_cost.rs` was executed by no recipe for years). §7 is where the recipe
   is.

3. **Does the heap-tree fixture understate R3?** `just abi-attached` measures the
   per-call guard at ~45 ns on an arena attached read-only across a process
   boundary; this file measures ~16 ns on a three-edge heap tree. Both are at
   `[profile.embedder]`. The difference is unexplained and belongs to `0022`
   amendment 4's open list, but if §7 gates R3 it should probably gate the
   dearer configuration.

   **Recommendation: yes, it understates it; gate the dearer configuration; and
   the difference is no longer unexplained.** The mechanism is a **working set**,
   it is the *stamp* array and not the arena as a whole, and every number below
   was already in the tree — nobody had put the three tables side by side.

   **First, the variable is the fixture, not the backing.** `0022` amendment 5
   measured the per-call guard on the §11.1 fixture at `[profile.embedder]`,
   counters off, on both backings in one binary: **heap +34.4 ns, memfd
   +35.8 ns**. So changing the backing and holding the fixture is worth ~1.4 ns,
   while changing the fixture and holding the backing (heap, `embedder`) is
   16 → 34.4 ns. **Whatever this is, it is not "shared memory is dearer"** —
   which is what `abi_cost.rs`'s R3 comment currently guesses ("whatever makes
   the shared case ~3× dearer"), and it is wrong. That comment is corrected in
   the same commit as this paragraph.

   **Second, the mechanism.** `Guard` carries the per-step bracket-search cursor
   (`plan.rs`, `Guard::cursor`), and a fresh guard initialises it to the
   `EdgeId(0)` sentinel — the source comment says it outright: *"a fresh guard
   matches no edge and every step takes the cold path once"*. So a per-call guard
   does not merely cost its constructor; it makes **every step of every lookup
   restart its bracket search at the window midpoint** instead of resuming beside
   the previous answer. `docs/design/fast-path.md` §12 measured what that costs
   and found it is a cache cliff in the **stamp array**, not a probe curve:
   `sample(exact)` is flat at 12.8–14.4 ns from capacity 64 to 1024, then steps
   to 32.5 at capacity 4096 — whose stamp array is 32 KiB, this host's L1d — and
   43.3 at 16384. The cursor is worth **58.54 → 41.37 ns/sample at capacity
   16384** on a monotone sweep, and the pose array's size costs nothing (the
   `Hold` control is flat to 1% across a 256× range).

   **Third, the arithmetic on the two fixtures**, from `ArenaLayout::compute`
   (64 B/slot poses, 8 B/slot stamps, capacities rounded to powers of two):

   | | `abi_cost.rs`'s tree | the §11.1 fixture |
   |---|---|---|
   | dynamic edges on the measured path | 2 × 256 slots | 512 + 2048 + **16384** slots |
   | stamp bytes searched, per edge | 2 KiB | 4 / 16 / **128 KiB** |
   | pose bytes, per edge | 16 KiB | 32 / 128 / **1024 KiB** |
   | whole arena | **~54 KiB** | **~1.34 MiB** |
   | vs this host (L1d 32 KiB, L2 512 KiB) | every hot array in L1 | the 1 kHz edge's stamps are **4× L1d** |

   So the three-edge tree's searched arrays are 2 KiB and sit on the flat part of
   §12's curve, where a cold search costs a couple of nanoseconds more than a warm
   one; §11.1's 1 kHz edge sits past the cliff, where §12 measures the cursor at
   **~17 ns/sample**. Predicted difference in R3's absolute cost between the two
   fixtures: ~17 ns. Measured: 34.4 − 16 = **~18 ns**. The prediction and the
   measurement are made from independent runs and agree; **the working-set
   reading is supported, not asserted.**

   **What is still not attributed, and must not be attributed by subtraction.**
   §11.1 heap in-process is 34.4 ns and §11.1 attached read-only cross-process is
   43–47 ns. That ~9 ns is a *cross-harness* difference (`arena_backing` vs
   `abi_attached`, different sweep shapes, counters compiled in but flushing
   nothing on a read-only arena) and this record does not claim to know what it
   is. It is smaller than the effect above and does not change the
   recommendation.

   **What follows for the gate.** A three-edge tree that fits in L1 is not what a
   robot runs — §11.1 is the fixture the whole benchmark suite, the tf2
   differential harness and the CLI demo already share, and its 1 kHz edge with
   10 s of history is an ordinary IMU. Gating R3 on the toy fixture gates the
   configuration in which the cost being gated is mostly absent. **Move R3's
   measurement onto the §11.1 fixture** — `abi_attached` already builds that
   ladder — and re-derive its allowance there rather than carrying 1.25 across,
   because 1.25 was set against a ~16 ns numerator and the §11.1 one is ~2.5×
   larger against a similar denominator. R1, R2 and C are unaffected: they price
   the boundary, and `0022` amendment 4 found the boundary's own terms
   (validation, layout store, `catch_unwind`) fixture-independent at ~7 ns
   combined.

   **The measurement that would make this airtight**, and which nobody has run:
   both fixtures' guard cost **in one binary, one profile, interleaved**. The
   16-versus-34.4 comparison above is across two binaries, so it is a strong
   inference and not a paired measurement — and this repository has been wrong
   three times about exactly this kind of comparison. Adding a three-edge arm to
   `arena_backing`'s guard-cost pair is a small job and is the right first step
   of whatever commit ratifies this record.

4. **Is 1.10 the right allowance for R1?** *(Added by this revision; it was
   implicit in the threshold table's falsifier column and deserves to be a
   question, because it is the number most likely to be wrong.)*

   **Recommendation: no — 1.10 is provisional, and it was set on a host that was
   not quiet.** Say so plainly rather than letting the table read as a
   performance target. The twelve runs behind it were taken while a neighbouring
   project was building: the *absolute* baseline wandered **217–248 ns**, ~14%,
   in that window. The quotients held to under a percentage point, which is the
   whole reason this criterion is a ratio — but an allowance chosen as "~2.5× the
   largest excess I saw on a contended machine" is an allowance sized by the
   contention, not by the ABI. At 1.10 against a measured 1.025–1.038, R1 would
   sit still for a **three-fold** increase in everything the boundary does. That
   is loose enough to miss a real regression, and calling it "loose on purpose"
   is only half true: the other half is that nobody knows how tight it could be.

   **Tighten it on a quiet host before ratification.** "Quiet" is not a mood
   here, it has a definition in the code: `tf_tree_bench::mp::QUIET_ENOUGH` —
   **≤ 10% busy**, sampled over 300 ms immediately before the run
   (`require_quiet_machine`), which `report.rs`'s `Fitness` folds into
   `fair_for_ratios` along with "not a debug build". (`fair_for_timing`
   additionally wants no SMT and a `performance` governor; this host has SMT on
   and an unreadable governor and therefore **cannot** pass it — see *Context*.
   `fair_for_ratios` is the axis available here, and it is the axis a ratio
   needs.)

   Two consequences, both concrete:

   - **`abi_cost.rs` measures no busy fraction today and should.** It lives in
     `tf_tree_c`, which cannot depend on `tf_tree_bench` — so it reads
     `/proc/stat` itself, in ~20 lines, and *prints* the busy fraction beside
     each verdict. It must not refuse on it: the whole point of the ratio
     construction is that it survives a busy host. What a printed number buys is
     that a reader can tell a 1.09 taken at 60% busy from a 1.09 taken at 2%,
     which is the difference between "fine" and "the gate is about to be
     useless".
   - **The re-derivation rule, stated in advance so it cannot be chosen after
     seeing the data:** twelve runs at `[profile.embedder]`, `taskset -c 2`, each
     with busy ≤ 10%, and set R1's allowance to **1 + 2.5 × (max observed excess
     over 1)** — the same factor 1.10 was built from, so only the *input* changes
     — floored at 1.02, which is the control's own band and the tightest a row
     over two timed arms can honestly be. On the numbers this contended host
     shows (max excess 0.038) the rule returns ~1.095, i.e. **if a quiet host
     reproduces 1.038 then 1.10 is vindicated and stays**; if a quiet host shows
     1.015 it returns 1.04, and R1 becomes a gate that could actually catch
     something. Writing the rule before the run is what stops the second outcome
     being argued away when it arrives.
