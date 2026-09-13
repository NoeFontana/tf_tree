# 0057: an owner is not dead until its files close

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** (none yet)

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

### What the documents promise, swept

`rg -n -i 'microsecond|immediately|at once|instantly'` over `docs/`,
`crates/tf_tree/src` and `crates/tf_tree_ipc/src`, kept where the match is about
owner death, hangup, `owner_lost` or inheritance:

| Site | What it says | What it should say |
|---|---|---|
| [`PHASE2.md`](../PHASE2.md) §3.7 step 9 | a participant learns the owner died *"in microseconds"* | when the owner's exit closes the socket. **Corrected in the same change as this record**, citing it as a draft |
| `Tree::owner_lost` rustdoc, `tree.rs:3081` | *"`POLLHUP` in microseconds, exactly"* | same; release-visible, so it goes in `CHANGELOG.md` (step 3) |
| [`PROJECT.md`](../PROJECT.md) D17, quoted at `server.rs:15` and `client.rs:52` | the owner sees a participant's `EPOLLHUP` *"in microseconds — exact, immediate"* | same mechanism, from the other end (below) |
| [`PHASE2.md`](../PHASE2.md) §3.3, *Verified behaviour* table, quoted at `runtime_dir.rs:172` | *"Holder dies without unlocking → lock released by the kernel, immediately"* | immediately **at the end of exit**. The `runtime_dir.rs` quote contrasts this with NFS lease expiry, and that contrast still holds |
| [`PHASE2.md`](../PHASE2.md) §12.2, *owner kill → new owner serving* | 0.6–1.2 ms p50 | not wrong: it is measured with `SIGKILL` on a small owner, and should say so |

[`RUNBOOK.md`](../RUNBOOK.md)'s *The arena's owner died* gives no latency figure,
and neither do [`0019`](./0019-one-binary-and-topology-you-can-wait-for.md),
[`0037`](./0037-a-takeover-is-not-a-second-open.md),
[`0043`](./0043-owner-lost-is-a-question-about-the-owner.md) or
[`0044`](./0044-recovery-the-languages-a-robot-is-written-in-cannot-reach.md).
None of them mentions a core dump either; nothing under `docs/` does.
[`PHASE2.md`](../PHASE2.md) §11.2 scenario 9's *"kill the owner, and immediately"*
describes when a test starts its process, not a latency, and is left alone.

The teardown half was already known, from `SIGKILL`. §0.0's `shm_torture` row
records that a `SIGKILL`ed process releases its socket and participant byte in
`exit_files()`, which `do_exit` runs after `exit_mm()`, and that the delay scales
with dirty pages at about 0.09 ms per resident MB. **What that row did not have is
the dump**, and on the hosts measured here the dump is the larger term by an order
of magnitude.

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
| `K` | `SIGKILL` | 2.7 MB | 20 | 0.237 / 0.310 / 0.359 ms | 0 / 20 |
| `AN` | `abort()` under `prlimit --core=1:1` | 2.7 MB | 20 | 0.292 / 0.437 / 0.494 ms | 0 / 20 |
| `A0` | `abort()`, host default (pipe to apport) | 2.7 MB | 20 | **1097.1 / 1105.4 / 1107.9 ms** | 20 / 20 |
| `A256` | `abort()`, host default | 265 MB | 20 | **1123.2 / 1127.1 / 1132.8 ms** | 20 / 20 |
| `A1024` | `abort()`, host default | 1051 MB | 20 | **1200.1 / 1211.3 / 1215.4 ms** | 20 / 20 |
| `K1024` | `SIGKILL` | 1051 MB | 20 | **98.0 / 103.1 / 106.6 ms** | 0 / 20 |

Also measured, in the same trials:

- **`owner_lost()` went true 0.04–0.11 ms before the reap in 120 of 120 trials,
  and never while `CoreDumping: 1` was still readable.** That held in all 60
  dump trials. Across the six arms, the first `Inherited` came 0.16–0.29 ms
  after `owner_lost()` and the joiner's first success 0.42–0.50 ms after
  (medians). So the survivor's trigger, the heir's bind and a fresh join are
  all bounded by the owner's exit, and they follow it closely.
