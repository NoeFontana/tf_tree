# 0053: the branchless bracket that branches

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** the prose half has landed (this record's step 1). The code
half has not, and step 3 is why.

## Context

`SampleRing::bracket` is the binary search every lookup runs. Its index update
is written as a mask:

```rust
let cmp = u64::from(self.stamp_at(base + half) <= t);
base = base.wrapping_add(half & 0u64.wrapping_sub(cmp));
```

Five places said that this cannot compile to a conditional branch, and one of
them steered future work away from the line on the strength of it:

| Site | What it said |
|---|---|
| `crates/tf_tree_core/src/sample.rs`, the `bracket` doc block | *"it does not depend on the optimizer continuing to choose `cmov` across future edits, and its cost is independent of the stamp distribution"* |
| the same file, the inline comment on the line | *"an AND the backend cannot turn back into control flow"* |
| `docs/design/fast-path.md` | *"`bracket` is now branchless so its cost no longer depends on the stamp distribution at all"* |
| `docs/benchmarks/tf2.md`, the mispredict caveat | the same AND claim, as the mechanism behind a 7.70 → 7.32 improvement |
| `docs/benchmarks/tf2.md`, the per-file mispredict table | *"The bracket search is already branchless (a `cmp` folded into the index), so what remains is the bounds and seqlock structure around it"* |

**None of it was ever read off the object code, and all of it is false.** LLVM
folds `x & sext(cmp)` back into a `select`, and because the select sits on the
loop-carried dependency chain (`base` feeds the next probe's address) the x86
cmov-conversion pass expands the select into control flow to break that chain.
That is the pass doing its job; the mask is simply not a lever that reaches it.

Every inlined copy of the loop in the shipped `--release` rlib is
`cmpq` / `jle` / `xorl` over the loaded stamp — no `cmov`, and no `and` for the
select (the `andq` that *is* there is `stamp_at`'s ring mask). Two seconds to
check, and the command is in the `bracket` doc block so it travels with the
claim:

```text
cargo rustc -p tf_tree_core --release --lib -- --emit asm -C debuginfo=2
F=$(ls -t target/release/deps/tf_tree_core-*.s | head -1)
ID=$(awk '/^\t\.file\t[0-9]+ .*sample\.rs"/ {print $2; exit}' "$F")
LN=$(grep -n '^ *base = base.wrapping_add(half &' crates/tf_tree_core/src/sample.rs | cut -d: -f1)
grep -c -P "\\.loc\\t$ID $LN " "$F"                       # inlined copies of the line
grep -B12 -A4 -P "\\.loc\\t$ID $LN " "$F" | grep -c cmov  # 0
```

This is the **second** wrong mechanism the same residual has been given.
`tf2.md` already carries one correction of it — an earlier rewrite recovered
only part of the mispredicts and that was written up as *"LLVM had already
emitted a `cmov`"*, which the record marks **"That explanation was wrong"**. The
replacement explanation was asserted the same way, from the source, and it is
wrong the same way. The instrument that settles it is four lines long and was
never run.

## The measurement

`soak --workload robot --duration 3s --interval 2s`, under cachegrind
(`--branch-sim=yes --cache-sim=no`) in this repository's own `tf_tree/tf2-bench`
container, `[profile.profiling]`. One build per arm, one run per build,
2026-09-06, on the development host.

The soak is duration-bounded, so each arm executes a different number of
lookups and **only the per-lookup ratios are comparable** — which is
`tf2.md`'s standing rule for every cachegrind figure in this repository. The
lookup denominator is `sample.rs`'s `if h == 0` branch count; `let half = len / 2`
is the control, and it holds at 19.2–19.3 bracket iterations per lookup across
all four arms, so the arms are doing the same search.

| arm | Ir/lookup | ΔIr | Bcm/lookup | ΔBcm | emits a select? |
|---|---|---|---|---|---|
| `half & (0 - cmp)` — **shipped** | 928.2 | — | 6.288 | — | no |
| `if cmp { mid } else { base }` | 926.3 | −0.20 % | 5.894 | −6.3 % | no |
| `black_box(0 - cmp)` on the mask | 973.3 | **+4.86 %** | 2.773 | −55.9 % | yes |
| `core::hint::select_unpredictable` | 930.1 | +0.21 % | 2.763 | −56.1 % | yes |

Per-line attribution on the shipped arm, from the same run: the index-select
line is **57.2 %** of the whole process's mispredicted branches at a **37.3 %**
mispredict rate on that single branch, and the `while len > 1` back-edge is a
further 18.1 % — the two lines of `bracket` are three quarters of every
mispredict in the binary.

Four things follow, in descending order of how much they should change
what anyone does:

1. **The shipped mask is worse than the textbook `if` it replaced**, on both
   axes, on this toolchain. The `if` was rewritten away partly on the argument
   that it "depends on the optimizer continuing to choose `cmov`". It does not
   choose `cmov` for either form any more, and the form kept for not depending
   on that choice is the more expensive of the two.
2. **`select_unpredictable` is the only spelling that gets the branch out at
   roughly no instruction cost.** It is a hint, so a future backend may revert
   it — which is exactly the exposure the deleted comment wrongly claimed the
   mask had escaped. That is an argument for keeping a *measurement*, not for
   keeping an assertion.
3. **The mask cannot be rescued in place.** Wrapping the *mask* in the hint —
   `half & select_unpredictable(cmp, u64::MAX, 0)` — still emits no `cmov`:
   LLVM canonicalises `x & select(c, ~0, 0)` back into `select(c, x, 0)` and the
   conversion pass expands that exactly as before (measured: 13 `.loc` sites,
   0 `cmov`). The hint has to be applied to the *index*, not to the mask, which
   means the mask spelling goes away either way.
4. **`black_box` reaches the same mispredict number and is not worth it.** It
   spills the mask through the stack (`movq %r15, 144(%rsp)` … `andq
   144(%rsp), %r11`) on the loop-carried chain, and costs 4.9 % of the whole
   lookup's instructions to do it. It is also a barrier with no stability
   contract about what it prevents.

## Decision (proposed, not taken)

Move `[workspace.package] rust-version` from `1.87` to `1.88` and write the
update as `core::hint::select_unpredictable(self.stamp_at(mid) <= t, mid, base)`,
which is a value-level identity with the current expression.

**It is proposed rather than taken because the cost is not the edit.**
`select_unpredictable` stabilised in 1.88. `just msrv` holds the floor in three
places including the prose a user reads (`README.md`, `SUPPORT.md`, `lib.rs`),
so this is a user-visible support-window move on a published crate, and
`CLAUDE.md`'s own workflow puts that in a record rather than in a PR. The whole
point of this document is that the alternative — landing it inside a
documentation fix, on the strength of a cachegrind ratio — is how the claim it
corrects got in.

## Rationale

**Why not just leave the mask and fix the words.** That is what step 1 does, and
it is most of the value: the false sentence in `tf2.md`'s per-file table is the
one that tells the next optimiser the search is finished. But leaving the code
alone also leaves the *worse of the two branchy forms* in place, which no
argument now supports.

**Why not revert to the plain `if`.** It measures better than the mask here and
needs no MSRV move, so it is the cheapest thing on the table. It is not proposed
because the gain is 0.2 % of instructions and 6 % of mispredicts — small enough
that on the next toolchain the ordering could invert — and because reverting a
recorded change on one cachegrind run, on a host that fails the project's own
timing-fitness probe, is the shape this repository has already been burned by
twice on this exact line. If the MSRV move is refused, this becomes the fallback
and wants its own re-measurement.

**Why not `black_box`.** Measured above: same mispredict result, 4.9 % more
instructions, and no stability contract.

**Why not an inline `asm!` select.** `tf_tree_core`'s unsafe budget
([`0007`](./0007-the-unsafe-budget-and-the-c-abi.md)) permits unsafe only at a
boundary the compiler cannot see across, and only in `buffer.rs` and
`arena_view.rs`. A codegen hint is not a boundary.

## Consequences

- The MSRV floor becomes a thing this repository moves for a hot-path reason and
  not only for a dependency's. `Cargo.toml`'s MSRV comment block records why each
  previous bump happened; this would be the first one that is ours.
- A hint is not a guarantee. Whatever lands, **the durable artifact is the
  cachegrind Bcm-per-lookup number and the command that produces it**, not a
  sentence in a comment. Nothing in `just lint`, `just bench-check` or
  `scripts/evidence-audit.sh` can see a `cmov` turn back into a `jle`; the
  disassembly command in the `bracket` doc block is the only instrument, and it
  is a person running it.
- If the search does become branchless, `tf2.md`'s per-file mispredict table has
  to be re-derived rather than edited — the whole column moves.

## Implementation plan

1. **Correct the sites and keep the measurement beside them.** — landed with
   this record, as marked `CORRECTION`s rather than rewrites. **The five in the
   table above are the ones that made the mechanism claim; the sweep found four
   more spellings that inherit it**, which is why the instrument is
   `grep -rn 'branchless\|cmov' crates/ docs/` and not the list: `sample.rs`'s
   module doc ("is a branchless binary search"), `bracket`'s own summary line
   ("**branchlessly**"), `bracket_from`'s pointer to it, and `fast-path.md`'s
   lever ledger row **3b**. `fast-path.md` also carried the *first* retracted
   explanation ("LLVM had already turned it into a `cmov`") as if current, in its
   own *What this document got wrong* section and again in its rule box —
   `tf2.md` had marked that one wrong and `fast-path.md` never heard. Verified by
   that grep reading as a correction, a heading, or history at every hit.
2. **Register the run.** — landed: a probe row in
   [`EVIDENCE.md`](../benchmarks/EVIDENCE.md) naming the container command, so
   the table above has a producer a doubter can re-run.
3. **Decide the MSRV move.** Open. It needs an owner's answer on the support
   floor, not more measurement.
4. **If step 3 says yes**: bump `rust-version`, the three prose sites `just msrv`
   holds, and `CHANGELOG.md`; write the select; re-run the container command
   above and update this record's table with the shipped arm's new row.
   Verified by `just msrv`, `just test`, and the two `grep -c cmov` lines above
   reading non-zero at the bracket sites. **That command was red-tested both
   ways** while this record was written: against the shipped mask it reports
   7 inlined sites and 0 `cmov`; against
   `base = select_unpredictable(stamp <= t, mid, base)` it reports 6 sites and
   6 `cmov`.
5. **Re-time on a fit host.** The mispredict reduction is a cachegrind model
   result. `perf_event_paranoid=4` on the development host, and that host fails
   `Fitness::probe`, so nobody here can say what 3.5 fewer mispredicts per lookup
   is worth in nanoseconds. Until someone can, the claim this record supports is
   *"the branch exists and is large in the model"*, never *"removing it is worth
   N ns"*.

## Open questions

1. **Is a 1.87 → 1.88 floor acceptable on the `0.0.x` line?** Not a code
   question. `Cargo.toml`'s comment block shows every previous move was forced by
   a dependency; this one would be chosen.
2. **Does the mispredict reduction survive a real predictor?** cachegrind models
   a two-level predictor, not a Zen 3 TAGE. A 37.3 % mispredict rate on a binary
   search's own comparison is close to the coin flip the loop's structure
   predicts, so the model is plausible here — but plausible is not measured, and
   step 5 is the only thing that closes it.
3. **Is `bracket`'s back-edge the next target?** It is 18.1 % of the process's
   mispredicts in the same run and this record does not touch it. Unrolling to a
   fixed trip count would remove it and is a different change with a different
   cost.

## Index row

Owed to `docs/decisions/README.md`. Left to the integrator on purpose: four
branches editing one table is how that file conflicts.
