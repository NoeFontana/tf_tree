# 0030: the atfork handler and inherited descriptors

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** none.

**Read before proposing a design: the obvious `[AtomicI32; N]` registry ("appended
at registration, *read* in the handler") is refuted (question 1), and the leak is
smaller than it looks.** The buildable design is question 1's **five rules**; rule
(v) changes the type of every fd-holding field in the seam.

**The leak is one participant slot per dead-parent-with-surviving-inheritor event,
not one per forked child.** A fork child never registers (`Open::open` is the only
caller of `register_at` / `register_creator`), and while the parent lives the
inherited byte is the parent's own and correct: a 64-worker `multiprocessing` pool
uses **zero** extra of `MAX_PARTICIPANTS = 64`. It only lengthens a single stuck
slot once the parent dies, which is enough, since `0028` piece 2 decides liveness
from the byte alone.

## Context

Split out of [`0028`](./0028-the-slot-a-killed-participant-keeps.md) step 7 on
governance: closing inherited descriptors amends normative `docs/PHASE2.md` §6.2
and §7.3. Two descriptions are inherited across `fork`: the **client socket**
(`CLOEXEC`, but `fork` does not `exec`), so the owner's `epoll` never sees `HUP`
and `0028` step 4's fast path is defeated until the last inheritor exits; and the
**participant lock byte**, so the kernel says "alive" for a process with no
mapping and a poisoned `Tree`.

**What to settle:** can a child-side `pthread_atfork` handler close the inherited
descriptors under async-signal-safety? `close(2)` is safe; the hard part is
*knowing which*, from a registry readable without allocating or locking
(`crates/tf_tree_ipc/src/fork.rs` holds the handler). Closing in the child
releases nothing while the parent lives, so it is safe unconditionally.

## Decision

**None yet.** A normative amendment needs its own argument, §11.2/§11.3 walk and
test.

## Implementation plan

Empty until the questions are answered. `0028` step 7's verification carries
over: extend `crates/tf_tree_bench/tests/fork.rs` (fork after attach, `SIGKILL` the
parent, assert the owner observes `HUP` **while the child still runs**, the
child's `Tree` is still poisoned and its destructors release nothing of the
parent's). The §6.2/§7.3 amendment is **NORMATIVE** and must be narrow: not "§5.1
forbids deciding liveness from anything but the byte" (§5.1 forbids `state` and
`heartbeat`; every tree without a probe keeps `/proc`, §0.0's §5.1 row).
[`0029`](./0029-the-topology-lock-is-a-kernel-lock.md) enumerates which tree gets
which fact.

## Open questions

1. **RESOLVED 2026-08-22: buildable, and the sketch is not the thing to build.**
   A standalone prototype (`[AtomicI32; 256]`, real listener in `epoll`, real
   `F_OFD_SETLK` byte, participant `SIGKILL`ed while its forked child runs)
   restores both signals (`epoll` event, byte FREE; else none, HELD), and its
   handler is async-signal-safe in fact: one `call` (`close`), and 0 allocations
   over 2 000 forks under an `_exit`-on-allocate `#[global_allocator]`.

   **The sketch is unsound three times, all reproduced:**
   *(a) Read-don't-clear is wrong under nested `fork`:* the child reuses the
   numbers the handler freed, forks again, and the grandchild's handler closes 3 of
   3 of the child's own descriptions. `swap`, not `load`.

   *(b) The child's own owning handles close numbers the handler already took.*
   `crates/tf_tree_ipc/src/fork.rs:12-19` says the child runs `Tree`, `ClaimLease`
   and `Attachment` destructors, including on `multiprocessing`'s `os._exit`-less
   path. With swap and unregister-before-close in place the child's own new
   descriptor was still closed (exit 42, 3 runs of 3). An early `return` in
   `Drop::drop` does not help: `MappedArena`'s fork-child guard skips `munmap`, but
   the `fd: OwnedFd` field closes as drop glue.

   *(c) `fork` copies the descriptor table before the address space*, so a number
   foreign at copy time can be a registered tree fd by the time memory is copied,
   and the child closes it: 395 of 574 forks saw the skew, 0 the converse; 89
   foreign closes in 300 000 forks at 63.7 registrations/fork, **0 in 60 000**
   with a static registry. Quote it as reproducible under concurrent registration,
   not as a per-fork constant.

   **The buildable design is five rules:**
   (i) `swap`-clear in the child handler;
   (ii) a `prepare` spinlock excluding `register` across the fork, released by
   `parent` and by a plain store in `child` (else the child's first `register`
   deadlocks); 0 foreign closes in 300 000 forks;
   (iii) `unregister` strictly before `close`: with `register` excluded, the
   registry at copy time is a subset of that at `prepare`, so a listed number had
   not been closed and cannot be foreign (breaking only the ordering: 818 in
   30 000 with a 400 us gap; gap-dependent, so "co-load-bearing", not a rate);
   (iv) a bounded walk behind a high-water mark;
   (v) **the registry slot owns the descriptor and the handle owns the slot**: the
   handle stores its slot index, and its `Drop` `swap`s that slot to `EMPTY` and
   closes only if it got a live fd back, so a handler that already took it makes
   the child's later `Drop` a no-op.

   **Rule (v) is the integration cost:** `OwnedFd` and `File` cannot be the handle
   (unconditional `Drop`), so every field in question 2's list changes type.

   *Overflow:* `register` at capacity returns `Full`; **refuse, as an `OpenError`
   at attach** (the bound is concurrent, not cumulative). Skipping silently
   reopens the hole; growing is a fifth unsafe boundary
   ([`0007`](./0007-the-unsafe-budget-and-the-c-abi.md)'s). `CAP = 256` (1 KiB BSS)
   covers a full owner; `fork` costs 154 us empty, 161 us at 8 entries.

   **`fork.rs`'s module `// SAFETY:` block becomes false** ("no `prepare` or
   `parent` handler"; "a single `fetch_add`") and the amendment must say so.
   `MappedArena`'s `owner_pid` doc guards the analogous `munmap` hazard with
   `getpid` and admits "no test fails when this check is removed"; the fd side
   needs the same guard and honesty.

   *Not established:* the prototype is not in the repository. **Unmeasured:** a thread preempted holding the `prepare` lock stalls every
   forking thread in a yield-free spin, and a `fork` from a signal handler that
   interrupted `register` deadlocks in `prepare`.

2. **RESOLVED 2026-08-22: five descriptions for a joiner, plus the accept set for
   an owner.** Every entry is also a rule-(v) type change.

   **Every role:**
   1. **The `Session`'s `LockFile`:** the participant byte (and byte 0 for an
      owner), what `0028` piece 2 decides liveness from. **Register.**
   2. **The claim-lease `LockFile`:** a separate description of the same file
      holding every claim byte; a forked child keeps a dead parent's leases, so no
      reaper collects the edge and a successor gets
      `ClaimApiError::LeaseContended`. **Register.**
   3. **The `LivenessProbe`'s `LockFile`:** takes no lock (`F_OFD_GETLK` only).
      **Separately harmless.**
   4. **The client socket** (`Attachment::Joined._socket`). **Register.**
   5. **The memfd** (`MappedArena.fd`): one description shared by every
      participant (`SCM_RIGHTS` passes a reference). **Not harmless:** a memfd
      inode lives while *any* reference does, so a forked child holding only the
      inherited descriptor pinned **256 MiB of `Shmem`** after the parent had
      `munmap`ped and closed. Closing it is unobservable and not a liveness
      signal. **Register, but across a crate boundary:** `tf_tree_arena` does not
      depend on `tf_tree_ipc`, so either the facade registers a number it does not
      own (breaking rule (v)) or the dependency inverts. **That sub-decision is
      still open.**

   **Owner only:**

   6. **`OwnerServer.listener`:** a child holding it keeps the socket listening
      after the owner dies, so §3.4 step 1's `connect()` succeeds and a fresh
      process "joins" a corpse instead of taking byte 0 (`connect` returned 0 with
      the handler off, `ECONNREFUSED` on): `ArenaHeldButUnreachable` by an
      unenumerated route, reachable today. **Register.**
   7. **`OwnerServer.shutdown` and `ShutdownHandle.eventfd`:** two descriptors, one
      description. A child's `Tree` drop shutting down the parent's server is
      already blocked by `OwnerThread::fork_gen`. **Register both, or neither.**
   8. **The `epoll` set and accept table** (locals of a serving thread absent in
      the child): only a registry can close them. Free today, but §3.5 takeover is
      "a watcher on the client socket", so then a forked child silently blocks
      every heir. **Register at accept; unregister before the `drop(sock)` in the
      hangup arm.**

   **A read-only attacher holds the same five as a read-write joiner**
   (`register_at` takes a byte regardless of `AccessMode`; `Joined` calls
   `use_claim_leases` unconditionally). **A `build_shared` / `attach_shared` tree
   holds only the memfd**, reached through `exec` where the registry is a freshly
   zeroed static, so registering it does not break inheritance.

3. **Does closing them in the child change what the child can observe?** Its
   `Tree` is already poisoned, so it should not; "should not" needs a test.
4. **Does this interact with `0028` step 4's rebase?** The test in question 3 must
   be written against the rebased hangup callback.

## What would make this `ready`

- Questions 1 and 2: **MET** (the prototype refuted the sketch; the enumeration
  turned up the memfd).
- Question 3 and the §11.2/§11.3 walk (larger, since rule (v) touches every
  fd-holding field): **NOT MET.**
- The §6.2/§7.3 amendment, narrow and against `0029`'s enumeration: **NOT MET, no
  longer blocked** (`0029` is `implemented`, #269). It must also cover the lock
  file's **byte 1** (A2's topology lock, §3.3): inheritable, but held only ~2.94 us
  per `Tree::reparent`, so a child inherits a *held* one only if a `fork` from
  another thread lands in that window **and** the parent dies before it closes.
  Do not equate that risk with the claim byte's, held for a `Publisher`'s life.