- **Every join attempt during the dump was refused** with
  `Rendezvous(ArenaHeldButUnreachable { holder_slots: 3, first_slot: Some(0), …,
  ownership_held: true })`. That was 52 attempts per trial in `A0`, 53 in
  `A256`, 56–57 in `A1024`, 4–5 in `K1024`, and 0 in `K` and `AN`. The counts
  come from the joiner's 20 ms per-attempt timeout and mean only that it was
  refused for the whole window.
- **After the last `CoreDumping: 1` reading, `owner_lost()` took a further
  0.49–0.60 ms in `A0`, a median 26.0 ms in `A256` and 101.5 ms in `A1024`.**
  That scales like `K1024`'s teardown, which fits the address space being torn
  down after the dump and adding to it.
- **The owner's `wchar` grew by 61 440–61 466 bytes during every dump**,
  whatever the RSS.
- **`CoreDumping: 1` showed on one poll in 16 of 20 `AN` trials**, with no dump,
  no `wchar` growth and a reap under 0.55 ms.

**An independent re-run by a second agent, same host, the same day, reproduced
every headline.**

- The original binary, N=10 per arm, `K`/`A0`/`AN` interleaved: medians of
  0.221 ms, 1099.3 ms and 0.251 ms.
- A variant with **no joiner and no `/proc` poller**: 0.213 ms, 1097.9 ms and
  0.256 ms. So the poller is not a confound.
- **A tf_tree-free program**, the one under *Reproduction* below. At ballast 0,
  N=8, the hangup medians were `K` 0.413 ms, `A` 1097.3 ms and `AN` 0.496 ms.
  At 1 GiB, N=6: `K` 106.9 ms, `AN` 102.4 ms and `A` 1203.0 ms. **In every arm
  the hangup, byte 0 reading free and the reap arrived within about 0.1 ms of
  each other.**

`AN` at 1 GiB is the arm the main run did not have: an abort with no dump still
pays the teardown. This record's author ran the trimmed script before writing.
At ballast 0, n=3, the medians were 0.380 ms, 1097.6 ms and 0.495 ms. At 1 GiB,
n=2, `K` and `AN` were 102.9 ms and 102.8 ms.

**Measured, therefore:** on this host an owner that aborts and dumps through the
pipe cannot be detected, inherited from or joined for about 1.1 s. An owner
with 1 GiB of dirty 4 KiB anonymous pages cannot be for about 100 ms, whether it
was killed or aborted. With the dump suppressed and the owner small, all three
happen in under half a millisecond.

**Inferred, not measured:**

- **That the ~1.1 s is apport's own runtime, not the kernel writing a core.**
  `wchar` stopped at about 60 KiB at every RSS, which fits the kernel filling a
  64 KiB pipe the helper never drains, then waiting for the helper to exit
  because `core_pipe_limit` is non-zero. apport's log is root-only and was not
  read. It is equally unconfirmed that `wchar` counts dump writes at all.
- **The kernel ordering.** The dump is written from signal delivery, before
  `do_exit`; within `do_exit`, `exit_mm()` runs before `exit_files()`. That is
  read from kernel source, and the timings agree with it, but no trace was taken.
- **That the brief `CoreDumping: 1` under `AN` is `coredump_wait()` running**
  before `do_coredump` reaches the limit-of-1 refusal. Also read from source.

### The mechanism, and why nothing a survivor does can shorten it

`owner_lost()` is a zero-timeout `poll` for `POLLHUP` on the survivor's attach
socket, then `F_OFD_GETLK` on byte 0 ([`0043`](./0043-owner-lost-is-a-question-about-the-owner.md)).
`inherit_ownership()` returns `OwnerAlive` without trying anything while
`owner_lost()` is false (`crates/tf_tree/src/open.rs:594-596`). Both questions
are about kernel objects the owner holds until `exit_files()`: its end of the
connection, and the OFD lock on byte 0. So the facade inherits the kernel's
delay exactly, and the tf_tree arms match the raw-socket arms to within about
2 ms.

**A survivor with more information still could not act on it.** `/proc` says
`CoreDumping: 1` within a tenth of a millisecond, and a survivor that believed it
would still find byte 0 **held**. `Session::take_over_ownership` would get
`EAGAIN`, and it should: byte 0 is what makes one process the server
([`PHASE2.md`](../PHASE2.md) §3.3), and §3.4's split-brain argument depends on
nobody serving without it. Only the dying process can release it earlier (option
*a* below).

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
- **The dying owner's own claims stay held**, and a successor asking for one of
  those edges is refused, because the claim leases are OFD locks on a
  description the owner holds until `exit_files()`. `Tree::reap()` cannot
  collect them either, since `F_OFD_GETLK` still reports them held. That is §6.1 working: a dumping owner is not yet dead.
