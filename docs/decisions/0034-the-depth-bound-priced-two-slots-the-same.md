# 0034: the depth bound priced two slots the same

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** landed in one change (#251). Both bounds moved, `fold` takes
the two `u32` slices, variant C, and the twenty prose/test sites.

> **Implementation notes**, kept because two of them are numbers this record
> predicted and got wrong.
>
> * **The combined cost is +83.5%, and the record's own prediction that deleting
>   the array would pay for the raise was wrong.** Re-measured on the landed
>   change with a three-arm design — HEAD, an intermediate build carrying only
>   `MAX_DEPTH = 32`, and this one: the constant costs +114.2% and the deletion
>   buys back −14.1% (0 of 30 reps slower). See *Consequences*, which carries the
>   full table; the remainder is #264.
> * **The bit-equality harness has a non-vacuity control, and needed one.** 4376
>   shapes bit-identical to a rebuilt HEAD — exhaustive over every static/dynamic
>   pattern × every LCA split for lengths 1–8, so straight chains, Y shapes and
>   every interleaving, with non-identity poses. A third build that right-
>   associates each static run differs on **1881 of them** at max 2.132e-14,
>   which is two orders *inside* the suite's `TOL = 1e-12`. That is rationale
>   (D)'s argument, executed: the whole existing suite would accept the
>   reassociation this record refuses. An independent reviewer rebuilt the
>   harness from scratch over 28919 rows and reproduced both numbers.
> * **One mutant came back equivalent, and that was the finding.** Replacing the
>   combined `nt + ns` raw bound with the per-side spelling passed 881/881,
>   because every depth test used a straight chain where the two are identical.
>   A `y_arena(40, 40)` fixture was added; the same mutant then returns
>   `Ok(Plan { len: 1 })` for an 80-edge path against a bound of 64.
> * **The `depth == MAX_PATH_EDGES` seam is the row both message renderers
>   needed.** A path of exactly the walk's bound is walked in full and then
>   refused by the compiled bound, so `depth` takes its largest compiled-refusal
>   value there — and `>=` instead of `>` blames the walk for a path it walked.
>   Both the Rust and the Python suite were green on that mutant until a row at
>   the seam was added to each.
> * Three stale numbers were corrected rather than propagated: `plan.rs`'s
>   "keeps `Guard` at 48 bytes" (208 before this change, 336 after),
>   `PHASE4.md`'s "the guard's 208 bytes", and a 21% disagreement between two
>   harnesses on the worst accepted compile — recorded as the 0.9–1.1 µs range it
>   is, rather than quoted to three digits from one of them.

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

### This record, as first drafted, does not solve the problem it names

Separating the two bounds and leaving `MAX_DEPTH` at 16 rescues **6 of the 26
robots HEAD refuses**. Twenty stay refused, on the *compiled* bound, because
their long paths are nearly all revolute and folding has nothing to collapse.

That is not an inference. Its own open question 1 said "nobody has surveyed real
URDF depths, and that survey is what should choose the number"; the survey was
then done — **91 distinct kinematic structures from 26 robot-description
repositories** (`.xacro` expanded inside `ros:lyrical-ros-base`, `fixed` mapped
to `EdgeKind::Static` and every other joint type to a dynamic edge, the way
`robot_state_publisher` splits `/tf_static` from `/tf`). Every one of the 91 was
built as a real `tf_tree::Tree` and its worst pair handed to `Tree::plan`, so the
verdicts below are the engine's and not a model's. Two parsers were written
independently and disagreed on **0 of 91**; the tree extraction was hand-checked
against UR5 and turtlebot3, the second of which is the useful control because its
worst *folded* pair is not on its diameter.

| | robots |
|---|---|
| accepted by HEAD | 65 of 91 |
| refused by HEAD, rescued by this record as first drafted | **6** |
| refused by HEAD, **still refused** with `MAX_PATH_EDGES` alone | **20** |

**Twenty is the raw count and it is vendor-inflated**: 17 of the 20 are Unitree
near-duplicate humanoid variants out of one repository. The honest non-Unitree
count is **three robots** — PR2 (`pr2_no_kinect`, folded 26), MoveIt's dual-arm
Panda (19), and the Shadow bimanual hand (17) — plus Unitree H2 Plus at 28, which
is the deepest structure in the corpus. Three is a small number and it is the
right one to argue from; it is also PR2, which is the canonical tf robot, and
every dual-arm MoveIt configuration.

**The binding number is the graph DIAMETER, not the root-to-leaf depth**, because
`compile` walks up to the LCA and back down. The survey measured both, and the
difference decides the constant:

| metric, all 91 structures | min | p50 | p90 | p95 | max |
|---|---|---|---|---|---|
| DIAMETER (raw edges) | 1 | 10 | 22 | 24 | **30** |
| MAX folded (compiled steps) | 1 | 8 | 22 | 24 | **28** |
| raw root-to-leaf depth | 1 | 7 | 14 | 15 | **18** |

A survey that had measured depth would have found a maximum of 18 and concluded
that 24 was plenty. It is not: the deepest *pair* is fingertip to fingertip
across a torso, and it is 30 edges.

**The counts under-report.** Six of the 26 repositories produced no tree: five
have no `.urdf` or `.xacro` on disk at all, and `clearpath_common` ships only
macro libraries because its robot is generated at runtime from a `robot.yaml`.
Three more need packages the corpus does not carry (`pal_urdf_utils`,
`husarion_components_description`, `velodyne_description`). Two of the missing —
Clearpath and TIAGo-dual — are **exactly the mobile-base-plus-arm-plus-mast shape
this Context describes**, so whatever they would have contributed is contributed
by nothing here. TIAGo survives only because a pre-expanded URDF was already on
disk, and it is one of the 26 HEAD refuses.

### Why the two slots cannot share a number

Measured, at `MAX_DEPTH = 16`:

| | |
|---|---|
| `size_of::<Step>()` | **128** (`Iso3` is 64; the discriminant forces a second cacheline) |
| `size_of::<Plan>()` | **2112** = 64 + 128·`MAX_DEPTH` |
| plan cache | 16 direct-mapped slots, **thread-local**, `Plan` by value — 34.0 KiB per thread |
| `Guard` | **208 bytes**, carrying `[Cell<u64>; MAX_DEPTH]`, written in full by every `Guard::new` |

Sweeping the constant and rebuilding: 24 → 3136, 32 → 4160, 48 → 6208, 64 →
8256. At 64 the thread-local cache alone is ~130 KiB per thread.

A **raw** slot, by contrast, is a `u32` edge id in `compile`'s stack frame: 4
bytes, paid once, on a call D3 already places off the hot path. 128 bytes against
4 is why one number cannot price both.

Two rows above are corrections to the table this record carried while it was a
draft, both re-measured against `09efc9b`: the cache is **34.0 KiB** per thread,
not 34.8 — `Option<Entry>` has a niche, so it is exactly `16 × 2176` — and
`Guard` is **208 bytes**, not the "48 bytes" asserted in
`crates/tf_tree_core/src/plan.rs`'s `DETACHED` doc comment, a sentence that
predates the cursor array and is false at HEAD independently of this record.

## Decision

**Separate the two bounds, and move both.**

1. **`MAX_PATH_EDGES = 64`** bounds the raw walk. It is checked on **`nt + ns`,
   inside the walk loop**, so that "64" means 64 edges walked and not 128.
2. **`MAX_DEPTH` goes 16 → 32** and becomes what its doc always said: the length
   of the compiled `[Step; MAX_DEPTH]` array, counted *after* folding.
3. **`fold` takes the two `u32` slices; the intermediate `[Step; MAX_DEPTH]`
   array is deleted.**
4. **The compiled bound is checked after the fold loop, and the fold loop does
   not return early** — variant C below.
5. **No new `LookupError` variant.**

**The enabling observation, and the reason this is small:** `compile` already has
the raw buffer. It walks into `t_edges` / `s_edges`, two `[u32; MAX_DEPTH]`
arrays, and then *copies them into* a `[Step; MAX_DEPTH]` array purely to hand
that to `fold`. That intermediate array holds no information the two `u32` slices
do not — an edge id, plus an `inverted` flag that is `true` iff the edge came
from `t_edges`. Delete it, let `fold` take the two slices, and the raw bound is
free to be generous.

That claim was checked rather than asserted: `fold`-from-slices is **bit-equal to
HEAD on 21 shapes**, including interleaved static/dynamic ones. The interleaving
matters — the harness that first checked it had 13 shapes and every one of them
produced either `len == n` or `len == 1`, so the collapse branch never executed.
It was a vacuous control.

### Sizing

**`MAX_PATH_EDGES = 64`.** The floor is the corpus's raw diameter of 30 plus a
deployed `/tf` prefix: `map -> odom -> base_footprint` sits above the robot root,
so ~33. 64 is ~1.9× that. Not 32, which clears the floor by nothing and would
refuse H2 Plus's 30-edge diameter under any deployment prefix at all. Not 256:
the bound sets the worst *accepted* compile, measured **1.09 µs at 64 against
3.97 µs at 256**, and #259 — a failed `Tree::plan` is never cached, because the
`?` precedes the store at `crates/tf_tree/src/cache.rs:214-216` — means a refused
pair pays that on **every lookup, forever**, with nothing amortising it.

**`MAX_DEPTH = 32`.** The corpus needs 28 folded steps; 32 is the next power of
two, leaves room for the two dynamic prefix edges above the robot root, and every
hot-path row measured is flat there (§ *Rationale (A)*). 24 is not worth choosing:
it costs 82 % of the compile path and still refuses PR2 and three humanoids.

### `TreeTooDeep` keeps its shape, and its message changes

`depth` stays a `u16`. A new variant would need a new `tft_status` in a **frozen**
C ABI and a new Python arm, to describe a path nobody has.

What does change is what `depth` *means*, and the message must stop naming a
maximum. Today `crates/tf_tree/src/tree.rs:3634` renders

```
path depth 16 exceeds the maximum of 16
```

— self-contradictory, and it ships. `crates/tf_tree_py/src/errors.rs:756-768`
already documents that exact phrasing as false and renders a better one; the Rust
facade never got the fix. After this record there are **two** bounds, so naming
one `MAX` is not merely contradictory but ambiguous.

**The remedy the message names must be reachable from the binding it is read
in.** The prototype's phrasing ended "or make the fixed links static so they
fold", and that is not a universal remedy — **Python cannot declare a static edge
at all**: `crates/tf_tree_py/src/tree.rs:1556-1562` shows `tf_tree.build` calling
only `dynamic_edge`, so this is stronger than "not on an existing tree". C reaches
one only through the unstable, feature-gated `tft_bridge_create` TOML. Per
`API.md` R5 the identifier is the contract and the prose is a separate layer, so
the core carries the identifier and **each binding's prose layer carries the
remedy its own callers can act on**. The core's own rendering says what happened
and offers the one remedy every binding has: re-parent so the two frames share a
nearer ancestor.

Nothing about `Plan`'s shape, the plan cache's slot count, `PyPlan` or `tft_plan`
changes. `Plan` gets bigger; it does not change kind.

## Rationale

Four shapes were considered; two lose, one is subsumed, and one was reversed by
measurement.

### (A) Raise `MAX_DEPTH` — REVERSED, by measurement

As drafted, this record rejected it:

> **(A) Raise `MAX_DEPTH`.** Everyone pays, on the hot path, so a minority shape
> can be served — see the table above. Note also that `0022`'s "cutting
> `MAX_DEPTH` 16 → 8 moved nothing" must **not** be read as licence: it measured
> a 2× shrink below a plateau, which says nothing about a 4× grow above it.
> Nobody has measured the grow direction.

**The second sentence is why the first is now known to be wrong.** The warning
about `0022` was exactly right, and it is the reason the measurement had to be
made rather than reasoned about. It has been made: three builds differing in one
character (`crates/tf_tree_core/src/lib.rs:170`, `16 | 24 | 32`), `taskset -c 2`,
**row-level round-robin** so the same benchmark row runs on all three builds ~1.3 s
apart with the build order rotating every repetition, and the reported figure is
the **median of the per-repetition paired differences**, n = 30. The harness is the
project's own criterion targets plus `examples/guard_cost.rs`, plus one added probe
for the three things no existing target reports (`size_of`, the facade
plan-cache-hit path, and a plan with a genuinely 6-step compiled length — the
criterion suite's deepest row folds to three). The `16` and `32` columns are the
best of all repetitions per build, in ns; `unanimity` counts the repetitions in
which the 32 build was the slower one, so ~n/2 is noise and n/n is an effect. The
delta is the median paired difference and **not** the difference of the two
columns beside it.

| row | 16 | 32 | median paired Δ | unanimity |
|---|---|---|---|---|
| `lookup/depth1/sclerp` | 65.42 | 65.44 | −0.06 ns (−0.1 %) | 14/30 |
| `lookup/depth3/sclerp` | 189.06 | 188.98 | +0.72 ns (+0.4 %) | 18/30 |
| `lookup/depth3/lerpslerp` | 131.38 | 131.58 | +0.23 ns (+0.2 %) | 18/30 |
| `lookup/depth6/sclerp` | 132.84 | 133.13 | +0.65 ns (+0.5 %) | 18/30 |
| `lookup/depth3/sclerp/exact_hit` | 39.46 | 39.51 | −0.03 ns (−0.1 %) | 14/30 |
| `query_mix/depth3` | 146.34 | 146.01 | +0.05 ns (+0.0 %) | 16/30 |
| `at_many/monotone_1024` (per 1024 call) | 291204 | 291935 | +439 ns (+0.15 %) | 10/14 |
| `at_many/into_mat4_1024` (per 1024 call) | 286081 | 285409 | −750 ns (−0.26 %) | 1/14 |
| probe `at()` len 6, guard hoisted | 206.11 | 203.04 | −2.76 ns (−1.3 %) | 8/30 |
| **`Guard::new` alone** | 2.47 | 3.59 | **+1.12 ns (+45.3 %)** | **30/30** |
| `guard per call, at()` | 120.60 | 120.90 | +0.50 ns (+0.41 %) | 7/8 |
| `Tree::lookup` cache hit, len 3 | 405.39 | 406.62 | +2.08 ns (+0.5 %) | 22/30 |
| **`Tree::plan`, cache MISS, len 6** | 166.23 | 362.48 | **+196.75 ns (+118.4 %)** | **30/30** |

**Everyone does not pay on the hot path.** Two effects are real and both are unanimous, and they are of very
different sizes:

* `Guard::new` writes the whole cursor array unconditionally, so it is O(`MAX_DEPTH`)
  by construction — **and it is 1.1 ns**. `Tree::lookup` builds a fresh `Guard` on
  every call, which is the worst case for that cost, and it is invisible there.
  `Plan::at` folds over `self.steps()`, which is `&self.steps[..len]`, so the fold
  is O(`len`) and the constant sets only the array's declared size. That is why the
  flat rows are expected rather than lucky.
* **The real cost is `Tree::plan` on a cache miss**, and it is not the fold: `compile`
  materialises **two** `[Step; MAX_DEPTH]` arrays per call and returns `Plan` by
  value — 4 KiB of stores at 16, 8 KiB at 32, independent of the path's real length.

That second one is the cost **this record's own step 2 removes**, which is why the
two changes belong in one landing and not two.

Memory, measured the same way and cross-checked two ways per build:

| | 16 | 32 |
|---|---|---|
| `size_of::<Plan>()` | 2112 | 4160 |
| `size_of::<Guard>()` | 208 | 336 |
| `size_of::<cache::Entry>()` | 2176 | 4224 |
| 16-slot thread-local plan cache | **34.0 KiB** | **66.0 KiB** per thread |

**Shortening `Guard`'s cursor array to pay for the raise is measured and
rejected.** `sample_hinted` already degrades out of range, so `cursor:
[Cell<u64>; 16]` with `MAX_DEPTH = 32` is sound with no other change, and it does
restore `Guard::new` to 2.47 ns exactly. It also costs **31 % of the hot evaluate
path** — `at()` len 3 104.88 → 136.45 ns, `lookup/depth3/sclerp` 188.85 → 247.29 —
with two clean controls: a cursor cap *equal* to `MAX_DEPTH` is indistinguishable
from HEAD shape (±0.1 %), so the regression is not the refactor, and a cursor cap
of 64, making `Guard` 592 bytes, is within +2.3 %, so it is not `Guard`'s size
either. The mechanism left over is that `cursor.len() < MAX_DEPTH` makes
`self.cursor.get(k)` unprovable from `k < len ≤ MAX_DEPTH`, so the check and its
`else` arm survive inside the inlined fold; the cost is per step (10.5 ns/step
either way, over 3 steps and over 6). **That mechanism is a hypothesis carried by
two controls, not a disassembly** — the `objdump` of `fold_at` in the two builds
was not taken. The controls are what the decision rests on; the explanation is
what it offers.

So `Guard::new` writing all `MAX_DEPTH` cells is the cheap arm of a trade rather
than an oversight: it buys 1.1 ns to save ~10 ns per step. Whoever lands this puts
that in `Guard::cursor`'s doc, because shortening it is the obvious optimisation.

### (D) Fold during the walk — unnecessary, and would have been wrong

The shape that looked most elegant, and it is *unnecessary* rather than merely
risky — deleting the intermediate array already buys (D)'s whole prize. It would
also have been wrong: `compile` emits the source half in **reverse** of walk
order, so `fold` composes `((s[n-1] * s[n-2]) * ...)` while an accumulator meeting
`s[0]` first can only produce the right-nested association. `Iso3` composition is
not associative under rounding, and every existing test is tolerance-based
(`TOL = 1e-12`), so **nothing in the suite would have caught the change.** It
would also drag the domain check and an accumulator into the seqlock retry loop.

### (C) Two budgets without the deletion

Carrying a second `[Step; N]` scratch array — 128 bytes a slot for data that fits
in 4. Subsumed by the deletion.

### Where the compiled bound is checked — three variants, and taking step 3 literally panics

This record's draft said the compiled bound is checked "after fold". Taken
literally that **panics**: `fold` writes `out[n]` inside its loop, so with a
generous raw bound and nothing folding, `n` runs past `MAX_DEPTH` and the write is
out of bounds. Three shapes were built:

| variant | shape | verdict |
|---|---|---|
| **A** | check inside the fold loop, return `TreeTooDeep` early | **loses.** A defect *past* the compiled bound then returns `TreeTooDeep` instead of `UnknownEdge` / `MixedTimeDomains` — it does not deliver the precedence this record's *Consequences* promises |
| **B** | fold into a `[Step; MAX_PATH_EDGES]` scratch, check after | **loses on cost.** Delivers the precedence, at 32 KiB of stack when the raw bound is 256 |
| **C** | output array stays `[Step; MAX_DEPTH]`; the loop never returns early | **chosen** |

**Variant C in full**, because it is the part an implementer would otherwise have
to invent: when `n >= MAX_DEPTH` the write is **skipped**, `n` keeps incrementing,
and every remaining edge is still resolved through `edge_meta`, so `UnknownEdge`
and `MixedTimeDomains` are still raised from past the array's end. The collapse
decision reads a tracked `last_static: bool` instead of `out[n - 1]`, which is
what lets it work there at all. After the loop, `if n > MAX_DEPTH { return
TreeTooDeep { depth: n } }`.

C was measured against B on 40 rows and matched on all of them, and is bit-equal
to HEAD on every accepted shape, at A's stack cost. Its only cost is on **refused**
paths — a refused 64-edge dynamic chain is 993.7 ns under A and 1777.6 ns under C,
because C resolves every edge before refusing — with two controls in which A and C
do identical work showing +3.2 and −6.2 ns, i.e. noise. Paying ~800 ns on a path
that is about to be refused, to refuse it for the *right* reason, is the trade this
record wants; #259 is the reason it is not free, and #259 is the argument for the
smaller raw bound rather than for variant A.

## Consequences

* **`MAX_DEPTH`'s meaning changes and so does its value.** It is `pub const` and
  re-exported, so this is a semver-relevant change on the `0.0.x` line, and
  `docs/PHASE1.md` §7.1 contradicts it as written. That is why this is a record.
* **The worst *accepted* compile gets much slower**, which is the honest cost and
  is the axis `MAX_PATH_EDGES`'s value sets. Measured, `taskset -c 2`, median of
  15 × 20 000 `Tree::plan` calls on a straight static chain of *k* edges. `head`
  is `09efc9b`, where the single bound is 16:

  | k (raw edges) | head | bound 32 | bound 64 | bound 256 |
  |---|---|---|---|---|
  | 5 | 213.7 `Ok(1)` | 183.9 | 185.8 | 188.6 |
  | 16 | 377.5 `Ok(1)` | 349.4 | 350.2 | 352.6 |
  | 30 | **70.2 `TreeTooDeep{16}`** | 567.0 `Ok(1)` | 561.0 | 563.9 |
  | 60 | 69.2 `TreeTooDeep{16}` | 133.4 `TreeTooDeep{32}` | **1051.8 `Ok(1)`** | 1035.1 |
  | 128 | — | — | — | 2058.0 `Ok(1)` |
  | 256 | — | — | — | 3982.5 `Ok(1)` |

  Paths that already compiled get **faster** — the deleted copy is real work, 8–17 %
  across every accepted row. **The bound itself is nearly free; the path length is
  what costs**: at every *k* all three bounds accept, the three columns are within
  noise, with the only consistent gap ~+4 ns (+2 %) for 256 over 32 at k = 5,
  vanishing by k = 16. That is the arm where an effect of the bound should be
  absent, and it is. It also corrects a sentence this record carried as a draft:
  "256 sets the worst accepted compile latency at 2.8 µs" is true only of a
  256-edge *path*, not a cost the bound imposes on shorter ones. What survives is
  the ceiling — 1.09 µs at 64 against 3.97 µs at 256 — and #259 is what sharpens
  it, because a refused pair recompiles on every lookup with nothing amortising it.
* **The two halves of this record pull in opposite directions on the same line of
  code, and only the combination is the price.** Measured separately: deleting the
  intermediate array took a 5-edge compile from 182.7 to 145.8 ns at
  `MAX_DEPTH = 16`; raising `MAX_DEPTH` to 32 with the array still in place took a
  6-step compile from 166.23 to 362.48 ns. Landed together, a `Tree::plan` cache
  miss on that same 6-step path measures **308.57 ns against HEAD's 166.23 —
  +83.5%**, medians of paired per-rep deltas, 30/30 slower. That is the number
  that prices this record, because neither half ships without the other.

  **It was expected to come out at or under HEAD, and it does not.** The
  three-arm design is what shows why: an intermediate build carrying *only*
  `MAX_DEPTH = 32` (`lib.rs:170`, one line, `diff -r` confirming nothing else)
  isolates the constant from the change. The constant costs +114.2%; deleting the
  array buys back **−14.1%, 0 of 30 reps slower** — real work removed, and not
  close to paying for the raise. The reason is that the deleted array was one of
  three array-sized movements on that path and the other two grew with the
  constant: `fold` still fills a `[Step; MAX_DEPTH]`, and `Plan` is still returned
  by value at 4160 B. A follow-up that had `fold` write in place would attack the
  remainder; it is not this record's, and it is filed rather than folded in.

  **Taken anyway, and the trade is stated rather than buried.** The cost lands on
  a `Tree::plan` *cache miss*, which D3 already places off the hot path; the
  cache-hit control moved +2.13 ns (+0.4%). What it buys is that PR2
  gripper-tip-to-gripper-tip, MoveIt's dual-arm Panda finger-to-finger and the
  Shadow bimanual hand stop being refused outright — queries `tf2` answers today.
  A library offered as a faster `tf2` can afford 142 ns on a compile the plan
  cache amortises; it cannot afford refusing a lookup the incumbent returns.
* **Memory: `Plan` 2112 → 4160 B, `Guard` 208 → 336 B, and the thread-local plan
  cache 34.0 → 66.0 KiB per thread.** The cache stays 16 direct-mapped slots, so a
  working set past 16 hot pairs still pays a compile per lookup — and now pays a
  larger one.
* **Error precedence moves.** Today the depth check runs before `fold`, so any
  path over 16 raw edges returns `TreeTooDeep` whatever is on it. After, `fold`
  runs first, so a long path with an unknown edge or a domain clash returns
  `UnknownEdge` / `MixedTimeDomains`, and one with the `edge == 0` sentinel
  returns `MissingEdge`. Arguably better in every case; it is still a change in
  what a caller sees and it belongs in the spec text, not only here.
* **`MissingEdge` wins over `TreeTooDeep` by *position*, not by path length**, and
  that is unchanged by this record but was never written down. `push_edge!` raises
  it inside the walk, so a sentinel at walk step 5 gives `MissingEdge` and the same
  sentinel at step 20 gives `TreeTooDeep`. The precedence table step 5 of the plan
  asks for is what pins this.
* **`depth` is not "always the bound" today, and after this it means two different
  things.** This record claimed as a draft that `depth` "becomes the true raw depth
  instead of always the bound". Measured at HEAD, `TreeTooDeep { depth }` is
  `nt + ns` at the moment a guard fired, which is: the **bound** for a one-sided
  chain (17, 22, 30 and 40 edges all report `depth: 16` — and a one-sided chain is
  what the prototype fixture was); the **truth** for a balanced two-sided path
  (TIAGo 17, ANYmal C 20, PR2 26, H2 Plus 28); and **neither** for a lopsided one
  (p = 2, q = 20, raw 22, reports 16; p = 5, q = 20, raw 25, reports 17). After this
  record it is the true raw edge count when the *raw* bound refuses and the true
  folded step count when the *compiled* bound refuses — two different quantities
  behind one field, which is the other reason the message must not name a maximum.
* **A Y-shaped path walks up to 2× the bound before refusal, and this record fixes
  that.** The per-side guards bound `nt` and `ns` independently and the only
  combined check runs after the walk, so with `MAX_PATH_EDGES = 256` a balanced
  two-sided path returned `Err(TreeTooDeep { depth: 512 })` — 512 edges walked
  under a bound of 256. HEAD has the same shape at 16. Checking `nt + ns` in the
  loop is what makes the constant mean what it says.
* **What this declines to serve.** At `MAX_DEPTH = 32` the corpus is covered: the
  deepest folded pair anywhere in it is 28, and the deployed `/tf` prefix adds two
  dynamic edges above the robot root, so 30 of 32 is the worst case the survey can
  construct. The bound is chosen against that evidence and not for its roundness.
  The next shape past it is a **cross-robot** query — a fingertip on robot A to a
  fingertip on robot B through a shared `map`, which is two diameters plus two
  prefixes and lands near 60 folded steps. Nothing in the corpus is that, and this
  record does not serve it; a fleet-scale query is a different problem from a rigid
  sub-assembly and would want a different answer than a bigger array.
* Results are unchanged: 676 of 676 evaluations bit-equal against a rebuilt HEAD,
  and 21 of 21 shapes bit-equal for the slice-taking `fold`. Bit-equal, not within
  tolerance — that is the check (D) would have failed.

### The honest limits of the evidence above

* **The measurement host fails `Fitness::probe`** — 4 physical cores, SMT on, no
  readable cpufreq governor. So **no absolute number in the `(A)` tables is a
  `docs/PHASE5.md` §9 claim**; the claims are the **paired differences** between
  builds measured seconds apart on one core, which is the comparison §9.3 permits.
  The host was also shared with another agent's `cargo` for part of the session
  (1-minute load 1.37–10.78 across the 30 repetitions); a first, coarser interleave
  with 20 s between arms produced ±50 % swings and was discarded as a negative
  control on the method, and the row-level interleave agrees to ±0.5 % across that
  whole load range.
* **`just bench-check` PASSes at 16, 24 and 32**, with the one `MEASURED` row
  bit-identical at all three. **That is evidence about numerics, not latency.** On
  this host nine of its ten rows are `unavailable` — no ROS 2, no representative
  `.tft`, and the fitness probe fails — so the gate holds exactly one metric and it
  is a host-independent accuracy row. Do not quote it as a latency result.
* **The corpus counts under-report**, for the six repositories named in *Context*.
* **The cursor-shortening mechanism is a hypothesis with two controls**, not a
  disassembly.

## Implementation plan

Steps 1–3 are one landing or none: step 2 is the mitigation for the compile cost
step 3's constant adds, and step 1 without step 3 refuses PR2. Steps 4 onward can
follow as separate PRs.

1. **`MAX_PATH_EDGES = 64` added, `MAX_DEPTH` raised to 32, and both documented.**
   `MAX_DEPTH`'s doc loses its "(used by the next PR)" leftover, which has outlived
   several PRs, and its "Real trees are 4–8; 16 is generous" justification, which
   the survey refutes. Verified by `cargo doc` and by **both** constants appearing
   in `docs/API.md` — a **new** entry, not a check on an existing one: `MAX_DEPTH`
   appears in `docs/API.md` zero times today, so the draft's "the constant appearing
   in `docs/API.md`'s surface list" could not have been met as written. §6's delta
   table is where it goes, since that table's *Lands in* column is what makes a row
   schedulable and this record is what it lands in.
2. **`fold` takes the two `u32` slices; the intermediate `[Step; MAX_DEPTH]` array
   is deleted.** Verified by a bit-equality harness against a rebuilt HEAD — not a
   tolerance test, see (D) — over shapes that **include interleaved static/dynamic
   paths**, because a harness whose shapes all fold to `len == n` or `len == 1`
   never runs the collapse branch.
3. **The depth checks move.** Raw bound checked on `nt + ns` in each walk loop;
   compiled bound checked after `fold`, as **variant C**: writes past `MAX_DEPTH`
   skipped, `n` still incremented, every remaining edge still resolved through
   `edge_meta`, the collapse decision reading a tracked `last_static` rather than
   `out[n - 1]`. Verified by the k = 5/16/30/60 table above, by
   `Err(TreeTooDeep { depth: 128 })` for a balanced path under a bound of 64 being
   **absent** (the Y-walk finding), and by the precedence table in step 5.
4. **Every prose site that states the limit, audited rather than grepped for "16".**
   The draft named three and said the reviewer had found three more; the audit finds
   at least eleven, and two of them are not prose at all:

   | site | what it says today |
   |---|---|
   | `crates/tf_tree_core/src/lib.rs:168-170` | the constant's doc: "used by the next PR", "Real trees are 4–8; 16 is generous" |
   | `crates/tf_tree_core/src/error.rs:89-92` | the variant doc and its field doc, both false after the change |
   | `crates/tf_tree_core/src/plan.rs:519` | "a 2 KiB `[Step; MAX_DEPTH]`" — 4 KiB at 32 |
   | `crates/tf_tree_core/src/plan.rs:2492` | `compile`'s doc: "the combined path exceeds `MAX_DEPTH`" — now two bounds |
   | `crates/tf_tree_core/src/plan.rs` `DETACHED` doc | "48 bytes" for `Guard`; it is 208 at HEAD and 336 at 32 |
   | `crates/tf_tree/src/tree.rs:3634-3635` | "path depth 16 exceeds the maximum of 16" |
   | `crates/tf_tree/src/cache.rs:132` | hard-codes 2112, 2136 and 2176 |
   | `crates/tf_tree_py/src/errors.rs:756-768` | the rendered message and the comment arguing for it |
   | `crates/tf_tree_py/src/tree.rs:443` | hard-codes "align(64), size 2112" as the reason `PyPlan` boxes |
   | `crates/tf_tree_bench/src/workload.rs:35` | "`MAX_DEPTH` (**16**) caps a compiled plan. A 24-deep spine is refused outright" |
   | **`crates/tf_tree_bench/src/workload.rs:1051-1055`** | **not prose — a `bail!`** comparing a *raw* deepest chain against `MAX_DEPTH`. After the split it must compare against `MAX_PATH_EDGES`; the compiled bound is not knowable without compiling |
   | `crates/tf_tree_bench/src/bin/scale_sweep.rs:517-521` | **prints** "A tf2 tree deeper than this cannot be migrated as-is" to the user; it now has two numbers to print |
   | `crates/tf_tree_bench/benches/tf2_compare.rs:348` | "(16), so a 24-deep spine is rejected outright" |
   | `crates/tf_tree_bench/examples/hugepage_grant.rs:60` | "`MAX_DEPTH` caps a chain at 16" |
   | `crates/tf_tree_bench/examples/step_cost.rs:442` | "2048-byte `[Step; MAX_DEPTH]` array" |
   | `crates/tf_tree_bench/tests/workload.rs:8, 131-134` | asserts every catalogue entry folds within `MAX_DEPTH` |
   | `crates/tf_tree_c/src/error.rs:69` and `include/tf_tree.h:281` | both name `TFT_MAX_DEPTH` — see *Not part of this record* |
   | `docs/PHASE1.md:587-606` §7.1 | the literal `16` in the code block and "generous, since real trees are 4–8" |
   | `docs/benchmarks/tf2.md:1795-1808` | "caps a compiled plan at **16** steps"; "If you are migrating from tf2 and your tree is deeper than 16, tf_tree will refuse the lookup" |
   | `docs/design/fast-path.md:173, 185, 522, 650` | "**2048 bytes**" twice, as the size the fold walks |

   Verified by grepping the **built artifacts** for the old numbers, not the sources,
   and by `just doc`.
5. **Pin the precedence, because a sentence cannot settle it.** A table of
   `(shape, defect, expected error)` rows: a defect before the compiled bound, a
   defect past it, a defect past the raw bound, a `MissingEdge` sentinel at walk
   step 5 and the same sentinel at step 20, a `MixedTimeDomains` clash on a 40-edge
   path, and a balanced two-sided path at exactly the raw bound. Verified by running
   it against variants A and C: **A and C disagree on the "defect past the compiled
   bound" row**, which is precisely the row a spec sentence cannot adjudicate.
6. **The two assertions that encode the old behaviour, and will fail.**
   `crates/tf_tree_core/src/tests.rs:1113` sizes an arena for 20 frames and indexes
   `chain[MAX_DEPTH]`, which is an out-of-bounds panic at 32 (measured: "the len is
   19 but the index is 32"), and `:1122-1127` asserts
   `TreeTooDeep { depth: MAX_DEPTH }` for a 17-edge chain, which is the "depth is
   always the bound" reading this record has just retracted. Both are widened in the
   same commit as step 3. `tests/python/test_errors.py:111-120` builds a 24-link
   chain described as "comfortably past" 16 — at `MAX_DEPTH = 32` it **compiles**,
   and `_too_deep` stops raising. Verified by `just test` and `just py-test`.
7. **The spec half: `docs/PHASE1.md` §7.1, `docs/benchmarks/tf2.md`,
   `docs/API.md` §6.** §7.1 is the NORMATIVE statement of the constant and states
   the wrong one; `tf2.md`'s migration warning is the sentence a user reads before
   adopting, and it now says something different and better.
8. **The gates.** The draft named `bench-check`, `loom`, `miri`, `tsan`, `msrv` and
   `audit`, and omitted the four that this change is most likely to break:
   `just test` (step 6's two assertions), `just py-test` (the Python one),
   `just doc` (step 4's doc comments, warnings denied) and `just lint` (which is
   also the only place `just artifact-versions` and `just evidence-audit` run). The
   full list for the landing: `just build`, `just test`, `just lint`, `just doc`,
   `just py-test`, `just py-lint`, `just shm-check`, `just msrv`, `just audit`,
   `just loom`, `just miri`, `just tsan`, `just bench-check`. `bench-check` is
   expected to PASS and is **not** evidence about latency on any host that fails
   `Fitness::probe` — the paired-difference tables in *Rationale (A)* are.

## Open questions

Resolved before `draft -> ready`. A `ready` doc has none.

**All three are resolved, and two of them changed the Decision.** Each keeps the
question that produced it, because the reasoning is what the answer rests on.

1. **RESOLVED — `MAX_PATH_EDGES = 64`, and `MAX_DEPTH` moves too.**
   ~~What is `MAX_PATH_EDGES`? … Nobody has surveyed real URDF depths, and that
   survey is what should choose the number.~~

   The survey is in *Context*: 91 structures, 26 repositories, both parsers
   agreeing on all 91, every verdict taken from `Tree::plan` rather than a model.
   64 is sized in *Decision*. Two things the question did not anticipate came out
   of doing it. First, **the binding number is the diameter and not the depth** —
   30 against 18 — so a less careful survey would have chosen 24 and been wrong.
   Second, and this is what changed the Decision, **`MAX_PATH_EDGES` alone rescues
   6 of the 26 robots HEAD refuses**; the other 20 are refused on the compiled
   bound, so this record could not both claim to serve the URDF sub-assembly shape
   and leave `MAX_DEPTH` at 16.

   The question's own third axis survives, sharpened: the latency ceiling is real
   (1.09 µs at 64 against 3.97 µs at 256) and it matters more than the draft could
   know, because #259 means a refused pair pays the compile on **every** lookup.
   What does not survive is the draft's phrasing that 256 sets a 2.8 µs worst
   accepted compile for everyone: measured across bounds at fixed *k*, the columns
   are within noise, so the ceiling belongs to the path and not to the bound.

2. **RESOLVED — both, and a test is what forces the choice.**
   ~~Does the precedence change need a test, or only a spec sentence? No test pins
   error precedence today, which is why the change was invisible.~~

   A spec sentence alone cannot settle it, and that is not a preference. "The
   compiled bound is checked after fold" has **two admissible readings** — check
   inside the loop and leave (variant A), or resolve every step and check at the
   end (variant C) — and they return **different errors** for a defect past the
   compiled bound: `TreeTooDeep` under A, `UnknownEdge` / `MixedTimeDomains` under
   C. Only a table of `(shape, defect, expected error)` rows chooses between them,
   and the sentence has to be written *after* the table so it describes what was
   chosen. Both land: the table as step 5, the sentence in `PHASE1.md` §7.1 as
   step 7. Taken literally, incidentally, the draft's own step 3 is neither reading
   — it **panics**, because `fold` writes `out[n]` inside the loop.

3. **RESOLVED — no, and the fix is stronger than the question assumed.**
   ~~Is `TreeTooDeep`'s advice actionable from every binding? The prototype's new
   message ends "or make the fixed links static so they fold", and the Python API
   cannot declare a static edge on an existing tree.~~

   Python cannot declare a static edge **at all**: `tf_tree.build` calls only
   `dynamic_edge` (`crates/tf_tree_py/src/tree.rs:1556-1562`), so the qualifier "on
   an existing tree" was too weak. C reaches one only through the unstable,
   feature-gated `tft_bridge_create` TOML. So the core's message must not name that
   remedy at all, and per `API.md` R5 it should not have carried a per-binding
   remedy in the first place: the identifier is the contract, the prose is a
   separate layer, and each binding's layer names what its own callers can do. See
   *Decision*, which also retires the self-contradictory
   "path depth 16 exceeds the maximum of 16" the Rust facade ships today.

## What made this `ready`

- **A survey existed.** Question 1 could not be answered without one and nobody
  had done it; the answer then reversed rationale (A) and doubled the record's
  scope, which is the outcome a `ready` gate is for.
- **The grow direction was measured**, exactly as the draft's own warning about
  `0022` demanded, on the project's own harness with a round-robin interleave and
  paired deltas. Rationale (A) is reversed by that measurement and not by a change
  of mind; its original text is kept above so a reader can see what was overturned.
- **The fold variant was built three ways and chosen on evidence**, which also
  found that the draft's step 3 taken literally panics.
- **The site list was audited**, and it is four times the length the draft claimed,
  with one functional `bail!` and two assertions hiding among the prose.
- **One number was measured last, and separately.** The two halves of this record
  were each measured in isolation and they pull opposite ways on the same line of
  code, so the combined cost of `MAX_DEPTH = 32` *with* the deleted array on the
  cache-miss path is a quantity neither of them reports. It is stated in
  *Consequences* and it was taken after the rest of this document was written. It
  does not gate the Decision — the two isolated measurements bracket it, and no
  sizing argument here turns on where it lands between them — but it is what an
  implementer will be asked for, so it is measured rather than estimated.

## Not part of this record

`TFT_MAX_DEPTH` is referenced at `crates/tf_tree_c/include/tf_tree.h:281` and
`crates/tf_tree_c/src/error.rs:69` and is **defined nowhere** — two `rg` hits
outside this document, both of them doc comments. Pre-existing, in the frozen
header, found while surveying the constant's consumers. It needs `xtask headers`
and its own change.

**Do not define it before this lands.** The moment there are two bounds, the name
`TFT_MAX_DEPTH` is ambiguous between them, and a C caller's reading of
`TFT_ERR_TREE_TOO_DEEP` depends on which one it names. Defining it first would
mint a frozen-header constant that this record immediately makes wrong; defining
it after is a choice between two documented numbers.
