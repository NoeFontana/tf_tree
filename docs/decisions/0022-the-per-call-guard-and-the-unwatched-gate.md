# 0022: the C ABI builds a guard per call, and §7 gate 1 has been failing unwatched

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** (filled in as work lands)

## Context

`docs/PHASE4.md` §7 gate 1 records `tft_plan_at` at **1.020× native Rust** and
calls it a PASS. `docs/benchmarks/tf2.md` records a C++ caller at **306.7 ns**
where native Rust measures **201.5 ns** on the same host and fixture — **+52%**.
Both numbers are correctly measured. They disagree because they are measured on
**different arena backings**, and the gate is measuring the one no shared-memory
consumer uses.

`just abi-split` (`crates/tf_tree_bench/src/backing.rs`) walks the whole ladder
on one fixture — the §11.1 topology, `imu_link ← map`, off-grid stamps so
`I::eval` actually runs:

| Rung | API | Arena | ns/lookup |
|---|---|---|---|
| H | native Rust | heap, in-process | 200.7 |
| S | native Rust | `MAP_SHARED` memfd, in-process RW | 203.2 |
| A | native Rust | memfd, read-only, **cross-process** | 202.5 |
| C | **`tft_plan_at`** | same arena as A | **302.0** |
| C′ | **`tft_plan_at_many`** | same arena as A | **261.0** |

Every candidate except one is eliminated by measurement, not by argument:

- **the mapping**: ≤ 9.6 ns, paired over nine runs (median quotient
  1.0066–1.0112×, point estimate ~1.8 ns);
- **the cross-process read-only attach**: −0.7 ns;
- **the link mode**: ~1 ns — `crates/tf_tree_c/tests/cpp/bench.cpp`, the same
  source built against `libtf_tree_c.a` and `libtf_tree_c.so`, measures **245.4
  against 244.4 ns**.

What remains is **+99.5 ns, or +49%, inside the C ABI**, on an arena where native
Rust costs 202.5 ns.

### The mechanism, measured

`tft_plan_at` builds a `Guard` on every call:

```rust
// crates/tf_tree_c/src/lib.rs:684
let g = h.share.tree.guard();
```

and the Rust API does not have to — `ratio.rs`, `backing.rs` and every
in-process consumer acquire one guard and reuse it for a whole sweep. The C
signature has nowhere to keep one between calls, so the ABI has no choice.

`backing::guard_cost_both` prices that directly, in **safe Rust with no C ABI in
the loop at all**: the same `Plan::at` sweep run twice, once with a hoisted
guard and once acquiring one inside the loop.

| build | heap arena | memfd arena |
|---|---|---|
| `counters` on (default) | **+35.4 ns** | **+27.0 ns** |
| `counters` off | +16.8 ns | +18.9 ns |

Two things follow, and the second one kills this record's first draft.

1. **A per-call guard costs ~17 ns, and Phase 5's diagnostic counters roughly
   double it.** `de658da` put a counter flush in `Guard`'s destructor. It was
   measured for what counters cost a *publish*; nothing re-measured a hot path
   that builds a guard per *lookup*.
2. **The backing is irrelevant to it.** Heap and memfd are within noise of each
   other in both builds.

### §7 gate criterion 1 is failing, and that is the actual headline

While measuring the above, `examples/abi_cost.rs` was run — and it prints
**FAIL**. `tft_plan_at` measures **1.34–1.46×** against its 1.05 gate (four
pinned runs: 1.335, 1.414, 1.460, 1.461; ~60 ns absolute, stable).

`docs/PHASE4.md` recorded **1.020×, PASS**. Both readings are honest; the
recorded one is simply years-stale, because **`abi_cost.rs` was executed by no
recipe and no workflow** — it appeared in exactly one `justfile` comment. `just
abi-cost` now runs it, and `docs/PHASE4.md` §7 records the failing state.

