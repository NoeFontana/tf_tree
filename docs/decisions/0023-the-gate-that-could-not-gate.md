# 0023: §7 gate criterion 1 — three rungs at a real boundary, not one quotient at a fake one

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** the measurement half has landed (`just abi-cost`), open
question 3's falsifier has been built and run (`just abi-split`'s
*0023 q3* block, `backing::guard_cost_fixture_pair`), and `docs/PHASE4.md` §7's
gate list is edited to the wording under *Decision*.

All four open questions carry a recommendation and all four are adopted; two
change what the *Decision* means: **R3 becomes the primary criterion, measured on
the §11.1 fixture** (question 3, settled by building its falsifier: the effect is
real and about twice the size predicted), and **R1's 1.10 is provisional**
(question 4).

## Context

`docs/PHASE4.md` §7 gate criterion 1 reads **"C ABI within 5% of native for
depth-3 lookup."** One quotient, one threshold; it never gated anything, for two
compounding reasons that are the same defect.

**The profile erased the boundary.** `crates/tf_tree_c/examples/abi_cost.rs` was
built at the workspace `release` profile (`lto = "thin"`), which inlines
`tft_plan_at` into a Rust caller, so the boundary the criterion prices was not in
the binary pricing it (`[profile.embedder]`, `lto = false`, exists because of this
trap). The same ABI prices at **1.016-1.019x** with the boundary gone and
**1.025-1.038x** with it present.

**The denominator was at LLVM's discretion.** Adding a second, unrelated
`Tree::guard()` call site to `abi_cost.rs` moved the native baseline from **133 ns
to 190 ns (43%)** and flipped the verdict FAIL -> PASS while the ABI arm stayed at
194-196 ns. With `tft_plan_at` inlined, the ratio turns on how well LLVM
specialises the comparand.

**This host cannot escape into absolute nanoseconds:** `report.rs`'s `Fitness`
refuses `Sensitivity::AbsoluteTiming` rows here (SMT on, unreadable governor);
`fair_for_ratios` survives, since common-mode drift lands on both arms of an
interleaved pair. The criterion has to stay a ratio, so the denominator must be
non-optimisable **on purpose**.

## Decision

**Replace §7 gate criterion 1's single quotient with three, measured at
`[profile.embedder]`, plus a control that fails if the instrument stops working.**
§7's gate list becomes, for criterion 1:

> 1. C ABI, measured at `[profile.embedder]` (`lto = false`) by `just abi-cost`:
>    **R1** `tft_plan_at` over a native lookup in the shape the C signature
>    forces < **1.10**; **R2** the panic guard < **1.05**; **R3** a guard per
>    lookup over one hoisted out of the loop < **1.25**; and **C**, the control,
>    within **±0.02** of 1.

The ladder, five arms interleaved within every round so drift is common-mode, each
rung differing from the one below by one thing:

| rung | arm | what it adds |
|---|---|---|
| 0 | native, guard hoisted | the shape a Rust embedder writes |
| 1 | native, guard per call | the shape the **C signature** forces |
| C | (control: rung 1's twin) | nothing; it must agree with rung 1 |
| 2 | the ABI, no panic guard | the boundary, minus `catch_unwind` |
| 3 | `tft_plan_at` | the shipped call |

R1 = rung 3 / rung 1. R2 = rung 3 / rung 2. R3 = rung 1 / rung 0. C = control /
rung 1.
**The comparands are pinned:** every native arm is an `#[inline(never)]` function
with `black_box` on the stamp in and the scalar out, so all call sites share one
machine-code body. **The control is permanent and measured every run:** a
structural twin of rung 1 (separate symbol and call site, reading `buf[15]` not
`buf[0]` so identical-code folding cannot merge them); if the compiler ever
specialises a comparand per call site the run goes red first. **`just abi-cost` builds and runs both profiles and gates only the
`embedder` one;** the `release` run is a labelled contrast.

## Rationale

**Three rungs, not one quotient.** R1 is the C *ABI* (handle validation, layout
dispatch, output slice, `catch_unwind` pad); R3 is the C *signature* (`tft_plan_at`
has nowhere to keep a guard between calls). Different owners (`tf_tree_c`, `0022`),
and rolled together neither is diagnosable. `0022` declined the guard handle, so
both rows are regression detectors, not targets. **Not absolute nanoseconds** (this
host forbids it) and **not 1.05** (a figure for a quotient nobody could
reproduce). The §7-as-written quotient (rung 3 over rung 0, R1 x R3) is still
*printed*, at **1.098-1.108x**, so the re-cut cannot hide a regression.

| gate | measured (embedder, 12 runs, `taskset -c 2`) | allowance | why |
|---|---|---|---|
| R1 | 1.025-1.038 | **1.10** | ~2.5x the largest excess over 1; loose on purpose (a row red for noise gets re-run until green). **Provisional: taken on a contended host (question 4)** |
| R2 | 0.999-1.006 | **1.05** | §3.4 predicts ~0. Fails if the landing pads stop being free |
| R3 | 1.059-1.075 | **1.25** | a *regression* detector on `Guard`'s construction cost. Measured on the wrong fixture (question 3) |
| C | 0.992-1.002 | **±0.02** | over twice the 0.8% widest excursion, far tighter than the rungs it protects (the failure it hunts moved the comparand 43%) |

## Consequences

- **`just abi-cost` is red-on-failure**, at the `embedder` profile only; it
  compiles at `release` and `embedder` (the latter directory shared with
  `embed-cost`, 166 MiB).
- **§7's criterion 1 is four numbers;** R1 is "the ABI's cost" and the reference
  quotient is printed. `docs/API.md` §6 is untouched.
- **The control is load-bearing** and is in the exit status: if it fails no rung
  on that run means anything.

## Implementation plan

1. **Pin the comparands, interleave the arms, add the control** in `abi_cost.rs`.
   *Landed.*
2. **Build and run at `[profile.embedder]`** from `just abi-cost`, gating only that
   arm, `release` as labelled contrast (R1 1.016 vs 1.038). *Landed.*
3. **Demonstrate the pin holds** by re-applying the edit that broke the old gate.
   *Landed:* ratios moved <= 0.4 pp (R1 1.028 -> 1.029, R3 1.064 -> 1.059) while
   the host moved the absolute baseline 14%. Recorded in `docs/PHASE4.md` §7.
4. **Edit `docs/PHASE4.md` §7's gate list** to the wording under *Decision*.
   *Landed.*
5. **Re-measure on a quiet host and re-derive R1 by question 4's rule**: twelve
   runs each with busy <= `mp::QUIET_ENOUGH` (**0.10**). **Open.** The instrument
   landed 2026-09-11: `tf_tree_bench`'s `quiet_check` bin calls
   `mp::require_quiet_machine`, and `just abi-cost` brackets the two `abi_cost`
   runs with it. It exits **2**, where `abi_cost` exits 1 for a missed ratio, so a
   loud host cannot read as the ABI regressing (`docs/PROJECT.md` §6's *INVALID is
   not FAIL*); the `TF_TREE_BENCH_FORCE` override prints that it fired.
   - **The sample is taken before the run, never during it** (`abi_cost`
     saturates one core, ~12.5% of 8 CPUs, so a fraction sampled inside its loop
     could never satisfy 0.10), and through **one entry point** (`quiet_check`),
     not a second copy of the sampler or a dev-dependency cycle on
     `tf_tree_bench` (permitted by cargo, but it pulls the bench tree into every
     example build).
   - **Still owed: the twelve readings**, needing a window when nothing else on
     the box is building (a scheduling problem; the host reads 0.000-0.021 at rest).
   - **A bound:** `busy_fraction` reads `/proc/stat`'s aggregate line while
     `abi_cost` is `taskset -c 2`, so 8% aggregate could be one neighbour pinned to
     CPU 2. Per-CPU sampling is not built.
6. **Pair the two fixtures in one binary before moving R3.** *Landed:*
   `backing::guard_cost_fixture_pair`, reported by `just abi-split`. Its falsifier
   ("the paired difference reproduces ~18 ns, else question 3 is withdrawn") came
   out at **30-44 ns**: same sign, about double. The *number* missed; what it
   guarded against (the two-binary comparison being an artifact) passed decisively.
   The literal reading is recorded because a falsifier reinterpreted after the
   result is not one.
7. **Move R3 onto the §11.1 fixture and re-derive its allowance there**, by the
   same rule as R1's, not carried across from 1.25. Verified by `just abi-cost`
   reporting R3 against a numerator ~2.5x larger. **Open.**

## Open questions

Each recommendation predates the measurements in steps 3, 5 and 6; where a later
step contradicts one, the step is what happened.

1. **Is rung 1 the right denominator for R1?** It charges the ABI only for what the
   boundary does and the per-call guard to R3; rung 0 would make R1 include a cost
   the C *signature* forces (`0022`'s subject).

   **Recommendation: keep rung 1, and make R3 the primary criterion.** R1 measures
   1.025-1.038, about **6-9 ns** on a ~245 ns lookup, a health check. R3 measures
   the per-call guard, which
   [`0022`](./0022-the-per-call-guard-and-the-unwatched-gate.md) amendment 3 prices
   at **~48 of the ~56 ns** a C++ caller pays over native Rust (~85%). With rung 0
   under it, R1 would be R1 x R3 and move whenever the guard moved. *Objection:* R3
   is not `tf_tree_c`'s to fix. *Reply:* §7 gates what a C caller pays, and `0022`
   declines to lower R3 (the answer is `tft_plan_at_many`), so it is a permanent
   regression detector on `Guard`'s construction. `abi_cost.rs`'s R3 comment ("the
   row `0022` intends to *lower*") is now false.
2. **Should R3 exist in §7, or move to `0022`?** **Stays:** `0022` builds nothing,
   so a gate parked there is one nobody runs, the original defect (`abi_cost.rs`
   ran in no recipe for years).
3. **Does the heap-tree fixture understate R3?** `just abi-attached` measures the
   per-call guard at ~45 ns on a read-only cross-process arena; `abi_cost.rs`
   ~16 ns on a three-edge heap tree.

   **Recommendation: yes; gate the dearer configuration.** The variable is the
   **fixture**, not the backing: `0022` amendment 5 measured the §11.1 fixture in
   one binary, **heap +34.4 ns, memfd +35.8 ns** (backing ~1.4 ns; fixture 16 ->
   34.4 ns).

   The mechanism is a **working set** in the *stamp* array. A fresh `Guard` starts
   its bracket-search cursor at the `EdgeId(0)` sentinel, so a per-call guard
   restarts every step's search at the window midpoint. `docs/design/fast-path.md`
   §12 measured the cache cliff: `sample(exact)` flat at 12.8-14.4 ns from capacity
   64 to 1024, then 32.5 ns at 4096 (32 KiB of stamps, this host's L1d) and 43.3 at
   16384.

   | | `abi_cost.rs`'s tree | the §11.1 fixture |
   |---|---|---|
   | dynamic edges on the path | 2 x 256 slots | 512 + 2048 + **16384** slots |
   | stamp bytes searched, per edge | 2 KiB | 4 / 16 / **128 KiB** |
   | whole arena | **~54 KiB** | **~1.34 MiB** |
   | vs L1d 32 KiB, L2 512 KiB | every hot array in L1 | the 1 kHz edge's stamps are **4x L1d** |

   Predicted difference ~17 ns; measured 34.4 - 16 = ~18 ns from independent runs.
   The ~9 ns between §11.1 heap in-process (34.4) and attached cross-process
   (43-47) is a cross-harness difference, **not attributed, and must not be
   attributed by subtraction**.

   **For the gate:** a three-edge tree that fits in L1 is not what a robot runs;
   §11.1 (a 1 kHz edge with 10 s of history) is the fixture the whole suite, the
   tf2 differential harness and the CLI demo share. **Move R3 onto it**
   (`abi_attached` builds that ladder) and re-derive its allowance (1.25 was set
   against a ~16 ns numerator; §11.1's is ~2.5x larger). R1, R2 and C price the
   boundary, which `0022` amendment 4 found fixture-independent (~7 ns).

   **Paired measurement** (`backing::guard_cost_fixture_pair`; heap-backed,
   `[profile.embedder]`, median of per-round differences):

   | run | three-edge, 256 slots | §11.1 fixture | paired difference |
   |---|---|---|---|
   | 1 | +18.1 ns | +48.2 ns | **+30.1 ns** |
   | 2 | +15.6 ns | +52.0 ns | **+36.4 ns** |
   | 3 | +18.7 ns | +62.5 ns | **+43.9 ns** |

   The direction is confirmed and the three-edge column reproduces ~16 ns, but the
   predicted ~18 ns became **30-44 ns**: the stamp-array cliff accounts for perhaps
   half and **the remainder is not attributed**. The §11.1 column is noisy (14.3 ns
   span against 3.1 ns), so R3's new allowance carries the same *provisional*
   label as R1's.
4. **Is 1.10 the right allowance for R1?**

   **Recommendation: no, it is provisional.** The twelve runs were taken while a
   neighbouring project built (absolute baseline 217-248 ns, ~14%). An allowance of
   "~2.5x the largest excess seen on a contended machine" is sized by the
   contention, not the ABI: at 1.10 against 1.025-1.038, R1 would sit still for a
   **three-fold** increase in everything the boundary does.

**Tighten it on a quiet host.** "Quiet" is defined in code:
`tf_tree_bench::mp::QUIET_ENOUGH`, **<= 10% busy** over 300 ms immediately before
the run (`require_quiet_machine`), which `report.rs`'s `Fitness` folds into
`fair_for_ratios` (a ratio needs only that axis; `fair_for_timing` also wants no
SMT and a `performance` governor, which this host cannot pass).

**The re-derivation rule, fixed in advance so it cannot be chosen after seeing
   the data:** twelve runs at `[profile.embedder]`, `taskset -c 2`, each busy
   <= 10%; R1's allowance = **1 + 2.5 x (max observed excess over 1)**, floored at
   **1.02** (the control's own band, the tightest a row over two timed arms can
   honestly be). On today's max excess of 0.038 the rule returns ~1.095, so a quiet
   host reproducing 1.038 vindicates 1.10; one showing 1.015 returns 1.04 and R1
   becomes a gate that could catch something.
