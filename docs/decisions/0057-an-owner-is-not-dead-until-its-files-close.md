# 0057: an owner is not dead until its files close

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** step 1 (the `PHASE2.md` §3.7 step 9 correction, the index
row and the evidence register's probe row) landed beside this record as a
`draft` (#333); step 2 (#337) landed the specs and the runbook; step 3 (this
change) landed the release-visible prose and ran the pin's mutants; step 4
(#341) runs `shm_torture`'s recipes, and the bounded rendezvous reaps in
`just shm-check`, `just shm-rendezvous` and `just no-network`, under
`RLIMIT_CORE=1`, with its runner-side verification (the `AN` arm on the runner
and the first crash-points nightly after landing) still owed; step 5
has not.

## Context

§3.5's recovery starts when a survivor learns the owner died, and the documents
said that happens *"in microseconds"* ([`PHASE2.md`](../PHASE2.md) §3.7 step 9;
`Tree::owner_lost`'s rustdoc; [`PROJECT.md`](../PROJECT.md) D17, quoted at
`crates/tf_tree_ipc/src/server.rs:15`). The half about timeouts is true and
untouched. The half about microseconds holds only for a small process that does
not dump core: the kernel closes a process's files when its **exit** reaches them,
after any core dump and the address-space teardown. The socket's hangup and the
release of every OFD lock the process holds, byte 0 included, wait for both.

**A forked child sharing the owner's descriptions makes it worse.** A `fork`
without `exec` shares every open file description ([`PHASE2.md`](../PHASE2.md)
§6.2; [`0030`](./0030-the-atfork-handler-and-inherited-descriptors.md) question
2), so the listener, every accepted connection and byte 0 stay held until **that
child** exits, possibly never. Every figure below is an owner with no forked child.

**The bound is therefore the last close of the two kernel objects `owner_lost()`
asks about**: the peer end of the survivor's attach connection and the open file
description holding byte 0 ([`0043`](./0043-owner-lost-is-a-question-about-the-owner.md)).
A survivor whose own connection went to an earlier owner, before a migration,
learns of the current owner's death through byte 0 alone.

### What the documents promise, swept

Sites that overstated the speed, and what each should say (all fixed by steps 1-3
except where noted):

| Site | Should say |
|---|---|
| `PHASE2.md` §3.7 step 9; `Tree::owner_lost` rustdoc; `tf_tree_ipc` `client.rs` module doc; `PROJECT.md` D17 (quoted at `client.rs`/`server.rs`) | the event: last description on the socket and byte 0 closes |
| `PHASE2.md` §3.3 *Verified behaviour* ("released by the kernel, immediately"); `tf_tree_ipc` crate doc and `README.md` | "immediately **at the end of the holder's exit**"; the NFS contrast in `runtime_dir.rs`/`error.rs` still holds |
| `PHASE2.md` §11.4 kill marker ("tens of microseconds") and §0.0's `shm_torture` row | sub-millisecond (0.3 ms median reap, plain child, `SIGKILL`) |
| `PHASE2.md` §12.2 *owner kill → new owner serving* (0.6-1.2 ms p50) | not wrong: a `SIGKILL` of a small owner, say so |

## What was measured

**Dev host, 2026-09-13.** Linux `6.8.0-138-generic`, 8 CPUs, 31 GiB,
`core_pattern=|/usr/share/apport/apport …`, `core_pipe_limit=10`, soft
`RLIMIT_CORE` 0, THP `madvise` (`AnonHugePages` 0). The probe was a standalone
cargo project outside the repository (path dependency on `crates/tf_tree` with
`shm`, `--release`): an **owner** (`Open::new().mode(ReadWrite).create(IfAbsent)`,
N MiB of dirty ballast, `abort()` or a parent `SIGKILL`), a read-write
**survivor** (`owner_lost()` in a tight loop, then §3.5's `inherit_ownership()`
loop), a **joiner** (`create(Never).timeout(20 ms)` retries from the death stamp)
and a **parent** (reap, `core_dumped()`, `/proc/<pid>/status` `CoreDumping:`,
`wchar`). Six arms interleaved, 20 rounds, 120 trials. Raw CSV not in the repository.

| Arm | How the owner died | Owner RSS | death → `owner_lost()` true, median / p90 / max | `core_dumped` |
|---|---|---|---|---|
| `K` | `SIGKILL` | 2.7 MiB | 0.237 / 0.310 / 0.359 ms | 0 / 20 |
| `AN` | `abort()` under `prlimit --core=1:1` | 2.7 MiB | 0.292 / 0.437 / 0.494 ms | 0 / 20 |
| `A0` | `abort()`, host default (pipe to apport) | 2.7 MiB | **1097.1 / 1105.4 / 1107.9 ms** | 20 / 20 |
| `A256` | `abort()`, host default | 259 MiB | **1123.2 / 1127.1 / 1132.8 ms** | 20 / 20 |
| `A1024` | `abort()`, host default | 1027 MiB | **1200.1 / 1211.3 / 1215.4 ms** | 20 / 20 |
| `K1024` | `SIGKILL` | 1027 MiB | **98.0 / 103.1 / 106.6 ms** | 0 / 20 |

- **`owner_lost()` went true 0.04-0.41 ms before the reap in 120 of 120 trials**
  and never while `CoreDumping: 1` was still readable. Per-arm median gap to the
  first `Inherited`: 0.16-0.27 ms; to the joiner's first success: 0.41-0.48 ms.
- **Every join attempt in the window (dump plus teardown) was refused** as
  `Rendezvous(ArenaHeldButUnreachable)`.
- After the last `CoreDumping: 1`, `owner_lost()` took a further 0.49 ms (`A0`),
  26.0 ms (`A256`) and 101.5 ms (`A1024`): the teardown, which adds to the dump.
- The owner's `wchar` grew by exactly 61 440 bytes during every dump, whatever the
  RSS. `CoreDumping: 1` showed on one poll in 16 of 20 `AN` trials with no dump.
- **A second agent reproduced `K`, `A0`, `AN` and the 1 GiB arms** with the
  tf_tree-free program under *Reproduction* (ballast 0: `K` 0.413 ms, `A` 1097.3 ms,
  `AN` 0.496 ms; 1 GiB: `K` 106.9, `AN` 102.4, `A` 1203.0 ms). Hangup, byte 0 free
  and reap arrived within 0.14 ms of each other. Removing joiner and poller
  changed no timing. `A256` was not re-run.

**Measured, therefore:** on this host an aborting, dumping owner could not be
detected, inherited from or joined for about 1.1 s (1.2 s at 1 GiB); 1 GiB of
dirty 4 KiB pages costs about 100 ms whether killed or aborted; with the dump
suppressed and the owner small, detection took at most 0.49 ms and inheritance
and a fresh join followed within about 1 ms.

**Inferred, not measured:** that the ~1.1 s is apport's runtime, not the kernel
writing a core (`wchar` fits a 64 KiB pipe never drained; apport's log was not
read); the kernel ordering (dump from signal delivery, then `do_exit`, where
`exit_mm()` precedes `exit_files()`; read from source, no trace); that the brief
`CoreDumping: 1` under `AN` is `coredump_wait()`.

### The mechanism, and why nothing a survivor can take shortens it

`owner_lost()` is a zero-timeout `poll` for `POLLHUP` on the attach socket, then
`F_OFD_GETLK` on byte 0. `inherit_ownership()` returns `OwnerAlive` without trying
while `owner_lost()` is false (`crates/tf_tree/src/open.rs:594`). Both are about
kernel objects the owner holds until `exit_files()`, so the facade inherits the
kernel's delay exactly (tf_tree arms match the raw-socket arms to ~0.2 ms at
ballast 0 and ~3-9 ms at 1 GiB).

A survivor that reads `CoreDumping: 1` from `/proc` still finds byte 0 **held**;
`Session::take_over_ownership` would get `EAGAIN`, correctly: byte 0 is what makes
one process the server ([`PHASE2.md`](../PHASE2.md) §3.3) and §3.4's split-brain
argument depends on it. The teardown needs no dump and no setting removes it;
§0.0's row carries the per-MB figure and its THP caveat (far faster under
`transparent_hugepage=always`, as on GitHub's runners). An OOM kill's cost is not
measured.

### What the window costs

Nobody inherits and nobody new joins. From the spec and code:

- **Lookups are unaffected** (§3.5): `Plan::at` touches the mapping and `Guard`.
- **Existing publishers keep publishing**; a claim on a free edge is an OFD lock
  involving no owner.
- **The dying owner's claims stay held**: a successor is refused
  (`ClaimApiError::AlreadyClaimed`) and `reap_dead` cannot clear the lease. The
  refusal outlasts the window: no hangup callback covers a dead owner, so the edge
  stays refused until a survivor calls `reap_dead`.
- **A `reparent` by any survivor is refused** (`ReparentError::LockContended`) if
  the owner died holding A2's byte 1 ([`0029`](./0029-the-topology-lock-is-a-kernel-lock.md)).
- **A participant dying in the window is not collected by a hangup callback**; the
  byte-keyed collectors still work.
- **Supervisors are unaffected**: the reap followed `owner_lost()` by 0.04-0.41 ms,
  so a restarted process never meets its predecessor's held bytes. What the window
  delays is every *other* process.

Symmetrically, a participant that dumps core delays the owner's hangup-driven reap
of it (reaping latency, not recoverability; the owner's `epoll` path was not timed).

### Why it matters in practice

- **A crash usually dumps** (`SIGABRT`, `SIGSEGV`, `SIGBUS`, `SIGILL`, `SIGFPE`);
  `SIGKILL` usually comes from an operator, supervisor timeout or the OOM killer.
- **A soft `RLIMIT_CORE` of 0 does not stop a pipe dump**, and `core_pattern` is
  host-wide (a container gets its host's helper).
- **Both measured hosts pipe dumps.** The GitHub runner uses systemd-coredump
  (`core_pipe_limit=16`, `osrelease=6.17.0-1022-azure`, soft 0); in the two
  crash-points jobs (nightly runs 34769883403, 34769889900) all 50 reaped aborts
  read `core_dumped=true`. **No dump was timed on the runner**, so whether it
  drains the core and grows with RSS is not measured (question 2).
- **The owner is often a large process** (the bridge, [`0015`](./0015-the-bridge-fills-a-shared-arena.md),
  or any node); teardown alone is ~100 ms per GiB of 4 KiB pages.

The 2026-09-12 `shm_torture --crash-points` wedge (an armed heir aborting in its
serving thread, every join refused for an interval bounded between ~54 ms and ~2 s)
is consistent with a dumping heir and not shown by it: that run's log carries no
core pattern and no dump timing.

### The secondary finding: a single non-`Inherited` answer is not final

With the joiner running, the survivor's **first** `inherit_ownership()` after
`owner_lost()` answered `true` returned something other than `Inherited` in
**21 of 120** trials (`Contended` 15, `OwnerAlive` 6), in every arm, and **every
one returned `Inherited` on the next call.** The re-run saw 10 of 30 with joiner
and poller and **0 of 30 with both removed**. The cause is inferred: a fresh
`open()` that finds nobody serving takes byte 0 (§3.4 step 2), meets the
survivor's participant byte (step 4) and releases byte 0 again; landing between
the survivor's poll and its lock attempt, it reads as `OwnerAlive` or `Contended`.
**0043's table has these rows; what it does not say is that the holder may be a
joiner passing through.** The integrator loop keeps no latch and is unaffected;
**a caller that treats one `Contended` or `OwnerAlive` as final is not.** An early
one-shot smoke run left the arena ownerless for its full 90 s joiner deadline.

Sites that read a held byte 0 after a hangup as an heir (fixed by steps 2-3):
`Inheritance::Contended`/`OwnerAlive` docs and `inherit_ownership`'s example in
`crates/tf_tree/src/open.rs`, `crates/tf_tree_c/src/unstable.rs` and its header;
the Python `owner_lost` docstring (`crates/tf_tree_py/src/tree.rs:604`) and stub
`python/tf_tree/_core.pyi`; [`RUNBOOK.md`](../RUNBOOK.md)'s recovery snippet;
`PHASE2.md` §3.4 step 2, §3.5's pseudo-code and §0.0's *Ownership migration* row;
`Session::take_over_ownership` (`crates/tf_tree_ipc/src/open.rs:640`) and the code
copy of §3.4 step 2 (`crates/tf_tree_ipc/src/lockfile.rs:126`).

## Decision

Decided 2026-09-14 under the owner's delegation to *"choose the most desirable
approach for the library goals"*.

**1. The spec states the bound truthfully.** A survivor learns of the owner's
death once its attach connection has hung up and the last open file description
holding byte 0 has closed: for a dying owner the end of its exit (after any core
dump and teardown); for an owner whose `fork` child outlives it, that child's
exit. Nothing the protocol lets a survivor take shortens it.

**2. The runbook gains operator guidance** under *The arena's owner died*:

- the trade: a crash dump, or recovery bounded by teardown, for any process that
  can hold the role;
- the dump window as *the crash helper's run, which can grow with the size of the
  dump*, with no runbook figure, pointing at *Reproduction*;
- suppressing dumps per process: `prlimit --core=1:1 -- <cmd>` (measured),
  `LimitCORE=1` or a launch wrapper (same limit; confirm with `/proc/<pid>/limits`);
  `ulimit -c 0` is **not** the setting when the host pipes dumps;
- systemd-coredump `Storage=`/`ProcessSizeMax=` as unmeasured guidance;
- the teardown, ~100 ms per GiB of dirty 4 KiB pages, with the THP caveat;
- one sentence that a supervisor's `SIGKILL` of a dumping owner ends the window and
  forfeits the core, not recommended (*e*).

It must say *every process that may hold the role*: ownership migrates
([`0055`](./0055-the-recovery-capacity-a-fleet-cannot-add-later.md)), so suppressing
dumps on today's owner protects one handover.

**3. The inheritance docs stop implying that a held byte 0 means an heir.** Every
site under *The secondary finding* gains the case: a fresh `open()` passing through
§3.4 steps 2-4 holds byte 0 briefly and gives it back; while `owner_lost()` stays
`true`, no single non-`Inherited` answer is final.

**4. The library does not release the rendezvous early.** No fatal-signal handler
is installed; a dying owner keeps its socket, byte 0 and the tree's lock-file
description until the kernel closes them (*Rationale a*). The rejection rests on
(1) NORMATIVE §3.5 requirement 5 and the §6.1 zombie writer, and (2) a library
may not own process-global signal dispositions.

**5. The bound is NORMATIVE in §3.5, and a test pins it.** `PHASE2.md` §3.5:

> **NORMATIVE.** `owner_lost()` answers `true` once the survivor's attach
> connection has hung up and the last open file description holding byte 0 has
> closed (`0043`). For a dying owner that is the end of its exit, which includes
> any core dump and the teardown of its address space; a `fork` child sharing
> those descriptions holds them until it exits. tf_tree adds no delay, heartbeat
> or timeout to that event (D17).

The pin is two existing tests in `crates/tf_tree/tests/rendezvous.rs`, with no
timing: `a_read_only_survivor_reports_that_it_cannot_inherit` (first
`owner_lost()` after `owner.kill()` must be `true`) and
`a_survivor_that_did_not_inherit_stops_being_told_the_owner_is_gone` (a
`join-heir` child's first call must be `true Inherited`; after the heir is killed
the other survivor's first call must be `true` again). Neither is
`crash-points`-gated, so both run under `just shm-check` and `just shm-rendezvous`
on x86-64 and aarch64.

**Why a first call after the reap may not answer `false`:** `do_exit` runs
`exit_files()`, then `exit_task_work()`'s deferred final `__fput` (the socket
release raising the peer's `POLLHUP`, and `locks_remove_file` releasing byte 0),
both before `exit_notify()` makes the process reapable (read from source; 120 of
120 trials agree). Preconditions, which both tests meet: the owner has no child
sharing its descriptions; no `open()` is inside §3.4 steps 2-4; no other survivor
inherits between reap and call; and **no other task holds a transient reference
to the owner's socket or lock-file description** (a `/proc/<pid>/fd` reader such as
`lsof`, `pidfd_getfd`, an `SCM_RIGHTS` descriptor in flight; tf_tree sends only the
segment). The last is the only legitimate failure and each assertion's message
names it. **Mutants** (a latch answering `false` until the hangup is seen twice; a
grace period) are expected to fail both tests. The pin cannot see a call that
blocks and then answers `true`, and does not bound the dump window or teardown.

**6. `shm_torture` suppresses its children's dumps.** The torture recipes run
under `prlimit --core=1:1 --`, so no `unsafe` is added and CI inherits it. The
driver prints a `[diag]` warning, reported and never a verdict, when armed crash
points run with a pipe `core_pattern` and a soft core limit other than 1. The
harness gates PHASE2 §12.3 gate 3 and §3.5 recovery, so its result must not depend
on the host's crash helper. **This does not explain the 2026-09-12 wedge**, and a
green crash-points run afterwards is not evidence about it; a recurrence under
`RLIMIT_CORE=1` would refute the dump explanation. The 2026-09-13 wedge was the
plain soak's (nightly run 34747459144's `shm_torture (30 min)` failed; its
crash-points job passed), so it cannot be a dumping armed heir; the justfile's
hypothesis is a role-holder cap exit (`[diag] role-holder-cap-exit`). The bare
binary outside the recipe still runs the dumping configuration.

## Rationale

### a. An owner-side fatal-signal handler: rejected

A handler for `SIGABRT`/`SIGSEGV`/`SIGBUS`/`SIGILL`/`SIGFPE` would `close(2)` the
listener, accepted sockets and lock-file descriptor, restore the default
disposition and re-raise. It is the only option the library could build that keeps
the core and removes both windows for a crash. It is rejected because:

1. **Correctness outranks latency.** *(a)* Releasing byte 0 while the serving
   thread runs breaks NORMATIVE §3.5 requirement 5 with no D15 crash-matrix walk
   behind it (two servers and an arbitrated split, not a torn record); closing the
   sockets first narrows the window, and a narrower window is not an argument.
   *(b)* Closing the tree's `lock_file` description releases the claim leases and
   byte 1 while threads may still be in `push` or `reparent`, recreating the zombie
   writer §6.1 makes impossible and an unwalked byte-1 steal
   ([`0029`](./0029-the-topology-lock-is-a-kernel-lock.md)). Closing the
   *session's* description would take the participant byte with byte 0 and allow
   two live processes on one slot index
   ([`0028`](./0028-the-slot-a-killed-participant-keeps.md)); a handler must
   `F_UNLCK` byte 0 alone. A dump window costs latency, never a corrupt read.
2. **A library must not own process-global signal dispositions.** `sigaction`
   replaces rather than stacks; CPython's `faulthandler`, sanitizers, crash
   reporters and (in Rust binaries) the stack-overflow handler own these signals.
   Chaining to one installed after ours is unverifiable.
3. **Partial coverage**: nothing for `SIGKILL` (OOM, supervisor timeout) or for an
   owner with a forked child until `0030`'s hole is closed.
4. **Suppression is already an operator trade** (`RLIMIT_CORE=1`, the `AN` arm).
   Removing the teardown for a crashing large owner is not available that way
   (raw-socket `AN` at 1 GiB: 102.8 ms); that benefit is real and is given up on
   reasons 1 and 2.

Also weighed: a handler needs an fd registry in async-signal-safe statics, with
`0030`'s rules (a snapshot skew does not arise; the double-close race and rule (v)
do), one arena per process plus re-registration on `inherit_ownership`, and a new
`scripts/unsafe-budget.txt` row (kind 2, no new kind, [`0007`](./0007-the-unsafe-budget-and-the-c-abi.md)/[`0048`](./0048-a-kind-is-not-a-crate-name.md)).
`0019` and D17 do not forbid it.

**What would reopen it:** field evidence of a deployment needing both the core and
recovery shorter than its dump. The shape should be an **opt-in, async-signal-safe
release call invoked from the application's own crash handler** (close the listener
and accepted sockets, `F_UNLCK` byte 0; leave the participant byte and `lock_file`),
never a library-installed handler. It must bring: a §3.5 requirement 5 walk; the
chaining story for `faulthandler`, sanitizers, crash reporters and Rust's
stack-overflow handler, **measured**; the `lock_file`/byte 1 question argued against
§6.1 and `0029`; a registry taking no lock reachable from the handler and keeping
`0030`'s rule (v); what it does for a forked child; and **a test that can fail**:
an aborting owner with dumps suppressed and ballast held, whose survivor must see
`owner_lost()` before the reap by roughly the teardown time (about 100 ms at 1 GiB
against 0.04-0.41 ms without a handler), with Decision 5's pin re-read, since a
handler moves the close earlier.

