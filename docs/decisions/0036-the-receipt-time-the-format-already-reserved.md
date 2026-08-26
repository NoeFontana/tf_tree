# 0036: the receipt time the format already reserved

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** **step 1 has landed**, with step 2's amendment to
`docs/PHASE2.md` §6.4 in the same change — a normative sentence describing code
should not be one PR behind it. Steps 3–5 are open; `docs/PHASE5.md` §0.0's
*"sixteen detect"* is therefore unchanged, and only `TFT004`'s *reason* moved.
The four recommendations below were ratified by merging this record, per the
mechanism [`0023`](./0023-the-gate-that-could-not-gate.md) states in as many
words (*"they are recommendations and not decisions because this record is
`draft`: a human ratifies by merging"*).

> **Implementation notes for step 1, kept because the number this record was
> written around is the one it got wrong.**
>
> * **The sampler costs +1.1 ns per push (~+23%), and at the interval measured
>   the clock read is 3% of it.** Paired, both arms in one process, six sittings
>   (`just push-sampler-cost`, `crates/tf_tree_bench/benches/push_sampler.rs`):
>   4.8–5.0 ns without, 5.9–6.1 ns with. `SystemTime::now()` re-measured at
>   38.4 ns, so at the 1024-push default it contributes 0.04 ns amortised. **The
>   remaining 97% is the counter** — the load, compare and store through `&self`
>   that question 1 called *"a non-atomic counter increment and a compare against
>   a value in a register"* and priced at nothing.
> * **The 3% is not a property of the sampler, it is a property of 1024**, and
>   the first revision of this note said otherwise in five documents. The
>   benchmarked edge declares no rate, so it runs at the *largest* `sample_every`
>   the design produces. The cost is `counter + 38.4 / sample_every`, so at a
>   declared 10 Hz the clock is 78% of a ~4.9 ns per-push overhead. **What is
>   constant is the cost per second of publishing** — `38.4 + rate x 1.06` ns,
>   under a microsecond at 1 kHz and ~49 ns at 10 Hz — which is the quantity
>   question 1 was actually reasoning about. So question 4's *"move the interval,
>   not the placement"* is right at low rates and nearly useless at 1 kHz, where
>   the counter is the only thing left to delete. `docs/PHASE1.md` §11.2
>   tabulates it and marks the one measured row.
> * **The mechanism this record proposed was built, and it is slower.** *Shape
>   proposed* says to sample off the counter the push path already maintains —
>   `heartbeat & mask == 0`, a load of a line `SampleRing::push` has just
>   written. Implemented and benchmarked against the `Cell`: **+1.4 ns against
>   +1.1 ns**, three sittings. It also forces `sample_every` to a power of two,
>   and — because `heartbeat` belongs to the *edge* and not to the claim — it
>   cannot sample a new writer's first push, which is the property the two notes
>   below turn on. Rejected on a measurement rather than on the layering argument
>   question 1 gave.
> * **A claim's first push samples, and a claim clears the receipt it inherits.**
>   Neither was in the plan and both are load-bearing. Starting the countdown a
>   full interval away leaves a 10 Hz undeclared edge reading `0` for 102 seconds
>   and a 0.2 Hz one for 85 minutes — indistinguishable from the pre-`0036` state
>   step 3 plans to skip on. And **nothing in the system resets
>   `last_push_nanos`** — not `release`, not the reaper, not
>   `tf_tree_core::edge::claim` — so a replacement writer would publish under the
>   departed one's timestamp and `TFT004` would bill it as that publisher's clock
>   skew. `Tree::claim` now stores `0` beside the interval derivation.
> * **The first before/after said +47% and was wrong.** Two `cargo bench` runs
>   minutes apart, on a host `bench_report`'s fitness probe rejects outright: the
>   same unsampled push read 5.94 ns and then 4.82 ns, against an effect of
>   1.1 ns. `benches/push_sampler.rs` exists because of that, and its module doc
>   says so.
> * **The counter counts down, not up**, so the hot path compares against an
>   immediate zero and loads `sample_every` only on the sample. Worth ~0.15 ns
>   and three runs out of four — kept, and written down as weak evidence rather
>   than presented as a result.
> * **`heap`-tree `push` is ~4.9 ns here, not the 3 ns this record's table
>   says.** That table measured `EdgeWriter::push` on a 1024-slot ring in a
>   different sitting; the §11.1 fixture's edge is larger. It does not change any
>   ratio the record argues from — 38.4 against 4.9 is still an order apart —
>   but the 13× in *Why it is not simply a defect to fix* reads ~8× on this
>   fixture.

## Context

`docs/PHASE5.md` §6 says one thing about `TFT004` that it says about no other
check:

> **`TFT004` deserves special care** — it is the check most likely to find
> something nobody knew. Compute per-publisher offset between header stamp and
> arena receipt time, track a rolling median, and report publishers whose median
> differs from the fleet median by more than a threshold. On a multi-machine
> robot with imperfect PTP this finds real problems that present as intermittent
> extrapolation errors.

That is a description of the failure this library's users actually have. A
distributed robot with imperfect time sync produces transforms that resolve most
of the time and fail intermittently at the edges of a buffer, and nothing else in
a ROS 2 stack points at the clock. It is also the failure that is hardest to
attribute from the symptom, because the symptom is an extrapolation error on an
edge whose publisher is fine.

**`TFT004` detects nothing, in any configuration**, and §0.0 lists it beside
`TFT002`/`TFT003` as one of three that "cannot detect anything in any
configuration and say so". Its stated reason, from
`crates/tf_tree_cli/src/checks.rs`:

> **`TFT004`** (clock skew) needs a per-publisher *arena receipt time* to
> difference against the header stamp. Nothing records one: `SampleRing::push`
> stores the stamp the publisher supplied and no second timestamp of its own.

**That is the module doc, and the shipped skip reason agrees with it** — the
`Tft::Tft004` arm reports *"no per-publisher arena receipt time is recorded — a
push stores the publisher's stamp and nothing of its own to difference against"*.
Checked, because a record that argues against a comment while the code says
something else is arguing with nobody.

## The stated reason is true and its implication is not

`SampleRing::push` records no timestamp of its own — correct. What the sentence
leaves a reader to conclude is that recording one means **adding an arena field**,
which is where `CLAUDE.md`'s *"do not add arena fields opportunistically"*,
`FORMAT_VERSION` and [`0032`](./0032-the-region-table-was-not-part-of-the-purchase.md)
all come in, and that is what has kept the check where it is.

**The field is already there.** `ClaimRecord` (`crates/tf_tree_core/src/edge.rs`)
carries:

```rust
/// Advisory only. NEVER a reaping trigger on its own (§6.4).
pub last_push_nanos: AtomicI64,
```

`rg last_push_nanos crates/` returns **four** hits, and all four are the two
struct definitions and their two zero-initialisers. **Nothing writes it, and
nothing reads it.** It occupies its bytes in every shipped arena, is part of
`layout_hash`, and has done nothing since it was declared.

**`PHASE2.md` §6.4 says otherwise, and it is half wrong:**

> `heartbeat` and `last_push_nanos` remain in `ClaimRecord`, bumped on every
> push, and are **never** a reaping trigger.

`heartbeat` *is* bumped on every push — `SampleRing::push` stores `h + 1`
(`buffer.rs`), and [`0014`](./0014-the-push-heartbeat-is-a-store.md) made it a
store rather than a `fetch_add` and pinned it equal to `head` with a
`debug_assert`. `last_push_nanos` is not bumped by anything. So this is a
**normative sentence the code does not implement**, not a missing feature — and
populating the field engages no format decision at all.

## Why it is not simply a defect to fix

Because of what it costs, and the numbers are not close.

Measured on the development workstation, `-O`, 2 000 000 iterations each after a
warm-up:

| | |
|---|---|
| `EdgeWriter::push` on a heap tree, 1024-slot ring | **3 ns** |
| `SystemTime::now()` + `duration_since(UNIX_EPOCH)` | **38 ns** |
| `Instant::now()` (monotonic — wrong clock, see below) | **28 ns** |

**Writing a wall-clock receipt time on every push makes `push` about 13× more
expensive.** That is not a trade this project makes for a diagnostic, and §6.4's
own framing — *advisory*, *never a reaping trigger* — says the field is a
diagnostic.

**Two qualifications on the 13×, both in the direction of honesty rather than of
the argument.** The 3 ns is a *best case*: a heap-backed, single-threaded,
entirely L1-resident ring with no contention. A real publisher writing into a
shared segment with a cold ring pays more, so the true multiplier on a robot is
smaller than 13× — how much smaller is unmeasured, and `publish_to_visible` is
`unavailable` in the committed baseline (this host has 4 physical cores where the
row needs 17, and no ROS 2), so it was not available to check against. What the
measurement establishes is the **order** of the mismatch, which is enough to rule
out the unconditional form and not enough to price any other.

**`Instant::now()` is cheaper and is the wrong clock.** `TFT004` differences a
receipt time against a *header stamp*, and header stamps are wall-clock-domain
(`API.md` R3). A monotonic receipt time cannot be differenced against one.

## Decision

**None yet.** `draft`. The shape that looks right, and is not costed:

**Sample the receipt time rather than taking it every push, off a counter the
push path already computes.** `SampleRing::push` reads `head` into a local `h` and
stores `h + 1` into `heartbeat`, so a predicate of the form `h % INTERVAL == 0`
costs a mask and a branch on a value already in a register. At `INTERVAL = 1024`
the amortised cost is 38/1024 ≈ **0.04 ns**, inside the noise of a 3 ns push.

**`0014`'s `heartbeat == head` pin is *not* what makes this work, and an earlier
revision of this paragraph cited it as though it were.** The predicate reads the
local `h`, not `heartbeat`, so the equality is true and irrelevant — and it is a
`debug_assert`, absent from the release builds a robot runs. Leaning on it would
have been resting a hot-path decision on a check that does not execute where the
decision is paid for.

Sampling is not a compromise here — it is what the check wants. §6 asks for a
**rolling median** of per-publisher offset, which needs a distribution and not
every sample. A clock offset is a slowly-varying quantity; a publisher whose PTP
is drifting does not need to be measured at 1 kHz to be caught.

### The question that makes this a record and not a patch

**A fixed interval is right for one publisher rate and wrong for the others.** At
`INTERVAL = 1024` a 1 kHz IMU yields about one offset per second — ample — and a
10 Hz global localiser yields one per 100 seconds, which is too sparse for a
median inside any diagnostic window an operator will wait for. At `INTERVAL = 8`
the localiser is well served and the IMU pays 4.75 ns per push, which is more
than the push.

**Writing the field needs no plumbing whatsoever, which is worth separating from
the cost question.** `Publisher` already holds `claim: &'a ClaimRecord`, and
`last_push_nanos` is a field of exactly that record. There is nothing to thread
through and no borrow to rearrange; the only obstacle is the 38 ns, which is why
this record is about a sampling rule and not about a refactor.

There is a per-edge quantity that already knows the answer:
`EdgeRecord::nominal_rate_mhz`, written at declaration time from
`EdgeCfg::nominal_rate_hz` and reachable from a topology file's `rate_hz` —
landed for `TFT007` with no arena field added and no format bump. Deriving
`INTERVAL` per edge from it would give every publisher the same *offset sample
rate* rather than the same *push interval*. Whether that is worth the indirection
on the push path, and what an edge that declares no rate gets, is exactly what
this record has to decide.

## Rationale for filing it rather than building it

**`CLAUDE.md` sends this through a record for two independent reasons**, and
either alone would be enough.

1. **It contradicts a NORMATIVE sentence.** §6.4 says *"bumped on every push"*.
   Anything sampled does not do that, so §6.4 needs amending, and a normative
   amendment is a record.
2. **It changes what §0.0 claims about the catalogue.** `TFT004` moves out of
   the "cannot detect anything in any configuration" group, which is the
   catalogue's own headline statistic (*"detects 16 of 19"*).

And a third that is about this repository's habits rather than its rules: the
whole of [`0029`](./0029-the-topology-lock-is-a-kernel-lock.md) exists because a
change that looked like an adapter was not one, and the adapter it proposed was
measured to be wrong in the corrupting direction. A clock read on the push path
is the same shape of "small change" — one line, on the hottest write path in the
system, at 13× its current cost.

### Alternatives considered

- **Write it unconditionally.** Ruled out by the measurement, not by taste.
- **Put it behind the `counters` feature.** `PHASE5.md` §5 is explicit that
  counters *"cost nothing, they cannot affect a lookup result … No environment
  variable, no runtime flag"*. A 38 ns clock read does not fit inside that
  contract, and widening the contract to admit it would make the one guarantee
  §5 offers untrue of a build that has counters on.
- **Have `doctor` sample the offset itself**, from outside, rather than the
  publisher recording it. It cannot: `doctor` sees the newest *stamp* in a ring,
  and the quantity wanted is the offset **at the moment of publication**. Reading
  a stamp now and a clock now measures the sum of the offset and however long ago
  the sample was published, which is `TFT009`'s question and not this one.
- **Do nothing; leave `TFT004` blind.** A real option — `PHASE5.md` §8 is a whole
  section about not building something. The argument against is §6's own
  sentence: this is the check most likely to find something nobody knew, and it
  is blind for a reason that turns out to be a field nobody wired up.

## Consequences, if it is taken

- `TFT004` becomes detectable, and `PHASE5.md` §0.0's "sixteen detect" becomes
  seventeen. The three-that-cannot group drops to `TFT002`/`TFT003`, whose reason
  is a genuine locality problem and not an unwired field.
- **The push path grows a branch it did not have.** Costed above as ≈0.04 ns at
  `INTERVAL = 1024`; that is a claim about an amortised mean, and the *sampled*
  push pays the full 38 ns. A publisher with a hard per-push deadline sees a
  1-in-`INTERVAL` spike, which is the kind of thing this project measures at p99.9
  rather than at p50 (`PHASE1.md` §11).
- §6.4 is amended from "bumped on every push" to whatever this record decides,
  and the amendment has to keep saying the loud part: **never a reaping trigger**
  (§6.4's refusal to reap on staleness is not weakened by having a better
  timestamp to be stale against).
- A second *check* consumer appears for `EdgeRecord::nominal_rate_mhz`, if the
  per-edge interval is taken. `TFT007` is currently the only check that uses it;
  `doctor`'s report struct also carries it through
  (`doctor.rs`'s `nominal_rate_mhz: Option<u32>`), which is reporting rather than
  deciding.

## Open questions

Each carries a **recommendation** written in below, in the shape `0023` uses.
They are recommendations rather than decisions because a human ratifies by
merging; nothing normative moves until then.

1. **Fixed interval, or derived from `nominal_rate_mhz`?**

   **Recommendation: derived — one offset per second of published data, computed
   once at claim time, and the whole mechanism lives in `tf_tree`'s
   `EdgeWriter`. `tf_tree_core` does not change.**

   *Why derived.* A fixed interval fixes the wrong quantity. What `TFT004` wants
   is a comparable number of offsets per publisher per diagnostic window, and
   publishers differ by two orders of magnitude in rate. Deriving `INTERVAL` from
   the declared rate makes the *offset sample rate* the constant instead of the
   push interval: every publisher yields ~1 offset/second, and every publisher
   pays the same **38 ns per second of publishing** — 3.8 × 10⁻⁶ % duty, at any
   rate. A fixed interval cannot be chosen to do that for both a 1 kHz IMU and a
   10 Hz localiser, which is what made this the first question.

   *Why the cost objection dissolves.* The derivation is one division **per
   claim**, not per push. The push path gains a non-atomic counter increment and
   a compare against a value in a register.

   *Why the facade and not the core.* `tf_tree_core` is `no_std` and cannot read
   a clock at all (D14: `libm` + `bytemuck` + `blake3`). Sampling there would
   mean passing a clock into `Publisher::push` — a signature change on a
   published crate, for a diagnostic. The facade already owns every
   std-dependent concern on this path (`ClaimLease`, the fork generation,
   `0029`'s topology lease), so this belongs there by the rule already in force.
   The facade also already holds the `EdgeRecord` at claim time, so
   `nominal_rate_mhz` is in scope exactly where the division happens.

   *Two details that are not free and are not obstacles.* `EdgeWriter::push`
   takes `&self`, so the counter is a `Cell<u32>`, which is `Send` and not
   `Sync` — checked: nothing in the workspace asserts `EdgeWriter: Sync`, and D7
   makes one writer per edge a type-system property, so it has no reason to be.
   And an edge that declares no rate (`nominal_rate_mhz == 0`, which `TFT007`
   already skips on) gets a fixed default rather than no sampling, so a tree
   built without a topology file still produces offsets.

2. **Which clock, and what happens when the epochs do not match?**

   **Recommendation: the host wall clock, in integer nanoseconds; inherit
   `TFT005`'s epoch skip verbatim; and skip on a frozen `.tft` source.**

   The quantity is a difference against a *header stamp*, and header stamps are
   wall-clock-domain (`API.md` R3), so a monotonic receipt time is unusable
   however much cheaper it is (28 ns against 38 ns — the record's own table).
   `TFT005` already skips when the arena's stamps do not share an epoch with the
   system clock, and `Clock` in `tf_tree_cli` is where that is decided;
   `TFT004` must consult the same object rather than grow a second rule, which
   is what `PROJECT.md` §6 forbids as a second spelling.

   **A frozen `.tft` is a byte copy of the arena**, so its receipt times survive
   the freeze and mean nothing against a later reader's clock — the same shape as
   `TFT014`'s frozen-source skip, and it should be spelled the same way.

3. **What does `doctor` do with one offset per edge?**

   **Recommendation: ship the fleet comparison at one instant, and do not wait
   for a series.**

   Every edge's `(receipt − stamp)` against the fleet median, flagging outliers
   past a threshold. That needs no history, runs in a single-shot `doctor`, and
   catches exactly the case §6 describes — *one machine's clock is off from the
   rest*. §6 asks for a rolling median, and a rolling median is strictly better
   for a *drifting* clock; it is also a different tool, because it needs the
   polling loop `tf_tree top` has and `doctor` does not. Shipping the instant
   comparison first is the difference between a check that exists and a check
   that is still a paragraph.

   The threshold is deliberately not proposed here: it is the kind of number
   `0023` shows should be derived from a measured spread rather than chosen, and
   there is no fleet in this repository to measure one on. **`TFT004` should
   therefore report the spread and flag nothing until a threshold has evidence**
   — the shape `TFT007` was corrected into after review found it passing having
   compared nothing.

4. **Is a 1-in-`INTERVAL` 38 ns spike acceptable on the publish path?**

   **Recommendation: yes, with one hard constraint and one disclosure.**

   **The constraint: the clock read must sit outside the seqlock window.** A
   longer write window is not merely a slower push — it is more `SlotContended`
   retries for every reader of that edge, which converts a writer's diagnostic
   into a reader's latency. Taking the clock in the facade *after*
   `publisher.push` has returned gives this for free, and it is the reason the
   placement in question 1 is not just a layering convenience. **Any
   implementation that moves the read inside `SampleRing::push` gives this up.**

   **The disclosure: at one sample per second, the spike lands at or near p99.9
   for a 1 kHz publisher, which is the percentile `PHASE1.md` §11 says matters
   most.** 1-in-1000 *is* the 99.9th percentile, so a p99.9 push goes from ~3 ns
   to ~41 ns while p50 does not move. That is a real regression at the
   percentile this project gates on, it is arithmetic rather than a measurement,
   and it must be stated in `PHASE1.md` §11's terms rather than buried in an
   amortised mean. It is judged acceptable because the absolute number is 41 ns
   against a publish-to-visible budget measured in microseconds — but that
   judgement is an argument, and `publish_to_visible` is `unavailable` on this
   host, so **it ships unmeasured unless a host that can run it appears.**
   A reader who thinks 41 ns at p99.9 is too much should move the interval, not
   the placement.

## Implementation plan

Ordered, each step landable alone. `CLAUDE.md` makes a `ready` record's plan the
per-PR breakdown, so this is that breakdown and not a sketch.

1. **The sampler, entirely in `tf_tree`.** `EdgeWriter` gains two fields, both
   set at claim time: `sample_every: u32` = `nominal_rate_mhz / 1000` clamped to
   at least 1 — `nominal_rate_mhz` is **milli**hertz (`EdgeCfg::nominal_rate_hz`
   stores `rate_hz * 1000.0`), so that quotient is pushes-per-second and the rule
   *"one offset per second of published data"* is exactly `sample_every` — with a
   fixed default where the edge declares no rate; and `since_sample: Cell<u32>`.

   **The construction site was checked, not assumed.** `Tree::claim` builds
   `EdgeWriter { .. }` with both `view` and `eid` still in scope, so
   `view.edge(eid)`'s `nominal_rate_mhz` is reachable at exactly the point the
   two fields are initialised — no plumbing, no second lookup, and nothing to
   thread through `Publisher`.

   **And the `Cell` was checked against the auto traits it could have broken.**
   `OwnedWriter` is documented **and doctest-pinned** as `Send + !Sync`,
   inherited from this very field, with the `!Sync` half asserted as a
   `compile_fail,E0277` that `just test-doc-error-codes` runs on nightly —
   `0017`'s lifetime extension is what makes those pins load-bearing rather than
   decorative. `Cell<u32>` is `Send`, so the `Send` pin holds; it is `!Sync`, so
   the `!Sync` pin holds and is now over-determined rather than weakened
   (`Publisher` already supplied it). No `unsafe impl` is needed or wanted — that
   type's doc says there must never be one, because it would keep compiling after
   somebody swapped the field for something with no business crossing a thread.

   `EdgeWriter::push` calls `self.publisher.push(stamp, iso)?` **first**, then
   counts, and on wrap reads the clock and does one `Relaxed` store into
   `claim.last_push_nanos`.

   *Verified by* five tests in `crates/tf_tree/tests/receipt_time.rs`, each with
   a mutant that was **run** rather than predicted: (a) a declared-rate edge
   samples on its first push and every `sample_every`-th after, and the values
   land inside the wall-clock window the test itself brackets — which is what
   pins the clock as `SystemTime` and not `Instant`; (b) **a push that fails
   leaves the receipt untouched, and does not spend the interval** — the `?` is
   what puts the clock read after the ring write, so a rejected push stamping
   `last_push_nanos` is the observable form of the read having drifted inside the
   seqlock window, which is question 4's hard constraint and the only part of it
   a test can see; (c) an edge with `nominal_rate_mhz == 0` still samples, at the
   default; (d) a second claim of the same edge starts a fresh interval; (e) a
   fresh claim does not inherit the previous writer's receipt time.

   Each assertion detects the store by **zeroing the field**, never by comparing
   two readings: two samples ten pushes apart can land in the same nanosecond on
   a fast enough build, and a test that counted distinct values would flake in
   the direction of *"the sampler stopped working"*.

2. **`PHASE2.md` §6.4's amendment**, from *"bumped on every push"* to the sampled
   rule, **with *never a reaping trigger* restated rather than assumed**. A
   timestamp that is now actually written is exactly the thing a future reader
   would reach for as a staleness trigger, which §6.4 exists to forbid; the
   amendment that adds the first half without repeating the second is the one
   that gets misread. *Verified by:* the section naming the rule and the refusal
   in the same paragraph.

3. **`TFT004` itself.** Per edge: `last_push_nanos` and the newest stamp, offset
   = receipt − stamp; report the fleet spread; **flag nothing until a threshold
   has evidence** (question 3). **Four** skips, each with its own reason string
   — the fourth found by review of step 1 and not by this record:

   **A replayed source.** `tf_tree_ingest` publishes through `EdgeWriter::push`
   like any other writer, so ingesting a 2024 recording in 2026 stamps *2026*
   receipts against 2024 header stamps. The field is not wrong — it means "when
   this arena received this sample", and it did — but it is exactly what this
   check reads as skew, and `doctor --from-bag` hands the catalogue an ordinary
   live heap `Tree`, so neither the frozen-source skip nor `TFT005`'s epoch
   condition catches it. **`TFT004` must skip a tree whose samples were replayed
   rather than published live**, and the ingest path must carry whatever marks
   it as such.

   And the three this record already named: nothing sampled yet
   (`last_push_nanos == 0` — a publisher that has not reached its first sample is
   not a skew finding, now narrowed to *one* push by step 1's first-push rule),
   `TFT005`'s epoch condition via `Clock`, and a frozen `.tft` source. *Verified by:* a fixture with one
   deliberately skewed publisher, asserting the offset is attributed to *that*
   edge and that the reason string names which skip fired when it fires. *Mutant:*
   drop the `== 0` skip — a fresh arena reports every publisher as maximally
   skewed.

4. **`PHASE5.md` §0.0**, sixteen detecting to seventeen, and `TFT004` out of the
   "cannot detect anything in any configuration" group into the conditional one.
   *Verified by:* `tf_tree doctor` on the reference fixture printing the new
   counts, and §0.0 quoting them.

5. **The cost, on the record rather than in the commit message.** `just
   bench-check` against the committed baseline, and — if a host that can run
   `publish_to_visible` is available — the p99.9 figure question 4 owes. If it is
   not, `PHASE1.md` §11 gains one sentence saying the p99.9 effect is arithmetic
   and unmeasured, in §11's own terms. **Not a step to skip quietly**: a
   diagnostic that costs the publish path its p99.9 and says so in a decision
   record only is the shape `0023` was written about.

## What merging this ratifies

- The four recommendations above, as decisions.
- `PHASE2.md` §6.4's amendment from *"bumped on every push"* to the sampled rule,
  **keeping *never a reaping trigger* intact** — a better timestamp to be stale
  against does not weaken §6.4's refusal to reap on staleness, and the amendment
  must say so or it will be read as licence.
- `PHASE5.md` §0.0 moving `TFT004` out of the "cannot detect anything in any
  configuration" group, taking *"sixteen detect"* to seventeen.

**What it does not ratify, and what stays owed:** question 4's p99.9 effect is
arithmetic and not a measurement, and this host cannot supply one
(`publish_to_visible` is `unavailable`: 4 physical cores against 17 needed, no
ROS 2). The implementation owes `just bench-check` on a host that can, or an
explicit statement in `PHASE1.md` §11 that it shipped without one.

## Not in this record

**`TFT002`/`TFT003`.** They are also undetectable, and for an unrelated reason:
`tf_tree_bridge`'s `StaticStore` counts exactly those conditions and lives in the
bridge process's heap, so surfacing them needs the bridge to publish counters
into the arena, which `PHASE5.md` §1.2 reserves no space for. That is a locality
problem and a space problem; this is a field that exists and is unwritten. Fixing
one says nothing about the other.
