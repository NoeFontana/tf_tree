# tf_tree — operator runbook

> Required by [`PHASE2.md`](./PHASE2.md) §13. Every row below names a distinct
> error type and, where one exists today, the `tf_tree doctor` check that
> detects it.

This document is for whoever is on call when a robot's transform tree
misbehaves. It is organised by **symptom**, because that is what you have when
you arrive.

**Implementation status.** Phase 2's rendezvous and lifecycle are implemented
(decision [`0005`](./decisions/0005-the-shared-memory-seam.md)); the recorder
and `/tf` ingest are not. **There is no `tf_treed`, and there will not be** —
[`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md) replaces
it with `tf_tree serve`, and more usefully removes the reason the rows below
used to point at a daemon at all. Any row you find still marked
*(needs `tf_treed`)* is a defect in this document: it is telling you to run a
program that has never existed. The seven `doctor` checks are `cycle`,
`unclaimed-dynamic`, `multi-writer`, `short-buffer`, `inconsistent-rate`,
`unreachable`, `out-of-order`.

**Two of those seven go blind when `doctor` is attached to a live arena**, and it
says so on every run rather than implying a clean bill of health it did not earn:
`multi-writer` and `short-buffer` both need a recorded push stream, and a ring
retains stamps but not who wrote each one or how late it arrived. See
[Attaching to a running robot](#attaching-to-a-running-robot).

---

## First moves

```bash
tf_tree doctor --attach      # every check against the running arena
tf_tree tree --attach        # live topology, per-edge rate, occupancy, writer PID
tf_tree echo <target> <source> --attach --rate
tf_tree participants         # who is attached — works even with no arena
```

Without `--attach` these commands operate on an in-process fixture, which is
useful for seeing what healthy output looks like and useless for diagnosing a
robot. **`tf_tree participants` is the one to reach for first when nothing else
works**: it reads the lock file and never maps the arena, so it still answers
when the segment is gone, when its layout does not match your build, or when the
owner is wedged.

Two habits worth forming:

- **Read the edge name in the error.** Every `tf_tree` error that *can* name an
  edge *does* name one — that is a deliberate design decision
  ([`PHASE1.md`](./PHASE1.md) §9), and it exists because "lookup would require
  extrapolation into the future" without saying which edge is the single
  most-complained-about thing about tf2.
- **`doctor` before `strace`.** Most of what goes wrong here is a configuration
  or startup-ordering problem that a check already names.

---

## Lookups are failing

### `NoData { edge }`

Nothing has ever been published to that edge.

Almost always **startup ordering**: the consumer began querying before the
publisher started. Confirm with `tf_tree tree` — the edge will show a head of 0.
If the publisher *is* running, it has not claimed the edge; see
`unclaimed-dynamic` below.

### `Extrapolation { edge, requested, oldest, newest }`

The requested stamp falls outside the edge's retained window. The error carries
all four numbers, so compare them before theorising:

- `requested > newest` — you are asking for the future. Either the consumer's
  clock is ahead of the publisher's, or the publisher has stalled. Check
  `newest` against wall time.
- `requested < oldest` — the sample aged out. The ring is too shallow for the
  gap between publish and query. Raise that edge's capacity; `doctor`'s
  `short-buffer` check warns before this becomes an outage.

Extrapolation is refused by default rather than silently invented, which is the
right default for a control loop. `ExtrapPolicy::Hold` and `ConstantTwist` exist
if a caller genuinely wants the other behaviour.

### `Disconnected { target, source, cut_at }`

No path between the two frames. `cut_at` names where the walk ran out of parent.
Usually a publisher for one link has not started, so a subtree is detached —
cross-reference `doctor`'s `unreachable` check, which lists every frame not
reachable from the main root component.

### `TopologyChanged { plan, current }`

Your compiled `Plan` predates a topology mutation. **This is a legitimate,
actionable error, not a failure to hide with a retry loop** — recompile the plan
and continue. If it fires repeatedly in steady state, something is re-parenting
frames continuously, which is a bug in the publisher: topology should be
near-static after startup.

### `TimeDomainMismatch { expected, got }`

A lookup crossed a time-domain boundary (e.g. system clock vs sensor clock).
Domains are separated at the type level on purpose; the alignment machinery is
Phase 6. Until then, do not mix them in one plan.

### `SlotContended` / `SlotRecycled`

The reader lost a race with the writer.

- `SlotContended` — a slot stayed mid-write for `SEQ_RETRY_LIMIT` attempts. In
  practice this means the writer was descheduled at exactly the wrong moment.
- `SlotRecycled` — the ring lapped the reader mid-read. With 4096 samples at
  1 kHz there is four seconds of slack, so this indicates a severe stall, or a
  ring far too shallow for the publish rate.

Both are returned rather than retried internally, because only the caller knows
whether a retry is meaningful. Raise the edge's capacity; `doctor` warns at 80%
occupancy.

---

## Writers are failing

### `EdgeAlreadyClaimed { owner_pid }`

Two nodes are configured to publish the same edge. This is a **genuine
configuration error**, and `tf_tree` reports it rather than silently averaging
the two streams into garbage the way a multi-publisher `/tf` topic does.
`doctor`'s `multi-writer` check names both PIDs.

Decide which node owns the edge and stop the other. If you are bridging from
ROS, the ingest bridge's conflict policy (`FirstWriterWins` by default) is where
this surfaces first.

### `NonMonotonicStamp { last, got }`

A push arrived with a stamp older than the edge's newest. Equal stamps are
accepted — that is required for idempotent replay — but going backwards is not.

Usually a publisher restarting without resetting its clock, or two sources
merged into one edge. `doctor`'s `out-of-order` check (`TFT018`) reports it from
observed history.

**Before you go looking for the publisher, check the edge's domain.** A *burst*
of these on an edge in the **`SystemDomain`** (wall clock, tag 0) is usually not
a publisher fault at all: `CLOCK_REALTIME` is not monotone, and an NTP step or a
leap second moves it backwards. Invariant 6 then rejects every stamp until the
clock catches up — correct behaviour that looks exactly like a broken node.
`doctor`'s `TFT019` makes that call for you *when it has a recorded push stream
to make it from*: same evidence as `TFT018`, plus the domain tag, reported as a
clock step rather than as a publisher fault. Restarting the publisher will not
help; the data lost during the step is gone either way. Read to the end of this
section before you reach for it on a running robot — **the source it needs is a
recording, not an attach**, and that is stated below rather than left to be
discovered.

**It fires on a run, not on one inversion.** A single stamp out of place on a
wall-clock edge is a publisher fault, so `TFT019` needs a *burst*: at least eight
consecutive pushes that invariant 6 would have rejected — a step of eight publish
periods, so 8 ms at 1 kHz or 80 ms at 100 Hz. Below that it passes and says so in
a `note:` line, and `TFT018` still reports the rejected pushes. **Eight is this
implementation's number, not the specification's.**

On any other tag `TFT019` **skips and says which tag** rather than guessing —
`Domain` is an open trait and a user-declared tag carries no way to state that
its clock can step. There, `TFT018` alone is the answer and the publisher is the
place to look. When *some* of the affected edges are on tag 0 and others are not,
it fires on whichever of the tag-0 edges cleared the run length above, and names
everything it did not attribute — the other tags, and any tag-0 edge whose
rejections were too scattered — in the report's `note:` lines, which is the only
place a check that ran can say what it did not cover.

**Point it at a recording. That is the source these two checks need:**

```
tf_tree doctor --from-bag run.mcap
```

A recording is written in log order, so a stamp that went backwards is *in the
file* at the position it arrived at — which is exactly what invariant 6 would
have rejected and exactly what these two checks are about. The §3.2 ingest report
goes to stderr, so `--json` still gives you a document to pipe.

**Neither `--attach` nor `--from-file` can answer them, and the two fail
differently.**

* On a **live arena** the push stream is reconstructed from a ring being written
  while it is read, so a slot at the old end can already hold the next lap's
  sample — an inversion the publisher never made.
* On a **frozen `.tft`** there is no writer and the read is exact, and it still
  cannot answer: an arena's ring holds only the pushes the engine *accepted*.
  `SampleRing::push` refuses an out-of-order stamp, so the arrival these checks
  report was never stored. Running there would pass every `.tft` ever written.

Both skips say so in the report. **Their silence on an arena is not an
all-clear**, it is the absence of the evidence — which is why the skip reason
names `--from-bag` rather than merely stating a limitation.

`tf_tree ingest` remains the tool for a clock step **past** the reset threshold,
because such a recording does not ingest at all and so never reaches `doctor`:

```
tf_tree ingest --bag run.mcap
```

Its clock guard is per edge, and a jump backwards past
`--clock-reset-threshold` (default 100 ms) halts with, verbatim:

```
Error: edge odom -> base_link jumped 150000000 ns backwards at stamp 9850000000,
past the reset threshold; the recording's own log time there is 9850000000, which
is where to cut it. Raise --clock-reset-threshold if this publisher is merely late
rather than replayed
```

— the edge by name, the size of the step, and the recorder's own monotone log
time, which is the coordinate `ros2 bag`/`mcap` cut on and the one that is still
meaningful after a rewind. Smaller regressions are not a halt; they are counted
in the same report as *"N transforms arrived out of stamp order"*.

The fix is a domain that cannot step. Anything published **at rate** should use a
steady or PTP-disciplined domain rather than the system wall clock: declare the
edge with `SteadyDomain` (tag 3), or — `Domain` being an open trait — with your
own unit struct and `TAG` if the clock is PTP-disciplined and you want to say so.
Reserve `SystemDomain` for stamps that genuinely have to be comparable to
wall-clock time outside the process.

`TFT019` still fires only on tag 0 and so still skips a `SteadyDomain` edge —
correctly, because a steady clock cannot step, so a run of rejections there *is*
a publisher fault and `TFT018` alone is the honest answer.

Sim time is a different problem — a `/clock` reset from a bag loop or a sim
restart — and is handled by the bridge's authoritative jump signal, not by this
section.

### `ClaimRevoked { edge }`

This writer was judged dead and its claim reaped, then it resumed. The process
was stalled — investigate scheduling, a GC pause, or a page fault against a slow
device. The correct response in code is to stop publishing and re-claim.

> Under [`PHASE2.md`](./PHASE2.md) §6.1 this becomes very rare by construction: a
> stalled writer still holds its kernel lock and therefore cannot be reaped while
> alive. Seeing it at all is worth investigating as a possible bug in the
> `ClaimRecord` path.

### `ReadOnly` on `claim` / `reparent` / `frame`

This process attached read-only. That is the **default for consumers and the
only real safety boundary in the system** — a read-only participant is
incapable of corrupting the tree, enforced by the MMU rather than by convention.

If the process genuinely needs to publish, attach with `AttachMode::ReadWrite`.
If it does not, this error just saved you from a bug.

Note that a read-only participant can *resolve* any frame the creator declared;
it can only fail to **intern a new one**, because interning writes.

---

## Shared memory and startup

### `ReparentError::LockContended { owner_slot }`

Another participant holds the arena's topology lock and is still alive, so this
re-parent could not proceed. `owner_slot` names the holder; `tf_tree doctor`
resolves it to a pid.

The lock is stolen automatically when its holder is **dead**, so seeing this
means a live process is mutating topology concurrently. Sustained contention is
a design smell rather than a fault: topology should be near-static after startup
([`PHASE2.md`](./PHASE2.md) §1, A2).

### `LayoutMismatch { found, expected }`

Two binaries were built from different commits and their arena struct layouts
disagree. **Rebuild every participant.** A layout change requires a full restart;
there is no partial upgrade path.

This is the one operators actually hit, and the raw symptom — attach failing on
a machine where everything else looks fine — is otherwise a multi-hour debugging
session. Both hashes are printed for exactly that reason.

### `VersionMismatch { found, expected }`

The segment was written by a different `FORMAT_VERSION`. Version 1 arenas cannot
be attached by a version 2 build: the Phase 2 amendments changed the header and
region table. Recreate the arena.

### `HeaderInconsistent`

The header's region offsets do not match the geometry its own capacities imply.
Distinct from `LayoutMismatch`, which compares against a build constant — this
catches a header that is internally inconsistent, from a peer bug, a scribbled
byte, or a build sharing this one's record sizes but not its capacities. Treat
it as corruption and recreate the arena.

### `Unsealed`

A peer offered a segment without `F_SEAL_SHRINK`/`F_SEAL_GROW`. Refused, because
an unsealed segment can be truncated under a reader and fault it with `SIGBUS`
inside a lookup — unrecoverable, mid-control-loop. A peer that hands you one is
either buggy or hostile and the two are indistinguishable from here.

### `ParticipantTableFull` / `NoParticipantSlots`

More than `max_participants` processes attached. Raise the limit; it requires
recreating the arena, because capacity is fixed at construction by design
([`PROJECT.md`](./PROJECT.md) §5 D4).

### `SIGBUS` inside a lookup

**Structurally impossible with sealing.** If it ever happens, the segment was not
sealed — file a bug rather than working around it.

### Attaching to a running robot

`tf_tree <cmd> --attach` joins the arena that `$TF_TREE_RUNTIME_DIR`,
`$TF_TREE_DOMAIN` and `$TF_TREE_NAME` resolve to — override any of them with
`--domain` / `--name`. **The commonest mistake is a domain mismatch**, and its
dangerous form is silent in other systems: you attach to the wrong domain and are
shown a perfectly plausible tree. Here it fails, because a domain is a different
directory and a different lock file. `tf_tree participants --domain N` confirms
which one has anything in it.

Attach is **read-only** and **will not create**. `--rw` and `--create` exist and
are opt-in: a diagnostic tool that can write to a robot's tree can corrupt it
with any bug it happens to have (D18), and a tool that creates on a typo will
conjure an empty arena and then report it healthy.

`doctor --attach` prints which checks it could not run. Two of the seven need a
recorded push stream — `multi-writer` cannot see a writer that has already been
replaced, and `short-buffer` needs each sample's arrival lateness, which nothing
in the arena records. Neither can fire on a live arena, so neither is claimed.

### Why `doctor --from-bag` reports `TFT010` and `TFT011` as *not run*

Because they have nothing to read, and saying so is the point.

Both are built on the `docs/PHASE5.md` §5 counters, and those are incremented by
**lookups**. An arena built from a recording has been written and never read —
the ingest publishes into it and asks it nothing — so every counter is zero. A
zero extrapolation count is also exactly what a healthy, heavily-used arena
looks like, so a `pass` there would be an all-clear about instrumentation nobody
had exercised. `doctor` skips instead and names the reason.

**This is not specific to `--from-bag`.** The same skip appears on the built-in
fixture, and on a live arena you attach to *before its first consumer has done a
lookup* — which is the most likely moment to run `doctor` at bringup. Run one
consumer, then re-run `doctor`, and both checks come back.

To get a verdict on extrapolation, point `doctor` at the arena the consumers are
actually using:

```
tf_tree doctor --attach          # after consumers have been running
```

`TFT011` is two checks under one id and skips only when both halves are blind;
where one half still has evidence it runs and the report's `note:` lines say
which half could not fire.

### Why `doctor --from-bag` warns `TFT017` on every edge

An arena built from a recording has **no writer at all** — the ingest's claims
are released when it finishes — so *dynamic edge with no live writer* is true of
every dynamic edge in it. The report says so in a `note:` line: the finding
names the arena, not any edge in it.

It is a warning rather than a skip on purpose. A fleet whose publishers have all
stopped produces the identical arena state, and that is the fault this check
exists to name. On a recording, ignore it; on an `--attach`, do not.

### Reading `tf_tree participants`

| column | meaning |
|---|---|
| `state = live` | the kernel still holds this slot's lock byte. A `SIGSTOP`ped process reads **live**, correctly — it has not died, and reaping it would be wrong |
| `state = stale` | the byte is released but the identity record remains: the process is gone and left a record behind. A reaper will collect it; `tf_tree doctor --attach --rw` forces one |
| `comm = <no record>` | the byte is held but no record has been written — a participant caught between taking its slot and describing itself. Momentary; re-run |
| `mode = ro` | attached read-only. It cannot publish and cannot corrupt anything |

An empty machine prints "no lock file" and **exits zero**. That is an answer, not
a failure, and the exit code says so.

### A writer stopped publishing and `push` returns `ClaimRevoked`

Its claim was reaped: something judged the process dead while it was stopped or
stalled, and the edge is now free or owned by somebody else. The correct response
is to stop publishing and re-claim — never to retry the push, which is the one
thing that would put two writers on a single-writer ring.

This is by design (A4). A process that was `SIGSTOP`ped long enough for its
*kernel lock* to be released cannot have been merely slow — the lock is released
by process death, not by a timeout — so if this fires, the process really did
die and come back, or somebody reaped by hand.

### The tree works in the parent and everything fails in a forked child

Errors will be `ChildDetached` from every entry point. A shared arena is mapped
`MADV_DONTFORK`, so the child has no mapping where the arena was; the handle it
inherited names memory it does not have.

Open a new tree in the child, or `exec`. There is no repair — this is not a
transient. Python's `multiprocessing` defaults to `fork` on Linux, so this is the
single most likely way to meet it; use the `spawn` start method, or open inside
the worker.

### `ArenaHeldButUnreachable`

A participant holds a live arena but is stopped or wedged and cannot take over
ownership. `doctor` will name the slot and PID; resume or kill it.

`--force-new` abandons the existing arena. Use it **only** when the holder is
confirmed unrecoverable — the reason `open()` blocks instead of creating a second
arena is that two live arenas diverging silently is worse than failing to start.

### Two processes see different data

Should be impossible; it means two `instance_uuid`s exist. `doctor` prints the
uuid and the resolved runtime dir on both. Almost always a runtime-directory or
domain mismatch — different container mounts, or different `ROS_DOMAIN_ID`.

### `open()` created an arena when one was expected

**On a build carrying
[`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md) this
should no longer be reachable, and that is the first thing to check.** `Open`'s
defaults are now the *consumer* — `AttachMode::ReadOnly` plus
`CreatePolicy::Never` — and the two are no longer independently settable into an
incoherent pair: a read-only attach combined with any creating policy is refused
with `OpenError::ReadOnlyCannotCreate`, before the runtime directory is even
resolved. `tf_tree::open()` creates nothing.