### b. Survivor-side detection through `SO_PEERCRED` and `/proc`: rejected

It does not help (the dumping owner still holds byte 0, and serving without it
breaks §3.4's split-brain argument); it is a heuristic on a path whose premise is
that there are none (§3.3); and `CoreDumping: 1` appeared in 16 of 20 `AN` trials
that never dumped.

### c. Disabling dumps by default in the library: rejected

`prctl(PR_SET_DUMPABLE, 0)` or `RLIMIT_CORE=1` at attach takes crash evidence from
users who never asked (and `PR_SET_DUMPABLE` also removes the debugger). As an
opt-in it is one line of launch configuration, which is Decision 2's guidance.

### d. Document and do nothing else

Decisions 1-3 with 5 and 6 (a pinned normative line and a harness setting, neither
library behaviour). It cannot make recovery fast for a fleet that keeps its dumps;
that cost is accepted as the operator's to trade.

### e. Killing a dumping owner from outside: rejected as library behaviour

A `SIGKILL` during a dump ends it (`shm_torture.rs`'s *Instrument 5*: on 6.8, a
`python3 -c 'os.abort()'` reaped at 1.8 s with a core alone, at once as signal 9
with no core when killed 50 ms in; that 1.8 s is not this record's ~1.1 s and the
difference is not explained). By inference a killed owner would pay only its
teardown; not measured on an owner. As a library mechanism it is *b* with a signal
on the end and forfeits every core on sight, can name a reused pid unless held via
`pidfd`, and makes one participant's crash evidence another's decision. The runbook
mentions it in one sentence and does not recommend it. It releases nothing while a
forked child shares the descriptions.

## Consequences

- **§3.5's recovery has a latency floor that belongs to the host**: the owner's
  exit time, or longer with a sharing forked child. No test could bound it without
  choosing a `core_pattern`.
- **The NORMATIVE line commits to the event, not a duration.** A heartbeat, grace
  period or timeout in owner detection contradicts it and D17 and needs a
  superseding record. Decision 5's pin catches withholding `true` after the close;
  answering earlier (from `/proc` or a timer) or slowly is caught by the sentence
  and *b*, not by a test.
- **The library keeps no process-global signal state**: no library crate outside
  `tf_tree_bench` installs `sigaction`, `SIG_IGN`, `prctl` or `setrlimit`
  (grep, 2026-09-14), and `tf_tree_ipc`'s stacking `pthread_atfork` handler stays
  its one process-wide hook. The price is the dump window and the teardown, paid in
  latency, never a corrupt read.
- **§12.2's migration row and §12.3 gate 4's *kill → re-claimable* margin are
  scoped, not wrong**: their victim is a small `SIGKILL`ed child, and a claim lease
  is released at the same point in the exit as byte 0. The gate stays a measurement
  of the library's half on a small victim.
- **[`0055`](./0055-the-recovery-capacity-a-fleet-cannot-add-later.md)'s vacancy
  gets a second, longer producer**: joins are refused for the whole owner exit,
  before `owner_lost()` can answer `true`, so a harness marker around a `kill()`
  cannot bracket a dump inside the victim's own thread. Decision 6 removes it from
  `shm_torture`; it remains in every fleet that keeps its dumps.
- **After step 4, `shm_torture`'s recipes no longer exercise recovery across a core
  dump**; that is this record's measurement's job, and the bare binary still runs
  it. It also removes one source of §0.0's *"`aborted` is a floor rather than a
  count"* (a child `SIGKILL`ed mid-dump). A host with a pipe `core_pattern` and a
  different harness limit is reported by `[diag]`, not failed.
- Nothing reopens what [`0009`](./0009-descoping-phase-6.md) cut, and nothing
  changes an arena byte, a wire message or a public type.

## Implementation plan

1. **Landed** (#333). `PHASE2.md` §3.7 step 9 corrected, the index row and the
   [`EVIDENCE.md`](../benchmarks/EVIDENCE.md) probe row.
2. **Landed** (#337). The specs and the runbook: §3.5's NORMATIVE sentence, §3.7
   step 9, §3.3's row, Decision 3's case in §3.4/§3.5/§0.0, §11.4 and §0.0's
   kill-marker phrase, §12.2/§12.3 scoping, a D17 amendment note, and the
   `RUNBOOK.md` subsection.
3. **Landed.** The release-visible prose (`Tree::owner_lost`, `tf_tree_ipc`'s docs
   and README, `Inheritance` docs, the C header, the Python docstring at
   `crates/tf_tree_py/src/tree.rs:604` and stub, a `CHANGELOG.md` entry) and the
   pin's two mutant runs, recorded in the tests' *Mutant, run:* notes.
4. **`shm_torture`'s dumps** (#341; runner-side verification owed).
   - `prlimit --core=1:1 --` at recipe level on `just shm-torture`,
     `just shm-torture-crash-points` and `just shm-torture-asan`.
   - Also on the two `cargo nextest` lines of `just shm-check` and
     `just shm-rendezvous`, and on `no-network.sh`'s `strace -f` (the third runner
     of the `rendezvous` binary): `crates/tf_tree/tests/rendezvous.rs` has six abort
     sites reaped by `wait()` (bounded only by nextest's 180 s) or
     `wait_within(20 s)`, which a slow crash helper decides. `torture.rs` aborts
     none. **`crates/tf_tree_core/src/crash_tests.rs` under `just test` gets no
     prefix** (no `prlimit` off Linux, `wait` bounded only by nextest), with the cost
     stated beside it.
   - The driver's `[diag]` warning and justfile comments saying why the limit is
     there; the pre-change per-job wedge rate of the crash-points nightly recorded
     in the justfile comment.
   - `PHASE2.md` §0.0's `shm_torture` row and §11.4 record that the recipes run
     under `RLIMIT_CORE=1`, no longer exercise a dump, and lost one source of
     *"`aborted` is a floor"*.

   **Still owed:** the first crash-points nightly after landing must show
   `core_dumped=false` on every `[diag] reap` line of an aborted armed child (the
   runner is kernel 6.17 with systemd-coredump, whose pattern passes a fixed limit
   in place of `%c`; suppression is measured only on 6.8 with apport), and a
   `workflow_dispatch` run of *Reproduction*'s `AN` arm on the runner is recorded in
   the PR. Verified by `[diag] host:` reading `core_rlimit soft=1 hard=1` under each
   recipe, with a red check that `shm-torture-asan` without the prefix does not read
   `hard=1`; `just shm-check`; `just shm-rendezvous`; `just shm-torture-self-test`;
   and a positive control: the warning prints for the bare binary with
   `--crash-points` on a pipe-`core_pattern` host.
5. **Status to `implemented`** when step 4's runner verification has landed.

**Why the dump window and the teardown are not tested.** A test of the dump window
is not portable: `core_pattern` is host-global, needs root to change, and differs
between apport, systemd-coredump, a file pattern and none. `--victim-ballast-mb`
failed on the runner for the same kind of reason (THP). A portable teardown control
(`MADV_NOHUGEPAGE`/`PR_SET_THP_DISABLE`) was not measured, needs the unsafe budget
in a test target, and §0.0 records that chunking allocations did not defeat THP.
`--stop-owner-ms` remains the harness's way to hold the socket open with no memory
physics. The reason is recorded in the pin tests' doc comments.

## Open questions

None. All four were answered 2026-09-14.

1. **Should a dying owner release the rendezvous from a fatal-signal handler?** No
   (Decision 4, *Rationale a*, including what would reopen it).
2. **Does the GitHub runner's systemd-coredump drain the core?** Closed, not
   measured: no decision depends on it. If wanted, a `workflow_dispatch` job runs
   the tf_tree-free program at ballast 0, 256 MiB and 1 GiB (THP-proof ballast),
   printing `core_pattern`, effective `Storage=`/`ProcessSizeMax=` and timings.
3. **Should `shm_torture` suppress dumps in its children?** Yes (Decision 6): a
   harness should test §3.5, not the host's crash helper.
4. **Does the bound deserve a NORMATIVE statement in §3.5?** Yes, with a pin
   (Decision 5). The sentence says "tf_tree adds no delay, heartbeat or timeout to
   that event" because that half is the one a test can hold.

## Reproduction

A program with no tf_tree in it. The forked "owner" holds a listening `AF_UNIX`
socket with one accepted connection and an OFD write lock on byte 0; the parent
blocks in `poll()` for the hangup, then spins on `F_OFD_GETLK`. It is **not added
to the repository as a file** (`just evidence-audit` cannot see one); it is
registered as a probe row in [`EVIDENCE.md`](../benchmarks/EVIDENCE.md) whose
command is the program's first line.

```python
# python3 hup_min.py N K,A,AN [BALLAST_MIB]    -- Linux only; no tf_tree
import fcntl, os, resource, select, signal, socket, statistics, struct, sys, time
FL = 'hhqqi4x'                                     # struct flock
def lock(fd, cmd):                                 # F_WRLCK on byte 0; returns l_type
    return struct.unpack(FL, fcntl.fcntl(fd, cmd, struct.pack(FL, fcntl.F_WRLCK, 0, 0, 1, 0)))[0]
def trial(arm, mib, path='hup.lock'):
    name = '\0hup-%d' % os.getpid(); open(path, 'a').close()
    r_ready, w_ready = os.pipe(); r_die, w_die = os.pipe()
    pid = os.fork()
    if pid == 0:                                   # the "owner"
        if arm == 'AN': resource.setrlimit(resource.RLIMIT_CORE, (1, 1))
        ls = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); ls.bind(name); ls.listen(1)
        lock(os.open(path, os.O_RDWR), fcntl.F_OFD_SETLK)          # byte 0
        ballast = bytearray(b'\xa5' * (mib << 20))                 # dirty anon pages
        os.write(w_ready, b'R'); conn, _ = ls.accept(); os.read(r_die, 1)
        os.write(w_ready, str(time.monotonic_ns()).encode().ljust(24))
        if arm == 'K': time.sleep(3600)
        os.abort()                                 # SIGABRT: default action is "core"
    os.read(r_ready, 1)
    peer = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); peer.connect(name)
    probe = os.open(path, os.O_RDWR); time.sleep(0.05)
    os.write(w_die, b'D'); death = int(os.read(r_ready, 24))
    if arm == 'K': death = time.monotonic_ns(); os.kill(pid, signal.SIGKILL)
    po = select.poll(); po.register(peer.fileno(), select.POLLHUP)
    po.poll(10_000); hup = time.monotonic_ns()     # what owner_lost()'s poll sees
    while lock(probe, fcntl.F_OFD_GETLK) != fcntl.F_UNLCK: pass
    free = time.monotonic_ns()                     # what its F_OFD_GETLK sees
    _, st = os.waitpid(pid, 0); reap = time.monotonic_ns()
    for fd in (probe, r_ready, w_ready, r_die, w_die): os.close(fd)
    peer.close()
    return (hup - death) / 1e6, (free - death) / 1e6, (reap - death) / 1e6, os.WCOREDUMP(st)
n, arms, mib = int(sys.argv[1]), sys.argv[2].split(','), int(sys.argv[3]) if sys.argv[3:] else 0
res = {a: [trial(a, mib) for _ in range(n)] for a in arms}
for a, rs in res.items():
    med = lambda i: statistics.median(r[i] for r in rs)
    print(f'{a:3} n={n} hangup {med(0):9.3f} ms  byte0 free {med(1):9.3f} ms  '
          f'reap {med(2):9.3f} ms  core dumped {sum(r[3] for r in rs)}/{n}')
```

On the dev host (`python3 hup_min.py 3 K,A,AN`):

```text
K   n=3 hangup     0.380 ms  byte0 free     0.406 ms  reap     0.418 ms  core dumped 0/3
A   n=3 hangup  1097.573 ms  byte0 free  1097.597 ms  reap  1097.631 ms  core dumped 3/3
AN  n=3 hangup     0.495 ms  byte0 free     0.514 ms  reap     0.526 ms  core dumped 0/3
```

Read the output against the host: `cat /proc/sys/kernel/core_pattern`,
`/proc/sys/kernel/core_pipe_limit` and
`/sys/kernel/mm/transparent_hugepage/enabled`. A file pattern, or `A` reading
`core dumped 0/3`, measures a different arm; under THP `always` a ballast run
understates the teardown. **An abort on a host with a crash helper may leave a
report behind** (apport under `/var/crash`, systemd-coredump's journal entry).
The tf_tree probe's source and CSVs stayed outside the repository; the output above
and the 1 GiB `n=2` run were copied from session output, not written by the run.