So the original framing of this record — "the gate measures a heap tree, which
is the wrong configuration" — was itself wrong in an interesting way. The gate
measures the right thing. It just was not running, and on a heap tree the ABI
is 1.46×, not 1.02×. There is no configuration in which the current number is
2%.
## What this record already got wrong

Recorded because the failure is instructive and cost two commits.

The first attempt measured the mapping (rungs H and S), found it ~free,
and **attributed the entire 105 ns residue to the shared-library boundary** —
publishing "at least 91% of the gap is the boundary" in `tf2.md`, in a commit
message, and in three source files. Nothing had measured the boundary. It was a
subtraction dressed as a finding, and it was wrong by roughly a factor of 100.

The check that caught it was cheap and should have come first: compile one
existing benchmark two ways and compare. It took four minutes.

This is the same failure mode `tf2.md` already documents twice — the withdrawn
4.7× CPU reading and the stale `.tft` reasons — and the same one that the
`heap_vs_shared` and 213-vs-217 priors fell into. **A residue is a hypothesis.**

## Amendment 4 — what is *inside* the 48 ns, and the three things it is not

Amendment 3 closed question 5 with "the per-call `Guard` is ~48 of ~56 ns" and
left that as the actionable finding. It is not one: "the guard" names a
constructor, not a cost. This amendment takes it apart, at
`[profile.embedder]` (`lto = false`, a real boundary), on the same §11.1 fixture
over the same shared arena, with new arms in
`crates/tf_tree_bench/src/bin/abi_attached.rs` — `just abi-attached`, six runs,
`taskset -c 2`. Every arm below is a **difference between two loops of identical
shape**, not a subtraction from a total.

| part | ns | how it was isolated |
|---|---|---|
| `tf_tree_ipc::fork::generation()` | **+0.2** | the same loop with and without the call |
| `Tree::view()` | +3.7 | a view built per iteration, over the empty loop |
| `Guard::new(view)` | +4.8 | over the view arm |
| the rest of `Tree::guard` — `detached()`, `is_shared()`, `with_fork_check` | +6.7 | over `Guard::new` |
| **= build + drop a guard, in isolation** | **15.1** | |
| the same, on `Plan::at`'s critical path | ~22 | arm `E`: guard built and dropped per call, plan *evaluated* through the hoisted one |
| the cold bracket-search cursor | ~4.8 | arm `B` − `A`: hoisted guard, stamps in a fixed permutation instead of in order |
| **still unattributed** | **~16** | |
| **rung 1, for reference** | **43–47** | |

Run-to-run spread on rung 1 across those six runs is 43.0–46.9 ns, so treat every
figure above as ±2 and nothing here as a three-significant-figure claim.

**The leading hypothesis was wrong.** `fork::generation` was the obvious suspect
— a cross-crate call that thin LTO inlines and `[profile.embedder]` does not, which
would have explained the 19-vs-48 gap between the profiles exactly. It costs
**0.2 ns**. It is `#[inline]`, so its MIR crosses the crate boundary and it is
inlined at `lto = false` too. **There is nothing to fix there, and no decision
record about a fork-safety mechanism is needed** — which was the outcome most
worth establishing, because that mechanism is the one thing here that is load
bearing for correctness (`docs/decisions/0005` step 9; `tf_tree_ipc::fork`'s
module header).

**Two further candidates were killed by changing them and finding the number did
not move.** Both changes were reverted; they are recorded because each is the
next thing somebody would try:

* **`#[inline]` on `Tree::guard`.** Rung 1 measured 43.2 ns against a 43.0–46.9
  spread without it, and the ABI arm 298 against 294–297. The guard is not
  paying for the *call*, so letting a caller inline it recovers nothing — and an
  embedder in C could not use it anyway.