- **A participant that dies during the window is not collected by any hangup
  callback**, because the owner's serving thread is not running. This is the
  ordinary state of an ownerless arena, and the byte-keyed collectors still
  work.
- **Supervisors are not affected.** A supervisor restarts a process when it reaps
  it, and the reap follows the files closing by about 0.1 ms, measured. So a
  restarted process never meets its predecessor's held bytes. What the window
  delays is every *other* process.

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
- **A soft `RLIMIT_CORE` of 0 does not stop a pipe dump.** It is the shell
  default on the dev host and on the runner below, and in both places aborts
  reaped with `core_dumped` true. `core_pattern` is a host-wide sysctl, so a
  container gets its host's helper.
- **A pipe `core_pattern` is the default on the hosts this project has
  measured.** The dev host uses apport. **The GitHub runner uses
  systemd-coredump.** #332's `[diag] host:` line in nightly runs 34769883403 and
  34769889900 (2026-09-13) reads:

  ```text
  core_pattern=`|/usr/lib/systemd/systemd-coredump %P %u %g %s %t 9223372036854775808 %h %d`
  core_pipe_limit=16 core_rlimit soft=0 hard=unlimited osrelease=6.17.0-1022-azure
  ```

  The torture and crash-points jobs print it with `soft=0`. **In the two
  crash-points jobs, all 50 reaped aborts (21 and 29) read
  `core_dumped=true`.** The ASan job prints `soft=1`, which is inferred to be the
  sanitizer runtime's doing, so its children do not dump. **No dump was timed on
  the runner**: those aborts were ordinary participants, the harness polls
  `CoreDumping` once per driver round, and every line reads `dump_seen=never`.
  So whether systemd-coredump drains the core, and the window grows with RSS, or
  behaves like apport here, is open question 2.
- **The owner is often a large process.** The owner is whichever process
  created the arena or last inherited it — the bridge, under
  [`0015`](./0015-the-bridge-fills-a-shared-arena.md), or any node. On a robot that is a perception or
  planning process as often as a thin one, and teardown alone costs it tens of
  milliseconds per GiB.

**The 2026-09-12 `shm_torture --crash-points` wedge is consistent with this, and
not shown by it.** In that run, an heir that had inherited at owner kill 12 was
armed at `hangup.after_probe_before_cas:1` and aborted in its own serving thread
0.5 s later. The driver never entered its kill window. For an interval the log
bounds below by several hundred milliseconds and above by about 2 s, no survivor
saw the role vacant. All eight non-owner children left on their ordinary exits
during it, their rejoins were refused, and the run ended. A dumping heir
explains that interval. That run's log carries no core pattern and no dump
timing, so this is an explanation, not a finding.

### The secondary finding: a single non-`Inherited` answer is not final

With the joiner running, the survivor's **first** `inherit_ownership()` after
`owner_lost()` answered `true` returned something other than `Inherited` in
**21 of 120** trials: `Contended` 15 and `OwnerAlive` 6. It happened in every
arm, dump or no dump (`K` 5, `A0` 3, `AN` 3, `A256` 1, `A1024` 8, `K1024` 1),
and **every one of them returned `Inherited` on the next call.** The re-run
found 10 of 30 with the joiner and **0 of 30 without it**.

Nothing instrumented who held byte 0, so the cause is inferred. The joiner is
the only other process, and removing it removed the effect. The mechanism fits
§3.4: a fresh `open()` that finds nobody serving takes byte 0 at step 2, meets
the survivor's participant byte at step 4, and releases byte 0 again. If that
lands between the survivor's poll and its lock attempt, the survivor sees a held
byte (`OwnerAlive`) or loses the `F_OFD_SETLK` (`Contended`). **0043's table
already has these rows. What it does not say is that the holder may be a joiner
passing through, who hands the byte back.** Four documents describe a held
byte 0 after a hangup as an heir:

- `Inheritance::Contended`'s doc: *"Another survivor won the ownership byte and
  is binding"*. The same text is in the C header.
