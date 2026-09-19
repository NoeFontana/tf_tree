# 0053: the branchless bracket that branches

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** the prose half has landed (this record's step 1). The code
half has not, and step 3 is why.

## Context

`SampleRing::bracket`'s index update is a mask:
```rust
let cmp = u64::from(self.stamp_at(base + half) <= t);
base = base.wrapping_add(half & 0u64.wrapping_sub(cmp));
```

LLVM folds the mask into a `select` and the x86 cmov-conversion pass expands it into
control flow: every inlined copy in the `--release` rlib is `jle`, no `cmov`.

```text
cargo rustc -p tf_tree_core --release --lib -- --emit asm -C debuginfo=2
```

Count `cmov` near that line's `.loc` in `target/release/deps/tf_tree_core-*.s`: 0.

## The measurement

`soak --workload robot --duration 3s --interval 2s` under cachegrind
(`--branch-sim=yes`), `[profile.profiling]`; only per-lookup ratios compare.

| arm | Ir/lookup | ΔIr | Bcm/lookup | ΔBcm | emits a select? |
|---|---|---|---|---|---|
| `half & (0 - cmp)` — **shipped** | 928.2 | — | 6.288 | — | no |
| `if cmp { mid } else { base }` | 926.3 | −0.20 % | 5.894 | −6.3 % | no |
| `black_box(0 - cmp)` on the mask | 973.3 | **+4.86 %** | 2.773 | −55.9 % | yes |
| `core::hint::select_unpredictable` | 930.1 | +0.21 % | 2.763 | −56.1 % | yes |

On the shipped arm the index-select line is **57.2 %** of mispredicted branches
(**37.3 %** rate); the `while len > 1` back-edge is 18.1 %. `select_unpredictable`
alone removes the branch at no instruction cost (a hint, so a backend may revert it)
and must be applied to the *index*, not the mask.

## Decision (proposed, not taken)

Move `rust-version` from `1.87` to `1.88` and write the update as
`core::hint::select_unpredictable(self.stamp_at(mid) <= t, mid, base)`, a
value-level identity. Not taken: `select_unpredictable` stabilised in 1.88 and
`just msrv` holds the floor in three places (`README.md`, `SUPPORT.md`, `lib.rs`).
The plain `if` is the fallback; `black_box` and inline `asm!`
([`0007`](./0007-the-unsafe-budget-and-the-c-abi.md)) are rejected. No gate sees a
`cmov` turn back into a `jle`; `tf2.md`'s mispredict table is re-derived, not edited.

## Implementation plan

1. **Landed.** Claims corrected (`grep -rn 'branchless\|cmov' crates/ docs/`).
2. **Landed.** A probe row in [`EVIDENCE.md`](../benchmarks/EVIDENCE.md).
3. **Decide the MSRV move.** Open.
4. **If yes**: bump `rust-version`, the three prose sites and `CHANGELOG.md`; write
   the select; re-run the command.
5. **Re-time on a fit host**; until then the claim is *"the branch exists in the
   model"*, never *"removing it is worth N ns"*.

## Open questions

1. **Is a 1.87 → 1.88 floor acceptable on the `0.0.x` line?**
2. **Does the reduction survive a real predictor?** Step 5.
