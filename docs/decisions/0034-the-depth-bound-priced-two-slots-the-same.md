# 0034: the depth bound priced two slots the same

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** none — prototyped and measured, not landed.

## Context

Filed as issue #251. `MAX_DEPTH` is documented as a bound on the *compiled* plan
and enforced as a bound on the *raw* walk, and those are not the same number.

Executed:

```
16 static edges -> OK,  plan.len() = 1
17 static edges -> ERR TreeTooDeep { depth: 16 }
40 static edges -> ERR TreeTooDeep { depth: 16 }
```

Every `OK` row is **one step**. The 17-edge chain would have been one step too;
it is refused for a cost it does not incur. And the control the issue lacked
shows it is not an all-static problem: a *mixed* path — 15 fixed links plus a
2-joint wrist — compiles to 3 steps and is refused at 17 raw edges just the same.

This is the URDF sub-assembly shape. `MAX_DEPTH`'s own justification is "Real
trees are 4–8; 16 is generous", which is sound about a moving `/tf` graph and
wrong about a rigid assembly, where a manipulator plus gripper plus tool frames,
or a sensor mast with per-joint fixed offsets, routinely exceeds 16.

### Why the two slots cannot share a number

Measured, at `MAX_DEPTH = 16`:

| | |
|---|---|
| `size_of::<Step>()` | **128** (`Iso3` is 64; the discriminant forces a second cacheline) |
| `size_of::<Plan>()` | **2112** = 64 + 128·`MAX_DEPTH` |
| plan cache | 16 direct-mapped slots, **thread-local**, `Plan` by value — 34.8 KiB per thread |
| `Guard` | carries `[Cell<u64>; MAX_DEPTH]`, written in full by every `Guard::new` |

Sweeping the constant and rebuilding: 24 → 3136, 32 → 4160, 48 → 6208, 64 →
8256. At 64 the thread-local cache alone is ~130 KiB per thread.

A **raw** slot, by contrast, is a `u32` edge id in `compile`'s stack frame: 4
bytes, paid once, on a call D3 already places off the hot path. 128 bytes against
4 is why one number cannot price both.

## Decision

**Separate the two bounds.** `MAX_DEPTH` keeps its name and its value and becomes
what its doc always said — the length of the compiled `[Step; MAX_DEPTH]` array,
counted *after* folding. A new `MAX_PATH_EDGES` bounds the walk.

**The enabling observation, and the reason this is small:** `compile` already has
the raw buffer. It walks into `t_edges` / `s_edges`, two `[u32; MAX_DEPTH]`
arrays, and then *copies them into* a `[Step; MAX_DEPTH]` array purely to hand
that to `fold`. That intermediate array holds no information the two `u32` slices
do not — an edge id, plus an `inverted` flag that is `true` iff the edge came
from `t_edges`. Delete it, let `fold` take the two slices, and the raw bound is
free to be generous.

Nothing about `Plan`, `Guard`, the plan cache, `PyPlan` or `tft_plan` changes.

**No new `LookupError` variant.** `TreeTooDeep { depth }` keeps its shape; `depth`
becomes the true raw depth instead of always the bound. A new variant would need
a new `tft_status` in a **frozen** C ABI and a new Python arm, to describe a path
nobody has.

## Rationale

Four shapes were considered; three lose.

**(A) Raise `MAX_DEPTH`.** Everyone pays, on the hot path, so a minority shape can
be served — see the table above. Note also that `0022`'s "cutting `MAX_DEPTH`
16 → 8 moved nothing" must **not** be read as licence: it measured a 2× shrink
below a plateau, which says nothing about a 4× grow above it. Nobody has measured
the grow direction.

**(D) Fold during the walk.** The shape that looked most elegant, and it is
*unnecessary* rather than merely risky — deleting the intermediate array already
buys (D)'s whole prize. It would also have been wrong: `compile` emits the source
half in **reverse** of walk order, so `fold` composes `((s[n-1] * s[n-2]) * ...)`
while an accumulator meeting `s[0]` first can only produce the right-nested
association. `Iso3` composition is not associative under rounding, and every
existing test is tolerance-based (`TOL = 1e-12`), so **nothing in the suite would
have caught the change.** It would also drag the domain check and an accumulator
into the seqlock retry loop.

**(C) alone, two budgets without the deletion**, means carrying a second
`[Step; N]` scratch array — 128 bytes a slot for data that fits in 4.

## Consequences

* **`MAX_DEPTH`'s meaning changes while its value does not.** It is `pub const`
  and re-exported, so this is a semver-relevant change on the `0.0.x` line, and
  `docs/PHASE1.md` §7.1 contradicts it as written. That is why this is a record.