So a process that created an arena asked for it explicitly, with both
`AttachMode::ReadWrite` and a `CreatePolicy` other than `Never`, and supplied
the `TreeBuilder` that sized it. Find that call. Either it is a consumer that
was written against the pre-`0019` defaults and still names them, in which case
delete both and let it wait for the publisher with `Open::await_open`, or it is
a second copy of a legitimate publisher, in which case it wants
`Open::require_create(true)` — which turns "an arena is already live" into
`OpenError::ArenaAlreadyLive` instead of a silent join.

If the process genuinely predates `0019`, its `open()` did default to
`CreatePolicy::IfAbsent` and would create on an empty machine; rebuild it.

### Rendezvous misbehaving on a shared filesystem

The runtime directory is on NFS or CIFS. File locks there have subtly different
semantics and the whole rendezvous depends on them being exact, so `open()`
rejects those filesystems. Point `TF_TREE_RUNTIME_DIR` at local storage.

### `FrameNotDeclared`

A read-only participant asked for a frame nobody has declared yet. This is a
startup-ordering problem, not a typo — and there are two distinct causes, which
want opposite responses.

**First, check the consumer is not the process that created the arena.** A
consumer that passes `CreatePolicy::IfAbsent` **and** a layout, and starts before
any publisher, creates the arena itself — with *its* topology, permanently, since
capacity and edges are fixed at creation. It then looks healthy and finds
nothing, forever. `tf_tree participants` showing a single read-only participant
on an arena with no edges is the signature.
[`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md) §2 makes
it unrepresentable: a read-only attach *implies* `CreatePolicy::Never`, and the
builder's own default is now `Never`. On a build that predates that, pass it
explicitly. (An earlier revision of this row said the *default* silently created
an empty arena. It did not — without a layout that combination fails
`NoLayoutToCreate`. The hazard needed a caller that also passed
`layout_if_creating`.)

**Otherwise the publisher genuinely has not started yet, and the answer is to
wait rather than to fail.** `Tree::await_frames(["map", "base_link"], deadline)`
blocks until the frames exist or the deadline passes, and returns their ids.
Reach for it in a consumer's startup path instead of planning immediately.

**If frames arrive during operation rather than at startup** — per-detection
frames, a sensor that appears late — that is `frame_headroom` / `edge_headroom`,
sized at build time. Exhaustion is a typed error naming the knob.

A supervised deployment that wants none of this ambiguity can pre-declare the
whole static structure up front with `tf_tree serve --config`, which is what
that subcommand is for. It is an option, not a prerequisite.

### `TopologyChurn`

The topology mutated `TOPO_BLOCKS` times during a single plan compilation.
Almost certainly a bug — topology should be near-static after startup.

---

## `doctor` checks and what to do about each

| Check | What it means | Response |
|---|---|---|
| `cycle` | A parent chain that never reaches a root | A publisher re-parented a frame under its own descendant. The mutation should have been rejected; if `doctor` sees one, file a bug |
| `unclaimed-dynamic` | A dynamic edge with no live writer | The publisher never started, or exited without releasing. Expected briefly at startup; sustained means a dead node |
| `multi-writer` | More than one PID published to one edge | Configuration error — two nodes own the same edge. Both PIDs are named |
| `short-buffer` | Ring shorter than the observed publish latency | Raise that edge's capacity. This is the warning that precedes `Extrapolation`/`SlotRecycled` outages |
| `inconsistent-rate` | A frame published at a wildly varying rate | Often benign (a genuinely event-driven publisher), sometimes a struggling node. Compare against the rate you expect |
| `unreachable` | Frames not reachable from the main root | A subtree is detached — usually a missing static declaration or a publisher that has not started |
| `out-of-order` (`TFT018`) | Stamps arriving non-monotonically | A publisher restarted without resetting its clock, or two sources feed one edge |
| `TFT019` | A **run** of at least eight of those rejections, on an edge in `SystemDomain` (wall clock, tag 0) | Not a publisher fault — the clock stepped (NTP, leap second). Move anything published at rate to a steady or PTP domain. Passes with a `note:` below the run length, skips naming the tag on any other domain, and skips with `TFT018` on a live arena — which, `doctor` having no recording source, is the only outcome either of them has on a deployment |

---

## Performance triage

If lookups are slower than expected:

1. **Check the depth, not the frame count.** Cost is ~5 ns fixed plus ~70 ns per
   *dynamic* step; static edges constant-fold away at plan compilation. A
   "depth 6" chain that is five static edges and one dynamic one is cheap.
2. **Reuse the `Plan`.** Compiling one costs about as much as evaluating it
   twice. `tree.lookup(...)` caches per-thread; the expert path compiles once.
3. **Check the interpolation policy.** `ScLerp` (the default, and the correct
   one) costs ~44 ns per evaluation against `LerpSlerp`'s ~16. If a plan is
   latency-critical and tf2 compatibility is what you need,
   `EdgeCfg::interp(InterpPolicy::LerpSlerp)` is the cheaper choice.
4. **Pin before you measure.** Unpinned benchmark runs migrate cores and swing
   by more than 30% — enough to invent a regression in code that did not change.
   See [`benchmarks/tf2.md`](./benchmarks/tf2.md).

---

## How big is my arena, and how much of it did I over-declare?

`Capacity` is denominated in **slots**; tf2 evicts by **time**. Those are not the
same knob, and the translation is where over-declaration happens:
`Capacity::history(1000.0, 10.0)` asks for ten seconds of a 1 kHz stream — 10 000
slots — and reserves **16 384**, because `mask == capacity - 1` is the ring's hot
index and a mask is only a mask at a power of two. Run the same declaration
against a 10 Hz publisher and that ring retains **27 minutes** of history.

The rounding is not removable. What it is, is *visible*: both `tf_tree doctor`
and `tf_tree top` print the declaration in bytes, whole-tree in the header and
per-edge in `top --edge <id>`:

```text
rings: 19072 slots declared = 1.31 MiB over 4 edge(s); 12600 used = 885.9 KiB (66%);
       at most 9532 slots = 670.2 KiB is next_pow2 rounding
arena = 16704 B fixed + 320 B/edge + 144-176 B/frame + 72 B/slot
```

"At most" is the honest word. The pre-rounding request is **not stored** — the
edge record carries the capacity after `next_pow2` — so a ring of capacity `C`
was declared with some count in `[C/2 + 1, C]` and this figure is the upper bound
of that bracket, not a measurement. A publisher that asked for exactly 16 384
wasted nothing and looks identical here.

### The sizing formula

```text
arena = 16 704 B fixed          header + participant table + participant counters
      +    320 B per edge       claim 64 + edge record 128 + edge counters 128
      +  144-176 B per frame    frame record 64 + 4 topology blocks x 12 + intern slots
      +     72 B per slot       stamp 8 + pose 64 (one cache line)
```

The per-frame term is a range because the intern table is `next_pow2(2 x frames)`
slots of 16 B — exactly 32 B/frame at a power-of-two frame count, up to 64 B/frame
just above one. On a *small* tree add up to 384 B of fixed `align64` region
padding, which is why a 1-frame arena measures 384 B/frame against a stated 144.
On the benchmark fixture the formula reproduces the arena size the tools report,
by an independent path — `tree.arena_size_bytes()` comes from the built arena,
not from this arithmetic:

```text
16 704 fixed + 320 x 24 edge slots + 3 904 for 25 frames + 72 x 19 072 sample slots
  = 1 401 472 B = 1368 KiB     (`tf_tree top` prints "arena 1368 KiB")
```

The frame term is 3 904 rather than 144 x 25 = 3 600 for two reasons, both of
which are why the per-frame figure is a range: 25 is not a power of two, so the
intern table takes 64 slots for 50 names (1 024 B, not 800), and each of the four
topology blocks rounds 300 B up to 320. That is 156 B/frame, inside the stated
144-176.

Every constant is checked against `crates/tf_tree_arena/src/layout.rs` by
differencing two real `ArenaLayout`s rather than transcribed from it — see
`crates/tf_tree_cli/src/sizing.rs`'s tests. They change when
`docs/PHASE5.md` §1 changes a region, and the test is what notices.

### What over-declaring actually costs

Since [`0021`](./decisions/0021-the-idle-arena-is-resident-because-of-its-alignment.md) the heap
arena reaches `calloc`, so slots you declared and never wrote are demand-faulted
pages that never become resident: **over-declaration costs no resident memory**.
It still costs *reservation* — address space, the `.tft` file on disk, the bytes
a segment transfer copies, and the headroom a machine under strict overcommit
must have. Treat the numbers above as capacity planning, not as a memory leak,
and note that neither tool warns on them: there is no threshold here it would be
honest to fire on, so this is a display and not a `TFT0xx` check.

---

## What this system deliberately does not do

Worth knowing before you go looking for it:

- **It does not average multiple publishers on one edge.** It reports the
  conflict. `tf2` averages them into garbage; surfacing it is the feature.
- **It does not extrapolate by default.** A control loop must not act on
  invented data.
- **It does not grow capacity at runtime.** Growth means remapping, which would
  invalidate every reader's mapping.
- **It is not a sandbox.** A read-write participant can corrupt the arena, and no
  checksum changes that. The read-only attach is the real boundary, which is why
  it is the default for consumers.
