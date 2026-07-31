# 0011: The online bridge's clock guard, and what a static conflict does about authority

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** (filled in as work lands)

## Context

A five-lens audit of the Phase 4/5 surface surfaced three defects in
`crates/tf_tree_bridge/src/ingest.rs` that are behaviour changes rather than
repairs, and therefore belong here rather than in a PR. All three concern the
same function, `Ingest::offer`, which is §5's decision pipeline.

One thing shared by all three, and the reason they are one record: **the bridge
conflates a per-edge, per-message *fact* with a global or temporal *judgment*.**
`ClockGuard::observe` answers "is this stamp behind the newest one I accepted" —
exact, per observation. `Authority::admit` answers "does this publisher own this
edge" — exact, per edge. `StaticStore::observe_static` answers "does this value
match the declared one" — exact, per edge. Each of those is a fact. "The clock
has been reset" and "this deployment is misconfigured and must not start" are
judgments *about a set of facts*, and the bridge currently derives each one from
a single fact, which is where all three defects come from.

### 1. The clock guard is one guard for the whole stream

`Ingest` holds a single `ClockGuard` and feeds it `sample.stamp_nanos` from every
accepted transform on every edge. `ClockGuard::observe` classifies any stamp more
than `DEFAULT_RESET_THRESHOLD_NANOS` (100 ms) below that shared high-water mark as
`ClockVerdict::Reset`, which under the default `OnClockReset::Halt` becomes
`Action::Halt` — latched by `tft_bridge_offer`, stopping the bridge permanently.

But `/tf` is not one publisher. AMCL and `robot_localization` publish
`map -> odom` dated their `transform_tolerance` (0.1–1.0 s by default) **into the
future**, while `odom -> base_link` from the wheel driver is stamped at publish
time. A SLAM node dates `map -> odom` at its last keyframe, hundreds of
milliseconds **behind**. Either direction is a steady inter-edge offset larger
than 100 ms on a correctly configured robot, and the lagging edge's next message
then reads as a backward jump off the leading edge's high-water mark.

**This project has already litigated this, and decided the other way — for the
offline half.** `crates/tf_tree_ingest/src/ingest.rs` carries a module-doc section
titled *"And the guard is per **edge**, not per stream"* making precisely this
argument, and keeps a `Vec<ClockGuard>` indexed by edge slot. Its conclusion:

> A single guard over the merged stream therefore reports a *reset* — the whole
> ingest halting, at the default, on an ordinary recording — for something that
> is not a clock reset at all but two publishers.

The online half has the identical exposure and the opposite implementation.

**What the evidence does and does not show.** `crates/tf_tree_cli/tests/topology.rs`
tolerates `Action::Drop { NonMonotonic }` on the real corpus and calls the global
guard *"global by design (it tracks the bridge's notion of now)"* — so there is a
written intent, not merely an oversight. Measured against
`testdata/tfstream/indoor_atelier.tfstream`, however, the guard currently drops
**nothing**: 1066 of 1066 samples published, `dropped_non_monotonic == 0`,
`clock_resets == 0`. That corpus's publishers are stamp-aligned, so it neither
demonstrates the defect nor refutes it. **The case rests on the mechanism and on
the offline half's recorded argument, not on an in-tree reproduction**, and this
record does not claim otherwise. See *Rationale* for why the mechanism is enough
here and what would count as better evidence.

### 2. `AuthorityPolicy::Strict` is unimplemented, and not only for statics

§5.7 specifies the order as: on a differing static value, "a diagnostic naming
both publishers and both values, **then** apply the authority policy". The
`StaticVerdict::Conflict` arm returns `Action::StaticConflict` directly, and every
arm of the `/tf_static` block returns, so `self.authority.admit(...)` is
unreachable for a static sample. The comment above it claimed the policy "decides
the disposition"; that comment has been corrected to describe what the code does,
and this record carries the question of what it *should* do.

The deeper finding is that fixing only that arm would fix the wrong thing.
§5.4's table defines `Strict` as *"Refuse to start if a conflict is detected
within a startup window. For CI."* — and **there is no startup window anywhere in
this crate**. `Authority::admit`'s `Strict` arm returns `Verdict::Fatal` on the
*second message that collides*, whenever that arrives, which is not "refuse to
start" and has no window in it. So `Strict` is unimplemented for **every**
conflict type, not merely for static ones, and routing statics into today's
`Strict` arm would give the static path a per-message halt wearing the startup
window's name.

### 3. `Action::Drop` has no `first_time`, so its ROS log arm is unreachable

Every other diagnostic action carries `first_time` as a rate limiter. `Action::Drop`
does not, so `fill()` never sets it, so the `if (out.first_time != 0)` gate in
`ros/tf_tree_ros/src/bridge_handle.cpp`'s `TFT_BRIDGE_DROPPED` tail is dead code:
three fault classes (`BadName`, `KindChange`, `NonMonotonic`) are silenced
entirely. Adding the field changes a public enum in `tf_tree_bridge`.

## Decision

All three are settled below. The common shape: **keep the primitive per edge and
exact, and make the promotion an explicit rule with its own window or quorum.**

### The crate's one notion of time: the transform ordinal

Both (1) and (2) need a window, and `tf_tree_bridge` has no time source of any
kind — no `std::time`, no `Instant`, no tick. Rather than introduce two
incompatible notions, the crate gains exactly one: **`BridgeStats::transforms`,
the count of transforms offered, is the bridge's clock.** It is incremented
unconditionally at the top of `offer` before any early return, so a caller cannot
forget to advance it; it is in-process and monotone; and — decisively — it cannot
be moved by a publisher's stamp, which is the very quantity under suspicion in (1).

