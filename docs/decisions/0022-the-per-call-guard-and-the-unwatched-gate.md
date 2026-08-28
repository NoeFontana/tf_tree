# 0022: the C ABI builds a guard per call, and §7 gate 1 has been failing unwatched

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** none, and none is planned. **The decision is to build
nothing** — see *Decision*. The measurement half of this record has all landed
(`just abi-cost`, `just abi-split`, `just abi-attached`, `just guard-cost`), and
the one documentation change it owes is named in *Implementation plan*.

**Why `ready` and not `implemented`.** `README.md`'s lifecycle defines
`implemented` as *code shipped, PRs linked, document frozen*; there is no code
and freezing the document would be wrong, because this decision has a stated
reopening condition (`PHASE7.md`, below) and the record has to stay editable to
receive it. Zero open questions is what makes it `ready`: questions 1 and 5 were
answered by measurement (amendments 3 and 5), and 2–4 are closed by declining.

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
`n == 0 || !self.view.is_writable()`, `tf_tree_core/src/plan.rs`).

> **Correction, 2026-08-28: this sentence read "No shipped configuration is
> both", and one is.** `Tree::lookup` builds a guard per call — `let g =
> self.guard();` inside its plan-cache closure, `crates/tf_tree/src/tree.rs` —
> and a `TreeBuilder::build()` tree is writable unconditionally
> (`ArenaBacking::is_writable` in the same file, whose `Heap(_)` arm is a bare
> `true`). Symbols rather than line numbers on those two: both files moved
> twice under concurrent work while this block was being written. So on a heap
> tree the drop **does** reach the flush, and the two bullets below are the
> configurations that matter rather than the only ones that exist. The
> paragraph after them already named the shape — *"a read-write participant
> doing per-call lookups"* — and then said the engine does not encourage it;
> what it missed is that the facade ships exactly one such entry point.
>
> **The decision is unchanged, and this is a precision fix rather than a
> reversal.** `Tree::lookup` is `docs/API.md` §1 R1's *collapsed convenience*,
> admitted only on two NORMATIVE conditions, the second of which is that it is
> "never the example in the README's hot loop" — it is by construction not the
> hot tier. The ~16 ns flush sits inside a call that pays a thread-local plan
> cache probe on top of the entire evaluation, and the fixture it was measured
> on — `abi_cost.rs`'s three-edge heap tree at `[profile.embedder]` — is a
> ~245 ns lookup, of which the flush is ~6.5%
> (`crates/tf_tree_c/examples/abi_cost.rs:125-131`). A few percent of the tier
> that exists to be convenient does not buy a conditional flush, and it does not
> buy making the §5 counters lie about the one workload that would trigger it.
> What was wrong was the word *none*, not the conclusion.

The two configurations that carry the argument:

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

> **Read "nothing anyone currently pays" as "nothing on the hot tier"** (same
> correction, 2026-08-28). `Tree::lookup` on a `TreeBuilder::build()` tree *is*
> the read-write per-call-lookup shape, so it pays the flush; the engine not
> encouraging the shape is exactly R1's point about the collapsed tier, not a
> claim that no caller reaches it.

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

**Do not build a `tft_guard` handle.** Questions 2, 3 and 4 are closed by
*declining* the design, not by answering its soundness and staleness questions:
the thing those questions would buy is already bought by an API that exists.

What is declined is the first draft's sketch, kept here so it is clear what was
on the table:

```c
tft_status tft_guard_acquire(const tft_tree *tree, tft_guard **out);
tft_status tft_plan_at_guarded(const tft_plan *plan, const tft_guard *g,
                               int64_t stamp, tft_layout layout, void *out);