* **The size of the `Guard`.** It is **208 bytes**, 128 of them the
  `[Cell<u64>; MAX_DEPTH]` cursor, on a fixture whose plan is three steps deep.
  Cutting `MAX_DEPTH` 16 → 8 made it 144 bytes and moved *nothing*: rung 1 44.3,
  isolated build+drop 15.0, arm `E` 21.1, cursor 5.0 — every one inside the
  spread. **Zeroing that array is not what a guard costs**, so "shrink the
  cursor to the plan's depth" is off the board as a performance argument.

**The whole fork-safety half of `Tree::guard` is 6.7 ns, ~15% of rung 1.** That
is the *entire* prize available from touching it, and it is not the relaxed load
— the load is 0.2 — it is the branching and the extra `Option<(u64, fn() -> u64)>`
the guard then carries. Making it free would leave 36 of the 43 ns in place. **A
decision record proposing to weaken or move the fork check now has to argue
against that number.**

### What is still unattributed, and deliberately so

~16 ns — evaluating through a *fresh* guard, beyond the cold cursor. The
plausible reading is that a guard materialised per iteration cannot have its
fields held in registers across `Plan::at` the way a hoisted one can. **That is
written down as a hypothesis and nothing in this record depends on it**, because
it is exactly what the three refuted candidates above also sounded like. One
known asymmetry to fold in before anyone tries again: the isolated build+drop arm
never performs a lookup, so its `Guard::drop` takes the `n == 0` early return,
while a real guard's drop reaches the `is_writable` one.

### What this changes for the questions below

**Question 1 — the conditional counter flush — loses its number and needs a new
one.** It is stated below as "~18 ns of the ~35 ns per-call guard", carried from
a measurement taken at the workspace `release` profile. At a real boundary the
guard's *whole* drop path is inside the 15.1 ns isolated figure alongside
`view()`, `Guard::new` and the fork check, and this arena is attached read-only
so the flush early-returns before touching a counter at all. Note also that `Guard::note_ok`, the per-lookup half of the counters, is paid
identically by the hoisted arm and the per-call arm, so it cannot appear in rung
1 at all. **The 18 ns is not a figure for this fixture** and question 1 should
not be started against it. What would replace it is `tf_tree_bench`'s
`counter_cost` binary (`cargo run --profile embedder -p tf_tree_bench --bin
counter_cost`, both ways on the `counters` feature) — it has no recipe today, and
`docs/benchmarks/EVIDENCE.md` already says it should get one when this record
moves to `ready`.

**Questions 2–4 are untouched.** The prize for holding a guard across calls is
still ~43–47 of the ~55 ns, because that is rung 1 and rung 1 did not move. What
this amendment removes is the hope of getting it back *without* a held guard: the
three cheap structural fixes are measured and none of them is worth more than
~7 ns.

## Amendment 5 — question 1 is CLOSED, and the answer is that nobody pays it

`just guard-cost` runs the 2x2 question 1 needs — {release, embedder} x
{counters on, off} — on **writable** arenas, which is the only configuration
where `Guard::drop` reaches the flush at all:

| profile | counters | heap | memfd |
|---|---|---|---|
| `release` (`lto = "thin"`) | on / off | +33.8 / +17.0 ns | +25.5 / +18.4 ns |
| **`embedder`** (`lto = false`) | **on / off** | **+50.3 / +34.4 ns** | **+51.6 / +35.8 ns** |

**At a real boundary the counter flush costs ~16 ns per guard drop**, consistent
across both backings (15.9 and 15.8). Amendment 4 withdrew the earlier ~18 ns
figure; this restores it, at a stated profile, and on the arena kind where it is
actually paid.

### And that is why question 1 is closed rather than answered

The flush is only reached when **both** conditions hold: a guard is built *per
call*, and the arena is *writable* (`Guard::drop` early-returns on
`!self.view.is_writable()`, `plan.rs`). No shipped configuration is both:

- **A C or C++ consumer attaches read-only.** `tft_tree_open` maps the arena
  read-only — the header states it twice ("The arena is mapped read-only, so
  nothing can be claimed for writing"; "the attach is **read-only**") and it is
  D18's whole point, the MMU being what stops a consumer corrupting a robot's
  tree. So the tier that is *forced* into per-call guards never reaches the
  flush. **Question 1's win for the C ABI is exactly zero.**
- **A Rust consumer hoists the guard**, so the flush is paid once per batch and
  amortises to nothing. That is independently what `counter_cost` finds for
  §5.7: no measurable difference at the hoisted cadence, at any thread count at
  or below the CPU count.

So making the flush conditional would buy nothing anyone currently pays, at the
cost of making the §5 counters lie about the one workload that would trigger
it — a read-write participant doing per-call lookups, which is a shape the
engine does not encourage and the C tier cannot express.

**Question 1 is withdrawn as a proposal.** What it was reaching for is real —
~16 ns — but it is inside the ~35 ns a *counter-free* per-call guard already
costs at a real boundary, and that residue is question 5's territory, not
question 1's.

**One consequence worth stating for the remaining design.** The per-call guard
costs ~35 ns at `[profile.embedder]` with counters compiled out entirely. So a
`tft_guard` handle's ceiling is that ~35 ns, not the ~50 ns the counters-on row
suggests — the C tier never pays the counter half, and pricing the handle
against the wrong row would overstate its value by 40%.

## Amendment 3 — question 5 is CLOSED, and this record's original premise was right

The full ladder, measured at `[profile.embedder]` (`lto = false`, a real
boundary) on the §11.1 fixture over a shared arena, three runs, `just
abi-attached`:

| rung | ns | Δ |
|---|---|---|
| native Rust, guard hoisted | 242 | — |
| **+ guard built per call** | **290** | **+48** |
| + the 56-byte `QVEC7` store the ABI must make | 289 | ~0 |
| the ABI, no panic guard | 296 | +7 |
| `tft_plan_at`, from Rust | 297 | +1 |
| `tft_plan_at`, from C++ | 302 | — |

**Question 5 is answered: there is no unexplained residue.** The per-call `Guard`
is **~48 ns of ~56**, about 85%. Handle and layout validation plus the
un-inlinable call are ~7 ns together; the layout store and the panic guard are
noise. A native Rust caller that builds a guard per lookup costs **the same as
going through the C ABI** — so the ABI adds essentially nothing beyond forcing
that shape.

**This corrects amendment 2, which said the guard was "a third, not all of it".**
That measured +19 ns at the workspace `release` profile, where thin LTO inlines
`Tree::guard` and shrinks the very thing being priced. At a real boundary it is
+48. The rule this record keeps relearning applies to its own amendments:
a number carried across build profiles answers a different question.

**So this record's original premise and conclusion were right, and the two
amendments walking them back were wrong** — the first because it mistook an
LTO-erased baseline for the real one, the second because it priced the guard in
the same erased build. What was genuinely wrong throughout is the *title*: the
problem is not that "the C tier" cannot hold a guard, it is that **nothing
non-inlined** can, and the C ABI is simply the surface where that is forced.

### What this authorizes, and what it still does not

A way to hold a guard across calls would recover **~48 of the ~56 ns**, which is
85% of what a C or C++ embedder pays over native Rust on this fixture. That is
now a measured target rather than an inferred one, and question 5 no longer
blocks the design.

**Questions 1–4 still do.** In particular the soundness question (2) — a
`tft_guard` outliving its `tft_tree` is a use-after-free a C caller writes
trivially, and `0017` is explicit that lifetime extension in the bindings is
what is being removed — and the staleness question (3), since a held guard pins
a topology generation. `tft_plan_at_many` remains the lever that exists today
and needs no new type.

## Amendment 2 — superseded by amendment 3; kept for the reasoning error

`just abi-attached` calls `tft_plan_at` **from Rust**, on the same shared arena a
C++ probe measures at 302 ns, under two build profiles:

| profile | native Rust | Rust → ABI | C++ → ABI |
|---|---|---|---|
| `release` (`lto = "thin"`) | 200.5 | 225.8 (+25) | 302.0 |
| **`embedder` (`lto = false`)** | 241.3 | **298.4 (+57)** | **302.0 (+61)** |

**A Rust caller and a C++ caller agree to within 4 ns at a real boundary.** So:

- **The gap is real** — about **+57 ns** on the §11.1 fixture — and amendment 1's
  worry that it was all baseline specialisation is wrong. The premise stands.
- **It is not "the C tier"**, and this record's title is misleading. A
  *non-inlined Rust* caller pays the same thing. The cost is the boundary, and
  what makes `abi_cost` report +2.3 ns is that the workspace `release` profile
  is `lto = "thin"` and inlines the ABI call into it.
- **The per-call `Guard` is ~19 ns of the ~57**, measured on this exact arena
  (`just abi-split`'s cross-process rung: hoisted 200.6, per-call 220.0). So the
  guard is **a third of it, not all of it**, and a `tft_guard` handle would
  recover at most that third.

**What the other ~38 ns is remains unmeasured**, and question 5 stays open on it.
Candidates in order of size to check: the 56-byte `QVEC7` layout write a Rust
caller never makes; handle and layout validation; the un-inlinable call itself.
`abi_cost`'s ladder prices those at ~+3 ns *combined* — but it does so with the
boundary erased and on an L1-resident 3-edge tree, so that number answers a
different question and must not be carried across.

**Nothing here authorizes the handle.** It would address a third of a gap whose
remaining two thirds are unattributed, and this record has now had two amendments
for exactly that kind of reasoning.

## Amendment 1 — this record's headline number is under review

**`abi_cost.rs`'s native baseline turned out to be unstable, and the "+60 ns,
mostly the guard" figure above rests on it.** Building the per-rung ladder §7
gate 1 needed moved the native comparand from **133 ns to ~190 ns** while the ABI
arm stayed at 194–196 in every variant. Bisected to the mere presence of a second
`Tree::guard()` call site in the binary — not the `#[inline(never)]` arm, and not
which tree is guarded.

Measured within one build, where the subtractions are valid, the ABI costs
**about +6 ns**: guard per call ~+2.5, handle and layout checks ~+2, the
un-inlinable call ~+0.5, the panic guard ~+0.6. Roughly half is the guard, so
this record's *direction* survives — but its magnitude does not, and "§7 gate 1
is failing" is retracted (`docs/PHASE4.md` §7).

**What is NOT explained away** is the C++/`memfd` result: 302 ns against native
Rust's 202 on the §11.1 fixture over a shared arena. `abi_cost` does not
reproduce a gap of that size in-process on a heap tree, and the Rust comparand in
`backing.rs` measured ~200 — the *unspecialised* value — so that one is not the
same artifact. Something about the C++/shared-arena configuration is still
unaccounted for.

**So question 5 is answered for the in-process case and reopened for the
cross-process one, and no `tft_guard` handle should be designed until the second
is understood.** Designing against a 100 ns gap that turns out to be a
measurement artifact would be the fifth such mistake in this area.

## Decision

*(draft — nothing below is authorized)*

Give the C tier a way to hold a guard across calls, so that the hot path can
express what the Rust tier expresses. `docs/API.md` §1 **R2** — *the hot tier
never allocates, locks or converts* — is the rule the current shape fails: a
per-call guard acquisition on a shared arena is per-call synchronization work in
the tier that promises none.

Sketch, to be settled by the questions below:

```c
tft_status tft_guard_acquire(const tft_tree *tree, tft_guard **out);
tft_status tft_plan_at_guarded(const tft_plan *plan, const tft_guard *g,
                               int64_t stamp, tft_layout layout, void *out);
void       tft_guard_release(tft_guard *g);
```

Separately, and independently of whether the above is built: **§7 gate 1 must
state which backing it measures**, and should gain a shared-arena row. A gate
that passes at 1.020× while every `shm` consumer pays 1.49× is not measuring the
product.

## Open questions

**Question 4 of the first draft is answered and closed.** It asked whether the
`is_shared()` fork check in `Tree::guard` could move per-tree, on the theory
that it was what made a shared arena expensive. It is not: the branch measures
**+2.1 ns** with counters off and **−8.4 ns** with them on — noise in both
directions. Measuring it cost twenty minutes and saved designing against a
premise that was false. It is recorded rather than deleted for that reason.

1. **Should `Guard`'s counter flush be conditional on the guard having done
   enough work to be worth counting?** **Amendment 4 withdraws this question's
   number.** It read "the largest measured, uncontested win on the board: ~18 ns
   of the ~35 ns per-call guard" — a figure taken at the workspace `release`
   profile, on an arena where the flush could run. On the `[profile.embedder]`
   ladder this record now uses, the arena is read-only and the flush
   early-returns, so that win is not available on this fixture and needs
   re-measuring before the question is started. The rest of the question stands:
   recoverable without
   any new API surface, in a destructor that already early-returns on two other
   conditions (`n == 0`, `!is_writable()`). It costs the diagnostic counters
   some fidelity on single-lookup guards, and `docs/PHASE5.md` §5 is what says
   whether that is acceptable. It stays first in the order below because it
   helps every consumer including Rust and needs no decision about the C tier at
   all — but "first" now means "measure it first", not "implement it first".
2. **Is a guard handle sound to expose?** `Guard<'_>` borrows the tree, and
   `0017` is explicit that lifetime extension in the bindings is what is being
   removed, not added. A `tft_guard` outliving its `tft_tree` is a use-after-free
   a C caller writes trivially. Does this need `OwnedWriter`'s treatment, a
   generation check, or refusal?
3. **How long may a guard be held?** A guard pins a topology generation. A C
   consumer holding one across a `select()` loop would silently read a stale
   topology after a declaration — which per-call acquisition currently makes
   impossible. Is there a bounded-lifetime rule, and can it be enforced rather
   than documented?
4. **Is `tft_plan_at_many` sufficient instead?** It needs no new type, raises no
   lifetime question, and already exists; it recovers 41 ns at a batch of 256.
   With question 1 done, the residual per-call guard is ~17 ns. Is that worth a
   new handle in the ABI, or is the honest answer "batch your lookups" plus a
   documentation change?
5. **What is the rest of the ~60 ns?** The guard explains ~35 of it. The
   remainder is handle validation, `catch_unwind`, and the un-inlinable call —
   `abi_cost.rs` already prices those three for the *publish* path (+17.0,
   +1.3, +0.7 ns) and finds validation dominant. The equivalent breakdown for
   the lookup path does not exist and should, before anything is designed
   around the remainder.

## Implementation plan

None. This record is `draft`; `docs/decisions/README.md` forbids starting.

When it moves to `ready`, the order is question 1 (the counter flush — largest
measured win, no API surface, helps Rust consumers too), then question 5 (price
the remaining ~25 ns before designing around it), and only then questions 2–4,
which are the ones that add public API.

**Nothing in this record authorizes a `tft_guard` handle.** Two attributions in
this area have already been wrong — the shared-library boundary, and the fork
check — and both were wrong because a residue was treated as a finding. The
handle is the expensive answer; it should be reached last and only against a
measured remainder.

## Related

- `docs/PHASE4.md` §7, *§7 gate criterion 1 is failing* — the failing gate and
  why nothing caught it.
- `docs/benchmarks/tf2.md`, *Where the 52% actually goes* — the full ladder, and
  the two eliminations (backing, link mode) that led here.
- `just abi-cost` — the gate, which now has a recipe.
- `just abi-split` — the ladder and the guard-cost rows.