- `inherit_ownership`'s example comment: *"Contended is fine: somebody won"*.
- §3.5's pseudo-code: *"held -> somebody already took over, or is mid-bind"*.
- `owner_lost`'s three-state table.

The loop §3.5 and the runbook tell an integrator to write is unaffected. It keeps
no latch, so the next cycle's `owner_lost()` answers `true` again and the call is
retried. **A caller that treats one `Contended` or `OwnerAlive` as final is not
unaffected.** An early smoke run, on an earlier revision of the probe with a
one-shot survivor, left the arena ownerless for its full 90 s joiner deadline
after one such answer. That run was not repeated, and such a caller already
ignores the documented loop, so it is recorded here as a hazard in the prose,
not as a defect in recovery.

## Decision

**This is a draft and it authorises nothing beyond step 1, which is a factual
correction and does not wait for review.** Three things are proposed and one is
left open, and the open one is the record's substance.

**1. The spec states the bound truthfully (landed with this record).** A
survivor learns of the owner's death when the owner's exit closes its files. Any
core dump and the address-space teardown come before that, and nothing a
survivor can do shortens it. [`PHASE2.md`](../PHASE2.md) §3.7 step 9's
*"in microseconds"* is corrected now, citing this record as a draft, because it
is a false statement of fact in the spec, not a design choice.
The other sites in the sweep table are step 3's.

**2. The runbook gains operator guidance** under *The arena's owner died*. It
covers:

- **the trade**: a crash dump, or recovery within about a millisecond, for any
  process that can hold the role.
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
`Inheritance::Contended` and `OwnerAlive`, `inherit_ownership`'s example,
`owner_lost`'s table and §3.5's pseudo-code gain the case above: a fresh
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
option that shortens the window at all**, because only the dying process holds
what has to be released.

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
  `[AtomicI32; N]` closing foreign descriptors when registration races, and five
  rules to make it sound. Rule (v) changes the type of every fd-holding field in
  the seam. Here the process is about to die, so the double close that drove
  rule (v) can arise only between the handler's `close` and the re-raise, not
  for a child's lifetime. **Racing registration arises in full**: a number
  closed while another thread reuses it closes whatever that thread just opened.
- **Closing the lock-file description releases every byte on it**: byte 0, the
  participant byte, and byte 1 if a `reparent` is in flight. Claim leases live
  on a separate description (`0030` question 2). Leave that one open and the
  dead owner's edges stay held for the dump. Close it and they become reapable
  while other threads of the dying process may still be inside `push`. A4's
  epoch fence, which §6.1 keeps, exists for exactly that, but it must be walked,
  not assumed.
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

- **It does not help.** The dumping owner still holds byte 0, so the survivor
  that detects the dump cannot take it. Serving without byte 0 breaks §3.4's
  split-brain argument, which is the one invariant the design will not trade.
