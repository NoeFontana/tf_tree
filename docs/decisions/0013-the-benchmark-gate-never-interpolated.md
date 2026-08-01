# 0013: The benchmark gate never interpolated, and what §11.3's numbers should be

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** (filled in as work lands)

## Context

A five-lens audit of `tf_tree_core`, `tf_tree_arena` and `tf_tree_ipc` — the same
exercise that produced [`0011`](./0011-the-bridge-clock-guard-and-the-static-conflict-disposition.md)
for the Phase 4/5 surface — found that the lookup benchmark and the go/no-go gate
report have never measured interpolation.

`crates/tf_tree_bench/src/fixture.rs:28` defines the query stamp:

```rust
pub const NOW_NS: i64 = 9_900_000_000;
```

The fixture's four dynamic edges publish at 50, 200, 1000 and 10 Hz
(`fixture.rs:198-201`). `NOW_NS` is an exact multiple of **all four** periods:

| Edge rate | Period (ns) | `NOW_NS % period` |
| --- | --- | --- |
| 50 Hz | 20 000 000 | **0** |
| 200 Hz | 5 000 000 | **0** |
| 1000 Hz | 1 000 000 | **0** |
| 10 Hz | 100 000 000 | **0** |

Every dynamic edge therefore takes the exact-hit branch in
`SampleRing::sample` (`crates/tf_tree_core/src/sample.rs:100`):

```rust
let result = if t_i == t {
    // Exact hit — no interpolation.
    self.read_slot((i & self.mask) as usize)?
} else {
    ...
    I::eval(&a, &b, s)
};
```

`I::eval` never runs. The benchmark that `docs/PHASE1.md` §11.3 gates on has been
timing `bracket` plus `read_slot` — the seqlock read — and nothing else. Both the
criterion suite (`crates/tf_tree_bench/benches/lookup.rs:25`) and the gate report
(`crates/tf_tree_bench/src/report.rs:1472`) build their stamp the same way, from
the same constant, so this is one root cause at two call sites.

**The tell was already in the published data.** ScLerp and LerpSlerp come out
within ~1% of each other, while §11.3 budgets them 150 ns and 100 ns — a 50%
spread. Two interpolators cannot be indistinguishable unless neither is running.

### Measured

`cargo bench -p tf_tree_bench --bench lookup -- --quick`, this host, on-grid
(shipped) against off-grid (`NOW_NS - 500_000`, which is off-grid for all four
periods):

| Benchmark | On-grid (shipped) | Off-grid | Ratio |
| --- | --- | --- | --- |
| `depth1/sclerp` | 28.6 ns | 106.7 ns | 3.7× |
| `depth3/sclerp` | 69.9 ns | 290.1 ns | 4.1× |
| `depth3/lerpslerp` | 69.1 ns | 221.8 ns | 3.2× |
| `depth6/sclerp` | 52.0 ns | 202.6 ns | 3.9× |

Off-grid, the ScLerp/LerpSlerp gap becomes 290.1 / 221.8 = **1.31**, which is the
spread the design predicts. On-grid it is 1.01.

**Nothing here is a performance regression.** No code got slower. The engine has
always cost this much to interpolate; the benchmark simply never asked it to.

### What forces the decision now

Fixing the stamp is four characters. The problem is what the honest number does
to the gate: `docs/PHASE1.md` §11.3 budgets **150 ns p50 at depth 3**, and the
real measurement is **290 ns — over by 1.93×**. So the choice is not "fix the
benchmark or not", it is "what should §11.3's numbers be, now that we can measure
the thing they were written about". That is a change to a normative document,
which `CLAUDE.md` routes here rather than to a PR.

## Decision

*(draft — this is the recommendation, not yet ratified)*

1. **Fix the stamp at both call sites.** `report.rs:1472` and `lookup.rs:25` take
   `fixture::NOW_NS - 500_000`. Add a comment at `fixture.rs:28` recording that
   `NOW_NS` is deliberately on-grid for all four rates and that query stamps must
   be offset from it, so the next person does not "tidy" the offset away.
2. **Re-baseline once**, after the other measurement-moving changes on
   `engine/five-lens-hygiene` have landed (the `push` heartbeat store and the
   counter calls on the batch entry points), so there is exactly one
   re-baselining rather than three.
3. **Amend `docs/PHASE1.md` §11.3** to budgets justified by the measurement, with
   the on-grid history recorded inline so the loosening is not mistaken for a
   concession to a regression.

Item 3 is the part that needs a decision. Two defensible readings:

- **(a) Re-cut the budget to the measured cost**, e.g. depth-3 p50 ≤ 350 ns
  ScLerp / ≤ 260 ns LerpSlerp, with headroom over the 290/222 measured here.
  Honest, and the gate starts gating.
- **(b) Keep 150 ns as a *target*, and gate on regression-from-baseline instead
  of an absolute.** The absolute number was picked before anyone had measured
  interpolation, so treating it as a standing goal rather than a pass/fail line
  is arguably closer to what it always meant.

## Rationale

**Why not just leave the on-grid stamp and note the caveat.** It preserves the
same misleading number in the file everyone quotes. The gate's purpose is to fail
when a change makes lookup slower; a gate that exercises neither interpolator
cannot fail for the most likely cause of a slowdown, and the ScLerp/LerpSlerp
rows actively misinform — they invite the conclusion that the interpolator choice
does not matter, which is the opposite of D5.

**Why not add an off-grid benchmark alongside the on-grid one.** Considered, and
it is a reasonable addition later. It does not resolve this record, because §11.3
still has to say which number is the gate.

**Why not treat the exact-hit path as the realistic case.** It is not. A consumer
queries at a sensor stamp or a control-loop tick; landing exactly on a publisher's
grid is the coincidence, not the norm. The on-grid case is worth keeping as a
separate labelled row precisely because it is the best case.

## Consequences

- The gate becomes able to fail, and will need a real baseline.
- §11.3's numbers stop being comparable to any figure published before this
  change. Anything quoting "150 ns at depth 3" — README, docs, talks — needs the
  same amendment, or it becomes the next stale claim of the kind this audit spent
  its time finding.
- `bench_report_cli`'s existing assertion (`p50 > 0 && p50 < 1 ms`) still passes
  at ~290 ns, so no test has to change to land item 1.

## Implementation plan

1. Fix the stamp at `report.rs:1472` and `lookup.rs:25`; comment `fixture.rs:28`
   — verified by `cargo bench -p tf_tree_bench --bench lookup -- --quick` showing
   the ScLerp/LerpSlerp gap open to ~1.3×.
2. Land the rest of `engine/five-lens-hygiene`, then
   `just bench-baseline-update` once — verified by `just bench-check` green.
3. Amend `docs/PHASE1.md` §11.3 per whichever of (a)/(b) is ratified — verified
   by `just bench` passing the gate it now states.

## Open questions

1. **(a) or (b)** — absolute re-cut, or regression-from-baseline? Blocks `ready`.
2. If (a): what headroom over the measured 290 ns, and is the budget stated
   per-interpolator or once for the slower one?
3. Should the on-grid case survive as its own labelled benchmark row
   (`depth3/sclerp/exact_hit`) to keep the best case visible? Cheap, and it makes
   the 4× difference between the two regimes a documented property rather than a
   trap.
