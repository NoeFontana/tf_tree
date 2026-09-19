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

§3.5's recovery starts when a survivor learns the owner died; the documents said
"in microseconds". That holds only for a small process that does not dump core: the
kernel closes a process's files at the end of its **exit**, after any core dump and
address-space teardown. A forked child sharing the owner's descriptions
([`0030`](./0030-the-atfork-handler-and-inherited-descriptors.md) question 2) holds
them until **that child** exits.

**The bound is the last close of the two kernel objects `owner_lost()` asks about**:
the peer end of the survivor's attach connection and the open file description
holding byte 0 ([`0043`](./0043-owner-lost-is-a-question-about-the-owner.md)).

## What was measured

Dev host, 2026-09-13: Linux 6.8, 8 CPUs, `core_pattern` piped to apport, soft
`RLIMIT_CORE` 0. Owner death to `owner_lost()` true, median:

| Arm | How the owner died | Owner RSS | median | `core_dumped` |
|---|---|---|---|---|
| `K` | `SIGKILL` | 2.7 MiB | 0.237 ms | 0 / 20 |
| `AN` | `abort()` under `prlimit --core=1:1` | 2.7 MiB | 0.292 ms | 0 / 20 |
| `A0` | `abort()`, host default | 2.7 MiB | **1097 ms** | 20 / 20 |
| `A1024` | `abort()`, host default | 1027 MiB | **1200 ms** | 20 / 20 |
| `K1024` | `SIGKILL` | 1027 MiB | **98 ms** | 0 / 20 |

