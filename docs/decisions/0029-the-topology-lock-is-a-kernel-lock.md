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

## The ordering constraint

§5.1's word-before-byte rule binds a **probe**; an `F_OFD_SETLK` **acquire** excludes later takes, so byte-then-word does not contradict it.
- **T1.** On a tree with a lock file, the topology word is CASed non-zero only while this process holds byte 1, and the byte is released *after* the word.
- **T2.** Holding byte 1 and observing a non-zero word means the holder is **dead** or **a writer with no lock file**.
- **T3.** The residual `/proc` predicate decides only T2's second disjunct, and can only *withhold* a steal.

## Decision

**`Tree::reparent` acquires an OFD lock on the lock file's byte 1 before it
touches the arena word.** With no lock file, nothing changes.
1. **Byte 1 is the topology mutation lock**, taken first and released last: the
   lease guard is declared *before* the `TopoGuard` (T1).
2. **Byte contention is refused, not resolved:** `ReparentError::LockContended`,
   naming what the word says and nothing where the word is zero (`owner_slot` is
   `Option<u32>`; `a_contended_topology_lock_never_renders_a_sentinel_slot`).
3. **The byte is re-attempted `TOPO_BYTE_ATTEMPTS` = 32 times** before contention
   is reported; an `fcntl` error is not retried
   (`the_topology_byte_is_retried_before_contention_is_reported`).
4. **A non-contention `fcntl` failure is `ReparentError::TopologyLease { raw_os_error }`.**
5. **The `/proc` predicate stays as the residual** (`participant_is_alive`: "may this word be stolen"). The owner's hangup callback (P5, D17) is untouched.

## Consequences

- Byte 1, not an arena field. `TopoLockView::acquire` keeps its `Fn(u32) -> bool` parameter (D14).
- A writer with no lock file is still decided by the triple (`0028` step 0b). A byte held by a forked child (`0030`) makes `reparent` refuse; `TFT014` reports it. A change to the *word* protocol owes a `loom` model with a live victim.

## Implementation plan, as landed

The steps are the table's, at the top of this record. Step 4's first test stores a stale `start_time` in a live joiner's record and asserts `reparent` refuses; the control, byte released, steals. The second uses `tf_tree_rendezvous_child hold-topo` and `SIGKILL`.

### Why the lifecycle is compressed

Steps 1–2 without step 3 would ship a protocol change with no crash-matrix walk (D15).

## Appendix: what the `/proc` triple was measured doing

Cited by `0033` and `decisions/README.md`. Ubuntu 24.04, kernel 6.8, unprivileged.

- **PID-namespace mismatch:** under `unshare -U --fork --pid`, `F_OFD_GETLK` says
  `F_WRLCK` (alive) while the triple says dead. Needs `TF_TREE_RUNTIME_DIR` named
  and an **empty** `uid_map`.
- **`hidepid=2`** also hides a same-user **non-dumpable** target.
- An unreaped zombie keeps its `/proc` entry: triple alive, byte released.
