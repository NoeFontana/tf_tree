# 0012: The authoritative clock-jump signal, common-mode inference, and the degradation ladder

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** (filled in as work lands)

**Supersedes the clock half of
[`0011`](./0011-the-bridge-clock-guard-and-the-static-conflict-disposition.md)** —
its §*Decision* 1 (the per-edge guard's promotion rule, `ResetQuorum`,
`QUORUM_EDGES`, `DEFAULT_CORRELATION_WINDOW`, the `Authority::distinct_owners()`
floor) and the parts of its *Rationale*, *Consequences* and *Implementation plan*
that serve it. **`0011`'s other two decisions stand unchanged and are not
revisited here:** its §*Decision* 2 (the `AuthorityPolicy::Strict` startup
window, `close_startup_window()`, `HaltReason::StartupConflicts`) and its
§*Decision* 3 (`Action::Drop` keeps its shape; the `rclcpp` side throttles).
`0011`'s per-edge `ClockGuard` also survives — it was the correct half of its
clock decision, and this record keeps it verbatim.

## Context

The online bridge infers "`/clock` was reset" from the per-message `/tf` stamps it
already receives. §5.5 asks for that in one sentence; the original implementation
did it one way, `0011` replaced that with a second, and `0011`'s own adversarial
review replaced *that* with a third. **All three were wrong.** Each was caught by
a concrete reproduction rather than by argument, and each failure was of a
different kind, which is what makes the pattern worth a record instead of a fourth
patch.

### The three rules, and what killed each

**Rule 1 — one global `ClockGuard` for the whole stream.** The pre-`0011`
implementation: a single high-water mark over the merged `/tf` stream, `Reset` at
100 ms behind it.

*Reproduction:* AMCL and `robot_localization` date `map -> odom` by their
`transform_tolerance` — 0.1 s to 1.0 s **into the future** — while the wheel
driver stamps `odom -> base_link` at publish time. The lagging edge's every
message is a past-threshold regression off the leading edge's mark, so a
correctly configured robot latches a permanent halt at boot. This is `0011`'s
own finding, and it is pinned today by
`ingest::tests::two_publishers_a_transform_tolerance_apart_never_halt`
(200 ms skew, 110 samples, `dropped_non_monotonic == 0`).

*What was wrong:* the observation "a stamp is behind a mark" conflates two
publishers' relative offset with one clock's motion, and no threshold separates
them, because `transform_tolerance` is a user parameter with no ceiling and
therefore ranges over exactly the magnitudes a reset does.

**Rule 2 — a guard per edge, promoted by a quorum over distinct *edges*.**
`0011`'s shipped form: two distinct edges regressing inside a correlation window
of 4096 transforms is the clock.

*Reproduction:* **one node owning two dynamic edges.** A localization node that
publishes both `map -> odom` and `odom -> base_link` restarts; both of its edges
regress in the same instant; two *edges* form a quorum; the bridge halts on
precisely the single-publisher event the quorum exists to tolerate. The
false-halt mode was not removed, it was moved from "any robot with a lagging
estimator" to "any robot whose estimator owns more than one edge".

*What was wrong:* every argument for the rule was about publishers — "separate
publishers do not restart in lockstep" — and edges were substituted for
publishers without the substitution being checked.

**Rule 3 — a quorum over distinct publishers, floored by
`Authority::distinct_owners()`.** `0011`'s corrections A and B: count owners, and
never demand more corroboration than the deployment can supply, so
`needed = QUORUM_EDGES.min(corroborators.max(1))`.

Two reproductions, and the second is the serious one.

*(a) The boot race.* `Authority::distinct_owners()` counts publishers that have
**already established ownership of an edge**, which at boot is not the same as
the publishers that exist. AMCL does not publish `map -> odom` until it has a
map; `robot_localization` does not publish until it has its first odometry
message. So for the first seconds of every run the wheel driver is the only
owner, `distinct_owners()` is 1, the floor is 1, and its first past-threshold
regression — a buffer replay, a hiccup, a node restart — **latches a permanent
halt**. The bridge is most fragile exactly during the interval `0011`'s own §5.4
startup window exists because discovery timing must not decide judgments.

*(b) Attribution became a correctness dependency.* `Publisher::UnknownGid` and
`Publisher::Unattributed` are **unit** variants. `Authority::distinct_owners()`
compared `Publisher` values, so on an RMW that does not report endpoint GIDs
every publisher in the deployment compares **equal**, `distinct_owners()` is
permanently 1, the floor is permanently 1, and **every** single-edge regression
halts the bridge. §5.3 says, in the sentence the whole attribution design rests
on:

> attribution is diagnostic value, never a correctness dependency.

Rule 3 made the halt/no-halt decision a function of whether the middleware
happened to expose GIDs. That is not a degradation, it is the forbidden coupling,
and it fires on the RMWs least able to diagnose it.