- `owner_lost()` went true 0.04-0.41 ms before the reap in 120 of 120 trials.
- Teardown adds to the dump; it needs no dump and no setting removes it: ~100 ms per
  GiB of dirty 4 KiB pages (far faster under THP `always`, as on GitHub's runners).

### Why nothing a survivor can take shortens it

`owner_lost()` is a zero-timeout `poll` for `POLLHUP` on the attach socket, then
`F_OFD_GETLK` on byte 0; `inherit_ownership()` returns `OwnerAlive` without trying
while it is false. A dumping owner still holds byte 0, and
`Session::take_over_ownership` correctly gets `EAGAIN`: byte 0 is what makes one
process the server ([`PHASE2.md`](../PHASE2.md) §3.3, §3.4).

### What the window costs

- Lookups are unaffected (§3.5); existing publishers keep publishing.
- The dying owner's claims stay held (`ClaimApiError::AlreadyClaimed`) until a
  survivor calls `reap_dead`: no hangup callback covers a dead owner.
- A `reparent` is refused (`ReparentError::LockContended`) if the owner died holding
  byte 1 ([`0029`](./0029-the-topology-lock-is-a-kernel-lock.md)).
- Nobody inherits, and every join is refused
  (`Rendezvous(ArenaHeldButUnreachable)`).

### The secondary finding: a single non-`Inherited` answer is not final

The survivor's **first** `inherit_ownership()` after `owner_lost()` answered `true`
returned `Contended` or `OwnerAlive` in 21 of 120 trials, and every one returned
`Inherited` on the next call. A fresh `open()` that finds nobody serving takes byte
0 (§3.4 step 2), meets the survivor's participant byte (step 4) and releases it; the
holder may be a joiner passing through. The integrator loop is unaffected; **a
caller that treats one `Contended` or `OwnerAlive` as final is not.**

## Decision

**1. The spec states the bound truthfully.** A survivor learns of the owner's death
once its attach connection has hung up and the last open file description holding
byte 0 has closed: for a dying owner the end of its exit (after any core dump and
teardown); for an owner whose `fork` child outlives it, that child's exit.

**2. The runbook gains operator guidance** under *The arena's owner died*: the
trade; the dump window as the crash helper's run, no figure, pointing at
*Reproduction*; suppressing dumps per process (`prlimit --core=1:1 -- <cmd>`,
`LimitCORE=1`; `ulimit -c 0` is **not** the setting when the host pipes dumps);
systemd-coredump `Storage=`/`ProcessSizeMax=` as unmeasured guidance; the teardown
with the THP caveat; one sentence that a supervisor's `SIGKILL` of a dumping owner
ends the window and forfeits the core, not recommended (*e*). It must say *every
process that may hold the role*: ownership migrates
([`0055`](./0055-the-recovery-capacity-a-fleet-cannot-add-later.md)).

**3. The inheritance docs stop implying that a held byte 0 means an heir.**
`Inheritance::Contended`/`OwnerAlive` docs, `inherit_ownership`'s example, the C
header, the Python `owner_lost` docstring and stub, `RUNBOOK.md`'s recovery snippet
and `PHASE2.md` §3.4/§3.5 gain the case: a fresh `open()` passing through §3.4
steps 2-4 holds byte 0 briefly; while `owner_lost()` stays `true`, no single
non-`Inherited` answer is final.

**4. The library does not release the rendezvous early.** No fatal-signal handler is
installed; a dying owner keeps its socket, byte 0 and lock-file description until
the kernel closes them (*Rationale a*).

**5. The bound is NORMATIVE in §3.5, and a test pins it.** `PHASE2.md` §3.5:

> **NORMATIVE.** `owner_lost()` answers `true` once the survivor's attach
> connection has hung up and the last open file description holding byte 0 has
> closed (`0043`). For a dying owner that is the end of its exit, which includes
> any core dump and the teardown of its address space; a `fork` child sharing
> those descriptions holds them until it exits. tf_tree adds no delay, heartbeat
> or timeout to that event (D17).

The pin is two tests in `crates/tf_tree/tests/rendezvous.rs`, with no timing:
`a_read_only_survivor_reports_that_it_cannot_inherit` (first `owner_lost()` after
`owner.kill()` must be `true`) and
`a_survivor_that_did_not_inherit_stops_being_told_the_owner_is_gone` (a `join-heir`
child's first call must be `true Inherited`; after the heir is killed the other
survivor's first call must be `true` again). Both run under `just shm-check` and
`just shm-rendezvous`. Preconditions: the owner has no child sharing its
descriptions; no `open()` is inside §3.4 steps 2-4; no other survivor inherits
between reap and call; no other task holds a transient reference to the owner's
socket or lock-file description (`lsof`, `pidfd_getfd`, an in-flight `SCM_RIGHTS`
descriptor). Mutants (a latch answering `false` until the hangup is seen twice; a
grace period) must fail both. The pin does not bound the dump window or teardown.

**6. `shm_torture` suppresses its children's dumps.** The torture recipes run under
`prlimit --core=1:1 --`, so no `unsafe` is added and CI inherits it. The driver
prints a `[diag]` warning, never a verdict, when armed crash points run with a pipe
`core_pattern` and a soft core limit other than 1. The bare binary outside the
recipe still runs the dumping configuration.

## Rationale

### a. An owner-side fatal-signal handler: rejected

It is the only option the library could build that keeps the core and removes both
windows for a crash. Rejected because:

1. **Correctness outranks latency.** Releasing byte 0 while the serving thread runs
   breaks NORMATIVE §3.5 requirement 5; closing the tree's `lock_file` description
   releases claim leases and byte 1 while threads may still be in `push` or
   `reparent`, recreating the zombie writer §6.1 makes impossible
   ([`0029`](./0029-the-topology-lock-is-a-kernel-lock.md),
   [`0028`](./0028-the-slot-a-killed-participant-keeps.md)). A dump window costs
   latency, never a corrupt read.
2. **A library must not own process-global signal dispositions**: `sigaction`
   replaces rather than stacks, and chaining is unverifiable.
3. **Partial coverage**: nothing for `SIGKILL`, or for an owner with a forked child.

**What would reopen it:** field evidence of a deployment needing both the core and
recovery shorter than its dump, as an **opt-in, async-signal-safe release call from
the application's own crash handler** (close listener and accepted sockets,
`F_UNLCK` byte 0), with a §3.5 requirement 5 walk, a measured chaining story, the
`lock_file`/byte 1 argument, and a test that can fail.

### b. Survivor-side detection through `SO_PEERCRED` and `/proc`: rejected

A dumping owner still holds byte 0, and `CoreDumping: 1` appeared in 16 of 20 `AN`
trials that never dumped.

### c. Disabling dumps by default in the library: rejected

It takes crash evidence from users who never asked; as an opt-in it is Decision 2.

### d. Document and do nothing else

Decisions 1-3 with 5 and 6; the operator trades dumps against recovery time.

### e. Killing a dumping owner from outside: rejected as library behaviour

A `SIGKILL` during a dump ends it (`shm_torture.rs`'s *Instrument 5*), but it
forfeits every core, can name a reused pid, and makes one participant's crash
evidence another's decision. It releases nothing while a forked child shares the
descriptions.

## Consequences

- **§3.5's recovery has a latency floor that belongs to the host**; no test could
  bound it without choosing a `core_pattern`.
- **The NORMATIVE line commits to the event, not a duration.** A heartbeat, grace
  period or timeout in owner detection contradicts it and D17 and needs a
  superseding record.
- **§12.2's migration row and §12.3 gate 4's *kill → re-claimable* margin are
  scoped, not wrong**: their victim is a small `SIGKILL`ed child.
- After step 4, `shm_torture`'s recipes no longer exercise recovery across a core
  dump, and lose one source of §0.0's *"`aborted` is a floor rather than a count"*.

## Implementation plan

1. **Landed** (#333). `PHASE2.md` §3.7 step 9, the index row and the
   [`EVIDENCE.md`](../benchmarks/EVIDENCE.md) probe row.
2. **Landed** (#337). The specs and the runbook.
3. **Landed.** The release-visible prose and the pin's two mutant runs.
4. **`shm_torture`'s dumps** (#341; runner-side verification owed).
   - `prlimit --core=1:1 --` on `just shm-torture`, `just shm-torture-crash-points`
     and `just shm-torture-asan`, and on the two `cargo nextest` lines of
     `just shm-check` and `just shm-rendezvous` and on `no-network.sh`'s
     `strace -f`. `crates/tf_tree_core/src/crash_tests.rs`'s under `just test` gets no
     prefix (no `prlimit` off Linux).
   - The driver's `[diag]` warning and justfile comments; `PHASE2.md` §0.0's
     `shm_torture` row and §11.4.

   **Still owed:** the first crash-points nightly after landing must show
   `core_dumped=false` on every `[diag] reap` line of an aborted armed child (the
   runner is kernel 6.17 with systemd-coredump; suppression is measured only on 6.8
   with apport), and a `workflow_dispatch` run of *Reproduction*'s `AN` arm on the
   runner is recorded in the PR. Verified by `[diag] host:` reading
   `core_rlimit soft=1 hard=1` under each recipe; `just shm-check`;
   `just shm-rendezvous`; `just shm-torture-self-test`; and a positive control: the
   warning prints for the bare binary with `--crash-points` on a pipe-`core_pattern`
   host.
5. **Status to `implemented`** when step 4's runner verification has landed.

The dump window and teardown are not tested: `core_pattern` is host-global and
needs root.

## Open questions

None. All four were answered 2026-09-14.

1. **Should a dying owner release the rendezvous from a fatal-signal handler?** No
   (Decision 4, *Rationale a*).
2. **Does the GitHub runner's systemd-coredump drain the core?** Closed, not
   measured: no decision depends on it.
3. **Should `shm_torture` suppress dumps in its children?** Yes (Decision 6).
4. **Does the bound deserve a NORMATIVE statement in §3.5?** Yes, with a pin
   (Decision 5).

## Reproduction

A program with no tf_tree in it: a forked "owner" holds a listening `AF_UNIX` socket
with one accepted connection and an OFD write lock on byte 0; the parent blocks in
`poll()` for the hangup, then spins on `F_OFD_GETLK`. It is registered as a probe
row in [`EVIDENCE.md`](../benchmarks/EVIDENCE.md) whose command is the program's
first line, not added as a file.

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

On the dev host `K` and `AN` read under 0.6 ms and `A` about 1098 ms. Read the output against `/proc/sys/kernel/core_pattern` and
`/sys/kernel/mm/transparent_hugepage/enabled`: a file pattern, or `A` reading
`core dumped 0/3`, measures a different arm; under THP `always` a ballast run
understates the teardown. An abort on a host with a crash helper may leave a report
behind.
