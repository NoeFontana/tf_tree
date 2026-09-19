# 0029: the topology lock is a kernel lock

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** this PR, in one change — see *Why the lifecycle is
compressed*.

| Step | What landed |
|---|---|
| 1 — byte 1 in the lock file | `tf_tree_ipc::LockFile::{try_take_topology, release_topology}`, `LockRole::Topology` |
| 2 — `reparent` takes it before the word | `Tree::reparent`, `TopologyLease`, `ReparentError::TopologyLease` |
| 3 — the §11.3 `topo.holding_lock` walk | `docs/PHASE2.md` §11.3, and §5.1's probe-vs-acquire amendment |
| 4 — the discriminating test | `a_live_holder_that_proc_calls_dead_keeps_the_topology_lock`, plus `a_killed_topology_lock_holder_releases_its_byte_to_the_kernel` across a process boundary |

## This record was re-scoped, and the re-scope is the decision

It proposed swapping the `/proc` triple for the `F_OFD_GETLK` probe; both say
"dead" about a live holder (other PID namespace, `hidepid`, or no participant
byte: `0031`'s class). On this path you can ask the kernel. A false "dead"
steals from a live mutator (§6.2); a false "alive" only delays.

## The ordering constraint

§5.1's word-before-byte rule binds a **probe**, which races every later take; an
`F_OFD_SETLK` **acquire** excludes them, so byte-then-word does not contradict
it. Invariants:

- **T1.** On a tree that carries a lock file, the arena topology word is CASed to
  a non-zero value only while this process holds topology byte 1, and the byte is
  released only *after* the word is.
- **T2.** Therefore, if this process holds byte 1 and observes a non-zero
  topology word, the holder is either **dead** or **a writer with no lock file**.
- **T3.** The residual `/proc` predicate decides only T2's second disjunct, and
  only in the safe direction: it authorises a steal solely where it can prove
  death, and it can only ever *withhold* one.

## Decision

**`Tree::reparent` acquires an OFD lock on the lock file's byte 1 before it
touches the arena word.** With no lock file, nothing changes.
1. **Byte 1 of the lock file is the topology mutation lock**, taken first and
   released last: the lease guard is declared *before* the `TopoGuard`, so drop
   order releases the word, then the byte (T1).
2. **Byte contention is refused, not resolved.** It returns
   `ReparentError::LockContended`, naming whatever the word says and **nothing
   where the word is still zero**: `owner_slot` is `Option<u32>` and the core's
   `u32::MAX` sentinel is translated once, at `From<TopoLockError>`
   (`a_contended_topology_lock_never_renders_a_sentinel_slot`).
3. **The byte keeps the word's patience:** `reparent` re-attempts it
   `TOPO_BYTE_ATTEMPTS` = 32 times before reporting contention (a refusal, where
   the word's exhaustion is a steal); an `fcntl` error is not retried
   (`the_topology_byte_is_retried_before_contention_is_reported`).
4. **A non-contention `fcntl` failure is `ReparentError::TopologyLease { raw_os_error }`.**
5. **The `/proc` predicate stays, unchanged, as the residual** deciding T2's
   second disjunct. `participant_is_alive` is not deleted: it answers "may this
   topology word be stolen", narrower than `Tree::participant_alive`.
6. **P5 is untouched.** The owner server's socket-hangup callback reclaims a
   *participant record* on the socket alone (D17), neither the byte nor the
   triple. It is the only facade path that mutates the participant table on a
   liveness verdict.

### The exposure, before and after (no row widens)

| holder | before | after |
|---|---|---|
| live, other PID namespace, or non-dumpable under `hidepid` | **stolen from** — #213 | byte held ⇒ refused |
| no lock file (`0031`'s class), live or dead, or unreaped zombie | decided by the triple | unchanged |

## Consequences

- **Byte 1, not an arena field** (`FORMAT_VERSION = 3` already happened).
  `TopoLockView::acquire` keeps its `Fn(u32) -> bool` parameter (D14). §0.0's
  #205 row shrinks from three paths to two; `reparent` costs 1.011 µs → 2.96 µs.
- **The residual is stated, not removed:** a writer with no lock file is still
  decided by the triple (`0028` step 0b did the same for `attach_shared(ReadWrite)`).
- A byte held by a forked child (`0030`) makes `reparent` refuse while the child
  lives; `TFT014` reports it. No `TFT0xx`, `probe_topology` or blocking
  `reparent` (`0018`, D17). A change to the *word* protocol owes a `loom` model
  with a live victim. "`0029` question 3" citations mean `0028` question 3.

## Implementation plan, as landed

Steps are the table's; the release order is held by scope, not a test. Step 4's
first test stores a stale `start_time` in a live joiner's record and asserts
`reparent` refuses and the word did not change hands; the control, byte
released, steals. It does not stage a PID namespace (`0033` does). The second
uses `tf_tree_rendezvous_child hold-topo` and `SIGKILL`.

### Why the lifecycle is compressed

Steps 1–2 without step 3 would ship a protocol change with no crash-matrix walk (D15).

## Appendix: what the `/proc` triple was measured doing

Cited by `0033` and `decisions/README.md`. Ubuntu 24.04, kernel 6.8, unprivileged.

- **PID-namespace mismatch:** under `unshare -U --fork --pid` a
  `tf_tree_rendezvous_child` completes the rendezvous; `F_OFD_GETLK` says
  `F_WRLCK` (alive) while the triple says dead (`/proc/1` is `systemd`,
  `Known(st) != stored`). Needs `TF_TREE_RUNTIME_DIR` named and an **empty**
  `uid_map`, which is what passes `runtime_dir.rs`'s uid gate.
- **`hidepid=2`** also hides a same-user **non-dumpable** target: a third
  dependency beside §3.10's same-user rule and a shared PID namespace.
- An unreaped zombie keeps its `/proc` entry: triple alive, byte released.