* **The worst *accepted* compile gets much slower**, which is the honest cost and
  is the third axis `MAX_PATH_EDGES`'s value sets. Measured, `taskset -c 2`,
  median of 15 × 20 000 `Tree::plan` calls:

  | path | HEAD | patched |
  |---|---|---|
  | 5 static edges | 182.7 ns `Ok(1)` | **145.8 ns** `Ok(1)` |
  | 16 static edges | 314.3 ns `Ok(1)` | **260.2 ns** `Ok(1)` |
  | 40 static edges | 49.5 ns `Err(TooDeep)` | 515.3 ns `Ok(1)` |
  | 128 static edges | 49.5 ns `Err(TooDeep)` | 1448.0 ns `Ok(1)` |
  | 256 static edges | 49.6 ns `Err(TooDeep)` | 2813.1 ns `Ok(1)` |

  Paths that already compiled get **faster** — the deleted copy is real work. The
  new cost is paths that were refused in 50 ns now succeeding in up to 2.8 µs,
  which is the feature and not a regression. But 2.8 µs is ~9× the previous worst
  *accepted* compile, and the facade plan cache is 16 direct-mapped slots, so a
  working set past 16 hot pairs pays it per lookup rather than once.
* **Error precedence moves.** Today the depth check runs before `fold`, so any
  path over 16 raw edges returns `TreeTooDeep` whatever is on it. After, `fold`
  runs first, so a long path with an unknown edge or a domain clash returns
  `UnknownEdge` / `MixedTimeDomains`, and one with the `edge == 0` sentinel
  returns `MissingEdge`. Arguably better in every case; it is still a change in
  what a caller sees and it belongs in the spec text, not only here.
* Results are unchanged: 676 of 676 evaluations bit-equal against a rebuilt HEAD.
  Bit-equal, not within tolerance — that is the check (D) would have failed.

## Implementation plan

1. **`MAX_PATH_EDGES`, and `MAX_DEPTH`'s doc corrected** — including deleting its
   "(used by the next PR)" leftover, which has outlived several PRs. Verified by
   `cargo doc` and by the constant appearing in `docs/API.md`'s surface list.
2. **`fold` takes the two `u32` slices; the intermediate `[Step; MAX_DEPTH]` array
   is deleted.** Verified by a bit-equality harness against a rebuilt HEAD, not by
   a tolerance test — see the (D) argument.
3. **The depth checks move**, raw bound in the walk, compiled bound after `fold`.
   Verified by the 17/40/128/256-edge table above and by `TreeTooDeep { depth }`
   reporting the true raw depth.
4. **Every prose site that says the limit is 16.** The prototype found five and
   the reviewer found three more that it missed: `crates/tf_tree_core/src/error.rs`
   (the variant's own doc and its field doc, both false after the change),
   `crates/tf_tree_bench/src/bin/scale_sweep.rs` (which **prints** "a tf2 tree
   deeper than 16 cannot be migrated" to the user), and
   `crates/tf_tree_c/src/error.rs` (which survives only by being vague). Verified
   by grepping the built artifacts, not the sources.
5. **`tests/python/test_errors.py`'s mutant note goes stale** — it asserts the
   rendered message is `TreeTooDeep { depth: 16 }`, and after the change the same
   chain renders `depth: 24`. Verified by running it.
6. **`docs/PHASE1.md` §7.1 and `docs/benchmarks/tf2.md`.** The spec half.
7. **The gates the prototype did not run:** `just bench-check`, `just loom`,
   `just miri`, `just tsan`, `just msrv`, `just audit`.

## Open questions

1. **What is `MAX_PATH_EDGES`?** The prototype chose 256 and justified it on stack
   bytes (two `[u32; 256]` buffers are 2 KiB together) and on bounding a runaway
   walk. The measurement above adds a third axis the prototype did not consider:
   256 sets the **worst accepted compile latency** at 2.8 µs. A smaller bound —
   64, say — would cap it near 700 ns while still covering every rigid assembly
   anyone has described. Nobody has surveyed real URDF depths, and that survey is
   what should choose the number.
2. **Does the precedence change need a test, or only a spec sentence?** No test
   pins error precedence today, which is why the change was invisible.
3. **Is `TreeTooDeep`'s advice actionable from every binding?** The prototype's new
   message ends "or make the fixed links static so they fold", and the Python API
   cannot declare a static edge on an existing tree. A message that names a remedy
   the caller cannot reach is worse than one that does not.

## Not part of this record

`TFT_MAX_DEPTH` is referenced at `crates/tf_tree_c/include/tf_tree.h:281` and
`crates/tf_tree_c/src/error.rs:69` and is **defined nowhere**. Pre-existing, in
the frozen header, found while surveying the constant's consumers. It needs
`xtask headers` and its own change.
