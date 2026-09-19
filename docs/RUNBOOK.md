# tf_tree — operator runbook

> Required by [`PHASE2.md`](./PHASE2.md) §13.

Organised by **symptom**. There is no `tf_treed` and `tf_tree serve` is not built
(`PHASE2.md` §0.0): nothing below may name either as a remedy. Check ids are in
`crates/tf_tree_cli/src/catalogue.rs`'s module docs and `PHASE5.md` §6.

## First moves

```bash
tf_tree doctor --attach      # every check against the running arena
tf_tree tree --attach        # live topology, per-edge rate, occupancy, writer PID
tf_tree echo <target> <source> --attach --rate
tf_tree participants         # who is attached — works even with no arena
```

Without `--attach` these operate on an in-process fixture. **Start with
`tf_tree participants`**: it never maps the arena, so it answers when the segment is
gone, mismatched or the owner is wedged.

## Lookups and writers are failing

| Error | Cause and response |
|---|---|
| `NoData { edge }` | Nothing has ever been published to that edge: **startup ordering** (`tf_tree tree` shows head 0), or the publisher has not claimed it (`unclaimed-dynamic`) |
| `Extrapolation { edge, requested, oldest, newest }` | Stamp outside the retained window. `requested > newest`: the consumer's clock is ahead or the publisher stalled. `requested < oldest`: the ring is too shallow; raise the edge's capacity (`short-buffer` warns first) |
| `Disconnected { target, source, cut_at }` | No path; `cut_at` is where the walk ran out of parent, usually a publisher that has not started (`unreachable`) |
| `TopologyChanged { plan, current }` | Your `Plan` predates a topology mutation: recompile, do not retry-loop. Repeats in steady state mean a publisher is re-parenting continuously, a bug |
| `TimeDomainMismatch { expected, got }` | A lookup crossed a time-domain boundary; do not mix domains in one plan |
| `SlotContended` / `SlotRecycled` | A slot stayed mid-write for `SEQ_RETRY_LIMIT` attempts (writer descheduled) / the ring lapped the reader. Returned, not retried. Raise the edge's capacity; `doctor` warns at 80% occupancy |
| `EdgeAlreadyClaimed { owner_slot }` | Two nodes publish the same edge. Names the edge and the **participant slot**, not a pid (C's `tft_error` carries the slot in `frame_a`, **except `tft_bridge_create`**, which overwrites `frame_a`/`frame_b` with the refused link's `FrameId`s); `tf_tree participants` maps slot to pid. `multi-writer` (`TFT001`) is **not** the tool: a refused claim never publishes. Stop one node; from ROS the bridge's conflict policy (`FirstWriterWins`) is where it surfaces |
| `ClaimRevoked { edge }` | This writer was judged dead and its claim reaped, then it resumed (a stall). Stop publishing and re-claim; never retry the push. A stalled writer still holds its kernel lock ([`PHASE2.md`](./PHASE2.md) §6.1), so suspect the `ClaimRecord` path or a hand-run reap |
| `ReadOnly` on `claim` / `reparent` / `frame` | Attached read-only, the **consumer default and the only real safety boundary** (enforced by the MMU). Publish with `AttachMode::ReadWrite`; a read-only participant cannot intern a new frame |

### `NonMonotonicStamp { edge, last, got }`

A push arrived with a stamp older than the edge's newest (equal stamps are
accepted): a publisher restarting without resetting its clock, or two sources on
one edge. `out-of-order` (`TFT018`) reports it from observed history.

**Check the edge's domain first.** A *burst* on a **`SystemDomain`** edge (wall
clock, tag 0) is usually a `CLOCK_REALTIME` step (NTP, leap second) that makes
invariant 6 reject every stamp until the clock catches up; restarting the
publisher will not help. `TFT019` fires on a **run** of at least eight consecutive
rejected pushes, passes with a `note:` below that, and on any other tag skips and
names the tag.

```
tf_tree doctor --from-bag run.mcap
```

