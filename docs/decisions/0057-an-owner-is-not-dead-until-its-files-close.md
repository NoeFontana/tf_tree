# 0057: an owner is not dead until its files close

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** step 1 (the `PHASE2.md` §3.7 step 9 correction, the index
row and the evidence register's probe row) has landed beside this record; steps
2–6 have not.

## Context

§3.5's recovery starts when a survivor learns the owner has died, and the
documents say that happens at once. [`PHASE2.md`](../PHASE2.md) §3.7 step 9 said
the attach socket is how a participant learns the owner has died *"in
microseconds, with no polling"*. `Tree::owner_lost`'s rustdoc says the owner's
death closes the socket *"and the kernel reports `POLLHUP` in microseconds,
exactly and with no timeout to tune"* (`crates/tf_tree/src/tree.rs:3081`).
[`PROJECT.md`](../PROJECT.md) D17 says the same of the owner's view of a
participant (*"the owner sees `EPOLLHUP` in microseconds — exact, immediate"*),
and `crates/tf_tree_ipc/src/server.rs:15` and `client.rs:52` quote it.

**The half about timeouts is true and this record does not touch it.** The half
about microseconds holds only for a small process that does not dump core. The
kernel closes a process's files when its **exit** reaches them, and two things
come before that: writing a core dump, and tearing down the address space. The
socket's hangup and the release of every OFD lock the process holds, byte 0
included, wait for both.

**And the owner's exit is only the last close when nothing else shares its
descriptions.** A `fork` without `exec` shares every open file description with
the child. [`PHASE2.md`](../PHASE2.md) §6.2 says so of the OFD locks, and
[`RUNBOOK.md`](../RUNBOOK.md)'s *The tree works in the parent and everything fails
in a forked child* says the child keeps the rendezvous socket and the lock byte
alive. So an owner whose forked child outlives it keeps
the listener, every accepted connection and byte 0 held until **that child**
exits, which can be never, and no survivor sees a hangup before then.
[`0030`](./0030-the-atfork-handler-and-inherited-descriptors.md) (`draft`)
enumerates those descriptions in its question 2, items 1, 6 and 8. That third
producer is unbounded, and it is not what this record measured: every figure
below is an owner with no forked child. **The bound this record states is
therefore the close of the last open file description on the owner's socket and
byte 0**, which is the owner's exit only when no child shares them.

### What the documents promise, swept

`rg -n -i 'microsecond|immediately|at once|instantly'` over `docs/`,
`crates/tf_tree/src`, `crates/tf_tree_ipc/src` and `crates/*/README.md`, kept
where the match is about owner death, hangup, `owner_lost`, inheritance or a
dead holder's lock:

| Site | What it says | What it should say |
|---|---|---|
| [`PHASE2.md`](../PHASE2.md) §3.7 step 9 | a participant learns the owner died *"in microseconds"* | when the last open file description on the owner's socket and byte 0 closes. **Corrected in the same change as this record**, citing it as a draft |
| `Tree::owner_lost` rustdoc, `tree.rs:3081` | *"`POLLHUP` in microseconds, exactly"* | same; release-visible, so it goes in `CHANGELOG.md` (step 3) |
| `tf_tree_ipc`'s `client.rs:8-11` module doc | process death *"closes the fd and the peer sees it immediately"* | same; release-visible (step 3) |
| [`PROJECT.md`](../PROJECT.md) D17, quoted at `server.rs:15` and `client.rs:52` | the owner sees a participant's `EPOLLHUP` *"in microseconds — exact, immediate"* | same mechanism, from the other end (below) |
| [`PHASE2.md`](../PHASE2.md) §3.3, *Verified behaviour* table, quoted at `runtime_dir.rs:172` and `error.rs:186` | *"Holder dies without unlocking → lock released by the kernel, immediately"* | immediately **at the end of the holder's exit**. The `runtime_dir.rs` and `error.rs` quotes contrast this with NFS lease expiry, and that contrast still holds |
| `tf_tree_ipc`'s crate doc, `lib.rs:22`, and its crates.io page, `crates/tf_tree_ipc/README.md:31` | *"A `SIGKILL`ed participant's lock is released by the kernel, immediately"* | same scoping as §3.3's row; both are release-visible (step 3), and the README is a package-index front page |
| [`PHASE2.md`](../PHASE2.md) §11.4's blockquote on the kill marker | the marker is *"the width of one reap per owner kill. Tens of microseconds normally"* | a driver `SIGKILL`, so no dump, but §0.0's `shm_torture` row measures that reap at 0.3 ms median for a plain child, so *"sub-millisecond"* |
| [`PHASE2.md`](../PHASE2.md) §12.2, *owner kill → new owner serving* | 0.6–1.2 ms p50 | not wrong: it is measured with `SIGKILL` on a small owner, and should say so |

[`RUNBOOK.md`](../RUNBOOK.md)'s *The arena's owner died* gives no latency figure,
and neither do [`0019`](./0019-one-binary-and-topology-you-can-wait-for.md),
[`0037`](./0037-a-takeover-is-not-a-second-open.md),
[`0043`](./0043-owner-lost-is-a-question-about-the-owner.md) or
[`0044`](./0044-recovery-the-languages-a-robot-is-written-in-cannot-reach.md).
None of them mentions a core dump either; nothing under `docs/` did before this
record.
[`PHASE2.md`](../PHASE2.md) §11.2 scenario 9's *"kill the owner, and immediately"*
describes when a test starts its process, not a latency, and is left alone.

The teardown half was already known, from `SIGKILL`. §0.0's `shm_torture` row
records that a `SIGKILL`ed process releases its socket and participant byte in
`exit_files()`, which `do_exit` runs after `exit_mm()`, and that the delay scales
with dirty pages at about 0.09 ms per resident MB. **What that row did not have is
the dump**, and on the dev host, the only one where a dump was timed, the dump is
the larger term by an order of magnitude.

### What was measured

**Dev host, 2026-09-13.** Kernel `6.8.0-138-generic`, 8 CPUs, 31 GiB, no swap.
`core_pattern=|/usr/share/apport/apport …` with apport active,
`core_pipe_limit=10`, soft `RLIMIT_CORE` 0 and hard unlimited (the shell
default), THP `madvise` with `AnonHugePages` 0 kB in every trial. tf_tree at
`b603260`.

**The probe** was a standalone cargo project outside the repository. It had a
path dependency on `crates/tf_tree` with `shm` and was built `--release
--offline`. It ran four roles as separate processes:

- an **owner**, opened with `Open::new().mode(ReadWrite).create(IfAbsent)`: the
  facade's normal owner path, with its serving thread. It dirties N MiB of
  anonymous ballast, stamps `CLOCK_REALTIME` and calls `std::process::abort()`.
  In the `K` arms the parent stamps and sends `SIGKILL` instead.
- one read-write **survivor**. Once registered, it evaluates `owner_lost()` in a
  tight loop, then runs §3.5's `inherit_ownership()` loop until `Inherited`.
- a **joiner**, released at the death stamp. It retries
  `Open::new().mode(ReadWrite).create(Never).timeout(20 ms)`.
- the **parent**. It reaps the owner, records `ExitStatus::core_dumped()`, and
  polls `/proc/<pid>/status` for `CoreDumping:` and `/proc/<pid>/io` for
  `wchar`.

Every trial ran in a fresh runtime directory, with the six arms interleaved
round-robin: 20 rounds, 120 trials, about 103 s. The raw material is a 39-column
CSV of nanosecond stamps and a full log, and neither is in the repository.

| Arm | How the owner died | Owner RSS | N | death → `owner_lost()` true, median / p90 / max | `core_dumped` |
|---|---|---|---|---|---|
| `K` | `SIGKILL` | 2.7 MiB | 20 | 0.237 / 0.310 / 0.359 ms | 0 / 20 |
| `AN` | `abort()` under `prlimit --core=1:1` | 2.7 MiB | 20 | 0.292 / 0.437 / 0.494 ms | 0 / 20 |
| `A0` | `abort()`, host default (pipe to apport) | 2.7 MiB | 20 | **1097.1 / 1105.4 / 1107.9 ms** | 20 / 20 |
| `A256` | `abort()`, host default | 259 MiB | 20 | **1123.2 / 1127.1 / 1132.8 ms** | 20 / 20 |
| `A1024` | `abort()`, host default | 1027 MiB | 20 | **1200.1 / 1211.3 / 1215.4 ms** | 20 / 20 |
| `K1024` | `SIGKILL` | 1027 MiB | 20 | **98.0 / 103.1 / 106.6 ms** | 0 / 20 |

Also measured, in the same trials:

- **`owner_lost()` went true 0.04–0.41 ms before the reap in 120 of 120 trials
  (per-arm medians 0.05–0.11 ms), and never while `CoreDumping: 1` was still
  readable.** That held in all 60 dump trials. 11 trials exceeded 0.11 ms, all
  in the three ballast arms. Across the six arms, the per-arm median gap from
  `owner_lost()` to the first `Inherited` was 0.16–0.27 ms, and to the joiner's
  first success 0.41–0.48 ms. So the survivor's trigger, the heir's bind and a
  fresh join are all bounded by the owner's exit, and they follow it closely.
- **Every join attempt in the window, the dump and the teardown after it, was
  refused** as `Rendezvous(ArenaHeldButUnreachable)`. That was 52 attempts per
  trial in `A0`, 53 in `A256`, 56–57 in `A1024`, and 0 in `K` and `AN`.
  `K1024`, which never dumps, was refused 4–5 times in its teardown alone. The
  probe kept the full payload only for each trial's first and last refusal. 159
  of those 160 read `holder_slots: 3, first_slot: Some(0), …, ownership_held:
  true` and named the owner. The other is `A1024` trial 41's last refusal,
  stamped 0.06 ms *after* `owner_lost()` went true: `holder_slots: 2,
  first_slot: Some(1)`, the survivor's slot, while it was inheriting. The counts
  come from the joiner's 20 ms per-attempt timeout and mean only that it was
  refused for the whole window.
- **After the last `CoreDumping: 1` reading, `owner_lost()` took a further
  0.35–0.60 ms (median 0.49) in `A0`, a median 26.0 ms in `A256` and 101.5 ms in
  `A1024`.** That scales like `K1024`'s teardown, which fits the address space
  being torn down after the dump and adding to it.
- **The owner's `wchar` grew by exactly 61 440 bytes (60 KiB) during every
  dump**, whatever the RSS. In three `A256` and three `A1024` trials the first
  poll came before the owner's own 26-byte `DEATH` line, written before
  `abort()`, and read 26 bytes lower.
- **`CoreDumping: 1` showed on one poll in 16 of 20 `AN` trials**, with no dump,
  no `wchar` growth and a reap under 0.55 ms.

**An independent re-run by a second agent, same host, the same day, reproduced
`K`, `A0` and `AN` with the probe, and the 1 GiB arms with the tf_tree-free
program. `A256` was not re-run.**

- The original binary, N=10 per arm, `K`/`A0`/`AN` interleaved: medians of
  0.221 ms, 1099.3 ms and 0.251 ms.
- A variant with **no joiner and no `/proc` poller**: 0.213 ms, 1097.9 ms and
  0.256 ms. So the two together are not a confound for the timing; neither was
  removed alone.
- **A tf_tree-free program**, the one under *Reproduction* below. At ballast 0,
  N=8, the hangup medians were `K` 0.413 ms, `A` 1097.3 ms and `AN` 0.496 ms.
  At 1 GiB, N=6: `K` 106.9 ms, `AN` 102.4 ms and `A` 1203.0 ms. **In every
  ballast-0 trial the hangup, byte 0 reading free and the reap arrived within
  0.14 ms of each other.** The 1 GiB run kept only medians, and those were
  within 0.07 ms of each other in each arm.

`AN` at 1 GiB is the arm the main run did not have: an abort with no dump still
pays the teardown. This record's author ran the trimmed script before writing.
At ballast 0, n=3, the medians were 0.380 ms, 1097.6 ms and 0.495 ms. At 1 GiB,
n=2, `K` and `AN` were 102.9 ms and 102.8 ms. Both outputs were copied from the
session, not from a results file (*Reproduction* says which numbers are).

**Measured, therefore:** on this host the probe's owner, aborting and dumping
through the pipe, could not be detected, inherited from or joined for about
1.1 s, or 1.2 s at 1 GiB. An owner with 1 GiB of dirty 4 KiB anonymous pages
could not be for about 100 ms, whether it was killed or aborted. With the dump
suppressed and the owner small, detection took under half a millisecond (at
most 0.49 ms), and inheritance and a fresh join followed within about 1 ms (at
most 1.06 ms).

**Inferred, not measured:**

- **That the ~1.1 s is apport's own runtime, not the kernel writing a core.**
  `wchar` grew by exactly 60 KiB at every RSS, which fits the kernel filling a
  64 KiB pipe the helper never drains, then waiting for the helper to exit
  because `core_pipe_limit` is non-zero. apport's log is root-only and was not
  read. It is equally unconfirmed that `wchar` counts dump writes at all.
- **The kernel ordering.** The dump is written from signal delivery, before
  `do_exit`; within `do_exit`, `exit_mm()` runs before `exit_files()`. That is
  read from kernel source, and the timings agree with it, but no trace was taken.
- **That the brief `CoreDumping: 1` under `AN` is `coredump_wait()` running**
  before `do_coredump` reaches the limit-of-1 refusal. Also read from source.

### The mechanism, and why nothing a survivor can take shortens it

`owner_lost()` is a zero-timeout `poll` for `POLLHUP` on the survivor's attach
socket, then `F_OFD_GETLK` on byte 0 ([`0043`](./0043-owner-lost-is-a-question-about-the-owner.md)).
`inherit_ownership()` returns `OwnerAlive` without trying anything while
`owner_lost()` is false (`crates/tf_tree/src/open.rs:594-596`). Both questions
are about kernel objects the owner holds until `exit_files()`, or a forked child
holds past it: its end of the connection, and the OFD lock on byte 0. So the
facade inherits the kernel's delay exactly. Across separate runs, the tf_tree
arms match the raw-socket arms to within about 0.2 ms at ballast 0 (N=20 against
N=8) and to within about 3–9 ms at 1 GiB (N=20 against N=6).

**A survivor with more information still could not act on it by taking
anything.** `/proc` says `CoreDumping: 1` within 0.3 ms of the death stamp
(median under 0.1 ms, at a poll interval of about 0.19 ms), and a survivor that
believed it would still find byte 0 **held**. `Session::take_over_ownership`
would get `EAGAIN`, and it should: byte 0 is what makes one process the server
([`PHASE2.md`](../PHASE2.md) §3.3), and §3.4's split-brain argument depends on
nobody serving without it. The dying process could close it earlier (option
*a* below), and a kill from outside ends a dump (alternative *e*). Neither
releases anything while a forked child shares the descriptions.

**The teardown half needs no dump, and no setting removes it.** `K1024` and the
raw-socket `AN` arm at 1 GiB both pay about 100 ms. §0.0's row carries the
earlier per-MB figure and its caveat: under `transparent_hugepage=always`, as on
GitHub's runners, the same RSS tears down far faster. Whether an OOM kill pays
the same cost is **not measured**. That is the ordinary way a *large* process
gets `SIGKILL`, and the kernel's OOM reaper takes a different path through the
address space.

### What the window costs, and what it does not

What the window does to recovery was measured above: **nobody inherits, and
nobody new joins.** The rest follows from the spec and the code rather than from
the probe:

- **Lookups are unaffected.** §3.5: `Plan::at` touches the mapping and the
  `Guard` and nothing else. The owner's death does not unmap anything from a
  survivor, and a dump does not either.
- **Existing publishers keep publishing.** `push` is arena-only. A claim on a
  free edge is an OFD lock on the lock file (§6.1) and involves no owner.
- **The dying owner's own claims stay held.** A successor asking for one of
  those edges is refused by the claim record's CAS
  (`ClaimApiError::AlreadyClaimed(EdgeAlreadyClaimed)`), and `Tree::reap_dead`
  cannot clear the record, because the lease is an OFD lock on a description
  the owner holds until `exit_files()` and `F_OFD_GETLK` still reports it held.
  That is §6.1 working: a dumping owner is not yet dead. **The refusal outlasts
  the window**: no hangup callback covers a dead owner (§3.9, which still spells
  the collector `Tree::reap()`), so the edge stays refused until some survivor
  calls `reap_dead` (`tft_tree_reap_dead` in C, `reap_dead()` in Python).
- **A `reparent` by any survivor is refused for the window** with
  `ReparentError::LockContended` if the owner died holding A2's byte 1, which is
  where §11.3's `topo.holding_lock` site aborts. Byte 1 sits on the same
  description as the claim leases ([`0029`](./0029-the-topology-lock-is-a-kernel-lock.md)),
  so it is released with them at the end of the owner's exit.
- **A participant that dies during the window is not collected by any hangup
  callback**, because the owner's serving thread is not running. This is the
  ordinary state of an ownerless arena, and the byte-keyed collectors still
  work.
- **Supervisors are not affected.** A supervisor restarts a process when it reaps
  it, and the reap followed `owner_lost()` by 0.04–0.41 ms, measured (and the
  hangup by at most 0.14 ms in the tf_tree-free program). So a
  restarted process never meets its predecessor's held bytes, unless the
  predecessor left a forked child holding them. What the window delays is every
  *other* process.

The same mechanism applies to D17 from the other end: a participant that dumps
core keeps its connection and participant byte for the length of its dump, so
the owner's hangup-driven reap of it is delayed by that much. The tf_tree-free
program measures a hangup on a connected pair, which is symmetric, but nobody
measured the owner's `epoll` path. The cost there is reaping latency, not
recoverability.

### Why it matters in practice

- **A crash usually dumps.** `SIGABRT`, `SIGSEGV`, `SIGBUS`, `SIGILL` and
  `SIGFPE` all have a default action of *core*. `abort()`, a failed `assert`,
  `std::terminate`, a Rust `panic = "abort"` and a null dereference all arrive
  this way. `SIGKILL` usually comes from an operator, a supervisor's stop
  timeout or the OOM killer, not from a bug in the process.
- **A soft `RLIMIT_CORE` of 0 does not stop a pipe dump.** It is the dev host's
  shell default, and the harness process's soft limit read 0 on the runner
  below. In both places aborts reaped with `core_dumped` true. `core_pattern` is
  a host-wide sysctl, so a container gets its host's helper.
- **A pipe `core_pattern` is what both measured hosts had configured.** The dev
  host uses apport. **The GitHub runner uses systemd-coredump.** #332's
  `[diag] host:` line in nightly runs 34769883403 and 34769889900 (2026-09-13)
  reads:

  ```text
  core_pattern=`|/usr/lib/systemd/systemd-coredump %P %u %g %s %t 9223372036854775808 %h %d`
  core_pipe_limit=16 core_rlimit soft=0 hard=unlimited osrelease=6.17.0-1022-azure
  ```

  The torture and crash-points jobs print it with `soft=0`. **In the two
  crash-points jobs, all 50 reaped aborts (21 and 29) read
  `core_dumped=true`.** The ASan job prints `soft=1`, which is inferred to be the
  sanitizer runtime's doing and, going by the dev host's `AN` arm, should
  suppress a pipe dump in the children that inherit it; no ASan-job abort was
  observed. **No dump was timed on the runner**: those aborts were ordinary
  participants, the harness polls `CoreDumping` once per driver round, and every
  line reads `dump_seen=never`.
  So whether systemd-coredump drains the core, and the window grows with RSS, or
  behaves like apport here, is open question 2.
- **The owner is often a large process.** The owner is whichever process
  created the arena or last inherited it — the bridge, under
  [`0015`](./0015-the-bridge-fills-a-shared-arena.md), or any node. On a robot that is a perception or
  planning process as often as a thin one, and on the dev host teardown alone
  cost about 100 ms per GiB of 4 KiB pages (far less under THP `always`, per
  §0.0's row).

**The 2026-09-12 `shm_torture --crash-points` wedge is consistent with this, and
not shown by it.** In that run, an heir that had inherited at owner kill 12 was
armed at `hangup.after_probe_before_cas:1` and aborted in its own serving thread
0.5 s later. The driver never entered its kill window. No survivor saw the role
vacant for an interval the log bounds below by about 54 ms and above by about
2 s — about 0.4 s if the two refusals that came 0.42 s after the first eight
were respawns the driver could make only after reaping the heir. The first eight
`could not join` lines each follow a 2 s open timeout, so those opens began
1–54 ms after the abort: processes were still leaving, not inheriting, 54 ms in.
Every refusal reports byte 0 free by its deadline. Attached survivors left on
their ordinary exits during the interval — how many cannot be told, because the
log has no spawn lines and its refusing pids hold more than one consecutive run —
every join was refused, and the run ended. A dumping heir explains that interval, although a window of 0.4 s or less
would be shorter than the dev host's ~1.1 s dump, and the runner's helper is a
different one. That run's log carries no core pattern and no dump timing, so
this is an explanation, not a finding.

### The secondary finding: a single non-`Inherited` answer is not final

With the joiner running, the survivor's **first** `inherit_ownership()` after
`owner_lost()` answered `true` returned something other than `Inherited` in
**21 of 120** trials: `Contended` 15 and `OwnerAlive` 6. It happened in every
arm, dump or no dump (`K` 5, `A0` 3, `AN` 3, `A256` 1, `A1024` 8, `K1024` 1),
and **every one of them returned `Inherited` on the next call.** The re-run
found 10 of 30 with the joiner and the `/proc` poller running, and **0 of 30
with both removed**. No run removed the joiner alone.

Nothing instrumented who held byte 0, so the cause is inferred. The joiner is
the only other process that opens the arena, and removing it together with the
poller removed the effect; the poller only reads `/proc`. The mechanism fits
§3.4: a fresh `open()` that finds nobody serving takes byte 0 at step 2, meets
the survivor's participant byte at step 4, and releases byte 0 again. If that
lands between the survivor's poll and its lock attempt, the survivor sees a held
byte (`OwnerAlive`) or loses the `F_OFD_SETLK` (`Contended`). **0043's table
already has these rows. What it does not say is that the holder may be a joiner
passing through, who hands the byte back.** These sites describe a held byte 0
after a hangup as an heir:

- `Inheritance::Contended`'s doc: *"Another survivor won the ownership byte and
  is binding"*, and `OwnerAlive`'s: *"The owner is alive. Nothing was
  attempted."* The same text is in `crates/tf_tree_c/src/unstable.rs` and the
  C header.
- `inherit_ownership`'s example comment: *"Contended is fine: somebody won"*.
- The Python binding's `owner_lost` docstring, `crates/tf_tree_py/src/tree.rs:604`
  (*"`Contended` is fine: somebody else won"*), and the shipped stub
  `python/tf_tree/_core.pyi:524` (*"somebody won"*).
- [`RUNBOOK.md`](../RUNBOOK.md)'s recovery snippet, line 551:
  *"another survivor won; keep going"*.
- §3.5's pseudo-code: *"held -> somebody already took over, or is mid-bind"*,
  and §3.4 step 2's comment: *"another process is mid-bind; it will be serving
  shortly"*.
- `owner_lost`'s three-state table.

The snippets in the bindings and the runbook are the ones integrators copy.

The loop §3.5 and the runbook tell an integrator to write is unaffected. It keeps
no latch, so the next cycle's `owner_lost()` answers `true` again and the call is
retried. **A caller that treats one `Contended` or `OwnerAlive` as final is not
unaffected.** An early smoke run, on an earlier revision of the probe with a
one-shot survivor, left the arena ownerless for its full 90 s joiner deadline
after one such answer. That run was not repeated. The probe's smoke results
directory was later overwritten by a run with a retrying survivor, so its output
was copied from the session transcript into a result file. Such a caller
already ignores the documented loop, so it is recorded here as a hazard in the
prose, not as a defect in recovery.

## Decision

**This is a draft and it authorises nothing.** Step 1 landed beside it as a
factual correction that stands on the measurement, not on this record. Three
things are proposed and one is left open, and the open one is the record's
substance.

**1. The spec states the bound truthfully (step 1, landed beside this record).**
A survivor learns of the owner's death when the last open file description on
the owner's socket and byte 0 closes. Normally that is the owner's exit, and any
core dump and the address-space teardown come before it. For an owner whose
`fork` child outlives it, it is that child's exit. Nothing the protocol lets a
survivor take shortens it. [`PHASE2.md`](../PHASE2.md) §3.7 step 9's
*"in microseconds"* is corrected now, citing this record as a draft, because it
is a false statement of fact in the spec, not a design choice.
The other sites in the sweep table belong to steps 3 and 4.

**2. The runbook gains operator guidance** under *The arena's owner died*. It
covers:

- **the trade**: a crash dump, or recovery bounded by the process's teardown
  time, for any process that can hold the role.
- **how to suppress a dump for chosen processes only**:
  `prlimit --core=1:1 -- <cmd>`, `LimitCORE=1` in a systemd unit, or the same
  limit set in a launch wrapper. Only the first was measured; the other two set
  the same limit and should be checked with `/proc/<pid>/limits` before the
  runbook says so. Also that `ulimit -c 0` is **not** the setting when the host
  pipes its dumps.
- **the teardown**: about 100 ms per GiB of dirty 4 KiB anonymous memory, which
  no setting removes.

It must say *every process that may hold the role*, not *the owner*. Ownership
migrates ([`0055`](./0055-the-recovery-capacity-a-fleet-cannot-add-later.md)'s
eligible heirs are exactly the processes that may hold it next), so suppressing
dumps on today's owner alone protects one handover.

**3. The inheritance docs stop implying that a held byte 0 means an heir.**
Every site listed under *The secondary finding* above — the Rust, C and Python
docs, the runbook snippet and the two PHASE2 passages — gains the case: a fresh
`open()` passing through §3.4 steps 2–4 holds byte 0 briefly and gives it back.
While `owner_lost()` keeps answering `true`, no single non-`Inherited` answer is
final.

**4. Open: whether the library should release the rendezvous early from a
dying owner.** That is option *a* below, and question 1 holds it.

## Rationale

Each alternative is argued against the rules it would have to live with.

### a. An owner-side fatal-signal handler: open, and the principal question

`tf_tree_ipc` would install handlers for `SIGABRT`, `SIGSEGV`, `SIGBUS`, `SIGILL`
and `SIGFPE`. Each would `close(2)` the rendezvous listener, the accepted client
sockets and the session's lock-file descriptor, then restore the default
disposition and re-raise, so the process still dies of the same signal and still
dumps. `close(2)` is async-signal-safe. This would remove the dump window and
leave the teardown window, since it does nothing for `SIGKILL`. **It is the only
option the library could build that shortens the window and keeps the core.** A
kill from outside (*e*) shortens it by forfeiting the core. And it shortens it
only for an owner whose descriptions no forked child shares: `close(2)` in the
dying parent releases nothing while such a child holds them, so for that owner
it depends on `0030`'s hole being closed first.

It is not rejected, because nothing established here refutes it. It is not
recommended either, because each item below is real:

- **§3.5 requirement 5 is the first obstacle.** *"Serving must stop before byte 0
  is released."* A handler runs on the faulting thread while every other thread
  keeps running until the re-raise stops them, and that includes the serving
  thread mid-handshake. Closing byte 0 there opens the window requirement 5
  forbids: an heir binds and grants while the old server may be writing the
  participant table for a grant of its own. Closing the listener and the
  accepted sockets **first** narrows that window, but a narrower window is not
  an argument. It needs D15's crash-matrix walk, and every §11.3 site that
  aborts, `takeover.after_ownership_lock_before_bind` among them, now dies by a
  different route.
- **The fd registry is [`0030`](./0030-the-atfork-handler-and-inherited-descriptors.md)'s
  problem, in a different shape.** A handler needs descriptor numbers in
  async-signal-safe statics. `0030`'s draft prototype showed a bare
  `[AtomicI32; N]` closing foreign descriptors in a fork child three ways, and
  built five rules against them. Mapped onto a handler, the hazards change:
  - **`0030`'s skew does not arise.** A handler takes no snapshot of the
    descriptor table, so the gap between `fork` copying the table and copying
    memory has no analogue. Its defence, rule (ii)'s `prepare` spinlock that
    `register` also takes, is inapplicable, and it would be wrong here: a fatal
    signal can arrive on a thread that holds that lock inside `register`, and
    `0030` already lists the fork-from-a-signal-handler form of that deadlock
    among its unmeasured failure modes.
  - **The double close does arise.** Every other thread keeps running until
    the re-raise stops it. If the handler closes number N, another thread opens
    a file and gets N, and an owning handle on a third thread then drops and
    closes N, that close takes a descriptor it does not own. That is the race
    rule (v) exists for: the registry slot owns the descriptor, and whichever of
    the handler and the handle `swap`s the live number out is the only one that
    closes it. With rule (i) `swap`-clear and rule (iii) unregister-before-close,
    it holds without a lock, and rule (v) is also the rule that changes the type
    of every fd-holding field in the seam.
- **Two lock-file descriptions, and closing either releases every byte on it.**
  The `Session`'s carries byte 0 and the participant byte. The tree's
  `lock_file` carries the claim leases *and* A2's topology byte, byte 1
  ([`0029`](./0029-the-topology-lock-is-a-kernel-lock.md); `Tree::lock_file`'s
  field doc). Close only the first, and the dead owner's edges stay held for the
  dump, and so does byte 1 if it died inside `reparent`. Close the second too,
  and both become takeable while other threads of the dying process may still be
  running. A claim can then be reaped beside a thread still inside `push`, which
  is what A4's epoch fence, kept by §6.1, exists for. Byte 1 can be taken by a
  stealer that reads the arena word's holder as dead and mutates topology beside
  a holder thread still inside `reparent`, which is what `0029`'s steal argument
  would have to be re-walked for. Both must be walked, not assumed.
- **One process-wide disposition, and the application owns it.**
  `pthread_atfork` handlers stack, which is why `crates/tf_tree_ipc/src/fork.rs`
  could install one without asking anybody. `sigaction` replaces. A library
  handler has to chain to whatever was installed before it, and to whatever is
  installed after it, which it cannot see:
  - Rust's standard library installs `SIGSEGV`/`SIGBUS` handlers for
    stack-overflow detection on an alternate signal stack. The probe's author
    reported that in a smoke trial, a `raise(SIGSEGV)` came out as `SIGABRT`
    through that handler; this was not re-checked.
  - CPython's `faulthandler`, when enabled, takes all five signals.
  - Sanitizers and crash reporters (Breakpad, Crashpad) install their own.
  - `rclcpp` installs `SIGINT`/`SIGTERM` handlers only, so a ROS 2 node is not
    in conflict on these five, but it is one more handler a chaining design has
    to be tested beside.
- **More than one arena per process** means the registry covers every attachment
  that owns, and **a process that later inherits** must register at
  `inherit_ownership`, not only at create.
- **The unsafe budget.** `sigaction` is kind 2, the OS, which `tf_tree_ipc`
  already carries for `fork.rs` and `ofd.rs`, so under
  [`0007`](./0007-the-unsafe-budget-and-the-c-abi.md) as
  [`0048`](./0048-a-kind-is-not-a-crate-name.md) restated it, **no new kind is
  needed**. A new file needs a row in `scripts/unsafe-budget.txt`, and the
  handler's `// SAFETY:` block has to argue async-signal-safety of the whole
  walk, as `0030` found `fork.rs`'s block would.
- **`0019` does not forbid it**: a signal handler is neither a thread nor a
  daemon, and nothing polls. **D17 does not forbid it**: no timeout is added,
  and the socket still carries the signal; the process closes it sooner.

### b. Survivor-side detection through `SO_PEERCRED` and `/proc`: rejected

The survivor would read the owner's pid off its socket and treat
`CoreDumping: 1` as death. This was rejected for three reasons:

- **It does not help by itself.** The dumping owner still holds byte 0, so the
  survivor that detects the dump cannot take it. Serving without byte 0 breaks
  §3.4's split-brain argument, which is the one invariant the design will not
  trade. The one thing such a survivor could do with the knowledge is kill the
  owner, which is *e*.
- **It is a heuristic on a path whose premise is that there are none**
  (§3.3: *"`/proc` parsing and PID-reuse defence are no longer on the rendezvous
  path at all"*).
- **Its signal does not say what it would be used for, measured.**
  `CoreDumping: 1` appeared in 16 of 20 `AN` trials that never dumped and closed
  their files within half a millisecond. Every one of those processes was dying,
  but the flag cannot tell a second-long dump from an exit that is nearly over.

### c. Disabling dumps by default in the library: rejected as a default

The library could call `prctl(PR_SET_DUMPABLE, 0)` or set `RLIMIT_CORE` to 1 at
attach. **Either takes crash evidence away from users who never asked**, on the
process most likely to be the one whose crash needs explaining. And
`PR_SET_DUMPABLE` also governs who may `ptrace` the process and who owns its
`/proc` entries, so it would take the debugger away as well. As an opt-in it is
one line of launch configuration, which is exactly (2)'s guidance, with no
library code and no second spelling of `prlimit`.

### d. Document and do nothing else

This is (1) + (2) + (3). **It is the proposal for everything except question 1**,
and it is what remains if question 1 is answered *no*. What it cannot do is make
recovery fast for a fleet that keeps its dumps, and that fleet is the default.

### e. Killing a dumping owner from outside: rejected as library behaviour, open for the runbook

A `SIGKILL` delivered to a process that is writing its core ends the dump. #332's
instrument comment records it in `crates/tf_tree_bench/src/bin/shm_torture.rs`
(*Instrument 5*): on 6.8 with a pipe `core_pattern`, an abort left alone reaped
at 1.8 s as signal 6 with a core, and the same abort killed 50 ms into its dump
reaped at once as signal 9 with no core. Its subject was not a tf_tree owner: the
script matching that description, which is not in the repository, timed
`python3 -c 'os.abort()'` from spawn. Since the reap followed `owner_lost()` by
under half a millisecond in every trial above, a killed owner should release
its socket and byte 0 at once and pay only its teardown. That is **inferred**,
not measured on an owner.

**That 1.8 s is not this record's ~1.1 s, and the difference is not
explained.** It includes interpreter start-up, and apport kept a report for that
packaged binary under `/var/crash`, where the probe's unpackaged binary left
none: the directory was listed before and after the main run. Whether the
report accounts for the other ~0.7 s is **not measured**, so the ~1.1 s figures
in this record belong to the probe's owner on that host, not to every process
there.

So an external kill shortens the window, and it pays exactly what *c* was
rejected for: the core. As a library mechanism it would be *b* with a signal on
the end. It keeps *b*'s second objection. Its third becomes one about cost:
the flag cannot tell a dump worth interrupting from an exit that is nearly over,
so a survivor that kills on sight forfeits every core. It kills a process by a
number that can name a different process once the owner's parent reaps it
(unless it is held through a `pidfd`), and it makes one participant's crash
evidence another participant's decision. **It is rejected as library
behaviour.** An operator or a supervisor can do it with no code, and whether the
runbook should offer it beside Decision 2's per-process limit is a question for
step 2: it keeps the dump for every crash nobody is waiting on, and forfeits it
only where somebody chose recovery. Like *a*, it releases nothing while a forked
child shares the owner's descriptions.

## Consequences

- **§3.5's recovery has a stated latency floor that belongs to the host, not to
  tf_tree.** The floor is the owner's exit time, with a core dump and
  address-space teardown in it, or longer if a forked child shares the owner's
  descriptions. No figure from any test in this workspace bounds it, and no test
  could without choosing a `core_pattern`.
- **§12.2's migration row becomes scoped, not wrong.** 0.6–1.2 ms p50 is a
  `SIGKILL` of a small owner, and has to say so beside the number.
- **An operator gets a choice they did not know they were making.** Keeping
  dumps on processes that may hold the role costs about a second of refused
  joins and no inheritance per crash on the dev host. Lookups continue
  throughout.
- **[`0055`](./0055-the-recovery-capacity-a-fleet-cannot-add-later.md)'s vacancy
  gets a second and longer producer.** `0055` holds that the eligible set can
  only shrink from the instant byte 0 falls free. The same argument applies
  earlier, for the whole of the owner's exit, because joins are already refused
  while the dying owner still holds byte 0, before `owner_lost()` can answer
  `true` for anyone. So the census problem `0055` describes applies across a
  core dump, and a harness marker around a `kill()` cannot bracket a dump that
  starts inside the victim's own thread. How that affects `shm_torture` is
  question 3.
- **Nothing here reopens anything [`0009`](./0009-descoping-phase-6.md) cut**,
  and nothing changes an arena byte, a wire message or a public type.

## Implementation plan

1. **Correct [`PHASE2.md`](../PHASE2.md) §3.7 step 9**, add this record and its
   index row, and register the measurement as a probe in
   [`EVIDENCE.md`](../benchmarks/EVIDENCE.md), as `0053` step 2 did for its
   figures. **Landed beside this record.** Verified by `just
   artifact-versions`: relative links, table rows, and the draft-citation check,
   which this citation passes because it uses no settled verb. The (`draft`)
   marker follows the pattern the spec's citations of `0052` and `0055` use, and
   is held by review, not by the script.
2. **[`RUNBOOK.md`](../RUNBOOK.md) guidance**, per Decision 2, as a subsection of
   *The arena's owner died* with a one-line pointer from
   *`ArenaHeldButUnreachable`*. It carries the trade, the three per-process
   spellings, the warning that soft 0 does not stop a pipe dump, the *every
   process that may hold the role* scope, and the teardown figure with its THP
   caveat. It also fixes the recovery snippet's *"another survivor won"*
   comment (line 551) per Decision 3, and says whether it offers *e*'s external
   kill. It is not blocked on question 1: if *a* is built it removes one term
   of the trade, and the guidance shrinks. Verified by `just artifact-versions`,
   and by reading the subsection against §3.7 step 9 so the two use the same
   figures and the same dates.
3. **The release-visible prose.**
   - `Tree::owner_lost` (`tree.rs:3081`) and its three-state table.
   - `tf_tree_ipc`'s `client.rs` module doc (lines 8-11), `peer_hung_up`
     (`client.rs:52`) and the `server.rs` module doc.
   - `tf_tree_ipc`'s crate doc (`lib.rs:22`) and its crates.io page
     (`crates/tf_tree_ipc/README.md:31`), scoped as §3.3's row is in step 4.
     `error.rs:186` and `runtime_dir.rs:172` quote that row (the first for the
     NFS contrast, which still holds) and follow it.
   - `Inheritance::Contended` and `OwnerAlive`, and `inherit_ownership`'s
     example, in `crates/tf_tree/src/open.rs`.
   - The matching text in `crates/tf_tree_c/src/unstable.rs` and
     `include/tf_tree_unstable.h`.
   - The Python binding's `owner_lost` docstring
     (`crates/tf_tree_py/src/tree.rs:604`) and the shipped stub
     (`python/tf_tree/_core.pyi:524`).
   - A `CHANGELOG.md` entry, because `tf_tree/src`, `tf_tree_ipc/src`, the C
     headers and the Python surface are release-visible.

   Verified by `just doc`, `just lint`, `just c-header-check`, `just py-lint`
   (the only gate that reaches `tf_tree_py`'s rustdoc), and `just
   artifact-versions`, whose changelog-currency rule is what requires the entry.
   A final `rg -n -i 'microsecond|immediately|at once|instantly'
   crates/tf_tree/src crates/tf_tree_ipc/src crates/*/README.md`, the sweep's own
   pattern, should return nothing unqualified about a hangup or a dead holder's
   lock.
4. **[`PROJECT.md`](../PROJECT.md) D17, and [`PHASE2.md`](../PHASE2.md) §3.3's
   table row, §3.4 step 2's comment, §3.5's pseudo-code, §11.4's kill-marker
   blockquote and §12.2's migration row.** D17 is a decision-log entry, so it
   gets an amendment note under it, the way D16 carries its own, and its text is
   not rewritten. §3.3's *"immediately"* gains *"at the end of the holder's
   exit"* (its two quotes in `tf_tree_ipc/src` follow in step 3, which carries
   the changelog entry). §3.5's pseudo-code and
   §3.4 step 2's *"it will be serving shortly"* comment gain Decision 3's case.
   §11.4's *"tens of microseconds"* becomes the sub-millisecond reap §0.0
   measures. Verified by `just artifact-versions`.
   **This step may not cite this record as settled while it is `draft`**; the
   wording has to stand on the measurement.
5. **No regression test for steps 1–4**, and the reason has to be recorded in
   the tests' neighbourhood, not left implicit. They document kernel behaviour,
   and a test would pin the kernel, not tf_tree. **A test of the dump window is
   not portable anyway.** `core_pattern` is host-global and needs root to
   change, and the answer differs between apport, systemd-coredump, a file
   pattern and none. `--victim-ballast-mb` failed on the runner for the same
   kind of reason (THP). **A portable control for the teardown window might
   exist**: `SIGKILL` an owner holding dirty anonymous memory with huge pages
   refused for that region (`MADV_NOHUGEPAGE`, or `PR_SET_THP_DISABLE` for the
   process). Neither was measured here, both need the unsafe budget in a test
   target, and the §0.0 row records that chunking allocations did not defeat THP.
   So *none* is a possible outcome. `--stop-owner-ms` remains the harness's way
   to hold the socket open with no memory physics in it.
6. **Question 1's mechanism, if it survives review.** Deliberately not broken
   into steps: planning it would be choosing it.

## Open questions

### 1. Should a dying owner release the rendezvous from a fatal-signal handler?

This is option *a*, with its hazards listed there. What would decide it:

- a §3.5 requirement 5 walk that closes the listener and accepted sockets before
  byte 0 and shows no double grant, or shows a remaining window and its cost;
- the chaining story for Rust's stack-overflow handler, CPython's
  `faulthandler`, sanitizers and a crash reporter, **measured**, not read;
- whether the claim-lease description, and byte 1 with it, is closed too,
  argued against A4 and `0029`'s steal;
- a registry design that takes no lock reachable from the handler, so none of
  `0030`'s rule (ii), and keeps rule (v) against a handler's `close` racing an
  owning handle's;
- what it does for an owner with a forked child, which is nothing until
  `0030`'s hole is closed;
- **a test that can fail.** The candidate is step 5's teardown control
  inverted: an aborting owner with dumps suppressed and ballast held, whose
  survivor must see `owner_lost()` before the reap by roughly the teardown time.
  Without the handler, the probe measured that gap at 0.04–0.41 ms. With the
  handler, the claim is that the gap becomes the teardown time, about 100 ms at
  1 GiB of 4 KiB pages on the dev host. A handler that closed nothing would
  leave it under half a millisecond, so a threshold well above 0.4 ms and well
  below the teardown can fail.

### 2. Does the GitHub runner's systemd-coredump drain the core?

If it does, the window grows with RSS and a large owner there waits longer than
on apport. If it does not, it is the helper's runtime. This can be measured
without a local reproduction. A `workflow_dispatch` job would run the
tf_tree-free program below at ballast 0, 256 MiB and 1 GiB with a THP-proof
ballast, and print `core_pattern`, `/etc/systemd/coredump.conf`'s effective
`Storage=`/`ProcessSizeMax=` and the timings. **A related unmeasured question for
the runbook**: whether `Storage=none` or `ProcessSizeMax=0` shortens the window
without touching per-process limits.

### 3. Should `shm_torture` suppress dumps in its children?

It could keep them because a real fleet has them, or suppress them because a
harness should test §3.5, not the host's crash helper. The 2026-09-12 wedge is
consistent with a dumping heir. Suppressing dumps would make that class
disappear from the nightly without explaining it. Keeping them makes the
crash-points job's recovery depend on `core_pattern`. This is a question about
what the harness pins, and **this record does not decide it**. `0055` holds the
related population question. **Not decision-affecting**: nothing in Decisions
1–4 depends on the answer, so it may stay open when this record moves to
`ready`.

### 4. Does the bound deserve a NORMATIVE statement in §3.5?

If it does, §3.5 would say something like: *"A survivor learns of the owner's
death when the last open file description on the owner's socket and byte 0
closes. For the owner that is the end of its exit, which includes any core dump
and the teardown of its address space; a `fork` child sharing those
descriptions holds them until it exits. Nothing in this protocol shortens
it."* Stating it normatively would stop the next document from promising
microseconds. It would also be a normative line no test checks, and `0055`'s
*Consequences* names that failure mode.

## Reproduction

A program with no tf_tree in it. The forked "owner" holds a listening
`AF_UNIX` socket with one accepted connection and an OFD write lock on byte 0,
which is what a tf_tree owner holds. The parent blocks in `poll()` for the
hangup, then spins on `F_OFD_GETLK`. It is trimmed from the second agent's
program and was run in this form before being quoted. **It is not added to the
repository as a file.** `just evidence-audit` could not see one, since it
enumerates only cargo `bin`, `example` and `bench` targets, and a script under
`scripts/` would join `just py-lint`'s scope as upkeep for a draft's evidence.
It is registered instead as a probe row in
[`EVIDENCE.md`](../benchmarks/EVIDENCE.md), whose documented command is the one
in the program's first line, the way that register carries its uncommitted MCAP
survey.

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
`core dumped 0/3`, measures a different arm. Under THP `always`, a ballast run
understates the teardown. **An abort on a host with a crash helper may leave a
report behind** (apport writes under `/var/crash` for packaged binaries, and
systemd-coredump keeps its journal entry), so run it where that is acceptable.

The tf_tree probe was the four-role program described under *What was
measured*. Its source, summariser and raw CSVs stayed outside the repository
with the session that produced them. What a reader can rerun is the program
above and the four roles as described. The numbers in this record come from that
CSV, from the re-run's result files and from the nightly logs cited, with three
exceptions whose result files were copied from session output rather than
written by the run itself: the output above, the author's 1 GiB `n=2` run
(`python3 hup_min.py 2 K,AN 1024`), and the 90 s one-shot-survivor smoke run.
