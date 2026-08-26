# 0036: the receipt time the format already reserved

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** none. This record authorises nothing.

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

**Sample the receipt time rather than taking it every push, using the counter the
push path already maintains.** `SampleRing::push` already computes `h` and stores
`h + 1` into `heartbeat`, and `0014` pins `heartbeat == head`, so a predicate of
the form `h % INTERVAL == 0` reads a value already in a register and costs a mask
and a branch. At `INTERVAL = 1024` the amortised cost is 38/1024 ≈ **0.04 ns**,
which is inside the noise of a 3 ns push.

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

1. **Fixed interval or derived from `nominal_rate_mhz`?** The tension above. A
   fixed interval is one mask; a derived one is a per-edge value the writer would
   have to hold, and `Publisher` does not carry `EdgeRecord` today. **This is the
   question to answer first** — the rest of the shape follows from it.
2. **Which clock, exactly, and what does the check do when the arena's stamps do
   not share an epoch with it?** `TFT005` already skips on that condition and
   `Clock` in `tf_tree_cli` is where it is decided; `TFT004` should inherit it
   rather than invent a second rule. Whether the receipt time should be recorded
   in the arena's *own* epoch and converted at read time, or in the host wall
   clock, is not obvious and decides whether a `.tft` freeze carries anything
   meaningful.
3. **What does `doctor` do with one offset per edge?** §6 asks for a rolling
   median per publisher, which needs a series. A single-shot `doctor` sees one
   value per edge; `tf_tree top` polls and could accumulate. Whether `TFT004`
   ships as a fleet comparison at one instant (every edge's offset against the
   fleet median, now) or waits for a series is a scope decision, and the first is
   much cheaper and already catches the case §6 describes.
4. **Is a 1-in-`INTERVAL` 38 ns spike acceptable on the publish path at p99.9?**
   The amortised number is not the number `PHASE1.md` §11 gates on. Nothing here
   has measured the spike's effect on `publish_to_visible`, and this host cannot
   (`unavailable` in the baseline, 4 cores against 17 needed).

## What would make this `ready`

- Question 1 answered, because it decides the shape.
- Question 4 measured on a host that can run `publish_to_visible`, or an explicit
  statement that it ships without that measurement and why.
- The §6.4 amendment drafted, keeping *never a reaping trigger* intact.
- A `TFT004` skip reason that is honest about what it is skipping on, in the shape
  `TFT007`'s two skip conditions were given after review found a `pass` that had
  compared nothing.

## Not in this record

**`TFT002`/`TFT003`.** They are also undetectable, and for an unrelated reason:
`tf_tree_bridge`'s `StaticStore` counts exactly those conditions and lives in the
bridge process's heap, so surfacing them needs the bridge to publish counters
into the arena, which `PHASE5.md` §1.2 reserves no space for. That is a locality
problem and a space problem; this is a field that exists and is unwritten. Fixing
one says nothing about the other.