It is an *ordinal*, not a duration, and both windows are therefore counts of
transforms. §*Rationale* argues that this is not a compromise for (1) but the
correct measure, and §*Consequences* states plainly where it is a compromise
for (2) and what the escape hatch is.

### 1. One `ClockGuard` per edge, promoted to a reset by a quorum of *publishers*

`Ingest::clock: ClockGuard` becomes a guard per edge, plus a promotion layer and
the one number the promotion rule is allowed to demand:

```rust
clocks: ByEdge<ClockGuard>,
on_clock_reset: OnClockReset,   // not recoverable from an existing guard
quorum: ResetQuorum,
```

`clocks` is built with `crate::edgemap::{lookup_mut, insert}` — **never**
`entry()` with an owned key — so the steady-state path allocates nothing and an
edge's two `String` keys are paid once, on its first sample.

The corroboration floor is **not** a field. It is `Authority::distinct_owners()`,
derived from the owner table `Authority` already keeps, for the reason
*Rationale* gives: it has to count publishers, and an edge count is a proxy that
fails on the exact topology the quorum was corrected for.

`ResetQuorum` lives in `clock.rs`, strictly **above** `ClockGuard` and never
inside it, and holds one row per regressing edge:

```rust
struct Regression {
    onset: u64,     // when this bout of regressing started; deliberately not refreshed
    last:  u64,     // the most recent regression, which is what keeps the row alive
    owner: String,  // who was publishing this edge when it regressed
}
```

with `QUORUM_EDGES = 2`, `DEFAULT_CORRELATION_WINDOW = 4096` observations, and
`MAX_TRACKED_EDGES = 1024` rows — a cap, because the keys come from outside the
type and refusing a row can only make a halt harder to reach, never easier.
`QUORUM_EDGES`' name is historical: it counts **publishers**, and it is a ceiling
on what may be demanded rather than a fixed demand. Both are corrections, below.

On `ClockVerdict::Reset` for edge *(p, c)*, published by *owner*, at ordinal *n*:

1. `stats.dropped_non_monotonic += 1` **always** — the transform is refused
   either way, and this term is in `BridgeStats::balanced()`.
2. `quorum.record(p, c, owner_key(publisher), n, self.authority.distinct_owners())`. The row
   for *(p, c)* is created or refreshed: `last = n`; `onset` is **not** moved, so
   a bout that never ends ages out of the window instead of corroborating every
   later hiccup in the tree; `owner` **is** overwritten, because an edge that
   changed hands is published by whoever publishes it now.
3. The demand is floored by what the deployment could possibly supply:
   `needed = QUORUM_EDGES.min(corroborators.max(1))`, with `corroborators =
   self.authority.distinct_owners()`.
4. Count **distinct owners** — not distinct edges — among the rows whose `onset`
   is within `DEFAULT_CORRELATION_WINDOW` of *n*. The regressing edge's own owner
   counts.
5. **Below `needed`** — `QuorumVerdict::Isolated`: one publisher restarting,
   hiccuping or replaying its own buffer. Return
   `Action::Drop { NonMonotonic { by_nanos } }`. `clock_resets` is **not**
   incremented. The guard's high-water mark is not moved, so that publisher stays
   refused until it catches up, which is what the arena would do anyway.
6. **At or above `needed`** — `QuorumVerdict::Reached { edges }`: the publishers
   cannot all be wrong by themselves, so this is the clock they share. `edges` is
   the distinct *edge* count in the window, not the publisher count — the
   publisher count decided the verdict, the edge count is what an operator can go
   and look at. `stats.clock_resets += 1`, then apply `on_clock_reset`:
   - `Halt` → `Action::Halt { HaltReason::ClockReset { by_nanos, correlated_edges } }`
   - `Recreate` → rewind every guard, clear the quorum, then
     `Action::RecreateArena { by_nanos }`

**Steps 3 and 4 are corrections to this record, made after it was `ready` and
implemented, and recorded as such** — the draft compared a count of *edges*
against a flat `2`. See *Rationale* → *Two corrections adversarial review
forced*.

`HaltReason::ClockReset` gains `correlated_edges: u32`. `ClockGuard::observe` and
`ClockVerdict` are **unchanged** — the quorum is a layer strictly above the
primitive, which is what keeps the offline half and the existing `clock.rs` unit
tests untouched.

`ingest::owner_key` maps a `Publisher` to one borrowed key. The three
unattributed variants (`UnknownGid`, `Unattributed`, `Declared`) collapse to
**fixed** bracketed sentinels rather than to per-sample identities: as far as a
quorum is concerned an unattributed publisher is one identity, which can only
make a quorum harder to reach — the safe direction, since a quorum reached in
error is a halt on a healthy robot. Bracketed because a ROS node name cannot
contain `<`, so a real node can never collide with a sentinel.

**`Recreate` rewinds each guard in place** — `ClockGuard::forget()`, which is
added — rather than calling `accept_reset` per edge or dropping the map. Not
`accept_reset`: the only stamp in hand belongs to the one edge that regressed,
and seeding every other edge's guard from it would reintroduce exactly the
cross-edge contamination this change removes. Not dropping the map: that frees
the two owned `String` keys per edge, so the first sample on every edge after the
recreate re-enters the allocating path, while the table's shape was fixed by the
topology at startup anyway. The draft of this record said "clear the map" and
"**no** `ClockGuard::forget()` is added"; implementation reversed both, for the
reason just given. `forget()` is additive on a type the offline product also
links, so it withdraws nothing.