void       tft_guard_release(tft_guard *g);
```

### 1. `tft_plan_at_many` already collects essentially the whole prize

The batch entry point pays **one** guard per batch instead of one per element, so
it recovers `guard × (1 − 1/n)` of the per-call guard by construction. Measured
on the same §11.1 fixture, the same `imu_link ← map` plan and the same shared
arena as everything else in this record:

| what | ns | where |
|---|---|---|
| `tft_plan_at`, C++ caller | 302.0 | rung C, *Context* |
| `tft_plan_at_many`, n = 256 | 261.0 | rung C′ |
| **recovered by batching** | **41.0** | |
| the whole per-call guard, `[profile.embedder]` | **43–47** | amendment 4, rung 1 |
| the whole C++-over-native gap | ~56–60 | amendment 3 |

**Batching recovers ~41 of the 43–47 ns the guard costs — around 90% of it, and
at n = 256 the arithmetic says it should be 99.6%.** The two figures come from
two harnesses and are not a paired measurement of one quantity, so read the
agreement as "the same size", not as three significant figures; the honest
statement is that **a `tft_guard` handle's remaining prize over `tft_plan_at_many`
is 0–7 ns, which is inside this host's spread on rung 1 alone (43.0–46.9).**

**Both of this record's guard figures were checked against the conclusion, not
one of them.** Amendment 5's counters-free, in-process, writable number is
**~35 ns** (34.4 heap / 35.8 memfd) rather than rung 1's 43–47, and it is the
smaller of the two — so taking it instead makes batching's 41 ns *exceed* the
whole guard, which is arithmetically impossible for a mechanism that pays one
guard per batch and therefore just says the two harnesses differ by a few
nanoseconds. **The conclusion does not depend on which is used**, and picking the
larger one is the choice that flatters the declined handle, not this decision.

Two profile facts, because this record has been burned twice by carrying a number
across builds. The 302.0/261.0 pair is a C++ caller against
`target/release/libtf_tree_c.a` — the workspace's thin LTO cannot cross into
`g++`, so **that boundary is real**, and amendment 2's control confirms it: the
C++ arm reads 302.0 under *both* workspace profiles. Rung 1's 43–47 ns is
`[profile.embedder]` (`lto = false`).

**What batching does not cover, stated plainly.** `tft_plan_at_many` wants stamps
in order — the cursor is what the batch path is for — and at n = 1 it recovers
nothing at all. So this argument is exactly as strong as the claim that a caller
who cares about 35–45 ns can batch, which is the next point.

### 2. The caller who cannot batch is not paying enough to care

The cost is per *lookup*. A caller doing enough lookups for 35–45 ns each to add
up to anything is, by definition, in a position to hand them over as an array:
a planner scoring trajectories, a point transform over a scan, a node
re-expressing a batch of detections. That caller batches.

The caller who genuinely cannot batch is doing **one lookup per control cycle**.
Against a 1–10 ms period, 45 ns is 0.0045%–0.00045% of the cycle. There is no
control loop that is feasible at 1.000000 ms and infeasible at 1.000045 ms.

### 3. The handle costs precisely what `0017` spent seven steps removing

`Guard<'_>` borrows the tree. A `tft_guard` outliving its `tft_tree` is a
use-after-free a C caller writes in one line, and the machinery to prevent it is
machinery this project has already built and already regretted needing:
[`0017`](./0017-owned-handles-and-the-lifetime-rule.md) deleted **both**
hand-rolled `extend_to_static` helpers, one of which had leaked a claim lease for
the life of every Python publisher and bypassed the fork guard. `OwnedWriter`
exists because a lifetime extension in a binding is a defect factory; adding a
second one — to a *read* path, for 0–7 ns — inverts that record's whole argument.
`API.md` §2.1's rule ("no type a user stores carries a lifetime") would force the
handle to own an `Arc<Tree>`, which is a second refcounted handle in the ABI
whose only job is to make a 45 ns saving safe.

Question 3's staleness objection compounds it: a held guard pins a topology
generation, so a C consumer holding one across a `select()` loop reads a stale
topology after a declaration, and per-call acquisition is what makes that
impossible today. Answering that needs a bounded-lifetime rule that is *enforced*
rather than documented, and nobody has proposed one.

### 4. The first draft's `API.md` R2 argument is withdrawn

It read: "a per-call guard acquisition on a shared arena is per-call
synchronization work in the tier that promises none". **That overstates what a
guard is.** `Guard::new` allocates nothing, takes no lock and waits for nothing:
since A1 collapsed the topology seqlock into one packed word, pinning a
generation is a **single acquire load** (`TopologyView::stable_generation` →
`generation` → one `load(Acquire)`), and the rest of the constructor zeroes a
cursor array. R2 is not violated by the current shape. The cost is real and
measured, but it is *work*, not *synchronization*, and a rule about the former
does not decide this.