Worse, `0011` had *already reasoned about* the sentinel collapse and concluded it
was the safe direction: "as far as a quorum is concerned an unattributed
publisher is one identity, which can only make a quorum harder to reach". That
was true under a **fixed** demand of two. Correction B made the demand a function
of the same count, which inverted the sign: collapsing the identities no longer
raised the bar, it lowered the bar to one. **Two changes each safe in isolation
composed into the defect.** No test caught it because every fixture in the
workspace attributes its publishers.

### The root cause

All three rules are instances of one mistake:

> **Inferring a property of the time source from observations of the signal under
> suspicion, anchored on proxies that are not physical time.**

The signal under suspicion is publisher stamps. The property being inferred is
"the clock jumped". The proxies were the transform ordinal (`0011`'s "the
bridge's clock is `stats.transforms`") and a publisher count derived from the
authority table — neither of which is time, and both of which are moved by the
very traffic being judged. A stalled publisher moves the transform ordinal; a
publisher that has not booted yet moves the corroborator count; an RMW without
introspection moves it again. Each rule tightened the inference against the
previous reproduction and left the anchor in place, which is why the third
failure was worse than the first.

The fix is not a fourth inference rule. **ROS 2 publishes clock jumps**, and this
record's first move is to stop guessing at a thing the platform reports.

## Decision

Four layers, L0–L3. L1 is the authoritative path; L2 is inference kept only as a
fallback and reworked so that it rejects the failure modes above by construction;
L3 is the ladder that makes attribution quality change *diagnosis* quality and
never correctness.

### The five principles

These are the criteria every part of the design below answers to, and they are
normative for any later change to this area.

| | Principle |
|---|---|
| **P1** | Prefer the authoritative signal to inference. |
| **P2** | A detector's reference clock must be **independent** of the clock under test. |
| **P3** | Windows are **physical time**, never event counts. |
| **P4** | Time is **injected**, never read ambiently inside `tf_tree_bridge`. |
| **P5** | A diagnostic may never become a correctness dependency (§5.3). |

*Rationale* names the prior art behind each.

### L0 — inject a steady receipt clock

`tf_tree_bridge` gains one newtype, in `clock.rs`, re-exported at the crate root:

```rust
/// A reading of a local **steady** (monotonic) clock, in nanoseconds.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct SteadyNanos(pub i64);
```

It is a **distinct type from a publisher's stamp** because confusing the two is
the entire bug class this record removes. It is never derived from `/clock` and
never from a publisher. `#[repr(transparent)]` so it costs nothing at the C
boundary. `SteadyNanos::UNKNOWN == SteadyNanos(0)` is the documented "no receipt
clock was supplied" sentinel and is what `Default` produces; a caller with no
steady clock passes it, and L2 is skipped for that sample.

`Sample` gains `pub received: SteadyNanos`. Online it is
`rclcpp::Clock(RCL_STEADY_TIME).now()` read **once per message** at callback
entry, not once per transform; offline it is the recording's log time.
`Sample::identity` keeps its three-argument shape and leaves `received` at
`UNKNOWN`; a `#[must_use] Sample::received_at(self, SteadyNanos) -> Sample`
builder sets it, which is what keeps ~50 existing construction sites compiling
untouched.

**No arithmetic may mix `SteadyNanos` with `stamp_nanos` except the single
documented offset subtraction in L2.**

### L1 — the authoritative path

```rust
pub enum JumpKind { ClockTypeChanged, Backward, Forward }

impl Ingest {
    pub fn note_time_jump(&mut self, delta_nanos: i64, kind: JumpKind) -> Action;
}
```

No threshold, no inference, no quorum, no corroboration. It applies
`OnClockReset` directly — `Action::Halt { HaltReason::ClockReset { .. } }` or
`Action::RecreateArena { .. }` — and resets the per-publisher offset state.

`delta_nanos` follows `rcl_time_jump_t::delta`: **new time minus the last time
before the jump, so a rewind is NEGATIVE.** This is a sign flip against `0011`'s
`by_nanos`, and it is deliberate: the authoritative source defines the
convention, and a design in which the same field means "how far back" on one path
and "signed delta" on the other is how a forward jump comes out printed as a
backward one.

Like `close_startup_window()`, `note_time_jump` **charges no ledger bucket** — it
has no transform in hand, so `BridgeStats::balanced()` is untouched. It does
increment `clock_resets`, which is not a term in `balanced()`.

This is the case §5.5 was actually written for — a bag loop, a sim reset — handled
exactly, at its source, once.

### L2 — inference as common-mode rejection

The fallback, for non-ROS C callers, for system-clock steps, and as defence in
depth. It is not the primary detector and must not be read as one.

Per **publisher**, keyed by `ingest::owner_key`, track

```text
offset = sample.stamp_nanos - sample.received.0
```

**A publisher's `transform_tolerance` *is* this offset.** Measured and
subtracted, it stops looking like a jump — which dissolves rule 1's defect rather
than working around it.

- Maintain a smoothed baseline per publisher: an integer EWMA with
  **alpha = 1/8** (`BASELINE_DIVISOR = 8`). Integer-only so it is deterministic
  on every target. Its steady-state lag under a per-sample drift `d` is
  `(1-α)/α · d = 7d`, so the pathological 1 ms-per-sample case sits 7 ms behind a
  100 ms threshold; it absorbs 63 % of a jitter excursion in 8 samples and 95 %
  in 24, which at 100 Hz is 80 ms and 240 ms — inside the 1 s correlation window.
- A **step** is `|offset - baseline| > reset_threshold_nanos`. On a step the
  baseline **snaps** to the new offset rather than smoothing toward it, so a
  permanently broken publisher costs exactly one step per bout instead of one per
  message.
- **Common mode:** at least **two distinct publishers** stepped within
  `correlation_window_nanos` **of each other in receipt time** (`SteadyNanos`,
  never stamps, never a transform count), **and** their step deltas agree:

  ```text
  |d_a - d_b| <= max(common_mode_tolerance_floor_nanos,
                     ratio * max(|d_a|, |d_b|))
  ```

Agreement is the strong evidence. A real `/clock` step moves everyone by the
**same amount**; independent restarts do not. This also detects **forward**
jumps, which a backward-regression watcher structurally cannot.

The table is a `BTreeMap<String, Offset>` probed with `get_mut(&str)` — never
`entry()` with an owned key — so the steady state and the refusal path both
allocate nothing after a publisher's first sample. It is capped at
`MAX_TRACKED_PUBLISHERS = 64`, the same class of externally-chosen key that
forced caps on `NameNormalizer::seen` and `Ingest::undeclared`; refusing a row
can only make a halt **harder** to reach, never easier.

### L3 — the degradation ladder

**This is the layer that kills the bug class.**

| Evidence | Disposition |
|---|---|
| authoritative jump signal (L1) | `Halt` / `RecreateArena` — exact |
| common-mode step, ≥ 2 agreeing publishers (L2) | `Halt` / `RecreateArena` |
| single-source regression, at **any** magnitude | `Drop`, count, diagnose. **NEVER HALT.** |

Because the bridge never halts on one witness **there is no floor**, and
therefore nothing about the floor to get wrong. Attribution quality now changes
**diagnosis** quality and never correctness, which is P5 satisfied by
construction rather than by care: on an RMW with no endpoint introspection every
publisher collapses to one sentinel, one offset row is tracked, common mode can
never reach two, and every regression degrades to `Drop`. The bridge stays
correct and says less.

Phase 1 rejects a non-monotonic stamp on its own account, so **the arena is
protected regardless of what this layer concludes.** The single-source path was
never load-bearing for arena integrity; it was only ever load-bearing for whether
the bridge kept running.

Concretely, in `Ingest::offer`, `ClockVerdict::Jitter` and `ClockVerdict::Reset`
collapse into **one** arm: drop, charge `dropped_non_monotonic`, diagnose.
Promotion happens only through `apply_clock_reset`, reached from the common-mode
arm or from `note_time_jump`.

### Configuration

Every knob physical, every default with a stated derivation.

```rust
pub struct ClockPolicy {
    pub reset_threshold_nanos: i64,              // 100_000_000
    pub correlation_window_nanos: i64,           // 1_000_000_000
    pub common_mode_tolerance_ratio: f64,        // 0.25
    pub common_mode_tolerance_floor_nanos: i64,  // 50_000_000
    pub on_reset: OnClockReset,
}
```

- `reset_threshold_nanos` **cites `DEFAULT_RESET_THRESHOLD_NANOS`** rather than
  restating 100 ms — the offline half (`tf_tree_ingest::IngestOptions::default`)
  already consumes that constant and the two must not drift.
- `correlation_window_nanos = 1 s`: an order of magnitude above a 100 Hz `/tf`
  publish period, so every publisher gets several messages inside it, and an order
  of magnitude below the interval over which two independent nodes might restart
  by coincidence.
- `common_mode_tolerance_ratio = 0.25` and
  `common_mode_tolerance_floor_nanos = 50 ms`: the ratio covers a large step where
  the two publishers' last pre-jump messages were up to a publish period apart;
  the floor covers a small step where a ratio of a small number is smaller than
  the jitter it must tolerate.

`Ingest::with(..)` keeps its existing signature and delegates to a new
`Ingest::with_policies(config, authority, ClockPolicy, tf_prefix)`, so no
downstream caller churns to get the defaults.

### The `HaltReason` payload

```rust
HaltReason::ClockReset { delta_nanos: i64, evidence: ClockEvidence }
Action::RecreateArena { delta_nanos: i64 }

pub enum ClockEvidence {
    Reported { kind: JumpKind },
    CommonMode { publishers: u32 },
}
```

`ClockEvidence` is `Copy`. It replaces `correlated_edges: u32`, which named the
wrong unit and cannot describe an authoritative jump at all — under L1 there is
no count, there is a report. The seam gets a better sentence out of it than a
bare number.

### Deleted

`ResetQuorum`, `QuorumVerdict`, `QUORUM_EDGES`, `DEFAULT_CORRELATION_WINDOW` (the
observation-count window), `MAX_TRACKED_EDGES`, `Regression`,
`Authority::distinct_owners()`, and `ingest::owner_key`'s role as a quorum key
(the function survives as L2's per-publisher key).

**`DEFAULT_RESET_THRESHOLD_NANOS` is not deleted** — the offline half consumes
it.

### Kept

**The per-edge `ClockGuard` stays, verbatim.** It still makes the per-edge
monotonicity **drop** decision that Phase 1 invariant 6 requires; it simply no
longer promotes to a halt on its own. `0011`'s per-edge scoping was the correct
half of its clock decision and is not relitigated.

**`0011`'s §*Decision* 2 (the startup window, `AuthorityPolicy::Strict`
accumulate-then-halt-once) and §*Decision* 3 (`Action::Drop` keeps its shape) are
untouched.** They are orthogonal to the clock and are not revisited by this
record. Note only that `0011`'s justification for the startup window's *unit* —
"the crate has no clock at all" — is false once `SteadyNanos` exists; the startup
window keeps its transform ordinal by **choice**, not by necessity, and the two
windows no longer share a rationale.

## Rationale

### The five principles and the prior art behind each

Every one of these is a rule some other system learned the same way this one did.

**P1 — prefer the authoritative signal to inference. Prior art: `rcl` jump
callbacks.** ROS 2 already reports clock discontinuities as a first-class event.
Verified in `docker/tf2` against ROS 2 *lyrical*: `rcl/time.h` declares
`rcl_jump_threshold_t { bool on_clock_change; rcl_duration_t min_forward;
rcl_duration_t min_backward; }`, `rcl_time_jump_t { rcl_clock_change_t
clock_change; rcl_duration_t delta; }` and `rcl_clock_add_jump_callback(..)`;
`rclcpp/clock.hpp` wraps them as `Clock::create_jump_callback(pre, post,
threshold) -> JumpHandler::SharedPtr`. The platform hands us the delta, the sign
and whether the clock type changed. Three rules were built to guess a number that
was being published. Inference is what you do when there is no report — which is
exactly the scope L2 is now confined to.

**P2 — a detector's reference clock must be independent of the clock under test.
Prior art: the Linux kernel's clocksource watchdog.** `kernel/time/clocksource.c`
does not ask a clocksource whether it is behaving. It compares the candidate
(e.g. the TSC) against an *independent* watchdog clocksource (HPET, `acpi_pm`)
over a **fixed wall interval** — `WATCHDOG_INTERVAL`, half a second — and marks
the candidate unstable when the two diverge by more than `WATCHDOG_THRESHOLD`,
62.5 ms. Rules 1–3 validated `/clock` against quantities `/clock` moves. A steady
receipt clock is the watchdog: `RCL_STEADY_TIME` is monotonic and, verified in
the same image, **is not affected by `use_sim_time`**.

**P3 — windows are physical time, never event counts. Prior art: NTP's step
thresholds.** `ntpd` steps rather than slews at an offset of **128 ms** and panics
at **1000 s** — both durations, chosen against how fast physical clocks actually
drift, not against how many packets arrived. PTP servos draw the same
step-versus-slew line at a `step_threshold` expressed in seconds. `0011` argued
that a transform count "auto-scales with the stream rate", and for the question it
was asking — "have the *other* edges' next messages arrived yet?" — that argument
is not silly. But it makes the window's meaning a function of the traffic under
suspicion: a publisher that stalls slows the window, a publisher that floods
shortens it, and a stuck publisher can hold a window open for the life of the
process. `correlation_window_nanos` is measured in `SteadyNanos` and no publisher
can move it.

**P4 — time is injected, never read ambiently. Prior art: `rcl`'s own API
shape.** There is no ambient `now()` in `rcl`; every time call takes an
`rcl_clock_t *`, and `rclcpp`'s throttle macros take an `rclcpp::Clock &`. The
caller chooses the clock, so the caller can be wrong *visibly* rather than by
accident. `0011`'s `clock.rs` rejected `Instant::now()` on the grounds that it
would be "a clock read on a path that runs once per transform at 1 kHz"; that
objection is answered precisely, not overruled — the read is taken **once per
message** by the caller who already owns a clock, and `tf_tree_bridge` reads no
clock at all. Injection also keeps every test deterministic and keeps the crate
`no_std`-shaped at its seams.

There is a second, harder reason, specific to `rclcpp`: the jump callback **does
not run on the ingest thread.** `NodeOptions::use_clock_thread` defaults to
`true`, so the post-callback fires on the `TimeSource`'s dedicated `/clock`
thread; and every `tft_bridge_*` entry point is thread-affine — a debug build of
`tf_tree_c` calls `std::process::abort()`, taking the whole ROS process with it.
A design that read the clock wherever it needed it would have discovered that at a
user's site, because `ros/build.sh` builds `--release`, where the same mistake
only returns `TFT_ERR_WRONG_THREAD`. Injection makes the hand-off explicit and
therefore reviewable.

**P5 — a diagnostic may never become a correctness dependency. Prior art: GNSS
common-mode receiver-clock jump detection, and §5.3 itself.** A GNSS receiver
distinguishes its own millisecond clock jump from a per-satellite cycle slip by
whether the residual is **common to every channel and equal in size**. The
important property for us is how that detector *degrades*: a receiver tracking
fewer satellites loses **sensitivity** — it may fail to declare a jump — but it
never declares one that did not happen, and it never stops navigating because it
could not identify a satellite. Rule 3 had the opposite degradation: worse
attribution produced *more* halts. L3's ladder restores the GNSS shape. Fewer
identifiable publishers means less detection, never a wrong stop.

### Why common-mode *agreement* is stronger evidence than coincidence

`0011`'s quorum asked only "did two things regress near each other?". That is
coincidence, and two publishers restarting within a couple of seconds of one
another — a supervisor respawning a crashed pair, a `ros2 launch` restart, a
network blip disconnecting two nodes at once — is a coincidence that a real
deployment produces.

Agreement asks a strictly stronger question: **did they move by the same
amount?** A `/clock` step is a change to a shared reference, so every consumer's
offset from steady time changes by exactly the step, to within one publish period
of quantization. Two independent restarts have no mechanism that would make their
regressions equal — one node replays a 5 s buffer, another rewinds 200 ms — and
the probability that they agree to within `max(50 ms, 25 %)` is small and, more
importantly, **not raised by whatever caused them both to restart**. Coincidence
is correlated by common causes; magnitude agreement is not.

Two further properties fall out, and both are things the quorum could not do:

- **It detects forward jumps.** A backward-regression watcher is structurally
  blind to `/clock` jumping ahead — every stamp is monotone, every sample is
  accepted, and a sim that skips forward silently produces an arena full of
  transforms that never happened at those times. A step in the *offset* has a
  sign, and both signs are steps.
- **It is scale-free in the right way.** The tolerance is
  `max(floor, ratio · max(|d_a|, |d_b|))`, so a 5 s bag loop is allowed 1.25 s of
  disagreement (one slow publisher's publish period, easily) and a 200 ms step is
  allowed the 50 ms floor. A fixed tolerance would either reject real large steps
  or accept unrelated small ones.

### Why the offset, and not the stamp, is the quantity to watch

The offset `stamp - received` is the only quantity in the system that is
*constant* for a healthy publisher and *steps* for an unhealthy clock. A
publisher's `transform_tolerance` makes the offset a large constant — 1.0 s, say
— and rule 1 read that constant as a jump. Under L2 it is the baseline, and it is
subtracted. A SLAM node's keyframe latency is a different large constant on a
different publisher, and it is subtracted too. **The defect that started all of
this is not tolerated by L2, it is arithmetically removed**, and that is the test
of whether a redesign has understood its predecessor's failure.

### Alternatives rejected

- **A fourth inference rule** — a better quorum, a smarter floor, a hysteresis on
  the corroborator count. Rejected: the three failures were not three tuning
  errors, they were three instances of the same category error, and a fourth rule
  anchored on the same proxies fails a fourth way. The category error is fixed by
  P1 and P2 or it is not fixed.
- **Subscribe to `/clock` and watch it directly.** This is `0011`'s
  §*Deliberately not now*, and it is superseded rather than deferred: `rcl`'s jump
  callbacks are the *same evidence*, already computed, with the threshold
  semantics, the clock-type-change signal and the sign convention supplied by the
  platform. Adding a `rosgraph_msgs` dependency and a subscription to rederive
  `delta` from consecutive `/clock` messages would be strictly more code for
  strictly less information.
- **Halt on a single witness when the deployment provably has only one
  publisher.** This is rule 3's floor, restated as a special case. Rejected:
  "provably" is exactly what the boot race and the unit-variant collapse
  falsified — a bridge cannot know how many publishers a deployment has, only how
  many it has *seen*, and that count is moved by discovery timing and by RMW
  capability. L3 declines to depend on it at all. The cost is stated under *Known
  limitations*.
- **A second `BridgeStats` bucket for clock refusals.** Rejected: it is a
  `struct_size`-versioned growth of `tft_bridge_stats` and a new term in
  `balanced()`, moving assertions across three products. `dropped_non_monotonic`
  widens in meaning instead, and its doc says so.
- **Make `received` default to `stamp_nanos` for a C caller that predates the
  field.** Rejected, and it is worth writing down because it looks harmless: it
  forces `offset ≡ 0` for every publisher, which silently re-enables L2 on raw
  stamps and reintroduces rule 1's defect for exactly the callers who cannot see
  the fix. It also stores a publisher-derived value in a type whose doc says it is
  never publisher-derived. `SteadyNanos::UNKNOWN` is the honest degradation, and
  L3 makes it safe.

### Two things the implementation found that the design did not predict

Recorded because they are true of the shipped code and a reader will otherwise
rediscover them.

**The per-edge guard's threshold is now unobservable through `Ingest`.** Under
the ladder, `ClockVerdict::Jitter` and `ClockVerdict::Reset` have identical
disposition and charge the identical counter, so replacing
`ClockGuard::with_threshold(policy, reset_threshold_nanos)` with
`ClockGuard::new(policy)` changes no observable behaviour of `offer`. Verified by
applying the mutant: it passes. The guard is still fed from
`ClockPolicy::reset_threshold_nanos` so the two rules cannot be configured apart,
but `reset_threshold_nanos` is now observable **only** through the step detector.
The doc says this rather than claiming a kill it does not have.

**`note_time_jump` resets the offset table under `Recreate` only, not under
`Halt`.** The brief for this work said the authoritative path resets "all
per-edge guards + offset state". Under `Halt` it must not touch the per-edge
high-water marks: `Ingest` holds no latch of its own — the C seam and the
`rclcpp` node hold it — so forgetting every mark would make the very next
post-rewind sample read as forward motion and come back `Action::Publish`,
writing into an arena the bridge has just been told to stop using. Keeping the
marks means every later sample keeps being refused with or without a latch above.
The argument is written on `Ingest::apply_clock_reset`.

## Consequences

### Committed to

- **The bridge never halts on one witness.** Any future promotion of a per-edge
  fact to a global judgment either carries an authoritative signal or requires two
  independent, *agreeing* sources. A rule that can halt on one is a regression to
  rule 3.
- **`tf_tree_bridge` reads no clock.** `SteadyNanos` arrives on `Sample` and
  through `note_time_jump`; adding `std::time` to this crate is a deliberate act
  needing its own record. (`0011` committed to the same discipline for a different
  reason and this record keeps it, having changed the reason.)
- **Physical windows.** Every window in the clock detector is nanoseconds of a
  steady clock. The §5.4 startup window's transform ordinal is now a **choice**
  and must be justified as one if it is ever revisited.
- **`delta_nanos` is signed, `rcl`-convention: new time minus old, so a rewind is
  negative.** Every consumer — the C seam's `detail` string, the `rclcpp`
  `report()` arms — must agree, or one family of messages prints "went backwards
  by" a positive number and the other a negative one.
- **`dropped_non_monotonic` widens** to "transforms the clock rules refused",
  because a **forward** common-mode jump refuses a sample that is perfectly
  monotone and lands in that bucket. The field's doc states the widened meaning;
  the name is kept because renaming it crosses the C ABI for no diagnostic gain.
- **The single-source refusal path allocates nothing.** It runs indefinitely — a
  publisher replaying stale stamps never advances its own high-water mark, so it
  stays on that path for the life of the bridge. `tests/steady_state_alloc.rs`
  gained a third scenario at the same budget (2) to pin it; the deleted quorum
  cost **two heap allocations and up to three 1024-row scans per sample** on that
  path, and no test could see it because every scenario fed strictly increasing
  stamps.
- **`clock_resets` still counts promotions, not regressions**, and is still not a
  term in `balanced()`. `note_time_jump` charges no bucket.
- **Attribution is not a correctness dependency, and this is now structural.**
  No code path may make the halt decision a function of publisher identity.

### Known limitations, stated rather than deferred

- **The EWMA needs warm-up.** A publisher's first sample establishes its baseline
  and cannot step; the next few samples have a baseline dominated by whatever the
  first ones happened to be. A clock step in a publisher's first ~8 samples is
  therefore likely to be absorbed into the baseline instead of detected. This is
  the fallback layer, and the authoritative path (L1) has no warm-up at all, so
  the exposure is a non-ROS caller stepping its clock immediately at boot.
- **A publisher whose offset drifts fast can mask a small step.** The baseline
  chases the offset at α = 1/8, so a drift of `d` per sample is tracked with a
  steady-state lag of `7d`; a genuine step smaller than
  `reset_threshold_nanos - 7d` on top of that drift is absorbed rather than
  declared. At the pathological 1 ms per sample the margin is 93 ms of the 100 ms
  threshold, so this is narrow — but a publisher with an unsynchronized,
  fast-drifting clock is exactly the deployment where it bites, and raising α to
  react faster would trade this for tolerating less jitter.
- **Non-ROS C callers get only the inference layer.** `tft_bridge_note_time_jump`
  exists, but a caller that never calls it — a bag-replay harness, a non-ROS
  middleware shim — has L2 and L3 and nothing else. With a real receipt clock that
  is a good detector; with `SteadyNanos::UNKNOWN` it is no detector at all, and
  such a caller gets correct per-edge drops and a `clock_resets` that stays 0.
  That is the honest degradation and it is visible in the counters, but it is a
  degradation.
- **A C caller predating the `tft_bridge_sample` field gets `UNKNOWN`.** It keeps
  working — the struct grows by an appended field, its `struct_size` is accepted
  as a prefix — and it silently loses L2. That is the intended trade against the
  alternative of refusing it outright, which is what exact-equality validation does
  today.
- **Two publishers that genuinely restart at the same instant *and* by the same
  amount are reported as a clock reset.** This is L2's false-positive mode, and it
  is much narrower than the quorum's (which needed only coincidence). The realistic
  shape is a supervisor respawning two nodes that each replay an identically sized
  buffer. `ClockEvidence::CommonMode { publishers }` names what was concluded.
- **A forward common-mode jump refuses a monotone sample.** An operator reading
  `dropped_non_monotonic` on a forward jump sees a counter whose name does not
  describe what happened. The doc carries the widened meaning; the counter does
  not.
- **The offline half (`tf_tree_ingest`) is not changed by this record** and still
  halts on the **first** `ClockVerdict::Reset`. The asymmetry is deliberate and
  now larger than `0011` described: a bag is a finished artifact and stopping to
  say "edge *X* regresses at *t*" costs a rerun, whereas a false halt online takes
  down a running robot. `0011`'s module-doc narrative in
  `crates/tf_tree_ingest/src/ingest.rs` argues the opposite asymmetry and is false.

### Comments and docs this makes false

A comment that contradicts its code is a defect here. These are fixed by the steps
that break them.

| Location | What it says |
|---|---|
| `crates/tf_tree_ingest/src/ingest.rs` module doc (~39–73) | narrates the quorum design at length, and intra-doc-links `tf_tree_bridge::clock::ResetQuorum`, which **breaks rustdoc** |
| `crates/tf_tree_bridge/src/ingest.rs` (`HaltReason::ClockReset` doc) | intra-doc-links `crate::clock::QUORUM_EDGES` from a field that survives |
| `crates/tf_tree_c/src/bridge.rs` (`ClockReset` arm) | formats *"the clock went backwards on {n} edge(s) at once"* — wrong unit, wrong sign, and cannot describe a reported jump |
| `crates/tf_tree_c/tests/bridge.rs` (~700) | a ~25-line doc comment reasoning about `ResetQuorum::record`, "the edge that completed the quorum" and `correlated_edges` |
| `docs/PHASE4.md` §5.5 | `0011`'s amendment — superseded by this record's |
| `docs/PHASE4.md` §5.4 closing line | *"The unit is transforms because the crate has no clock at all"* — false once `SteadyNanos` exists |
| `docs/decisions/README.md` row for `0011` | describes the quorum as live |

### Tests that change meaning

Three `tf_tree_c` integration tests reach a `HALT`/`RECREATE` with **one
publisher on one edge**, and say so in their doc comments. L3 abolishes that path,
so these are not re-wordings:

| Test | What must change |
|---|---|
| `a_clock_reset_under_recreate_latches_and_keeps_its_own_action` | reach the reset through `tft_bridge_note_time_jump` — cleanest, and it exercises the new symbol |
| `a_stop_is_announced_once_and_every_replay_after_it_is_rate_limited` | same; the property under test (`first_time` on a latched stop) is orthogonal and must be preserved |
| `a_clock_reset_needs_a_second_publisher_and_reports_how_many_corroborated` | survives in spirit — both publishers regress by exactly 5 s, so they *agree* — but its assertions move from edges to publishers and its samples need receipt clocks, or L2 stays dormant |

`ros/tf_tree_ros/test/test_ingest.cpp`'s
`a_clock_reset_is_announced_once_and_not_once_per_refused_transform` has the same
single-publisher shape and needs the same treatment.

### Gates a green `just test` does not cover

`crates/tf_tree_c/tests/bridge.rs` is behind a default-off `bridge` feature and
`ros/tf_tree_ros/` is outside the cargo workspace. `just test`'s dedicated
`-p tf_tree_c --features bridge` line covers the former; **only `just ros-test`,
in `docker/tf2`, covers the latter**, and it also rebuilds
`tf_tree_c --features bridge`, so a change on the Rust side of the seam is
covered by it too. Per `MEMORY.md`, CI has not run since 2026-07-23 — gate
locally.

## Implementation plan

Each step lands as one PR.

1. **This record, `0011`'s status line, `docs/decisions/README.md`, and
   `docs/PHASE4.md` §5.3/§5.5.** — verified by this record existing at `ready`
   with no open questions; by `0011`'s status naming *which half* is superseded;
   by the README row; and by §5.5 no longer containing an unqualified "on a
   detected backward jump beyond a threshold, the bridge stops and reports".
2. **L0–L3 in `tf_tree_bridge`.** `SteadyNanos`, `Sample::received` +
   `received_at`, `ClockPolicy`, `JumpKind`, `ClockEvidence`, `OffsetTable`
   (α = 1/8, `MAX_TRACKED_PUBLISHERS = 64`), `Ingest::note_time_jump`,
   `Ingest::with_policies`, the collapsed drop arm, `apply_clock_reset`. Delete
   `ResetQuorum`, `QuorumVerdict`, `QUORUM_EDGES`, `DEFAULT_CORRELATION_WINDOW`,
   `MAX_TRACKED_EDGES`, `Regression`, `Authority::distinct_owners()`. Rewrite the
   `clock.rs` module-doc section that argues a window must not be measured in
   time. Every new test doc names the mutant it kills, and the mutant is
   **applied**, the real failure quoted, and the source restored — three claims
   written from prediction in this area turned out false and must not be shipped
   as doc comments. Add a third scenario to `tests/steady_state_alloc.rs`: a
   publisher stuck below its own high-water mark, 2000 measured offers, all
   `Action::Drop`, budget 2. — verified by `just test`, `just lint`, and the
   allocation budgets holding on all three scenarios.
3. **The C seam.** Append `int64_t received_steady_nanos` to `tft_bridge_sample`
   in **both** hand-maintained twins (`crates/tf_tree_c/include/tf_tree_unstable.h`
   and `crates/tf_tree_c/src/bridge.rs`; 88 → 96 bytes, new field at offset 88, no
   existing offset moves) and add a size/offset parity assertion, since there is no
   cbindgen for this struct and nothing else catches divergence. Accept a legacy
   `struct_size` as a **prefix**: freeze a `tft_bridge_sample_v1` and replace the
   whole-struct `read_unaligned` with a bounded prefix copy into a
   default-initialised local — relaxing the equality check alone is an
   out-of-bounds read in the one crate whose entire unsafe budget is argument
   validation. Add `tft_bridge_note_time_jump` and
   `TFT_BRIDGE_JUMP_{CLOCK_TYPE_CHANGED,BACKWARD,FORWARD}`, and classify **all of
   them** in `xtask/src/headers.rs`'s `UNSTABLE` list **in the same commit**:
   functions fail `check_partition` loudly, but an unclassified `pub const` is
   emitted into the **frozen** `tf_tree.h` by complement with nothing failing.
   Bump `TFT_ABI_VERSION_MINOR` 1 → 2 in both `crates/tf_tree_c/src/lib.rs` and
   `crates/tf_tree_c/include/tf_tree.h`. Re-aim the three tests named above and
   rewrite the `ClockReset` `detail` string for the new payload and sign. —
   verified by `just test`, `just c-header-check`, `just c-abi-check`, and by
   `grep TFT_BRIDGE_JUMP crates/tf_tree_c/include/tf_tree.h` finding **nothing**.
4. **`rclcpp`.** One `rclcpp::Clock steady_{RCL_STEADY_TIME};` member on
   `BridgeHandle`, in the ingest-thread-private block, read **once** per
   `TFMessage` at the top of `ingest()` and threaded into `offer_one` as a
   parameter — never read inside it. Register a jump **post**-callback on
   `node_->get_clock()` (the pre-callback is `std::function<void()>` and carries no
   `rcl_time_jump_t` at all) with
   `{on_clock_change = true, min_forward = {1}, min_backward = {-1}}` — a zero
   **disables** that direction — and store the returned `JumpHandler::SharedPtr` as
   a member, or the whole authoritative path silently never fires. The callback
   **only records** `{delta, clock_change}` into a slot the ingest thread drains at
   the top of `ingest()` and once per `run()` loop iteration; it must not call any
   `tft_bridge_*` entry point, because it runs on the `TimeSource`'s dedicated
   `/clock` thread and the ABI is thread-affine. It must also not throw. Drain in
   `run()` as well as in `ingest()`, because a looping bag with `/tf` momentarily
   silent is exactly when the signal is needed. — verified by `just ros-test`, with
   a gtest that records `std::this_thread::get_id()` in the post-callback and
   asserts it differs from the ingest thread's.
5. **The CLI, the offline crate's prose, and the remaining docs.** Fix the five
   bare `Sample` struct literals in `crates/tf_tree_cli/` — rewrite them onto
   `Sample::identity(..)` plus a `pose` assignment so the file never breaks on an
   appended field again — passing `SteadyNanos::UNKNOWN`, because the `.tfstream`
   grammar has no log-time column and passing the stamp would force `offset ≡ 0`.
   Rewrite `crates/tf_tree_ingest/src/ingest.rs`'s module doc: delete the broken
   intra-doc link, and restate the promotion asymmetry as this record's
   *Consequences* does. Fix the surviving `QUORUM_EDGES` intra-doc link in
   `crates/tf_tree_bridge/src/ingest.rs`. — verified by `just test`, `just lint`
   and `cargo doc` introducing no new warnings.
6. **Full gate:** `just lint`, `just test`, `just c-abi-check`,
   `just c-header-check`, `just tf2-check`, `just ros-test`.

## Open questions

None.