`ResetQuorum::clear()` does go with the recreate: its rows describe regressions
against high-water marks that no longer exist, and carrying them into the new
arena would let the first ordinary hiccup after the rebuild form a quorum with
edges from before it.

`BridgeStats::clock_resets` therefore **narrows** in meaning: it counts
promotions, not per-edge regressions. Under `Halt` it is 0 or 1 for the life of
the bridge. Its doc moves with the code.

### 2. A real startup window; `Strict` accumulates inside it and halts at its close

`Ingest` gains a startup window that is open from construction and closes at
whichever comes first:

- `stats.transforms >= STARTUP_WINDOW_TRANSFORMS` (private constant, **4096**) —
  a backstop, so a caller that never closes it explicitly still reports; or
- `Ingest::close_startup_window(&mut self) -> Option<Action>`, the explicit close,
  which is how a caller that owns a real clock supplies a real duration.

**Inside the window**, no conflict halts per message. Both conflict kinds are
recorded and the sample is disposed of exactly as `FirstWriterWins` would:

- `Authority::admit`'s `Strict` arm keeps returning `Verdict::Fatal` (it is a
  fact: "Strict saw a conflict on this edge"), but now also performs the
  `reported`/`dropped` bookkeeping the `FirstWriterWins` arm does, so the
  conflict is visible to `Authority::conflicts()` and to `tf_tree doctor`. It
  still does **not** mutate `owners`. `Ingest` maps `Fatal` inside the window to
  `Action::AuthorityConflict { .., first_time }` and `dropped_authority += 1`.
- The `/tf_static` conflict arm is unchanged in what it returns
  (`Action::StaticConflict`, `static_conflicts += 1`, `dropped_authority += 1`) —
  the diagnostic §5.7 requires is already correct and stays byte for byte.

There is **no separate conflict ledger**. At close, the window reads what is
already recorded: `Authority::conflicts()`, plus a new
`StaticStore::conflicts_by_edge()` iterator (additive; `StaticStore` exposes only
a `u64` count today). Nothing accumulated during the window can be a
post-window conflict, so no filtering is needed.

**At close**, under `AuthorityPolicy::Strict` only, if either source is non-empty:

```rust
HaltReason::StartupConflicts { authority: u32, statics: u32 }
```

and the C seam's `detail` string enumerates **every** recorded edge with its two
publishers, not the first.

**Outside the window**, `Strict` degrades to `FirstWriterWins` plus counters.
This is stated, not incidental: a bridge that has been healthy for an hour must
not be killed by a late-joining publisher.

The window-close halt is checked **at the top of `offer`, before
`stats.transforms += 1`**, and does not increment any counter. A window-close
halt is not an event about the arriving transform — it is caused by transforms
already counted — so charging it a bucket would unbalance the ledger. The same
holds for `close_startup_window()`, which has no transform in hand at all.

The twelve-line comment at the `StaticVerdict::Conflict` arm that names this
record as unresolved is deleted by the commit that implements this.

### 3. `Action::Drop` keeps its shape — resolved as **no**

No enum change. The `rclcpp` side throttles, as the `TFT_BRIDGE_REJECTED` arm
already does — and it must do so at **three distinct call sites**, one per reason,
because rcutils' throttle state is a function-local `static` per macro expansion
site and a single throttled call would let a kilohertz `NonMonotonic` edge starve
a once-ever `KindChange` line. The `TFT_BRIDGE_DROPPED` tail also needs a
`reason_name()` (there is none in `ros/tf_tree_ros/` today), since `fill()`'s
`Drop` arm sets no `detail` and the log would otherwise render as
`"odom -> base dropped from /tf: "`. `BAD_POSE` reaches the same tail *with* a
detail set and must keep it.

## Rationale

### (1) Why a threshold cannot separate the two cases, and a quorum can

The two events the guard must tell apart produce the *same* observation — a stamp
behind a high-water mark by more than the threshold — and differ only in their
**correlation across edges**:

| | which edges regress | for how long |
|---|---|---|
| `/clock` reset (bag loop, sim reset) | **every** edge, at each publisher's next message | once, then the stream is monotone again from the new origin |
| `transform_tolerance` / SLAM latency offset | **one** edge, relative to another | persistently, for the life of a correctly configured robot |

A threshold is a function of a single scalar, `by_nanos`, on a single
observation. Both events can produce any value of `by_nanos`: a bag looping five
seconds in produces 5 s, and an AMCL `transform_tolerance` of 1.0 s produces 1 s,
and neither bound is under our control — `transform_tolerance` is a user
parameter with no ceiling. So for any threshold *T*: raise it above the largest
plausible offset and a bag loop shorter than *T* is missed; lower it below and
every correctly configured robot with a lagging estimator halts. **There is no
value of *T* that separates them, because the quantity that distinguishes them is
not on the axis *T* measures.** Correlation across edges is.

A quorum reads exactly that axis. One publisher regressing is a fact about one
publisher and is handled as one: dropped, counted, and reported per edge, which
is strictly more information than a global halt gave. Two distinct *publishers*
regressing close together is not a coincidence — separate publishers do not
restart in lockstep — so it is the shared thing beneath them, which is the clock.
Reset detection is therefore **preserved, not weakened**: a real reset moves
`/clock`, every publisher's next message regresses, and the quorum is met on the
second publisher's. Where a deployment has no second publisher to wait for, the
demand is floored to one and the first past-threshold regression is the answer —
which is the same conclusion reached without any corroboration, because with one
publisher there is nothing else the jump could be.

