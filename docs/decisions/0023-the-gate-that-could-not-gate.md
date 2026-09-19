# 0023: §7 gate criterion 1 — three rungs at a real boundary, not one quotient at a fake one

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** the measurement half has landed (`just abi-cost`), open
question 3's falsifier has been built and run (`just abi-split`'s
*0023 q3* block, `backing::guard_cost_fixture_pair`), and `docs/PHASE4.md` §7's
gate list is edited to the wording under *Decision*.

All four open questions are adopted; two change what the *Decision* means: **R3
becomes the primary criterion, measured on the §11.1 fixture** (question 3), and
**R1's 1.10 is provisional** (question 4).

## Context

`docs/PHASE4.md` §7 gate criterion 1 read "C ABI within 5% of native for depth-3
lookup" and never gated anything: `release`'s thin LTO inlined `tft_plan_at` (the
boundary was not in the binary), and the native denominator was at LLVM's
discretion. This host refuses absolute-timing rows (`report.rs`'s `Fitness`), so
the criterion stays a ratio with a non-optimisable denominator.

## Decision

**Replace §7 gate criterion 1's single quotient with three, measured at
`[profile.embedder]`, plus a control that fails if the instrument stops working.**
§7's gate list becomes, for criterion 1:

> 1. C ABI, measured at `[profile.embedder]` (`lto = false`) by `just abi-cost`:
>    **R1** `tft_plan_at` over a native lookup in the shape the C signature
>    forces < **1.10**; **R2** the panic guard < **1.05**; **R3** a guard per
>    lookup over one hoisted out of the loop < **1.25**; and **C**, the control,
>    within **±0.02** of 1.

Five arms, interleaved within every round so drift is common-mode:

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
with `black_box` on the stamp in and the scalar out. **The control is permanent:**
a structural twin of rung 1 (separate symbol and call site, reading `buf[15]` not
`buf[0]` so identical-code folding cannot merge them); if the compiler specialises
a comparand per call site the run goes red first. `just abi-cost` gates only
`embedder`; `release` is a labelled contrast.

## Rationale

R1 is the C *ABI*; R3 is the C *signature* (`tft_plan_at` has nowhere to keep a
guard between calls). Different owners (`tf_tree_c`, `0022`); rolled together
neither is diagnosable. Both are regression detectors, not targets. R1's 1.10 is
provisional (question 4); R3's 1.25 was set on the wrong fixture (question 3).

## Consequences

- **`just abi-cost` is red-on-failure**, at the `embedder` profile only.
- **The control is in the exit status.**

## Implementation plan

1. **Pin the comparands, interleave the arms, add the control** in `abi_cost.rs`.
   *Landed.*
2. **Build and run at `[profile.embedder]`** from `just abi-cost`, gating only that
   arm. *Landed.*
3. **Demonstrate the pin holds** by re-applying the edit that broke the old gate.
   *Landed;* recorded in `docs/PHASE4.md` §7.
4. **Edit `docs/PHASE4.md` §7's gate list** to the wording under *Decision*.
   *Landed.*
5. **Re-measure on a quiet host and re-derive R1 by question 4's rule**: twelve
   runs each with busy <= `mp::QUIET_ENOUGH` (**0.10**). **Open.** The instrument
   landed: `tf_tree_bench`'s `quiet_check` bin calls `mp::require_quiet_machine`,
   and `just abi-cost` brackets the two `abi_cost` runs with it. It exits **2**,
   where `abi_cost` exits 1 for a missed ratio, so a loud host cannot read as the
   ABI regressing; the `TF_TREE_BENCH_FORCE` override prints that it fired.
   - The sample is taken before the run, never during it.
   - **Still owed: the twelve readings.** `busy_fraction` reads `/proc/stat`'s
     aggregate line while `abi_cost` is `taskset -c 2`; per-CPU sampling is not
     built.
6. **Pair the two fixtures in one binary before moving R3.** *Landed:*
   `backing::guard_cost_fixture_pair`, reported by `just abi-split`. Its falsifier
   ("the paired difference reproduces ~18 ns, else question 3 is withdrawn") came
   out at 30-44 ns: same sign, about double, so question 3 stands.
7. **Move R3 onto the §11.1 fixture and re-derive its allowance there**, by the
   same rule as R1's. Verified by `just abi-cost` reporting R3 against a numerator
   ~2.5x larger. **Open.**

## Open questions

Where a later step contradicts a recommendation, the step is what happened.

1. **Is rung 1 the right denominator for R1?** **Keep rung 1, and make R3 the
   primary criterion.** With rung 0 under it, R1 would be R1 x R3 and move
   whenever the guard moved. R3 is not `tf_tree_c`'s to fix, but §7 gates what a C
   caller pays, and `0022` declines to lower it (the answer is
   `tft_plan_at_many`), so it is a permanent regression detector on `Guard`'s
   construction.
2. **Should R3 exist in §7, or move to `0022`?** **Stays:** `0022` builds nothing,
   so a gate parked there is one nobody runs.
3. **Does the heap-tree fixture understate R3?** **Yes; gate the dearer
   configuration.** The variable is the **fixture**, not the backing. A fresh
   `Guard` starts its bracket-search cursor at the `EdgeId(0)` sentinel, so a
   per-call guard restarts every step's search at the window midpoint; on
   `abi_cost.rs`'s three-edge tree (~54 KiB) every hot array is in L1, while the
   §11.1 fixture's 1 kHz edge has stamps 4x L1d (`docs/design/fast-path.md` §12).
   **Move R3 onto §11.1** (`abi_attached` builds that ladder) and re-derive its
   allowance. The paired measurement (`backing::guard_cost_fixture_pair`) put the
   difference at 30-44 ns against a predicted ~18; the remainder is not
   attributed and must not be attributed by subtraction. The §11.1 column is
   noisy, so R3's new allowance carries the same *provisional* label as R1's.
4. **Is 1.10 the right allowance for R1?** **No, it is provisional.** The twelve
   runs were taken on a contended host; an allowance sized by the contention would
   let R1 sit still through a three-fold increase in everything the boundary does.

   **Tighten it on a quiet host.** "Quiet" is `tf_tree_bench::mp::QUIET_ENOUGH`,
   **<= 10% busy** over 300 ms immediately before the run
   (`require_quiet_machine`), which `report.rs`'s `Fitness` folds into
   `fair_for_ratios`.

   **The re-derivation rule:** twelve runs at `[profile.embedder]`,
   `taskset -c 2`, each busy <= 10%; R1's allowance = **1 + 2.5 x (max observed
   excess over 1)**, floored at **1.02** (the control's own band).
