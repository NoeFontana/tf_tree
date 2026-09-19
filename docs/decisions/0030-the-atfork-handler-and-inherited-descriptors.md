# 0030: the atfork handler and inherited descriptors

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** none.

## Context

Split out of [`0028`](./0028-the-slot-a-killed-participant-keeps.md) step 7:
closing inherited descriptors amends normative `docs/PHASE2.md` §6.2 and §7.3. Two
descriptions are inherited across `fork`: the client socket, so the owner's `epoll`
never sees `HUP` and `0028` step 4's fast path is defeated; and the participant lock
byte, so the kernel says "alive" for a process with no mapping and a poisoned
`Tree`. The leak is one slot per dead-parent-with-surviving-inheritor event, not one
per forked child: a fork child never registers.

**What to settle:** can a child-side `pthread_atfork` handler close the inherited
descriptors under async-signal-safety, from a registry readable without allocating
or locking (`crates/tf_tree_ipc/src/fork.rs`)?

## Decision

None yet. A normative amendment needs its own argument, §11.2/§11.3 walk and test.

## Implementation plan

Empty until then. `0028` step 7's verification carries over: extend
`crates/tf_tree_bench/tests/fork.rs` (fork after attach, `SIGKILL` the parent,
assert the owner observes `HUP` while the child still runs and the child's `Tree`
is still poisoned). The §6.2/§7.3 amendment is **NORMATIVE** and must be narrow:
§5.1 forbids `state` and `heartbeat`, not every liveness source (§0.0's §5.1 row);
[`0029`](./0029-the-topology-lock-is-a-kernel-lock.md) enumerates which tree gets
which fact.

## Open questions

1. **RESOLVED: buildable, but not as the obvious `[AtomicI32; N]` sketch**, which
   is unsound three ways: read-don't-clear is wrong under nested `fork`; the child's
   own owning handles close numbers the handler already took (`MappedArena`'s
   `fd: OwnedFd` closes as drop glue); and `fork` copies the descriptor table before
   the address space, so a number foreign at copy time can become a registered tree
   fd by the time memory is copied.

   **The buildable design is five rules:** (i) `swap`-clear in the child handler;
   (ii) a `prepare` spinlock excluding `register` across the fork;
   (iii) `unregister` strictly before `close`; (iv) a bounded walk behind a high-water mark; (v) the registry slot owns
   the descriptor and the handle owns the slot: the handle stores its slot index, and
   its `Drop` `swap`s that slot to `EMPTY` and closes only if it got a live fd back.
   Rule (v) is the integration cost: `OwnedFd` and `File` cannot be the handle, so
   every field in question 2 changes type.

   `register` at capacity (`CAP = 256`) refuses, as an `OpenError` at attach
   (growing is a fifth unsafe boundary, [`0007`](./0007-the-unsafe-budget-and-the-c-abi.md)). `fork.rs`'s module
   `// SAFETY:` block becomes false ("no `prepare` or `parent` handler") and the
   amendment must say so; the fd side needs `MappedArena`'s `owner_pid`-style
   `getpid` guard. *Unmeasured:* a thread preempted holding the `prepare` lock.

2. **RESOLVED: five descriptions for a joiner, plus the accept set for an owner.**
   Every entry is also a rule-(v) type change.
   **Every role:**
   1. **The `Session`'s `LockFile`:** the participant byte (and byte 0 for an
      owner). **Register.**
   2. **The claim-lease `LockFile`:** a forked child keeps a dead parent's leases,
      so a successor gets `ClaimApiError::LeaseContended`. **Register.**
   3. **The `LivenessProbe`'s `LockFile`:** takes no lock. **Harmless.**
   4. **The client socket** (`Attachment::Joined._socket`). **Register.**
   5. **The memfd** (`MappedArena.fd`): a forked child pins the `Shmem` after the
      parent unmapped. **Register, but across a crate boundary:** `tf_tree_arena`
      does not depend on `tf_tree_ipc`, so either the facade registers a number it
      does not own (breaking rule (v)) or the dependency inverts. **Still open.**

   **Owner only:**
   6. **`OwnerServer.listener`:** a child holding it keeps the socket listening
      after the owner dies, so §3.4 step 1's `connect()` succeeds and a fresh
      process "joins" a corpse. **Register.**
   7. **`OwnerServer.shutdown` and `ShutdownHandle.eventfd`:** two descriptors, one
      description. **Register both, or neither.**
   8. **The `epoll` set and accept table:** §3.5 takeover is "a watcher on the
      client socket", so a forked child would block every heir. **Register at
      accept; unregister before the `drop(sock)` in the hangup arm.**

   A read-only attacher holds the same five as a read-write joiner. A
   `build_shared` / `attach_shared` tree holds only the memfd.

3. **Does closing them in the child change what the child can observe?** Its
   `Tree` is already poisoned, so it should not; that needs a test.
4. **Does this interact with `0028` step 4's rebase?** The test in question 3 must
   be written against the rebased hangup callback.

## What would make this `ready`

- Questions 1 and 2: **MET.**
- Question 3 and the §11.2/§11.3 walk (rule (v) touches every fd-holding field):
  **NOT MET.**
- The §6.2/§7.3 amendment, narrow and against `0029`'s enumeration: **NOT MET**. It
  must also cover the lock file's byte 1 (A2's topology lock, §3.3), held only per
  `Tree::reparent`: a far smaller risk than the claim byte's, held for a
  `Publisher`'s life.