**Why the window is measured in transforms, and why that is right rather than
merely available.** The question the correlation window asks is "have the *other*
edges' next messages also regressed?", and "next message" is a quantity in the
stream, not in seconds. A 1 kHz stream delivers every publisher's next message in
a millisecond; a 10 Hz stream takes a hundred times longer, and a wall-clock
window correct for one is wrong for the other. A transform count auto-scales with
the stream rate, which is the property wanted. 4096 transforms is roughly two
seconds of a typical 20-transform, 100 Hz `/tf` — comfortably more than one
publish period of a 1 Hz publisher — and proportionally more wall time on a
slower stream, where it is correspondingly less likely to have elapsed at all.

Alternatives rejected:

- **Raise the threshold.** Rejected above: no value works, and one large enough
  to absorb a configurable `transform_tolerance` is too large to catch a reset.
- **Fix it in the C ABI**, feeding the guard per edge from `tft_bridge_offer`.
  Rejected: it puts a §5.5 judgment on the wrong side of the seam, and
  `tf_tree_bridge` is the crate whose whole purpose is that both callers classify
  a stream the same way.
- **Leave it.** Rejected: the corpus does not exhibit the fault, but the corpus
  is one indoor recording with stamp-aligned publishers; `transform_tolerance` is
  on by default in two of the most widely deployed ROS 2 packages, and the
  failure mode is a permanently latched bridge on a healthy robot.

### Two corrections adversarial review forced

This record was `ready`, and implemented, before either of these was found. Both
are defects in the **decision**, not in the code that faithfully implemented it,
and both are written down rather than quietly absorbed: a reader who finds the
rule counting publishers and flooring its demand deserves to know that counting
edges and demanding a flat two were tried first, and what falsified each.

**A. The quorum counts distinct publishers, not distinct edges.** As drafted,
step 4 counted edges. Every *argument* for the rule is about publishers — the
sentence the whole design rests on is "separate publishers do not restart in
lockstep" — and edges were substituted for publishers without the substitution
being checked. Review supplied the case that breaks it, and it is not a corner:
**one node owning two dynamic edges.** A localization node publishing
`map -> odom` and `odom -> base_link` restarts; both of its edges regress in the
same instant; two *edges* form a quorum; and the bridge halts on precisely the
single-publisher event this decision exists to stop it halting on. The
false-halt mode was not removed, it was moved from "any robot with a lagging
estimator" to "any robot whose estimator owns more than one edge".

So `Regression` carries an `owner`, `ResetQuorum::record` takes one, and the
comparison runs over distinct owners (`fresh_publishers`). The *reported* number
stays in edges, because "three edges regressed" is what an operator can go and
look at while the publisher count is only what decided the verdict. The premise
was always publishers; edges were an implementation of the premise that is equal
to it in exactly the deployments where the rule was never needed.

**B. The quorum is floored by what the deployment can supply.** As drafted, two
corroborating parties were demanded unconditionally. On a deployment with **one**
dynamic edge — one publisher, therefore at most one possible witness — that
demand cannot ever be satisfied, so §5.5's reset detection there is not merely
degraded but **structurally unreachable, and silently so**. Measured on a bag
loop: `dropped_non_monotonic: 500`, `clock_resets: 0`. The counter an operator
reads to answer "did the clock reset" was pinned at zero by construction, and
nothing in the diagnostics said the rule had never been applicable.

And demanding corroboration there was never justified in the first place. The
quorum exists to separate "this publisher restarted" from "the clock moved", and
that ambiguity **requires two publishers to exist**. With one there is no second
party to mistake the event for: a past-threshold backward jump is unambiguous,
and the pre-`0011` behaviour — halt on the first one — is not a fallback but the
correct answer for that shape. The floor is what makes the rule degrade *into*
correctness rather than out of it.

The effective demand is therefore `QUORUM_EDGES.min(corroborators.max(1))`, with
`corroborators = Authority::distinct_owners()` — the number of distinct
publishers that have actually established ownership of an edge.

**It must be a publisher count and not an edge count**, and getting that wrong
was caught in review rather than by reasoning. An earlier revision of this
section passed the number of declared dynamic *edges* as a proxy. That proxy
fails on precisely the topology the quorum was corrected for: one node owning
`map -> odom` and `odom -> base_link` declares two edges, so the floor stayed at
two, so that node could never corroborate itself — and §5.5 went silently
unreachable for it. The defect the floor exists to remove, moved one step along
instead of removed. Deriving the count from `Authority`'s existing owner table
keeps one source of truth and costs no new state.

`QUORUM_EDGES` becomes a **ceiling on what may be asked** rather
than a fixed demand. The `max(1)` is not decoration: a quorum of zero is reached
by the empty set, which is a halt reported on no evidence at all.

**The floor also dissolves this record's own *Tests this breaks* section, and
that is the best evidence available that the demand was wrong rather than merely
inconvenient.** That section observed that *every* fixture in the workspace
exercising a clock halt declares exactly one dynamic edge, and concluded that all
of them must be re-fixtured with a second publisher to keep asserting what they
already asserted. Under the floor they are all correct as they stand: one dynamic
edge means one possible witness means halt on the first regression. A rule that
required four unrelated tests across three products to be rewritten in order to
keep testing the right thing was saying something about the rule.