### The alternative, kept on the shelf rather than lost

If the cost ever has to be recovered, **do not start from a new public type.**
Start from caching the guard inside the **existing** `tft_plan` handle, with
revalidation against the topology generation on each call:

- **No new public type and no caller-visible lifetime.** `tft_plan` already ties
  to its `tft_tree`; the ABI's soundness story does not change at all, which is
  what makes this cheaper than the handle by more than the ~45 ns it is chasing.
- **Revalidation answers question 3 directly** rather than documenting around it.
  The cached guard's pinned generation is compared against
  `header.topo`'s current one — **one acquire load per call**, the same load
  `Guard::new` already performs — and a mismatch rebuilds the guard. A stale
  topology is then not merely detected, it is *impossible*, which is strictly
  better than what a held `tft_guard` could promise.
- **What it costs is one acquire load instead of ~15 ns of construction**
  (amendment 4's isolated build+drop), plus whatever the warm cursor is worth on
  the fixture — which, per `0023` and `docs/design/fast-path.md` §12, is the
  larger half on a realistic ring and near zero on a toy one.
- **The difficulty is self-referential storage** — a `Guard<'a>` stored beside
  the `Arc<Tree>` it borrows — and that difficulty is *internal to `tf_tree_c`*.
  It is the same shape `OwnedWriter` solves for the write path, so the precedent
  and the `// SAFETY:` argument already exist. It would still be a decision
  record, because `0007`'s budget is a criterion and this adds an `unsafe` site
  to the C ABI's crate.

This is written down because it is the design somebody will re-derive, and
because it is strictly better than the declined one on every axis this record
weighed.

### What reopens this decision

**`docs/PHASE7.md`'s `tf2`-shaped shim is inherently scalar.**
`lookupTransform(target, source, t)` takes one stamp and returns one transform;
there is no batch spelling of it, and §2 puts the shim "header-only over the C
ABI", so a migrating node's forty call sites each pay a per-call guard. §1's
whole argument is that such a node keeps its call sites and converts *one* hot
loop — meaning the shim's steady state is precisely the caller point 2 above
dismisses, in bulk.

That does **not** reopen anything today: PHASE7 is gated by D21, its §0.0 lists
four gates and none is met, and answering a design question from that document
rather than from Phase 4's surprise log is the exact failure the gate exists to
prevent. But if those gates open, **this decision is to be re-taken, and the
shelf alternative above is where it starts** — a plan-cached guard costs a
`lookupTransform` shim nothing to adopt, because the shim already holds the plan.

### The §7 half of the first draft, now superseded

The first draft also asked that **§7 gate 1 state which backing it measures**.
That is right and has been overtaken:
[`0023`](./0023-the-gate-that-could-not-gate.md) re-cuts criterion 1 into three
rungs at a stated profile, and its open question 3 is exactly which fixture R3
should gate. The demand belongs to that record now, not this one. What this
record contributes to it is that **the backing is not the variable** — heap and
memfd differ by ~1.4 ns at `[profile.embedder]` (amendment 5) — so "add a
shared-arena row" was the wrong ask; "gate a realistic *fixture*" is the right
one.

## Open questions

**None. All five are closed**, and the five closures are of three different
kinds — worth distinguishing, because "closed" has meant something different each
time:

| # | subject | closed by | kind |
|---|---|---|---|
| 1 | the conditional counter flush | amendment 5 | **withdrawn**: ~16 ns is real, but no shipped configuration reaches the flush |
| 2 | is a guard handle sound to expose? | *Decision* §3 | **declined**: not asked, because the handle is not built |
| 3 | how long may a guard be held? | *Decision* §3 | **declined**, and answered anyway by the shelf alternative's revalidation |
| 4 | is `tft_plan_at_many` sufficient instead? | *Decision* §1–2 | **answered: yes** — 41 of 43–47 ns, and the caller who cannot batch is not paying enough to care |
| 5 | what is the rest of the ~60 ns? | amendment 3 | **measured**: there is no unexplained residue |

Question 4 is the one that decided the record. The others are its consequences.
The text of all five is kept below, unedited apart from the closure notes each
already carried, because two of them record refuted hypotheses that are worth
more than the answers.

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

**No engine, ABI or arena change. One documentation change, and it is the whole
plan:**

1. **Say "batch" where a C or C++ embedder looks.** `docs/benchmarks/tf2.md`
   already leads its guidance with *"use `tft_plan_at_many` on any hot path"*;
   the same sentence belongs on `tft_plan_at`'s own doc comment in
   `crates/tf_tree_c/include/tf_tree.h` and on `tf_tree.hpp`'s `Plan::at`, with
   the number attached (one guard per batch, ~41 ns of a ~302 ns scalar call at
   n = 256) and with the caveat that the stamps want to be in order. Verified by
   `just c-header-check` and by the sentence naming a measurement rather than an
   adjective. **A reader who reaches `tft_plan_at` and never learns that
   `tft_plan_at_many` exists is the only way this decision goes wrong**, since
   the decision *is* "batching is the answer".

   > **Status 2026-08-28: half landed, and the half that is missing is the C
   > header.** `crates/tf_tree_c/include/tf_tree.hpp` has it — `Plan::at`'s doc
   > comment at `:807-822` says *"On a hot path, prefer `at_many`"* in the first
   > line of its body (`:809`) and carries the numbers (302 ns scalar against `tft_plan_at_many`'s 261, 41 ns /
   > 13.6% at n = 256, native Rust 202 ns) and the sort caveat.
   > `crates/tf_tree_c/include/tf_tree.h` does **not**: `tft_plan_at`'s comment at
   > `:630-648` documents the layout and the safety contract and never names
   > `tft_plan_at_many`, which is declared 40 lines further down at `:688`. The
   > only other mentions of the batch call in that header are at `:72` and `:461`,
   > both about `TFT_LAYOUT_QVEC7_WXYZ_TWIST6` rather than about cost.
   >
   > So the reader this step was written for — a C caller who reaches
   > `tft_plan_at`, the one this record calls "the only way this decision goes
   > wrong" — is still unserved, and the C++ caller, who has the ergonomic
   > alternative anyway, is the one who got the sentence. `just c-header-check`
   > does not catch this: it checks header/ABI agreement, not that a doc comment
   > says a particular thing. **The remaining work is one paragraph on
   > `tf_tree.h:630-648`, and this record is not done until it is there.**

Everything else this record produced is already merged: `just abi-cost`,
`just abi-split`, `just abi-attached` and `just guard-cost`, plus the corrections
in `docs/PHASE4.md` §7 and `docs/benchmarks/tf2.md`.

**Nothing in this record authorizes a `tft_guard` handle, and now nothing in it
ever will** — reopening runs through *What reopens this decision*, i.e. through
D21's gates and a new record, not through this one. Three attributions in this
area have already been wrong — the shared-library boundary, the fork check, and
(in `0023`) the backing — and each was wrong because a residue was treated as a
finding. The handle was always the expensive answer to a gap whose cheap answer
shipped in Phase 4.

## Related

- `docs/PHASE4.md` §7, *§7 gate criterion 1 is failing* — the failing gate and
  why nothing caught it.
- `docs/benchmarks/tf2.md`, *Where the 52% actually goes* — the full ladder, and
  the two eliminations (backing, link mode) that led here.
- `just abi-cost` — the gate, which now has a recipe.
- `just abi-split` — the ladder and the guard-cost rows.
- [`0017`](./0017-owned-handles-and-the-lifetime-rule.md) — why a second
  lifetime extension in a binding is the cost the declined handle actually
  carries, and what the first one did before it was found.
- [`0023`](./0023-the-gate-that-could-not-gate.md) — inherits the "state which
  backing" demand as a question about the *fixture*, and gates R3, the row this
  record no longer intends to lower.
- `docs/PHASE7.md` §0.0 and §2 — the gated shim, and the reason it is the one
  consumer that would reopen this.
- **The amendments above *Decision* are in the order 4, 5, 3, 2, 1** — appended
  as they were written, not sorted. Amendments 1 and 2 are wrong and say so;
  read them for the reasoning error, not the numbers.
