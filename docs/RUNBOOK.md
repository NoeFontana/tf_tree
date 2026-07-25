# tf_tree — operator runbook

> Required by [`PHASE2.md`](./PHASE2.md) §13. Every row below names a distinct
> error type and, where one exists today, the `tf_tree doctor` check that
> detects it.

This document is for whoever is on call when a robot's transform tree
misbehaves. It is organised by **symptom**, because that is what you have when
you arrive.

**Implementation status.** `tf_tree` is mid-Phase-2. Rows marked
**(Phase 2, not yet implemented)** describe errors the design specifies but the
code does not yet produce — they are listed so the runbook is complete when the
feature lands, and so nobody mistakes their absence for "cannot happen". The
seven `doctor` checks that do exist are: `cycle`, `unclaimed-dynamic`,
`multi-writer`, `short-buffer`, `inconsistent-rate`, `unreachable`,
`out-of-order`.

---

## First moves

```bash
tf_tree doctor          # every check, with the offending frame/edge named
tf_tree tree            # live topology, per-edge rate, occupancy, writer PID
tf_tree echo <target> <source> --rate    # is this specific lookup working?
```

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
merged into one edge. `doctor`'s `out-of-order` check reports it from observed
history.

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

### `ArenaHeldButUnreachable` — *(Phase 2, not yet implemented)*

A participant holds a live arena but is stopped or wedged and cannot take over
ownership. `doctor` will name the slot and PID; resume or kill it.

`--force-new` abandons the existing arena. Use it **only** when the holder is
confirmed unrecoverable — the reason `open()` blocks instead of creating a second
arena is that two live arenas diverging silently is worse than failing to start.

### Two processes see different data — *(Phase 2, not yet implemented)*

Should be impossible; it means two `instance_uuid`s exist. `doctor` prints the
uuid and the resolved runtime dir on both. Almost always a runtime-directory or
domain mismatch — different container mounts, or different `ROS_DOMAIN_ID`.

### `open()` created an arena when one was expected — *(Phase 2, not yet implemented)*

A consumer used the default `CreatePolicy::IfAbsent` and started before the
publisher. Set `CreatePolicy::Never` on consumers in any supervised deployment,
so they fail loudly instead of creating an empty arena a later publisher then
refuses to join.

### Rendezvous misbehaving on a shared filesystem — *(Phase 2, not yet implemented)*

The runtime directory is on NFS or CIFS. File locks there have subtly different
semantics and the whole rendezvous depends on them being exact, so `open()`
rejects those filesystems. Point `TF_TREE_RUNTIME_DIR` at local storage.

### `FrameNotDeclared` — *(Phase 2, not yet implemented)*

A read-only participant asked for a frame nobody has declared yet. This is a
startup-ordering problem, not a typo: pre-declare the static structure in
`tf_treed`'s config so consumers can attach and plan before any publisher runs.

### `TopologyChurn` — *(Phase 2, not yet implemented)*

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
| `out-of-order` | Stamps arriving non-monotonically | A publisher restarted without resetting its clock, or two sources feed one edge |

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