**What neither correction changes:** the repo's own corpus still does not
demonstrate the original defect. `testdata/tfstream/indoor_atelier.tfstream`
publishes 1066 of 1066 samples with `dropped_non_monotonic == 0` and
`clock_resets == 0` before and after, because its publishers are stamp-aligned.
The case for (1) rests on the mechanism and on the offline half's recorded
argument, as it did when this record was drafted. What review added is not
evidence for (1) but two counter-examples to how (1) was specified.

### Deliberately not now: observing `/clock` directly

The strongest version of (1) does not infer a clock reset from `/tf` stamps at
all — it **subscribes to `/clock` and watches the clock itself**. That is better
evidence by construction: a `/clock` regression *is* the event, observed once at
its source, with no quorum, no correlation window and no threshold tuning, and
with none of the ambiguity that this record exists to resolve. Publisher stamps
would then be back to answering only the per-edge monotonicity question they are
actually authoritative for.

It is future work, and its precondition is real: **`ros/tf_tree_ros/` has no
`/clock` subscription today.** `use_sim_time` is read exactly once, in
`bridge_node.cpp`, only to warn when it is true and `time_domain` is 0;
`BridgeHandle` never reads it, so §5.8's form 3 has no sim-time awareness at all.
Reaching `/clock` needs a new subscription, a `rosgraph_msgs` dependency, and a
new ABI entry point to carry the observation to the Rust side where the §5.5
judgment belongs. That is a larger change than this record, it is `rclcpp`-only
(the offline half has no `/clock` to watch and would keep the quorum), and it
does not remove the need for per-edge guards — it removes the need for the
quorum. Doing the quorum first is therefore not wasted work: it is the part that
both halves keep.

### (2) Why a startup window beats a per-message halt

Two reasons, and neither is about implementation cost.

**CI wants every misconfiguration in one run.** `Strict` exists for CI — §5.4's
table says so in three words. A per-message halt reports the *first* conflict and
stops, so a deployment with four misconfigured publishers takes four runs to
diagnose, each one paying a full boot. Accumulating for a window and halting once
with everything found turns that into one run and one report. That is the entire
value proposition of the policy, and a halt-on-first implementation delivers a
quarter of it.

**`/tf_static` is `transient_local`, so *when* a conflict is observed is a DDS
discovery artefact, not a fault time.** Latched statics are delivered to a
subscriber when discovery matches it to the publisher, which can be seconds after
either process started and, for a late-joining subscriber, arbitrarily long after
the fault was introduced. A per-message halt on a static conflict therefore fires
at a time that carries no information about when anything went wrong — and, on a
bridge that has been running for an hour, kills a healthy robot because a
publisher it had never matched finally appeared. Making the *window* the thing
that decides puts the judgment on a bounded, deliberate interval instead of on
discovery timing.

The two together also explain why the static arm must not simply be routed into
today's `Strict` arm, which was the shape the draft proposed: that would be a
per-message halt wearing the startup window's name, and it would inherit both
defects.

Alternatives rejected:

- **Make `Authority::admit` window-aware.** Rejected: it would turn a pure
  per-edge fact table into something that knows about time, which is the exact
  conflation this record is about. The window lives in `Ingest`, above the
  primitive — the same placement as (1)'s quorum.
- **Add a startup-window duration to `tft_bridge_options`.** Rejected: it changes
  `sizeof`, and every validation site in `crates/tf_tree_c/src/bridge.rs` is an
  exact equality, so every unrebuilt caller would get `TFT_ERR_BAD_STRUCT_SIZE`.
  The explicit `close_startup_window` entry point is additive and gives the ROS
  node somewhere to put a real, parameterised duration.
- **A new `BridgeStats` bucket for accumulated conflicts.** Rejected: an
  accumulated conflict is still a dropped transform and `dropped_authority` is
  already its bucket. A new term in `balanced()` would move nine assertions
  across three products for no diagnostic gain.

### (3) Why `Action::Drop` does not grow a `first_time`

`first_time` and a throttle answer different questions, and the three drop
reasons do not share rate-limiting semantics:

- **`KindChange`** is bounded by the declared topology — genuinely once per edge,
  which is what `first_time` is for.
- **`NonMonotonic`** is high-frequency by nature. `first_time` would report the
  first regression and then nothing, **under**-reporting a fault whose severity is
  precisely its rate. A throttle reports it continuously and at a bounded cost.
- **`BadName`** is the decisive one: the name *failed* normalization, so the key
  of any per-edge table would be a string the **publisher** controls and is
  unbounded. A `first_time` table here reintroduces the exact unbounded-growth
  bug that `NameNormalizer::seen` was capped to fix, and whose fix carries an
  in-tree comment saying so.

Underneath: **`BridgeStats` is the reporting surface; logs are a convenience.**
`dropped_bad_name`, `dropped_kind_change` and `dropped_non_monotonic` are already
exact, already cross the C ABI, and are what `tf_tree doctor` reads. Throttling a
log line loses no diagnostic information, because the diagnostic was never in the
log line. Changing a public enum to improve a convenience, when the authoritative
surface is already correct, is the wrong trade — especially on an enum matched
exhaustively across two products.

## Consequences

### Committed to

- **The bridge's clock is `stats.transforms`.** Any future window in
  `tf_tree_bridge` is a count of transforms, or it needs a decision record.
  Adding `std::time` to this crate is now a deliberate act, not an omission.
