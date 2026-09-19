# tf_tree — operator runbook

> Required by [`PHASE2.md`](./PHASE2.md) §13. Every row below names a distinct
> error type and, where one exists, the `tf_tree doctor` check that detects it.

Organised by **symptom**, because that is what you have when you arrive.

**Implementation status.** Phase 2's rendezvous and lifecycle are implemented
([`0005`](./decisions/0005-the-shared-memory-seam.md)); the recorder and `/tf`
ingest are not. There is no `tf_treed`, and `tf_tree serve`
([`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md)) is not
built (`PHASE2.md` §0.0): nothing below may name either as a remedy. Check ids are
in `crates/tf_tree_cli/src/catalogue.rs`'s module docs (`--suppress` and `--json`
use them) and `PHASE5.md` §6.

**`multi-writer` and `short-buffer` go blind when `doctor` is attached to a live
arena**, and it says so on every run: both need a recorded push stream, and a
ring retains stamps but not who wrote each or how late it arrived. See
[Attaching to a running robot](#attaching-to-a-running-robot).

---

## First moves

```bash
tf_tree doctor --attach      # every check against the running arena
tf_tree tree --attach        # live topology, per-edge rate, occupancy, writer PID
tf_tree echo <target> <source> --attach --rate
tf_tree participants         # who is attached — works even with no arena
```

Without `--attach` these operate on an in-process fixture. **`tf_tree
participants` is the one to reach for first**: it reads the lock file and never
maps the arena, so it answers when the segment is gone, its layout does not match,
or the owner is wedged. Every error that can name an edge does
([`PHASE1.md`](./PHASE1.md) §9); and run `doctor` before `strace`.

---

## Lookups are failing

### `NoData { edge }`

Nothing has ever been published to that edge: almost always **startup ordering**
(`tf_tree tree` shows head 0). If the publisher runs, it has not claimed the edge;
see `unclaimed-dynamic`.

### `Extrapolation { edge, requested, oldest, newest }`

The stamp is outside the retained window; compare the four numbers.
`requested > newest`: the consumer's clock is ahead or the publisher stalled.
`requested < oldest`: the ring is too shallow; raise that edge's capacity
(`short-buffer` warns first). Refused by default; `ExtrapPolicy::Hold` and
`ConstantTwist` exist for callers who want otherwise.

### `Disconnected { target, source, cut_at }`

No path between the frames; `cut_at` is where the walk ran out of parent, usually
a link whose publisher has not started. See `unreachable`.

### `TopologyChanged { plan, current }`

Your `Plan` predates a topology mutation: recompile, do not retry-loop. Repeats in
steady state mean a publisher is re-parenting continuously, a bug.

### `TimeDomainMismatch { expected, got }`

A lookup crossed a time-domain boundary; do not mix domains in one plan
(alignment is Phase 6).

### `SlotContended` / `SlotRecycled`

The reader lost a race with the writer: `SlotContended` — a slot stayed mid-write
for `SEQ_RETRY_LIMIT` attempts (writer descheduled); `SlotRecycled` — the ring
lapped the reader (a severe stall, or a ring too shallow). Both are returned, not
retried, because only the caller knows whether a retry is meaningful. Raise the
edge's capacity; `doctor` warns at 80% occupancy.

---

## Writers are failing

### `EdgeAlreadyClaimed { owner_slot }`

Two nodes are configured to publish the same edge, a configuration error. The
error names the edge and the owning **participant slot**, not a pid
(`ClaimApiError::AlreadyClaimed`; C's `tft_error` carries `edge` and the slot in
`frame_a`, **except `tft_bridge_create`**, which overwrites `frame_a`/`frame_b`
with the refused link's `FrameId`s). `tf_tree participants` maps slot to pid.
`multi-writer` (`TFT001`) is **not** the tool: a refused claim never publishes.
Decide which node owns the edge and stop the other; from ROS the bridge's conflict
policy (`FirstWriterWins` by default) is where it surfaces.

### `NonMonotonicStamp { edge, last, got }`

A push arrived with a stamp older than the edge's newest. Equal stamps are
accepted (idempotent replay); going backwards is not. Usually a publisher
restarting without resetting its clock, or two sources on one edge. `doctor`'s
`out-of-order` (`TFT018`) reports it from observed history.

**Check the edge's domain first.** A *burst* on a **`SystemDomain`** edge (wall
clock, tag 0) is usually a `CLOCK_REALTIME` step (NTP, leap second) that makes
invariant 6 reject every stamp until the clock catches up, not a publisher fault;
restarting the publisher will not help. `TFT019` makes that call, *when it has a
recorded push stream*.

- It fires on a **run** of at least eight consecutive rejected pushes (this
  implementation's number, not the specification's); below that it passes with a
  `note:` and `TFT018` still reports them. On any other tag it **skips and names
  the tag** (`Domain` is an open trait), leaving `TFT018` alone.

**Point it at a recording — the source these two checks need:**

```
tf_tree doctor --from-bag run.mcap
```

A recording is in log order, so a backwards stamp is *in the file*. **Neither
`--attach` nor `--from-file` can answer these checks**: a live ring is read while
written, so a slot at the old end can hold the next lap's sample (an inversion the
publisher never made), and a frozen `.tft` holds only pushes `SampleRing::push`
*accepted*. Both skips say so; **their silence on an arena is not an all-clear.**

`tf_tree ingest --bag run.mcap` is the tool for a step **past** the reset
threshold (such a recording does not ingest): its per-edge clock guard halts on a
jump backwards past `--clock-reset-threshold` (default 100 ms) with, verbatim:

```
Error: edge odom -> base_link jumped 150000000 ns backwards at stamp 9850000000,
past the reset threshold; the recording's own log time there is 9850000000, which
is where to cut it. Raise --clock-reset-threshold if this publisher is merely late
rather than replayed
```

**The fix is a domain that cannot step**: publish at rate on `SteadyDomain` (tag
3), or your own `Domain` if the clock is PTP-disciplined; reserve `SystemDomain`
for stamps comparable to outside wall-clock time. `TFT019` skips `SteadyDomain`,
correctly. Sim-time `/clock` resets are the bridge's authoritative jump signal's.

### `ClaimRevoked { edge }`

This writer was judged dead and its claim reaped, then it resumed (a stall:
scheduling, GC pause, slow-device page fault). Stop publishing and re-claim; never
retry the push, which would put two writers on a single-writer ring. Under
[`PHASE2.md`](./PHASE2.md) §6.1 a stalled writer still holds its kernel lock, so
this is very rare: suspect the `ClaimRecord` path.

### `ReadOnly` on `claim` / `reparent` / `frame`

This process attached read-only, the **default for consumers and the only real
safety boundary** (enforced by the MMU). To publish, attach with
`AttachMode::ReadWrite`. A read-only participant can resolve any declared frame but
not **intern a new one**.

---

## Shared memory and startup

### `ReparentError::LockContended { owner_slot }`

Another live participant holds the topology lock (a dead holder's is released).
Contention, not a fault, and the one `reparent` error a caller loops on:

```rust
loop {
    match tree.reparent(child, parent) {
        Ok(()) => break,
        Err(tf_tree::ReparentError::LockContended { .. }) => std::hint::spin_loop(),
        Err(other) => return Err(other),
    }
}
```

Sustained contention is a design smell ([`PHASE2.md`](./PHASE2.md) §1, A2).

**`owner_slot` can be `None`**: the holder has not yet published which slot holds
it (an OFD lock reports `l_pid = -1`, §3.3). Retry; if it persists, `tf_tree
doctor` and `tf_tree top` list every live participant.

**When it will not clear.** A `fork` child inherits the parent's open file
description and any byte held at the fork (§6.2), so a *dead* parent's topology
lock stays held while the child lives. `doctor` reports it as `TFT014`; stop the
child, or start workers with `spawn`.

### `ReparentError::TopologyLease { raw_os_error }`

`fcntl` on the topology byte failed for a reason that is not contention; **not**
retryable. Check the runtime directory exists on a local filesystem
([`PHASE2.md`](./PHASE2.md) §3.1) and the process has descriptors left.

### `HandshakeRejected`

A **live, serving owner** answered the rendezvous socket and refused this attach.
The message names the status and the owner's side of the comparison, and stops:

```text
the arena owner refused this attach: LayoutMismatch (owner format_version 3, layout_hash 0x3D104195) (HandshakeRejected)
```

**The remedy is this table** ([`0055`](./decisions/0055-the-recovery-capacity-a-fleet-cannot-add-later.md)
step 7; Erratum in [`PHASE2.md` §3.7](./PHASE2.md#37-attach)). The message prints
the owner's hash, not this build's; rebuild every participant from one release
whichever pair you hold, and print this build's constants with `tf_tree doctor
--explain-version` **run from the refused binary's build**. The `VersionMismatch`
and `LayoutMismatch` sections below are a different check: a process validating a
mapped header against its own constant, as when opening a frozen `.tft`.

| `status` | What the owner compared | What to do |
|---|---|---|
| `VersionMismatch` | this binary's `FORMAT_VERSION` against the running arena's, **first**, because a version difference makes every later field uncertain | rebuild every participant from one release and restart them together. There is no partial upgrade path |
| `LayoutMismatch` | same version, a different record layout | rebuild every participant, as above. The owner's `layout_hash` is in the message; this binary's is printed by `tf_tree doctor --explain-version` **built from the same commit as the refused process** |
| `BootIdMismatch` | the boot id in the attach request against the one in the **arena header** | **not "the arena outlived a reboot"** — a serving owner proves it did not. The two processes disagree about which boot this is ([`PHASE2.md`](./PHASE2.md) §3.3): either one failed to read `/proc/sys/kernel/random/boot_id` and substituted all-zeros, or something presents a different one to it (a sandbox masking `/proc/sys`, a `/proc` overlay). **A third way needs no failure**: `tf_tree_ipc::procstat::boot_id` rejects a UUID with trailing junk and the arena header's writer, `tf_tree::tree::boot_id`, ignores it. Check the file from both processes first; if it is well formed and identical, the divergence is a bug to report |
| `NoParticipantSlots` | every slot, against **both** tables: the assigner walks the arena's participant records *and* the lock bytes and grants a slot only where both are free | triage is the *`ParticipantTableFull` / `NoParticipantSlots`* section below, because the two tables need two different commands. A read-only consumer holds a byte and writes no record. **There is no `--participants` flag**: capacity is fixed at construction ([`PROJECT.md`](./PROJECT.md) §5 D4) |
| `ModeNotPermitted` | **nothing in this workspace sends it**: `OwnerServer::serve` answers an undecodable datagram with `Malformed`, `OwnerServer::check` returns only the three mismatches, and `tf_tree::open`'s `assign` returns only `NoParticipantSlots`. Only somebody else's `assign` closure can | attach read-only, the consumer default ([`PROJECT.md`](./PROJECT.md) §5 D18). Against an owner built on `tf_tree_ipc` with its own `assign`, this is that policy and its author is who to ask; against a `tf_tree` owner no code path produces it, so report it |
| `Malformed` | nothing — it could not decode the request | **or it refused for a reason this build has no name for**: every unknown status decodes to `Malformed` (`HelloStatus::from_u32`), so a newer owner's newer refusal arrives here. Confirm both sides are the same release before reading this as corruption |

`Ok` has no row. **Adding a `HelloStatus` fails the tests** (`status_is_a_refusal`
in `tf_tree_ipc`'s `error.rs`, under `--all-targets`); whoever fixes it owes this
table a row, which `tf_tree_cli`'s `tests/runbook.rs` checks.

### `LayoutMismatch { found, expected }`

Binaries built from different commits disagree on arena layout. **Rebuild every
participant**; there is no partial upgrade. Both hashes are printed. This is the
mapped header's check; a refusal that never reached a segment ends
`(HandshakeRejected)` and is that section.

### `VersionMismatch { found, expected }`

The segment was written by a different `FORMAT_VERSION`; recreate the arena. The
owner's comparison is *`HandshakeRejected`* above.

### `HeaderInconsistent`

The header's region offsets do not match the geometry its capacities imply (unlike
`LayoutMismatch`, which compares a build constant): a peer bug, a scribbled byte,
or a build with the same record sizes but other capacities. Treat as corruption and
recreate the arena.

### `Unsealed`

A peer offered a segment without `F_SEAL_SHRINK`/`F_SEAL_GROW`; refused, because
it could be truncated under a reader and `SIGBUS` it mid-lookup. The peer is buggy
or hostile.

### `ParticipantTableFull` / `NoParticipantSlots`

More than `max_participants` processes attached. **There is no flag**; first look
for a leak, a participant that exited without releasing its slot.

**Two commands, two tables.** `tf_tree participants` reads the **lock file only**
(`live` where the kernel holds the byte, `stale` where not): it finds the ordinary
leak and is blind to a `TreeBuilder::build_shared` creator, which registers a
`LIVE` arena record and takes no byte
([`0031`](./decisions/0031-the-participant-record-with-no-byte.md)); against one it
prints *"no lock file: nothing has ever attached to this domain/name"* while the
slot is spent. **`tf_tree doctor --attach` reads the arena's table**: `TFT014`
says how many slots are spent and names each pid (see *`doctor` checks*). Its
limits are in `tft014` (`crates/tf_tree_cli/src/checks.rs`): a claim left by a dead
owner or a `build_shared` participant is invisible to it.

**A record with no lock byte is accused**: `doctor --attach` reports **`a record
left behind — … the lock byte is free`** about a process that is running and
publishing. Check the pid is alive before acting.

A dead **owner** is in contract; a surviving read-write peer's sweep is the
collector ([`PHASE2.md` §3.9](./PHASE2.md#39-teardown)). A byte-less
`build_shared` participant is **out of contract** where published into a
rendezvous by hand
([`PHASE2.md` §3.1](./PHASE2.md#31-the-sharing-boundary-is-the-runtime-directory--normative)):
the application should use `tf_tree::Open`. Until fixed, **do not sweep**
(`Tree::reap_dead` / `reap_participants`, `tft_tree_reap_dead`,
`Tree.reap_dead()`): it frees the records and takes the claims of running
publishers. No `tf_tree` subcommand sweeps.

Capacity comes from `tf_tree_arena::layout::DEFAULT_MAX_PARTICIPANTS` and header
validation refuses a disagreeing segment: raising it means changing that constant
and rebuilding and restarting every participant together.

### Attaching to a running robot

`tf_tree <cmd> --attach` joins the arena that `$TF_TREE_RUNTIME_DIR`,
`$TF_TREE_DOMAIN` and `$TF_TREE_NAME` resolve to (override with `--domain` /
`--name`). **The commonest mistake is a domain mismatch**;
`tf_tree participants --domain N` confirms which domain has anything in it.

Attach is **read-only** and **will not create**; `--rw` and `--create` are opt-in
(D18). `doctor --attach` prints which checks it could not run: `multi-writer`
cannot see a writer already replaced, and `short-buffer` needs arrival lateness,
which nothing in the arena records. The report's `not run:` block is the list.

### Why `doctor --from-bag` reports `TFT010` and `TFT011` as *not run*

Both are built on the `docs/PHASE5.md` §5 counters, which **lookups** increment.
An arena built from a recording, the built-in fixture, or a live arena attached
before its first consumer's lookup has all-zero counters, also what a healthy
arena can look like, so `doctor` skips rather than pass. Run one consumer and
re-run. `TFT011` skips only when both its halves are blind; `note:` lines say
which could not fire.

### Why `doctor --from-bag` warns `TFT017` on every edge

A recording's arena has **no writer** (the ingest's claims are released), so
*dynamic edge with no live writer* is true of every edge, as the `note:` line
says. It is a warning, not a skip, because a fleet whose publishers all stopped
looks identical: ignore it on a recording, not on an `--attach`.

### Reading `tf_tree participants`

| column | meaning |
|---|---|
| `state = live` | the kernel still holds this slot's lock byte. A `SIGSTOP`ped process reads **live**, correctly; reaping it would be wrong |
| `state = stale` | the byte is released but the identity record remains: the process is gone. A reaper will collect it; `tf_tree doctor --attach --rw` forces one |
| `comm = <no record>` | the byte is held but no record written yet — momentary; re-run |
| `mode = ro` | attached read-only; cannot publish or corrupt anything |

An empty machine prints "no lock file" and **exits zero**: an answer, not a
failure.

### A writer stopped publishing and `push` returns `ClaimRevoked`

See `ClaimRevoked` above. The kernel lock is released by process death, not a
timeout (A4), so the process really did die and come back, or somebody reaped by
hand.

### The tree works in the parent and everything fails in a forked child

Errors are `ChildDetached` from every entry point. A shared arena is mapped
`MADV_DONTFORK`, so the child has no mapping where the arena was. Open a new tree
in the child, or `exec`; there is no repair. Python's `multiprocessing` defaults to
`fork` on Linux, the likeliest way to meet this: use `spawn`, or open inside the
worker.

**The child is also holding a participant slot.** `fork` shares the open file
descriptions, so the child keeps the parent's rendezvous socket *and* lock byte:
the owner never sees a `HUP` and the kernel keeps answering "held". `doctor
--attach` reports it as the second `TFT014` shape (*byte still HELD*), the one leak
nothing may reclaim. The slot returns when the last inheritor exits.

**A participant in another PID namespace** produced this report until
[`0033`](./decisions/0033-the-identity-record-cannot-name-a-namespace.md); on an
older build, run `doctor` from **inside** its namespace.

**It is reported whether or not the parent was a writer**: a read-only participant
(D18) takes a byte and writes **no** arena record, so `doctor` reports *the record
is FREE (no arena record: a read-only participant, D18)* and `tf_tree participants`
shows the slot `live` with the dead parent's pid.

### The arena's owner died

Existing participants are fine ([`PHASE2.md`](./PHASE2.md) §3.5); the question is
whether anything can *join* again.

**A surviving read-write participant inherits the owner role in one call.** It
notices the hangup with `Tree::owner_lost()` and promotes itself with
`Tree::inherit_ownership()`, which takes the ownership byte on the file
description its session already holds and binds the rendezvous socket over the
**existing** segment ([`0037`](./decisions/0037-a-takeover-is-not-a-second-open.md)).
Nothing is copied, no lookup pauses, every survivor keeps its slot.

```rust
// In a read-write participant's own loop — between control cycles is fine.
if tree.owner_lost() {
    match tree.inherit_ownership()? {
        tf_tree::Inheritance::Inherited => { /* this process is now serving */ }
        tf_tree::Inheritance::Contended => {
            /* another survivor won -- or a fresh open() held the ownership byte
               in passing and will hand it back. Not final either way (nor is
               OwnerAlive): while owner_lost() says true, the next pass retries. */
        }
        _ => {}
    }
}
```

**Write it exactly like that — no latch, no backoff, no "only once" flag.**
`owner_lost()` asks whether the arena has an owner, not whether *this* socket is
dead ([`0043`](./decisions/0043-owner-lost-is-a-question-about-the-owner.md)), so
the *N−1* losers pay nothing after the winner binds: one `poll` reports a hangup,
one `F_OFD_GETLK` reports byte 0 held, and it returns `false` without touching the
ownership lock. **If you wrote a latch to stop the old behaviour, delete it**: a
latched survivor cannot inherit when the *second* owner dies.

**Nothing calls this for you, and that decides whether your fleet can recover**
([`PHASE2.md` §3.5](./PHASE2.md#35-ownership-migrates-the-data-plane-never-pauses--normative),
*The trigger is the caller's*). Check:

- **Is any survivor read-write?** `inherit_ownership()` answers
  `Inheritance::ReadOnly` on a read-only attachment, so **a fleet of read-only
  consumers cannot rescue itself** (D18). Read-only is the default
  (`crates/tf_tree/src/open.rs:886`); open one process read-write even if it never
  publishes. **Necessary and not sufficient**
  ([`0055`](./decisions/0055-the-recovery-capacity-a-fleet-cannot-add-later.md)
  part 1): it must *poll* `owner_lost()` and be attached **before** the owner dies;
  capacity is whoever was attached, eligible and polling when the role fell vacant,
  and only shrinks. Pinned by `a_read_only_survivor_reports_that_it_cannot_inherit`
  (`crates/tf_tree/tests/rendezvous.rs`).
- **What language is that survivor written in?** Since
  [`0044`](./decisions/0044-recovery-the-languages-a-robot-is-written-in-cannot-reach.md)
  **C and C++** have `tft_tree_open_named`, `tft_tree_owner_lost`,
  `tft_tree_inherit_ownership` and `tft_tree_reap_dead` in the *unstable* header
  (`#define TFT_ENABLE_UNSTABLE`); **Python** has `tree.owner_lost()`,
  `tree.inherit_ownership()` (returns the outcome's name) and `tree.reap_dead()`,
  with `tf_tree.open(mode="rw")`. A node built before 2026-08-28 cannot.
- **Does that survivor call it?** One that never polls is indistinguishable from
  one that cannot; `tf_tree participants` shows who is attached, not who is looking.
- **Did it try and fail?** Every error path restores the attachment and hands the
  byte back, so the next pass, or another survivor, can take it.

**When no survivor can or will inherit, stop every attached participant** and
start again. `SIGTERM` is enough (the kernel releases the byte and mapping) but
leaks the arena record of a participant stopped while the arena survives (the
`TFT014` row). `CreatePolicy::Always` abandons the arena and creates a second one
(see `ArenaHeldButUnreachable`); reach for inheritance first.

#### An owner is not dead until its exit ends

`owner_lost()` answers `true` once the survivor's attach connection has hung up and
the last open file description holding byte 0 has closed ([`PHASE2.md`](./PHASE2.md)
§3.5, NORMATIVE). For a dying owner that is the **end of its exit**: the kernel
writes any core dump and tears down the address space first. An owner whose `fork`
child outlives it keeps both until the child exits. Nothing shortens it
([`0057`](./decisions/0057-an-owner-is-not-dead-until-its-files-close.md)).

**During the window** lookups and existing publishers carry on; nobody inherits and
no fresh join completes (`open()` blocks, then is refused with
`ArenaHeldButUnreachable` with `ownership_held: true`). Edges the owner had claimed
stay refused until a survivor calls `reap_dead`.

**The trade, per process: a crash dump, or recovery bounded by teardown.** Make it
for **every process that may hold the role**, since ownership migrates
([`0055`](./decisions/0055-the-recovery-capacity-a-fleet-cannot-add-later.md)).

- **The dump window is the crash helper's run** and grows with the dump. Check
  `/proc/sys/kernel/core_pattern` (a leading `|` pipes to apport or
  systemd-coredump); to time it, run `0057`'s *Reproduction*.
- **To suppress dumps**, give the process a core limit of **1 byte**:
  `prlimit --core=1:1 -- <cmd>` (**measured**), `LimitCORE=1` in the systemd unit,
  or `setrlimit(RLIMIT_CORE, {1, 1})` in a launch wrapper; confirm with
  `grep 'core file' /proc/<pid>/limits`. **`ulimit -c 0` is not it** when the host
  pipes dumps. systemd-coredump's `Storage=`/`ProcessSizeMax=` are **not measured**.
- **The teardown is not a setting**: about **100 ms per GiB of dirty 4 KiB
  anonymous memory** on the host `0057` measured; far less under
  `transparent_hugepage=always`.
- **A supervisor's `SIGKILL` of a dumping owner ends the window and forfeits the
  core.** Inferred and not measured; **not recommended**.

### `ArenaHeldButUnreachable`

Somebody holds a live arena and nothing serves it, so [`PHASE2.md`](./PHASE2.md)
§3.4's split-brain check refuses to create a second. **The ordinary cause is not a
fault**: the owner exited and a healthy survivor still has the arena mapped, so
every open times out while any survivor lives (*The arena's owner died*). If the
owner has just died, refusals last at least until its exit ends.

```bash
tf_tree participants   # the holders, by slot and pid — reads the lock file, never maps the arena
```

**Reach for inheritance before a restart**: a surviving read-write participant
that calls `owner_lost()` / `inherit_ownership()` ends this state without stopping
anything ([`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md):
no daemon polls for you; D18: a read-only consumer cannot serve). Whether you have
such a survivor was decided before you got here
([`0055`](./decisions/0055-the-recovery-capacity-a-fleet-cannot-add-later.md)):
while any participant byte is held no process can *join*, so **nothing you start
now can become the heir**. Provision read-write pollers in advance.

**When there is no heir, stop every participant**, read-only consumers and
`tf_tree top --attach` included. The kernel releases each lock byte on death and
frees the segment when its last mapping drops (§3.9), so the next `open()` creates
cleanly. Restarting the publisher alone does not help.

If a holder must keep running and its arena is written off, the escape hatch is
**`CreatePolicy::Always`** — [`PHASE2.md`](./PHASE2.md) §3.4's `--force-new`, a
policy on the creating process and **not** a flag on `tf_tree` (§0.0). It creates
a *fresh* arena over the same name and abandons this one: it recovers the *name*,
not the arena.

**Whether a forced create passes is decided by what is held.** It skips §3.4's
participant scan and nothing else, so it still takes the ownership byte and
participant byte **0** (the creator's slot, held by the owner for its whole life
while joiners get `>= 1`):

| what is held | forced create |
|---|---|
| only slots `>= 1`, nothing else | **creates.** The case the hatch is for: the owner is gone, ordinary consumers survived |
| slot **0** | **refuses**, like an ordinary open. Byte 0 is usually the owner's, but `Session::release_ownership` can leave a live non-owner there. Stop that process; if it was the only holder an ordinary open then creates, otherwise you land on row 1 |
| the ownership byte, by a process that is not serving | **refuses.** Something took ownership and never bound its socket; stop it, then re-open |

**The error gives facts, not a remedy** (`0055` step 6): the participant mask, the
lowest held slot and its pid, and whether the ownership byte is held, ending
`(ArenaHeldButUnreachable)`. A refused process sees which bytes are held and not
who holds them, so it cannot tell one process holding two bytes from two holding
one each. `tf_tree participants` shows participant bytes but **not** the ownership
byte, hence the `ownership_held` bit. Read your row off the two facts:

| participant bytes | ownership byte | what to do |
|---|---|---|
| lowest slot is 0, nothing else | free | stop the process on slot 0; an ordinary open then creates. `CreatePolicy::Always` takes slot 0 or nothing |
| lowest slot is 0, nothing else | held | **usually one process holds both** (a creator takes both on one file description); stopping it releases both. If the ownership byte stays held, a second process has it and goes too |
| lowest slot is 0, others too | free | stop slot 0's holder first; the rest are ordinary participants and §3.4's hatch then applies |
| lowest slot is 0, others too | held | the ownership byte and slot 0 both have to be free, in either order, and one process may hold both; the hatch then applies to what is left |
| lowest slot is 1 or above | free | the stranded-participant case the hatch is for: `CreatePolicy::Always` **abandons** this arena, leaving the survivors publishing where nobody can reach them. Reach for inheritance first |
| lowest slot is 1 or above | held | **two things are held, so stopping one is not enough**: the ownership holder never bound a socket, and participant bytes are held too. A forced create cannot pass this either. Stop the ownership holder, then you are on row 5 |
| `nobody attached` … `held for the whole open timeout` | held | nobody is attached and ownership was held throughout by a process that never served; nothing was created. Stop that process |
| `no byte was held at the open deadline` | — | the blocker let go while you were timing out. Retry; the one state that clears itself |

**A forced create needs a layout and read-write mode**
([`0004`](./decisions/0004-builder-time-edge-declaration.md)), else
`NoLayoutToCreate` / `ReadOnlyCannotCreate`:

```rust
// `tf_tree::Open` is behind `features = ["shm"]`, Linux only.
tf_tree::Open::new()
    .mode(tf_tree::AttachMode::ReadWrite)
    .create(tf_tree::CreatePolicy::Always)   // skips the split-brain check
    .layout_if_creating(builder)             // required: decision 0004 sizes an arena from its edges
    .open()?
```

Use it **only** when the holder is confirmed unrecoverable:

- **The old arena stays alive.** Survivors keep publishing into a segment nobody
  else can reach: two arenas, two `instance_uuid`s.
- **It spends participant slots and never recovers one.** Survivors still hold
  their bytes in the *same* lock file and the new assigner skips held bytes, so it
  is the wrong instrument for a full participant table.
- **The survivors' claim leases alias the new arena's.** A lease is a byte at
  `CLAIM_BASE + edge_id` in that lock file (§6.1) and the replacement numbers its
  edges from zero, so claiming an id a survivor holds gives `LeaseContended` on an
  edge the new arena reports free, until the survivor exits.
- **Against a live non-owner holder of byte 0 it is refused, and this is the error
  an operator sees.** `Session::release_ownership` can leave one (§3.5); a creator
  takes byte 0 with one `F_OFD_SETLK` that *is* the check, and `open()` times out
  with

  ```text
  ArenaHeldButUnreachable { holder_slots: 0x1, first_slot: Some(0), first_pid: <the holder>, ownership_held: false }
  ```

  Row 1 of the remedy table: stop the process on slot 0. **Do not retry**: a second
  forced create is refused identically. The refusal runs before the owner server
  binds and spends no slot. The divergence #201 needs is pinned by
  `defect_201_release_ownership_strands_a_live_non_owner_on_byte_0`;
  `ParticipantSlotDiverged` is unreachable from the create path since `0035`
  (`PHASE2.md` §0.0). On `0.0.3` and earlier the same call returns a `Tree`
  ([`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md) step 0c).

`open()` probes the socket before taking the ownership byte, so the policy
abandons an unreachable arena, never a served one.

### Two processes see different data

Two `instance_uuid`s exist; `doctor` prints the uuid and runtime dir on both.
Almost always a runtime-directory or domain mismatch (container mounts,
`ROS_DOMAIN_ID`).

### `open()` created an arena when one was expected

Since [`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md) `Open`
defaults to the consumer (`ReadOnly` + `CreatePolicy::Never`), read-only with a
creating policy is `OpenError::ReadOnlyCannotCreate`, and `tf_tree::open()` creates
nothing. So the process asked for it (`ReadWrite`, a non-`Never` policy, a
`TreeBuilder`): a consumer should use `Open::await_open`, a second publisher
`Open::require_create(true)` (`OpenError::ArenaAlreadyLive`). Pre-`0019` builds
defaulted to `IfAbsent`; rebuild.

### Rendezvous misbehaving on a shared filesystem

NFS and CIFS lock semantics are unusable, so `open()` rejects them. Point
`TF_TREE_RUNTIME_DIR` at local storage.

### `FrameNotDeclared`

A read-only participant asked for a frame nobody has declared yet: a
startup-ordering problem, with two causes that want opposite responses.

**First, check the consumer did not create the arena.** A consumer passing
`CreatePolicy::IfAbsent` **and** a layout, starting before any publisher, creates
the arena with *its* topology, permanently, and then finds nothing forever.
`tf_tree participants` showing a single read-only participant on an arena with no
edges is the signature. [`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md)
§2 makes it unrepresentable (read-only implies `CreatePolicy::Never`); on an older
build pass it explicitly.

**Otherwise the publisher has not started, and the answer is to wait.**
`Tree::await_frames(["map", "base_link"], deadline)` blocks until the frames exist
and returns their ids; use it in a consumer's startup path.

**Frames arriving during operation** (per-detection frames, a late sensor) are
`frame_headroom` / `edge_headroom`, sized at build time; exhaustion is a typed
error naming the knob. To remove the ambiguity, pre-declare the static structure
in the topology config (`crates/tf_tree_bridge/src/config.rs`'s schema, which
`ros/tf_tree_ros` and Python's `build`/`open` accept
([`0041`](./decisions/0041-python-declares-a-topology-the-way-everything-else-does.md))
and `tf_tree topology --discover` writes) and pass it as `layout_if_creating`.
`tf_tree serve` does not exist (`PHASE2.md` §0.0).

### `TopologyChurn`

The topology mutated `TOPO_BLOCKS` times during one plan compilation. Almost
certainly a bug: topology should be near-static after startup.

---

## `doctor` checks and what to do about each

| Check | What it means | Response |
|---|---|---|
| `cycle` | A parent chain that never reaches a root | A publisher re-parented a frame under its own descendant. The mutation should have been rejected; file a bug |
| `unclaimed-dynamic` | A dynamic edge with no live writer | The publisher never started, or exited without releasing. Expected briefly at startup; sustained means a dead node |
| `multi-writer` | More than one PID published to one edge | Configuration error: two nodes own the same edge. Both PIDs are named |
| `short-buffer` | Ring shorter than the observed publish latency | Raise that edge's capacity. This warning precedes `Extrapolation`/`SlotRecycled` outages |
| `inconsistent-rate` (`TFT008`) | A frame published at a wildly varying rate — the spread of its inter-arrival intervals about their own centre, **not** a comparison against a declared rate (that is `TFT007`) | Often benign (an event-driven publisher), sometimes a struggling node. Reports **not run** when no edge has enough intervals, or every edge that had has stopped publishing — then read `TFT009` |
| `TFT009` | A gap between two retained stamps far above that edge's own median, and the gap that **has not ended** — no sample since, on a live arena | A publisher dropped samples, or stopped. **Not run** when it judged no edge: too few retained intervals (an edge sized `rate_hz * secs <= 4` never gets there), a stamp that goes backwards (read `TFT018`), or every stamp at one instant |
| `TFT013` | An edge declared dynamic that nothing has ever published to | The publisher never started. **Not run** inside a grace period measured against the longest-running publisher, so bringup is not accused; where nothing has published at all; and where publishers exist but no edge yields the two samples a median needs — the report names which: a ring too small for two (`rate_hz * secs <= 2`) is a sizing fix, a large ring holding one needs more data, and a full ring with no positive cadence is `TFT009`'s and `TFT018`'s subject. `TFT017` is the id for an edge whose writer is gone |
| `unreachable` | Frames not reachable from the main root | A subtree is detached: a missing static declaration or a publisher that has not started |
| `out-of-order` (`TFT018`) | Stamps arriving non-monotonically | A publisher restarted without resetting its clock, or two sources feed one edge |
| `TFT014` — *slot N pid P, byte free* | A participant record nothing will reassign: the kernel released its lock byte while the arena record still says `LIVE` (or `RESERVED`). **Usually the process is gone; check before you act.** A publisher that never took a byte reads identically while running ([`0031`](./decisions/0031-the-participant-record-with-no-byte.md)) | **Three things reclaim it, none of them `doctor`**: the owner's assigner when a grant walks past that index; the owner's socket-hangup callback; and **any read-write participant's `Tree::reap_participants()`**, the only one that reaches the *owner's own* slot. So attach a read-write consumer and sweep, not stop the fleet. Count the spent slots (`N of 64`); at 64 every attach fails `NoParticipantSlots`. **Two cases have no repair**: a slot *held* by a fork inheritor (the separate fork finding; [`0030`](./decisions/0030-the-atfork-handler-and-inherited-descriptors.md)), and an owner that died **with no read-write survivor calling `Tree::inherit_ownership`** (*The arena's owner died*). For those, stop every attached process; `SIGTERM` is not one, because nothing installs a handler. `tf_tree participants` shows the slots as `stale`. See [`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md) |
| `TFT014` — *slot N pid P, byte still HELD* | The **fork** case: a forked child inherited the open file descriptions, so the byte is held on behalf of a process that no longer exists. Reported for a read-only parent too, where there is no arena record (*the record is FREE (no arena record: a read-only participant, D18)*). **Not reported for a participant in another PID namespace** ([`0033`](./decisions/0033-the-identity-record-cannot-name-a-namespace.md)); on a build predating it, check the namespace before acting | **Do not look for a reaper**, and nothing may run one: the kernel's answer is *held*, and overruling it with a `/proc` guess would evict a running participant. Stop the child and start workers with a method that inherits no descriptors: `multiprocessing`'s `spawn`, or fork+exec. The byte returns when the last inheritor exits. Same root cause as *The tree works in the parent and everything fails in a forked child* |
| `TFT014` — *slot N, no pid recorded, byte free* | The same shape with **no process named**: no identity record was written or readable and the arena record's pid is zero, which is what `fill_slot` leaves when a registrant dies between claiming the slot and publishing into it | Nothing to check; the finding prints no pid. Reclaimed as the `byte free` row: a read-write peer's `Tree::reap_participants()`. If repeated, something is being killed inside registration |
| `TFT014` — *slot N pid P, byte not probed* | The same shape, seen by a run with **no kernel answer about the byte**: usually one that opened no lock file (`--from-bag`, the fixture); **an `--attach` run reaches it too** for any slot whose `F_OFD_GETLK` errored, since a failed probe is not reported as *free* | A weaker claim than `byte free`. After `--from-bag` or the fixture, run `doctor --attach`, the only source that opens the rendezvous. **If already attached**, the probe failed: check fd limits and the runtime directory's mount and re-run |
| `TFT019` | A **run** of at least eight of those rejections, on an edge in `SystemDomain` (wall clock, tag 0) | Not a publisher fault: the clock stepped (NTP, leap second). Move anything published at rate to a steady or PTP domain. Passes with a `note:` below the run length, skips naming the tag on any other domain, and skips with `TFT018` on a live arena. Use `tf_tree doctor --from-bag run.mcap` |

---

## Performance triage

Cost is ~5 ns fixed plus ~70 ns per *dynamic* step (static edges constant-fold), so
count dynamic edges, not frames. Reuse the `Plan`: compiling costs about two
evaluations. `ScLerp` (default) costs ~44 ns per evaluation, `LerpSlerp` ~16. Pin
before measuring; unpinned runs swing by more than 30%
([`benchmarks/tf2.md`](./benchmarks/tf2.md)).

---

## How big is my arena, and how much of it did I over-declare?

`Capacity` is denominated in **slots**; tf2 evicts by **time**.
`Capacity::history(1000.0, 10.0)` asks for 10 000 slots and reserves **16 384**
(`mask == capacity - 1` is the ring's hot index); against a 10 Hz publisher that
ring retains **27 minutes**. `tf_tree doctor` and `tf_tree top` print the
declaration in bytes (`top --edge <id>` per edge):

```text
rings: 19072 slots declared = 1.31 MiB over 4 edge(s); 12600 used = 885.9 KiB (66%);
       at most 9532 slots = 670.2 KiB is next_pow2 rounding
arena = 16704 B fixed + 320 B/edge + 144-176 B/frame + 72 B/slot
```

"At most" because the pre-rounding request is **not stored**: a ring of capacity
`C` was declared with a count in `[C/2 + 1, C]`.

### The sizing formula

```text
arena = 16 704 B fixed          header + participant table + participant counters
      +    320 B per edge       claim 64 + edge record 128 + edge counters 128
      +  144-176 B per frame    frame record 64 + 4 topology blocks x 12 + intern slots
      +     72 B per slot       stamp 8 + pose 64 (one cache line)
```

The per-frame term is a range because the intern table is `next_pow2(2 x frames)`
slots of 16 B. Constants are checked against `crates/tf_tree_arena/src/layout.rs`
by `crates/tf_tree_cli/src/sizing.rs`'s tests.

### What over-declaring costs

Since [`0021`](./decisions/0021-the-idle-arena-is-resident-because-of-its-alignment.md)
unwritten slots are demand-faulted and never resident, but they cost *reservation*
(address space, the `.tft` file, segment transfers, strict-overcommit headroom).
This is capacity planning, not a leak, and no `TFT0xx` check fires on it.