- **It is a heuristic on a path whose premise is that there are none**
  (§3.3: *"`/proc` parsing and PID-reuse defence are no longer on the rendezvous
  path at all"*).
  [`0033`](./0033-the-identity-record-cannot-name-a-namespace.md) already
  showed a recorded pid cannot be resolved across namespaces.
- **It gives false positives, measured.** `CoreDumping: 1` appeared in 16 of 20
  `AN` trials that never dumped.

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

## Consequences

- **§3.5's recovery has a stated latency floor that belongs to the host, not to
  tf_tree.** The floor is the owner's exit time, with a core dump and
  address-space teardown in it. No figure from any test in this workspace
  bounds it, and no test could without choosing a `core_pattern`.
- **§12.2's migration row becomes scoped, not wrong.** 0.6–1.2 ms p50 is a
  `SIGKILL` of a small owner, and has to say so beside the number.
- **An operator gets a choice they did not know they were making.** Keeping
  dumps on processes that may hold the role costs about a second of refused
  joins and no inheritance per crash on the measured hosts. Lookups continue
  throughout.
- **[`0055`](./0055-the-recovery-capacity-a-fleet-cannot-add-later.md)'s vacancy
  gets a second and longer producer.** `0055` holds that the set of eligible
  heirs can only shrink while joins are refused. This record adds that joins are
  refused for the whole of the owner's exit, before `owner_lost()` can answer
  `true` for anyone. So the census problem `0055` describes applies across a
  core dump, and a harness marker around a `kill()` cannot bracket a dump that
  starts inside the victim's own thread. How that affects `shm_torture` is
  question 3.
- **Nothing here reopens anything [`0009`](./0009-descoping-phase-6.md) cut**,
  and nothing changes an arena byte, a wire message or a public type.

## Implementation plan

1. **Correct [`PHASE2.md`](../PHASE2.md) §3.7 step 9** and add this record and
   its index row. **Landed with this record.** Verified by `just
   artifact-versions`: relative links, table rows, and the draft-citation check,
   which this citation passes because it uses no settled verb and says `draft`.
2. **[`RUNBOOK.md`](../RUNBOOK.md) guidance**, per Decision 2, as a subsection of
   *The arena's owner died* with a one-line pointer from
   *`ArenaHeldButUnreachable`*. It carries the trade, the three per-process
   spellings, the warning that soft 0 does not stop a pipe dump, the *every
   process that may hold the role* scope, and the teardown figure with its THP
   caveat. It is not blocked on question 1: if *a* is built it removes one term
   of the trade, and the guidance shrinks. Verified by `just artifact-versions`,
   and by reading the subsection against §3.7 step 9 so the two use the same
   figures and the same dates.
3. **The release-visible prose.**
   - `Tree::owner_lost` (`tree.rs:3081`) and its three-state table.
   - `tf_tree_ipc::peer_hung_up` (`client.rs:52`) and the `server.rs` module doc.
   - `Inheritance::Contended` and `OwnerAlive`, and `inherit_ownership`'s
     example, in `crates/tf_tree/src/open.rs`.
   - The matching text in `crates/tf_tree_c/src/unstable.rs` and
     `include/tf_tree_unstable.h`.
   - §3.5's pseudo-code and §3.4 step 2's *"it will be serving shortly"* comment.
   - A `CHANGELOG.md` entry, because `tf_tree/src`, `tf_tree_ipc/src` and the C
     headers are release-visible.

   Verified by `just doc`, `just lint`, `just c-header-check`, and `just
   artifact-versions`, whose changelog-currency rule is what requires the entry.
   A final `rg -n 'microsecond' crates/tf_tree/src crates/tf_tree_ipc/src` near
   `POLLHUP`/`EPOLLHUP` should return nothing unqualified.
4. **[`PROJECT.md`](../PROJECT.md) D17, §3.3's table row and §12.2's migration
   row.** D17 is a decision-log entry, so it gets an amendment note under it,
   the way D16 carries its own, and its text is not rewritten. §3.3's
   *"immediately"* gains *"at the end of the holder's exit"*, and the
   `runtime_dir.rs:172` quote follows it. Verified by `just artifact-versions`.
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
- whether claim leases are closed with it, argued against A4;
- a registry design that survives `0030`'s racing-registration measurement
  without its rule (v);
- **a test that can fail.** The candidate is question 5's teardown control
  inverted: an aborting owner with dumps suppressed and ballast held, whose
  survivor must see `owner_lost()` before the reap by roughly the teardown time.
  Without the handler, the probe measured that gap at 0.04–0.11 ms. With the
  handler, the claim is that the gap becomes the teardown time. A handler that
  closed nothing would leave it at 0.1 ms, so the test can fail.

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
related population question.

### 4. Does the bound deserve a NORMATIVE statement in §3.5?

If it does, §3.5 would say something like: *"A survivor learns of the owner's
death when the owner's exit closes its files; that includes any core dump and the
teardown of its address space; nothing in this protocol shortens it."* Stating it
normatively would stop the next document from promising microseconds. It would
also be a normative line no test checks, and `0055`'s *Consequences* names that
failure mode.

## Reproduction

A program with no tf_tree in it. The forked "owner" holds a listening
`AF_UNIX` socket with one accepted connection and an OFD write lock on byte 0,
which is what a tf_tree owner holds. The parent blocks in `poll()` for the
hangup, then spins on `F_OFD_GETLK`. It is trimmed from the second agent's
program and was run in this form before being quoted. **It is not added to the
repository**: a runnable file there would have to be registered with `just
evidence-audit`, and this is evidence for a draft, not a gate.

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
above and the four roles as described; the numbers in this record come from
that CSV and from the re-run.