- **A quorum halt names its trigger edge and its count, not its members.**
  `tft_bridge_outcome` has room for one `(parent, child)` pair, filled from the
  arriving sample; `correlated_edges` rides in `HaltReason` and in `detail`.
  Growing the POD is `struct_size`-versioned and is deliberately not done here.
- **`BridgeStats::clock_resets` narrows** to "promotions to a clock reset" and its
  doc moves with it. It is not a term in `balanced()`, so no ledger changes, but
  it crosses the C ABI unchanged and is read by `tf_tree doctor`.
- **`dropped_non_monotonic` must keep being incremented on the single-edge
  regression path**, or `balanced()` is false forever.
- **The quorum's unit is the publisher.** Any future rule that promotes a
  per-edge fact to a global judgment counts publishers, or says in writing why
  edges are the right unit for it. Edges are the unit only for what is
  *reported*.
- **A promotion rule may never demand more corroboration than the deployment can
  supply.** `QUORUM_EDGES` is a ceiling; the floor is derived from the declared
  topology. A rule whose demand a valid configuration cannot meet is not a
  conservative default, it is an unreachable branch with a counter wired to zero.
- **A window-close halt charges no bucket**, because it is not an event about the
  arriving transform. This is the ledger invariant that step 5's tests pin.
- **`Strict` outside the window is `FirstWriterWins` plus counters.** Documented
  behaviour, not a degradation to be quietly fixed later.

### Known limitations, stated rather than deferred

- **A transform ordinal is a poor proxy for a *duration*, and (2) genuinely wants
  a duration.** 4096 transforms is ~2 s of a busy `/tf` and several minutes of a
  sparse one; the startup window is a discovery-timing question, and discovery
  does not run at the message rate. The backstop exists so that a caller that
  never closes the window still reports; the *primary* mechanism is
  `close_startup_window()`, and the `rclcpp` node is expected to drive it from a
  one-shot **steady** timer (not `node_->get_clock()`, which is `/clock` under
  `use_sim_time` and regresses on exactly the bag loop (1) detects). A caller in
  another language that never calls it inherits the backstop.
- **A bridge that receives no traffic never closes its window.** There is no tick
  in the crate and no ABI entry point that fires without an offer. A silent
  bridge has no conflicts to report, so the gap is narrow — a `/tf_static`
  conflict on a bridge that then goes permanently silent — but it is real.
- **The online and offline halves now agree about *scope* and diverge about
  *promotion* — except where corroboration is impossible, where they agree
  again.** `tf_tree_ingest::survey` halts on the **first** edge that regresses;
  the online bridge asks for a second publisher when there could be one. This is
  deliberate, not an oversight: a bag is a finished artifact and stopping to tell
  the operator "edge *X* regresses at *t*" costs nothing but a rerun, whereas a
  false halt online takes down a running robot. The draft's claim that this
  change "makes the two halves agree" was true only of scope and is corrected
  here. Correction B then closes the divergence at exactly the deployment shape
  where it had no argument behind it: with one dynamic edge the online half halts
  on the first regression too. The divergence must be recorded in
  `crates/tf_tree_ingest/src/ingest.rs`'s module doc, which currently opens its
  per-edge argument with *"The online bridge watches one clock, because it *is*
  one publisher"* — a sentence this record makes false.
- **On a single-dynamic-edge deployment the bridge halts on the *first*
  past-threshold regression.** This is the one behaviour the floor adds, and it
  is worth an operator knowing: a lone publisher restarting, or replaying its own
  buffer more than 100 ms back, stops the bridge. It is correct per §5.5 — with
  one publisher a backward jump past the threshold cannot be anything else — and
  it is exactly the pre-`0011` behaviour on that shape, but it means a rig with a
  single dynamic publisher gets no tolerance for a node restart and should run
  `--on-clock-reset=recreate` if its workflow is a looping bag.
- **The floor is an upper bound on possible publishers, not a count of them, so
  it closes the unreachability only for the one-edge shape.** A deployment
  declaring two dynamic edges that are both owned by one node can supply one
  witness while the floor lets two be demanded, and a genuine reset there is
  never promoted — `dropped_non_monotonic` climbs and `clock_resets` stays 0, the
  same silent shape correction B removed one step down. Deriving the floor from
  the *live* publisher count was rejected: a publisher that has not spoken yet
  cannot be counted, so the demand would vary with DDS discovery timing, which is
  the defect §5.4's startup window exists to keep out of judgments. Declared
  dynamic edges is a bound that is known at construction and cannot move.
- **Two publishers that hiccup independently within 4096 transforms are reported
  as a clock reset.** This is the quorum's false-positive mode. It replaces a
  false-positive mode that fires on every correctly configured robot with one
  that requires a genuine coincidence of two faults, and the diagnostic names the
  correlated count so an operator can see what was concluded.
- **The online guard's threshold is still not configurable**, unlike the offline
  half's `--clock-reset-threshold`. Out of scope here; per-edge scoping makes the
  100 ms default defensible for the first time (it now bounds one publisher's own
  stamp regression rather than inter-publisher interleaving), but the `clock.rs`
  doc paragraphs that justify it by interleaving become arguments about something
  the code no longer does and are rewritten in step 2.

### Comments and docs this makes false

A comment that contradicts its code is worse than no comment. Six become false
and are fixed by the steps that break them — the draft listed two:

| Location | What it says |
|---|---|
| `crates/tf_tree_c/src/bridge.rs` (`TFT_BRIDGE_REJECTED` arm) | "dominated by the global clock guard … `newest` is the maximum over every accepted sample" |
| `crates/tf_tree_c/src/bridge.rs` (`rejected_by_arena` doc) | "a per-edge stamp the global clock guard could not see" |
| `crates/tf_tree_c/include/tf_tree_unstable.h` | the verbatim mirror of the line above — **generated**, must be regenerated |
| `crates/tf_tree_cli/tests/topology.rs` | "§5.5's clock guard is global by design (it tracks the *bridge's* notion of now)" |
| `crates/tf_tree_ingest/src/ingest.rs` (module doc) | "The online bridge watches one clock, because it *is* one publisher" |
| `docs/PHASE4.md` §"What the C bridge seam turned up" | "the global clock guard's high-water mark dominates every per-edge stamp" |

The first one's **conclusion survives and its argument gets stronger**:
`PushError::NonMonotonicStamp` stays unreachable because a per-edge guard's
`newest` *is* that edge's last accepted stamp and dominates that edge's ring
exactly. It must be rewritten to that simpler argument, not deleted — its
unreachability is the stated reason that arm has no test.

`crates/tf_tree_c/src/bridge.rs`'s `BridgeInner::stopped` doc and `PHASE4.md`
§3's mirror of it name `ClockGuard::accept_reset` as the `Recreate` mechanism;
after step 2 that path rewinds every edge's guard with `ClockGuard::forget()` and
clears the quorum instead.

### Tests this breaks — **superseded by correction B**

The draft reasoned: **every fixture in the workspace that exercises a clock halt
declares exactly one dynamic edge**, so a two-edge quorum is unreachable in all
of them, so all of them must be re-fixtured with a second regressing publisher.
The tests named were:

| Test | Real subject |
|---|---|
| `tf_tree_bridge::ingest::tests::a_strict_halt_leaves_the_ledger_balanced` (2nd half) | `balanced()` on a halting path |
| `tf_tree_c/tests/bridge.rs::a_clock_reset_under_recreate_latches_and_keeps_its_own_action` | the latch keeps its own action |
| `tf_tree_c/tests/bridge.rs::a_stop_is_announced_once_and_every_replay_after_it_is_rate_limited` | `first_time` on exactly the transition |
| `ros/tf_tree_ros/test/test_ingest.cpp::a_clock_reset_is_announced_once_and_not_once_per_refused_transform` | §5.4's "loud, rate-limited" on the HALT arm |

**With the corroboration floor, none of the clock fixtures needs re-fixturing.**
Their topologies declare one dynamic edge, so their quorum is floored to one and
the first regression halts, which is what each already asserts. They keep their
single-edge `TOPO`/`kTopology`, and the C and ROS fixtures are unchanged by (1).
The list is kept because the reasoning that produced it is the argument for the
floor — see *Rationale* → *Two corrections adversarial review forced*, **B** —
not because the work is outstanding.

Two do move, and for (2)'s reasons rather than (1)'s:
`authority.rs::strict_reports_a_conflict_as_fatal` (its counter assertions
change; "Fatal does not mutate ownership" still holds), and the strict-halt
ledger test, whose subject is now the window close rather than a clock halt and
which is superseded by the window tests step 5 adds.

And one that **stops testing without failing**:
`a_zero_stamped_static_does_not_reset_the_clock` names the mutant "run statics
through `clock.observe`". Under per-edge guards that mutant poisons only
`base -> lidar`, which receives no dynamic samples, so the test passes on mutated
code. The rule it guards gets *more* load-bearing under a quorum — two
zero-stamped statics on two static edges would meet it — so the test is re-aimed
at that, not deleted.

### Gates that a green `just test` does not cover

`crates/tf_tree_c/tests/bridge.rs` is behind a default-off `bridge` feature and
`ros/tf_tree_ros/` is outside the cargo workspace. `just test`'s dedicated
`-p tf_tree_c --features bridge` line covers the former; **only `just ros-test`,
in `docker/tf2`, covers the latter.** Per `MEMORY.md`, CI has not run since
2026-07-23 — gate locally.

## Implementation plan

Steps 2–4 are (1); 5–7 are (2); 8 is (3). Each is one PR.

1. **This record and `docs/decisions/README.md`.** — verified by the record
   existing at `ready` with no open questions, and by the README row.
2. **(1) in `tf_tree_bridge`.** `clocks: ByEdge<ClockGuard>` via
   `edgemap::{lookup_mut, insert}`, the `on_clock_reset` field, `ResetQuorum`
   with `Regression { onset, last, owner }`, `QUORUM_EDGES = 2`,
   `DEFAULT_CORRELATION_WINDOW = 4096`, `MAX_TRACKED_EDGES = 1024`,
   `ingest::owner_key`, the `Authority::distinct_owners()` corroboration floor, `Recreate` →
   `ClockGuard::forget()` per edge plus `ResetQuorum::clear()`,
   `HaltReason::ClockReset { by_nanos, correlated_edges }`, `clock_resets`
   counting promotions only. The owner and the floor are corrections A and B,
   which arrived after this plan was written; each needs its own test naming the
   mutant it kills — counting edges rather than owners, and dropping the floor —
   because those two mutants are what the reviewed implementation shipped. Add
   `map -> odom` to the module's `TOPO` and fix
   `a_tf_prefix_rewrites_the_declared_edges_not_only_the_wire`'s exact edge list.
   Rewrite the `clock.rs` doc paragraphs that justify the 100 ms constant by
   inter-publisher interleaving, and `a_rejected_publisher_cannot_move_the_clock`'s
   prose ("the clock" → "that edge's clock"; its assertions are unchanged).
   Re-aim `a_zero_stamped_static_does_not_reset_the_clock` at two static edges.
   Two new tests, modelled on `crates/tf_tree_ingest/tests/ingest.rs`'s
   `two_publishers_with_different_latencies_ingest_at_the_defaults` and
   `a_bag_loop_still_halts_with_a_per_edge_guard`, using the same 200 ms skew and
   the same rationale: two publishers a `transform_tolerance` apart do **not**
   halt, and a loop that regresses both edges still does. Plus one for the new
   boundary: a lone edge regressing past the threshold is a `Drop`, not a `Halt`.
   — verified by `just test`, `just lint`, and `tests/steady_state_alloc.rs`
   holding at `DECLARED_BUDGET = 2` unchanged.