**Neither `--attach` nor `--from-file` can answer `TFT018`/`TFT019`**: a live ring
is read while written, and a frozen `.tft` holds only accepted pushes. Both skips
say so; **their silence on an arena is not an all-clear.**

**The fix is a domain that cannot step**: publish at rate on `SteadyDomain` (tag
3), or your own `Domain` if the clock is PTP-disciplined; reserve `SystemDomain`
for stamps comparable to outside wall-clock time.

## Shared memory and startup

### `HandshakeRejected`

A **live, serving owner** refused this attach. The message stops at the status and
the owner's side of the comparison:

```text
the arena owner refused this attach: LayoutMismatch (owner format_version 3, layout_hash 0x3D104195) (HandshakeRejected)
```

**The remedy is this table** ([`0055`](./decisions/0055-the-recovery-capacity-a-fleet-cannot-add-later.md)
step 7; Erratum in [`PHASE2.md` §3.7](./PHASE2.md#37-attach)).

| `status` | What the owner compared | What to do |
|---|---|---|
| `VersionMismatch` | this binary's `FORMAT_VERSION` against the running arena's, **first** | rebuild every participant from one release and restart them together. There is no partial upgrade path |
| `LayoutMismatch` | same version, a different record layout | rebuild every participant, as above. The owner's `layout_hash` is in the message; this binary's is printed by `tf_tree doctor --explain-version` **built from the same commit as the refused process** |
| `BootIdMismatch` | the boot id in the attach request against the one in the **arena header** | **not "the arena outlived a reboot"** — a serving owner proves it did not. The two processes disagree about which boot this is ([`PHASE2.md`](./PHASE2.md) §3.3): one failed to read `/proc/sys/kernel/random/boot_id` and substituted all-zeros, or something presents a different one to it (a sandbox masking `/proc/sys`); or `tf_tree_ipc::procstat::boot_id` rejects a UUID with trailing junk that `tf_tree::tree::boot_id` ignores. Check the file from both processes; if well formed and identical, report a bug |
| `NoParticipantSlots` | every slot, against **both** the arena's participant records *and* the lock bytes | triage is the *`ParticipantTableFull` / `NoParticipantSlots`* section below. **There is no `--participants` flag**: capacity is fixed at construction ([`PROJECT.md`](./PROJECT.md) §5 D4) |
| `ModeNotPermitted` | **nothing in this workspace sends it**: only somebody else's `assign` closure can | attach read-only, the consumer default ([`PROJECT.md`](./PROJECT.md) §5 D18). Against an owner built on `tf_tree_ipc` with its own `assign`, that policy's author is who to ask; against a `tf_tree` owner, report it |
| `Malformed` | nothing — it could not decode the request | **or a refusal this build has no name for**: every unknown status decodes to `Malformed` (`HelloStatus::from_u32`). Confirm both sides are the same release before reading this as corruption |

`Ok` has no row. A new `HelloStatus` fails `status_is_a_refusal` (`tf_tree_ipc`'s
`error.rs`) and owes this table a row, checked by `tf_tree_cli`'s `tests/runbook.rs`.

### Header and lease errors

| Error | Cause and response |
|---|---|
| `ReparentError::LockContended { owner_slot }` | Another live participant holds the topology lock (a dead holder's is released): contention, and the one `reparent` error a caller loops on. Sustained contention is a design smell ([`PHASE2.md`](./PHASE2.md) §1, A2). `owner_slot` can be `None` while the holder has not published its slot. **When it will not clear**: a `fork` child inherits a *dead* parent's held byte (`TFT014`); stop the child, or start workers with `spawn` |
| `ReparentError::TopologyLease { raw_os_error }` | `fcntl` on the topology byte failed for a reason that is not contention; **not** retryable. Check the runtime directory is on a local filesystem ([`PHASE2.md`](./PHASE2.md) §3.1) and descriptors remain |
| `LayoutMismatch { found, expected }` / `VersionMismatch { found, expected }` | The mapped header's check: binaries from different commits disagree on layout (**rebuild every participant**), or a different `FORMAT_VERSION` wrote the segment (recreate the arena). A refusal that never reached a segment ends `(HandshakeRejected)` and is that section |
| `HeaderInconsistent` | Region offsets do not match the geometry the capacities imply: a peer bug, a scribbled byte, or the same record sizes with other capacities. Treat as corruption and recreate the arena |
| `Unsealed` | A peer offered a segment without `F_SEAL_SHRINK`/`F_SEAL_GROW`, which could be truncated under a reader and `SIGBUS` it. The peer is buggy or hostile |
| `TopologyChurn` | The topology mutated `TOPO_BLOCKS` times during one plan compilation: almost certainly a bug |
| two processes see different data | Two `instance_uuid`s exist (`doctor` prints the uuid and runtime dir on both): a runtime-directory or domain mismatch (container mounts, `ROS_DOMAIN_ID`) |
| rendezvous on NFS/CIFS | Their lock semantics are unusable, so `open()` rejects them. Point `TF_TREE_RUNTIME_DIR` at local storage |
| `open()` created an arena unexpectedly | `Open` defaults to the consumer (`ReadOnly` + `CreatePolicy::Never`) ([`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md)), so the process asked for it. A consumer should use `Open::await_open`, a second publisher `Open::require_create(true)` (`OpenError::ArenaAlreadyLive`) |

### `ParticipantTableFull` / `NoParticipantSlots`

More than `max_participants` processes attached. **There is no flag**; first look
for a leak, a participant that exited without releasing its slot.

**Two commands, two tables.** `tf_tree participants` reads the **lock file only**:
it is blind to a `TreeBuilder::build_shared` creator, which registers a `LIVE` arena
record and takes no byte
([`0031`](./decisions/0031-the-participant-record-with-no-byte.md)).
**`tf_tree doctor --attach` reads the arena's table**: `TFT014` counts spent slots
and names each pid (limits in `tft014`, `crates/tf_tree_cli/src/checks.rs`); its
**`a record left behind — … the lock byte is free`** can accuse a running publisher
that never took a byte, so check the pid.

A dead **owner** is in contract; a surviving read-write peer's sweep is the
collector ([`PHASE2.md` §3.9](./PHASE2.md#39-teardown)). A byte-less
`build_shared` participant is **out of contract** where published into a
rendezvous by hand
([`PHASE2.md` §3.1](./PHASE2.md#31-the-sharing-boundary-is-the-runtime-directory--normative)):
use `tf_tree::Open`. Until fixed, **do not sweep** (`Tree::reap_dead` /
`reap_participants`, `tft_tree_reap_dead`, `Tree.reap_dead()`): it frees the
records and takes the claims of running publishers. No `tf_tree` subcommand sweeps.

Capacity is `tf_tree_arena::layout::DEFAULT_MAX_PARTICIPANTS`.

### Attaching to a running robot

`--attach` joins the arena that `$TF_TREE_RUNTIME_DIR`, `$TF_TREE_DOMAIN` and
`$TF_TREE_NAME` resolve to (override with `--domain` / `--name`); **the commonest
mistake is a domain mismatch**. It is **read-only** and **will not create**
(`--rw`, `--create` opt in; D18). `doctor --attach` lists the checks it could not
run under `not run:`: `multi-writer` cannot see a writer already replaced, and
`short-buffer` needs arrival lateness, which nothing in the arena records.

### `doctor --from-bag` verdicts on `TFT010`, `TFT011`, `TFT017`

`TFT010`/`TFT011` are built on the `PHASE5.md` §5 counters, which **lookups**
increment; a recording's arena, the fixture, or a live arena attached before its
first lookup has all-zero counters, so they skip rather than pass. Run one consumer
and re-run. `TFT017` warns on every edge of a recording (it has **no writer**); a
fleet whose publishers all stopped looks identical, so ignore it on a recording,
not on an `--attach`.

### Reading `tf_tree participants`

`state = live`: the kernel holds the slot's lock byte (a `SIGSTOP`ped process reads
**live**, correctly). `state = stale`: the byte is released but the identity record
remains; the process is gone, a reaper will collect it, and `tf_tree doctor
--attach --rw` forces one. `comm = <no record>`: momentary; re-run. `mode = ro`:
cannot publish or corrupt anything. An empty machine prints "no lock file" and
**exits zero**.

### The tree works in the parent and everything fails in a forked child

Errors are `ChildDetached` from every entry point. A shared arena is mapped
`MADV_DONTFORK`, so the child has no mapping where the arena was. Open a new tree
in the child, or `exec`; there is no repair. Python's `multiprocessing` defaults to
`fork` on Linux: use `spawn`.

**The child is also holding a participant slot.** `fork` shares the open file
descriptions, so the child keeps the parent's rendezvous socket *and* lock byte:
the owner never sees a `HUP` and the kernel keeps answering "held". `doctor
--attach` reports it as the second `TFT014` shape (*byte still HELD*), the one leak
nothing may reclaim. The slot returns when the last inheritor exits. A participant
in another PID namespace is not reported since
[`0033`](./decisions/0033-the-identity-record-cannot-name-a-namespace.md).

For a read-only parent (D18: a byte, no arena record) `doctor` reports *the record
is FREE (no arena record: a read-only participant, D18)* and `tf_tree
participants` shows the slot `live` with the dead parent's pid.

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
dead ([`0043`](./decisions/0043-owner-lost-is-a-question-about-the-owner.md)); a
latched survivor cannot inherit when the *second* owner dies.

**Nothing calls this for you, and that decides whether your fleet can recover**
([`PHASE2.md` §3.5](./PHASE2.md#35-ownership-migrates-the-data-plane-never-pauses--normative)).
Check:

- **Is any survivor read-write?** `inherit_ownership()` answers
  `Inheritance::ReadOnly` on a read-only attachment, so **a fleet of read-only
  consumers cannot rescue itself** (D18). Open one process read-write even if it
  never publishes. **Necessary and not sufficient**
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
  `tree.inherit_ownership()` and `tree.reap_dead()`, with
  `tf_tree.open(mode="rw")`.
- **Does that survivor call it?** `tf_tree participants` shows who is attached, not
  who is looking. Every failed attempt restores the attachment and hands the byte
  back.

**When no survivor can or will inherit, stop every attached participant** and
start again. `SIGTERM` is enough but leaks the arena record of a participant
stopped while the arena survives (`TFT014`).

#### An owner is not dead until its exit ends

`owner_lost()` answers `true` once the survivor's attach connection has hung up and
the last open file description holding byte 0 has closed ([`PHASE2.md`](./PHASE2.md)
§3.5, NORMATIVE). For a dying owner that is the **end of its exit**: the kernel
writes any core dump and tears down the address space first, and an owner whose
`fork` child outlives it keeps both until the child exits
([`0057`](./decisions/0057-an-owner-is-not-dead-until-its-files-close.md)).

**During the window** lookups and existing publishers carry on; nobody inherits and
no join completes (`open()` blocks, then is refused with
`ArenaHeldButUnreachable`, `ownership_held: true`). The owner's claimed edges stay
refused until a survivor calls `reap_dead`.

**The trade, per process: a crash dump, or recovery bounded by teardown.** Make it
for **every process that may hold the role**
([`0055`](./decisions/0055-the-recovery-capacity-a-fleet-cannot-add-later.md)).

- **The dump window is the crash helper's run**; check `/proc/sys/kernel/core_pattern`
  (a leading `|` pipes to apport or systemd-coredump). To suppress dumps set a core
  limit of **1 byte** (`prlimit --core=1:1 -- <cmd>`, `LimitCORE=1`, or
  `setrlimit(RLIMIT_CORE, {1, 1})`); **`ulimit -c 0` is not it** when the host pipes
  dumps.
- **The teardown is not a setting**: about **100 ms per GiB of dirty 4 KiB
  anonymous memory** on the host `0057` measured.

### `ArenaHeldButUnreachable`

Somebody holds a live arena and nothing serves it, so [`PHASE2.md`](./PHASE2.md)
§3.4's split-brain check refuses to create a second. **The ordinary cause is not a
fault**: the owner exited and a healthy survivor still has the arena mapped, so
every open times out while any survivor lives (*The arena's owner died*). Refusals
last at least until a dead owner's exit ends.
`tf_tree participants` lists the holders.

**Reach for inheritance before a restart**: a surviving read-write participant
that polls `owner_lost()` ends this state without stopping anything
([`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md): no daemon
polls for you; D18: a read-only consumer cannot serve). While any participant byte
is held no process can *join*, so **nothing you start now can become the heir**
([`0055`](./decisions/0055-the-recovery-capacity-a-fleet-cannot-add-later.md));
provision read-write pollers in advance.

**When there is no heir, stop every participant**, read-only consumers and
`tf_tree top --attach` included. The kernel releases each lock byte on death and
frees the segment when its last mapping drops (§3.9), so the next `open()` creates
cleanly. Restarting the publisher alone does not help.

If a holder must keep running and its arena is written off, the escape hatch is
**`CreatePolicy::Always`** — [`PHASE2.md`](./PHASE2.md) §3.4's `--force-new`, a
policy on the creating process and **not** a flag on `tf_tree`. It creates a
*fresh* arena over the same name and abandons this one. It skips §3.4's
participant scan and nothing else, so it still takes the ownership byte and
participant byte **0** (the creator's slot; joiners get `>= 1`). It **creates** when
only slots `>= 1` are held (the case the hatch is for) and **refuses** when slot 0
is held (`Session::release_ownership` can leave a live non-owner there) or the
ownership byte is held by a process that is not serving.

**The error gives facts, not a remedy** (`0055` step 6): the participant mask, the
lowest held slot and its pid, and `ownership_held` (`tf_tree participants` does not
show the ownership byte), ending `(ArenaHeldButUnreachable)`. Read your row off the
two facts:

| participant bytes | ownership byte | what to do |
|---|---|---|
| lowest slot is 0, nothing else | free | stop the process on slot 0; an ordinary open then creates. `CreatePolicy::Always` takes slot 0 or nothing |
| lowest slot is 0, nothing else | held | **usually one process holds both** (a creator takes both on one file description); stopping it releases both. If the ownership byte stays held, a second process has it and goes too |
| lowest slot is 0, others too | free | stop slot 0's holder first; the rest are ordinary participants and §3.4's hatch then applies |
| lowest slot is 0, others too | held | the ownership byte and slot 0 both have to be free, in either order; the hatch then applies to what is left |
| lowest slot is 1 or above | free | the stranded-participant case the hatch is for: `CreatePolicy::Always` **abandons** this arena, leaving the survivors publishing where nobody can reach them. Reach for inheritance first |
| lowest slot is 1 or above | held | **stopping one is not enough**: the ownership holder never bound a socket, and participant bytes are held too. Stop the ownership holder, then you are on row 5 |
| `nobody attached` … `held for the whole open timeout` | held | ownership was held throughout by a process that never served; stop it |
| `no byte was held at the open deadline` | — | the blocker let go while you were timing out; retry |

**A forced create needs a layout and read-write mode**
([`0004`](./decisions/0004-builder-time-edge-declaration.md)), else
`NoLayoutToCreate` / `ReadOnlyCannotCreate`: `tf_tree::Open::new()
.mode(AttachMode::ReadWrite).create(CreatePolicy::Always)
.layout_if_creating(builder).open()` (`shm` feature, Linux only).

Use it **only** when the holder is confirmed unrecoverable:

- **The old arena stays alive**: survivors keep publishing into a segment nobody
  else can reach (two `instance_uuid`s), and it never recovers a participant slot.
- **The survivors' claim leases alias the new arena's.** A lease is a byte at
  `CLAIM_BASE + edge_id` in the same lock file (§6.1) and the replacement numbers
  its edges from zero, so claiming an id a survivor holds gives `LeaseContended`
  until the survivor exits.
- **Against a live non-owner holder of byte 0 it is refused** and `open()` times
  out with `ArenaHeldButUnreachable { holder_slots: 0x1, first_slot: Some(0),
  first_pid: <the holder>, ownership_held: false }`: row 1 of the remedy table.
  **Do not retry.** Pinned by
  `defect_201_release_ownership_strands_a_live_non_owner_on_byte_0`.

`open()` probes the socket before taking the ownership byte, so the policy
abandons an unreachable arena, never a served one.

### `FrameNotDeclared`

A read-only participant asked for a frame nobody has declared yet.

**First, check the consumer did not create the arena.** A consumer passing
`CreatePolicy::IfAbsent` **and** a layout, starting before any publisher, creates
the arena with *its* topology, permanently; a single read-only participant on an
arena with no edges is the signature. Read-only implies `CreatePolicy::Never`
([`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md) §2).

**Otherwise the publisher has not started: wait.** `Tree::await_frames(["map",
"base_link"], deadline)` blocks until the frames exist; use it in a consumer's
startup path. **Frames arriving during operation** are `frame_headroom` /
`edge_headroom`, sized at build time; exhaustion is a typed error naming the knob.
Pre-declare the static structure in the topology config
(`crates/tf_tree_bridge/src/config.rs`'s schema, accepted by `ros/tf_tree_ros` and
Python's `build`/`open` ([`0041`](./decisions/0041-python-declares-a-topology-the-way-everything-else-does.md)),
written by `tf_tree topology --discover`) and pass it as `layout_if_creating`.

## `doctor` checks and what to do about each

| Check | What it means | Response |
|---|---|---|
| `cycle` | A parent chain that never reaches a root | A publisher re-parented a frame under its own descendant. The mutation should have been rejected; file a bug |
| `unclaimed-dynamic` | A dynamic edge with no live writer | The publisher never started, or exited without releasing. Expected briefly at startup; sustained means a dead node |
| `multi-writer` | More than one PID published to one edge | Configuration error: two nodes own the same edge. Both PIDs are named |
| `short-buffer` | Ring shorter than the observed publish latency | Raise that edge's capacity. This warning precedes `Extrapolation`/`SlotRecycled` outages |
| `inconsistent-rate` (`TFT008`) | A frame published at a wildly varying rate — the spread of its inter-arrival intervals, **not** a comparison against a declared rate (that is `TFT007`) | Often benign (an event-driven publisher), sometimes a struggling node. **Not run** when no edge has enough intervals, or every such edge has stopped publishing — then read `TFT009` |
| `TFT009` | A gap between two retained stamps far above that edge's own median, and the gap that **has not ended** — no sample since, on a live arena | A publisher dropped samples, or stopped. **Not run** when it judged no edge: too few retained intervals (an edge sized `rate_hz * secs <= 4` never gets there), a stamp that goes backwards (read `TFT018`), or every stamp at one instant |
| `TFT013` | An edge declared dynamic that nothing has ever published to | The publisher never started. **Not run** inside a grace period measured against the longest-running publisher, where nothing has published at all, or where no edge yields the two samples a median needs — the report names which. `TFT017` is the id for an edge whose writer is gone |
| `unreachable` | Frames not reachable from the main root | A subtree is detached: a missing static declaration or a publisher that has not started |
| `out-of-order` (`TFT018`) | Stamps arriving non-monotonically | A publisher restarted without resetting its clock, or two sources feed one edge |
| `TFT014` — *slot N pid P, byte free* | A participant record nothing will reassign: the kernel released its lock byte while the arena record still says `LIVE` (or `RESERVED`). **Usually the process is gone; check before you act.** A publisher that never took a byte reads identically while running ([`0031`](./decisions/0031-the-participant-record-with-no-byte.md)) | **Three things reclaim it, none of them `doctor`**: the owner's assigner when a grant walks past that index; the owner's socket-hangup callback; and **any read-write participant's `Tree::reap_participants()`**, the only one that reaches the *owner's own* slot. So attach a read-write consumer and sweep, not stop the fleet. Count the spent slots (`N of 64`); at 64 every attach fails `NoParticipantSlots`. **Two cases have no repair**: a slot *held* by a fork inheritor ([`0030`](./decisions/0030-the-atfork-handler-and-inherited-descriptors.md)), and an owner that died **with no read-write survivor calling `Tree::inherit_ownership`** (*The arena's owner died*). For those, stop every attached process; `SIGTERM` is not one, because nothing installs a handler. See [`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md) |
| `TFT014` — *slot N pid P, byte still HELD* | The **fork** case: a forked child inherited the open file descriptions, so the byte is held on behalf of a process that no longer exists. Reported for a read-only parent too, where there is no arena record. **Not reported for a participant in another PID namespace** ([`0033`](./decisions/0033-the-identity-record-cannot-name-a-namespace.md)) | **Do not look for a reaper**: the kernel's answer is *held*, and overruling it with a `/proc` guess would evict a running participant. Stop the child and start workers with a method that inherits no descriptors: `multiprocessing`'s `spawn`, or fork+exec. The byte returns when the last inheritor exits |
| `TFT014` — *slot N, no pid recorded, byte free* | The same shape with **no process named**: the arena record's pid is zero, which `fill_slot` leaves when a registrant dies between claiming the slot and publishing into it | Reclaimed as the `byte free` row: a read-write peer's `Tree::reap_participants()`. If repeated, something is being killed inside registration |
| `TFT014` — *slot N pid P, byte not probed* | The same shape, seen by a run with **no kernel answer about the byte**: usually one that opened no lock file (`--from-bag`, the fixture); **an `--attach` run reaches it too** for any slot whose `F_OFD_GETLK` errored | A weaker claim than `byte free`. After `--from-bag` or the fixture, run `doctor --attach`. **If already attached**, the probe failed: check fd limits and the runtime directory's mount and re-run |
| `TFT019` | A run of at least eight rejected pushes on a `SystemDomain` edge | See `NonMonotonicStamp` |

## Performance triage

~5 ns fixed plus ~70 ns per *dynamic* step (static edges constant-fold): count
dynamic edges, and reuse the `Plan` (compiling costs about two evaluations).
`ScLerp` (default) ~44 ns per evaluation, `LerpSlerp` ~16
([`benchmarks/tf2.md`](./benchmarks/tf2.md)).

## How big is my arena, and how much of it did I over-declare?

`Capacity` is denominated in **slots**; tf2 evicts by **time**.
`Capacity::history(1000.0, 10.0)` asks for 10 000 slots and reserves **16 384**
(`mask == capacity - 1` is the ring's hot index). `tf_tree doctor` and
`tf_tree top` print the declaration in bytes; "at most" because the pre-rounding
request is **not stored**:

```text
rings: 19072 slots declared = 1.31 MiB over 4 edge(s); 12600 used = 885.9 KiB (66%);
       at most 9532 slots = 670.2 KiB is next_pow2 rounding
arena = 16704 B fixed + 320 B/edge + 144-176 B/frame + 72 B/slot
```

### The sizing formula

```text
arena = 16 704 B fixed          header + participant table + participant counters
      +    320 B per edge       claim 64 + edge record 128 + edge counters 128
      +  144-176 B per frame    frame record 64 + 4 topology blocks x 12 + intern slots
      +     72 B per slot       stamp 8 + pose 64 (one cache line)
```

The per-frame term is a range because the intern table is `next_pow2(2 x frames)`
slots of 16 B; `crates/tf_tree_cli/src/sizing.rs`'s tests check the constants.

### What over-declaring costs

Unwritten slots are never resident
([`0021`](./decisions/0021-the-idle-arena-is-resident-because-of-its-alignment.md))
but cost *reservation* (address space, the `.tft` file, segment transfers,
overcommit headroom); no `TFT0xx` check fires.
