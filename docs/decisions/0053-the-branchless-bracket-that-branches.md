# 0053: the branchless bracket that branches

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** the prose half has landed (this record's step 1). The code
half has not, and step 3 is why.

## Context

`SampleRing::bracket` is the binary search every lookup runs. Its index update is
written as a mask:

```rust
let cmp = u64::from(self.stamp_at(base + half) <= t);
base = base.wrapping_add(half & 0u64.wrapping_sub(cmp));
```

Five sites (`sample.rs`'s `bracket` doc block and inline comment,
`docs/design/fast-path.md`, and two in `docs/benchmarks/tf2.md`) said this cannot
compile to a conditional branch. **None of it was read off the object code, and
all of it is false.** LLVM folds `x & sext(cmp)` back into a `select`, and because
the select sits on the loop-carried chain (`base` feeds the next probe's address)
the x86 cmov-conversion pass expands it into control flow. Every inlined copy in
the shipped `--release` rlib is `cmpq` / `jle` / `xorl` — no `cmov`. The command
that settles it:

```text
cargo rustc -p tf_tree_core --release --lib -- --emit asm -C debuginfo=2
F=$(ls -t target/release/deps/tf_tree_core-*.s | head -1)
ID=$(awk '/^\t\.file\t[0-9]+ .*sample\.rs"/ {print $2; exit}' "$F")
LN=$(grep -n '^ *base = base.wrapping_add(half &' crates/tf_tree_core/src/sample.rs | cut -d: -f1)
grep -c -P "\\.loc\\t$ID $LN " "$F"                       # inlined copies of the line
grep -B12 -A4 -P "\\.loc\\t$ID $LN " "$F" | grep -c cmov  # 0
```

## The measurement

`soak --workload robot --duration 3s --interval 2s` under cachegrind
(`--branch-sim=yes --cache-sim=no`) in `tf_tree/tf2-bench`,
`[profile.profiling]`, one run per arm, 2026-09-06. Arms execute different lookup
counts, so **only per-lookup ratios are comparable**; the denominator is
`sample.rs`'s `if h == 0` branch count, and `let half = len / 2` is the control
(19.2–19.3 iterations per lookup in all arms).

| arm | Ir/lookup | ΔIr | Bcm/lookup | ΔBcm | emits a select? |
|---|---|---|---|---|---|
| `half & (0 - cmp)` — **shipped** | 928.2 | — | 6.288 | — | no |
| `if cmp { mid } else { base }` | 926.3 | −0.20 % | 5.894 | −6.3 % | no |
| `black_box(0 - cmp)` on the mask | 973.3 | **+4.86 %** | 2.773 | −55.9 % | yes |
| `core::hint::select_unpredictable` | 930.1 | +0.21 % | 2.763 | −56.1 % | yes |

On the shipped arm the index-select line is **57.2 %** of the process's
mispredicted branches at a **37.3 %** mispredict rate, and the `while len > 1`
back-edge is a further 18.1 %.

1. **The shipped mask is worse than the textbook `if`** on both axes.
2. **`select_unpredictable` is the only spelling that removes the branch at
   roughly no instruction cost.** It is a hint, so a future backend may revert it.
3. **The mask cannot be rescued in place.** `half & select_unpredictable(cmp,
   u64::MAX, 0)` still emits no `cmov` (LLVM canonicalises it back to a select);
   the hint must be applied to the *index*.
4. **`black_box` is not worth it**: it spills the mask through the stack for
   +4.9 % instructions, with no stability contract.

## Decision (proposed, not taken)

Move `[workspace.package] rust-version` from `1.87` to `1.88` and write the update
as `core::hint::select_unpredictable(self.stamp_at(mid) <= t, mid, base)`, a
value-level identity with the current expression.

It is proposed rather than taken because `select_unpredictable` stabilised in
1.88 and `just msrv` holds the floor in three places including the prose a user
reads (`README.md`, `SUPPORT.md`, `lib.rs`): a user-visible support-window move on
a published crate belongs in a record, not a documentation-fix PR.

## Rationale

**Not just fixing the words:** step 1 does that but leaves the worse branchy form in
place. **Not the plain `if`:** its gain is 0.2 % of instructions and 6 % of
mispredicts, small enough to invert on the next toolchain, and reverting on one
cachegrind run on a host failing `Fitness::probe` is a shape this line has been
burned by twice; it is the fallback if the MSRV move is refused. **Not `black_box`**
(measured above) **or inline `asm!`** ([`0007`](./0007-the-unsafe-budget-and-the-c-abi.md):
a codegen hint is not a boundary the compiler cannot see across).

## Consequences

- The MSRV floor would move for a hot-path reason, the first that is ours.
- **The durable artifact is the cachegrind Bcm-per-lookup number and its command**;
  nothing in `just lint`, `just bench-check` or `scripts/evidence-audit.sh` can see a
  `cmov` turn back into a `jle`.
- If the search becomes branchless, `tf2.md`'s per-file mispredict table must be
  re-derived, not edited.

## Implementation plan

1. **Correct the sites and keep the measurement beside them.** — landed as
   `CORRECTION`s. The instrument is `grep -rn 'branchless\|cmov' crates/ docs/`
   (`sample.rs`'s module doc and `bracket` summary, `bracket_from`, `fast-path.md`'s
   lever row **3b**), each hit reading as a correction, heading or history.
2. **Register the run.** — landed: a probe row in
   [`EVIDENCE.md`](../benchmarks/EVIDENCE.md) naming the container command.
3. **Decide the MSRV move.** Open; it needs an owner's answer on the support floor.
4. **If yes**: bump `rust-version`, the three prose sites `just msrv` holds and
   `CHANGELOG.md`; write the select; re-run the command and add the shipped arm's row
   — `just msrv`, `just test`, the `grep` lines non-zero. Red-tested: the mask reports
   7 inlined sites and 0 `cmov`; `select_unpredictable` 6 sites and 6 `cmov`.
5. **Re-time on a fit host.** The reduction is a cachegrind model result on a host
   failing `Fitness::probe`, so the claim is *"the branch exists and is large in the
   model"*, never *"removing it is worth N ns"*.

## Open questions

1. **Is a 1.87 → 1.88 floor acceptable on the `0.0.x` line?** Every previous move
   was forced by a dependency; this one would be chosen.
2. **Does the mispredict reduction survive a real predictor?** cachegrind models a
   two-level predictor, not Zen 3 TAGE; step 5 is the only thing that closes it.
3. **Is `bracket`'s back-edge the next target?** 18.1 % of mispredicts; unrolling
   to a fixed trip count is a different change.