3. **(1) across the C seam.** The `Action::Halt` arm compiles against the new
   field; `detail` reports the correlated count — note it must be written *after*
   the existing unconditional `set(&mut inner.strings.detail, …)` at the end of
   that arm, which would otherwise clobber it. Rewrite the two false comments in
   `bridge.rs` and regenerate the header. The C tests' single-dynamic-edge
   `TOPO` stays as it is: correction B floors their quorum to one, so their
   existing first-regression assertions are right. — verified by `just test`
   (its `-p tf_tree_c --features bridge` line), `just c-header-check`,
   `just c-abi-check`.
4. **(1) in ROS, the CLI and the docs.** `test_ingest.cpp`'s `kTopology` keeps
   its single dynamic edge, for the same reason step 3's does. In
   `crates/tf_tree_cli/tests/topology.rs`, rewrite the "global by design" comment
   and tighten the now-dead `NonMonotonic` tolerance arm into an assertion —
   `published == 1066` — as positive evidence that the change did not start
   dropping. Amend `crates/tf_tree_ingest/src/ingest.rs`'s module doc to record
   the deliberate promotion asymmetry. Amend `docs/PHASE4.md` §5.5 with the
   per-edge scope, the publisher quorum and the corroboration floor (§5.5 says
   only "on a detected backward jump beyond a threshold, the bridge stops and
   reports" and never fixes the scope, so the scope is a clarification; the
   quorum is a qualification of "stops", which is why it is marked as an
   amendment), amend §5.4's `Strict` row with the accumulate-then-halt-once
   semantics, and fix §3's stale dominance sentence.
   — verified by `just ros-test` and `just test`.
5. **(2) in `tf_tree_bridge`.** The startup window,
   `STARTUP_WINDOW_TRANSFORMS = 4096`, `close_startup_window()`, the boundary
   check before `transforms += 1`, `Authority`'s `Strict` arm doing the
   `reported`/`dropped` bookkeeping, `StaticStore::conflicts_by_edge()`,
   `HaltReason::StartupConflicts { authority, statics }`. Delete the twelve-line
   comment at the `StaticVerdict::Conflict` arm that names this record as
   unresolved. Update `strict_reports_a_conflict_as_fatal` and the first half of
   `a_strict_halt_leaves_the_ledger_balanced`. New tests: a `Strict` conflict
   inside the window does not halt and *is* counted; the close names **both** a
   static and an authority conflict in one halt; the backstop closes the window
   without an explicit call; a conflict first seen after the close does not halt.
   `a_urdf_that_disagrees_with_the_declared_constant_is_reported_with_both_values`
   must pass **verbatim** — it is the guard that accumulation is additive.
   — verified by `just test`, `just lint`, and `balanced()` in every new test.
6. **(2) across the C seam.** `TFT_BRIDGE_REASON_STARTUP_CONFLICTS = 9` — and it
   **must** be appended to `UNSTABLE` in `xtask/src/headers.rs`, because the
   stable tier's cbindgen config is exclude-by-complement and `check_partition`
   inspects only `extern "C" fn` names: omitting it silently emits the constant
   into the **frozen** `tf_tree.h`, which §3.1 treats as a promise that cannot be
   withdrawn. New `tft_bridge_close_startup_window(b, out)`, latching `stopped`
   the way the `Halt` arm does. The halt's `detail` enumerates every recorded
   edge. Rewrite `a_halted_bridge_refuses_every_later_offer` to close the window
   and then assert the halt, keeping its coverage of `stopped` and
   `refused_after_halt`. — verified by `just c-header-check` (which fails on
   drift), `just test`, `just c-abi-check`, and by `grep TFT_BRIDGE_REASON_STARTUP
   crates/tf_tree_c/include/tf_tree.h` finding **nothing**.
7. **(2) in ROS.** A `startup_window_sec` parameter (default 5.0) and a one-shot
   `RCL_STEADY_TIME` timer calling the new entry point, routing its outcome
   through `report()`. Not a `tft_bridge_options` field — that would change
   `sizeof`. A gtest that two conflicting publishers under `Strict` produce one
   `RCLCPP_FATAL` naming both. — verified by `just ros-test`.
8. **(3) in ROS.** Split the `TFT_BRIDGE_DROPPED` tail into three
   `RCLCPP_WARN_THROTTLE` call sites, one per reason, and add a `reason_name()`.
   Preserve `BAD_POSE`'s existing `detail`. — verified by `just ros-test`.
9. **Full gate:** `just lint`, `just test`, `just c-abi-check`,
   `just c-header-check`, `just tf2-check`, `just ros-test`.

## Open questions

None.
