# 0030: the atfork handler and inherited descriptors

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** none.

## Context

Split out of [`0028`](./0028-the-slot-a-killed-participant-keeps.md) as its
step 7 when that record went `ready` on 2026-08-20. The routing was decided on
**governance, not feasibility**: closing inherited descriptors amends normative
`docs/PHASE2.md` §6.2 and §7.3, and `CLAUDE.md` sends a normative protocol change
through a record of its own whichever way the engineering question falls.
`0028`'s open question 5 anticipated exactly this disposition.

`0028` retains the fork hole as a **documented limitation**. Nothing in its plan
reclaims a slot held by a forked child, and that is deliberate — this record is
where it gets addressed.

## The defect, as `0028` established it

Two descriptions are inherited across `fork`, and both defeat a different half of
`0028`'s design:

- **The client socket.** It is `CLOEXEC`, but `fork` does not `exec`. The child
  keeps the connection's open file description alive, so the owner's `epoll`
  never sees `HUP` — and candidate B, which `0028` keeps as its fast path
  (step 4), is driven entirely by that `HUP`. Reclamation is deferred to the last
  inheritor's exit, which for a `multiprocessing` worker pool is its lifetime.
- **The participant lock byte.** By §6.2 the child holds it by the same
  mechanism. So the *kernel's own answer* — the fact `0028` makes authoritative —
  says "alive" for a process that provably cannot participate: it has no mapping
  and its `Tree` is poisoned by the `pthread_atfork` counter (§7.3).

That second point is why this cannot be waved at as a diagnostic problem. `0028`
piece 2 decides liveness from the byte and nothing else; a forked child makes the
byte say the wrong thing, and no fallback is left to catch it.

## What this record has to settle

**Can a child-side `pthread_atfork` handler close the inherited descriptors
inside the async-signal-safety constraint?** `close(2)` is itself
async-signal-safe. The hard part is *knowing which descriptors*: it needs a
registry populated before any fork and readable from the handler without
allocating and without taking a lock. `crates/tf_tree_ipc/src/fork.rs` is where
the existing handler lives and is the constraint any answer has to satisfy.

The obvious shape — a fixed-capacity array of `AtomicI32` with an atomic length,
appended at registration and read in the handler — looks like it satisfies it.
**That is a sketch, not an answer**, and this record should not adopt it without
working through capacity, overflow behaviour, and what happens to a description
that is registered concurrently with a fork in another thread.

Note also that closing a descriptor in the child releases nothing while the
parent lives, which is what makes the operation safe to perform unconditionally —
`0028` states this and it is the reason the fix is plausible at all.

## Decision

**None yet.** This record is `draft` and its purpose on creation is to hold the
question that `0028` could not carry: a normative amendment needs its own
argument, its own §11.3 or §11.2 walk, and its own test.

## Implementation plan

Deliberately empty until the questions below are answered. What is already known
about the shape:

- The verification `0028` step 7 asked for, which carries over verbatim: extend
  `crates/tf_tree_bench/tests/fork.rs` — fork after attach, `SIGKILL` the parent,
  assert the owner observes `HUP` **while the child is still running**, and assert
  the child's `Tree` is still poisoned and its destructors still release nothing
  of the parent's.
- Any amendment to §6.2 and §7.3 is **NORMATIVE** and must be drafted narrowly.
  In particular it must not be written as "§5.1 forbids deciding liveness from
  anything but the byte" — §5.1 forbids deciding it from `state` or `heartbeat`,
  and §0.0's own §5.1 row records that every tree without a probe keeps `/proc`.
  [`0029`](./0029-the-topology-lock-is-a-kernel-lock.md) is the record that
  enumerates which tree gets which fact.

## Open questions

1. **RESOLVED 2026-08-22: buildable — and the sketch is not the thing to build.**
   ~~Is the lock-free fd registry buildable under the constraint? Capacity,
   overflow, and a registration racing a fork in another thread. If the answer is
   no, the fork case stays a permanent documented limitation and this record
   closes as *rejected* rather than sitting open.~~ **This record should not close
   as rejected.**

   A standalone prototype (`[AtomicI32; 256]`, `register`/`unregister`, a
   `pthread_atfork` child handler) restores both signals end to end. Three real
   processes, a real `SEQPACKET` listener in `epoll` with `RDHUP|HUP`, a real
   `F_OFD_SETLK` byte at offset 16; the participant is `SIGKILL`ed **while its
   forked child is still running**:

   ```
   child-side close handler : false      child-side close handler : true
   forked child still alive : true       forked child still alive : true
   epoll events after kill  : 0          epoll events after kill  : 1
   participant byte after   : HELD       participant byte after   : FREE
   ```

   The handler is async-signal-safe *in fact*, two ways. Its disassembly contains
   exactly one `call`, to `close@GLIBC_2.2.5`; everything else is `xchg` and
   `lock incl`, with no PLT call to any allocator or lock. A
   `#[global_allocator]` that `_exit`s on any allocation while the handler runs
   counted **0** over 2 000 forks taken with the registry **full** and a second
   thread hammering glibc's malloc arena lock, each child under a 5 s watchdog.

   **But the sketch above — "appended at registration and *read* in the handler"
   — is unsound three times, and all three are reproduced rather than argued.**

   *Read-don't-clear is wrong under nested `fork`,* which is the
   `multiprocessing` case §7.3 and §14 are about. Deterministic: the child reuses
   the numbers the handler freed, forks again, and the grandchild's handler
   closes **3 of 3** of the child's own descriptions. `swap`, not `load`.

   *The child's own stale handles close the numbers the handler already took.*
   This is not the fork race; it is deterministic and it defeats the obvious
   design. After the handler `swap`s a number out and closes it, the **owning
   handle in the child still holds that number** and closes it again at scope
   exit — and `crates/tf_tree_ipc/src/fork.rs:12-19` states that the child runs
   `Tree`, `ClaimLease` and `Attachment` destructors, *"including on the
   `os._exit`-less path that `multiprocessing` takes"*. Built against the
   prototype with the swap handler and unregister-before-close discipline both
   in place, the child opened one file of its own, dropped the poisoned handles,
   and found its own descriptor closed: **exit 42, 3 runs of 3.** An early
   `return` inside `Drop::drop` does not help — that is exactly
   `MappedArena::drop`'s fork-child guard (`mapped.rs:556-558`), which skips the
   `munmap` while the `fd: OwnedFd` field (`:81`) still closes as drop glue.

   *`fork` does not snapshot the descriptor table and the address space at the
   same instant, and the gap is wide.* Measured with a thread that only ever
   opens — so "the child is missing an fd memory listed" cannot be confused with
   anything else — the address space is copied **after** the descriptor table:
   **395 of 574** forks saw at least one such fd, **0** saw the converse; a
   second run puts the window between roughly 50 and 100 µs, and it moves with
   the opener's rate, so treat it as an order of magnitude and not a measurement.
   So the registry the child handler reads is a *later* observation than the
   table it is closing against, and a number that was **foreign** when the table
   was copied can be a registered tree fd by the time memory is. The child then
   closes it. Reproduced against a victim file identified by
   `(st_dev, st_ino)`: **89 in 300 000 forks at 63.7 registrations/fork**, and
   **53 in 60 000** at the same churn. Quote it as *reproducible under concurrent
   registration, at a rate set by the registration rate* — **not** as a per-fork
   constant: with a **static** registry (8 long-lived fds, no registration in
   flight, which is the tf_tree shape whenever no attach is happening) it is
   **0 in 60 000**.

   The defence that works is a **`prepare` handler holding a spinlock that
   `register` also takes**, released by the `parent` handler and by a plain store
   in the `child` handler: **0 foreign closes in 300 000 forks**. The argument is
   *not* that the registry is bit-identical across the fork — `unregister` runs
   unlocked, so entries can be removed inside the window. It is a **conjunction**
   with unregister-before-close: with `register` excluded for the whole fork, the
   registry at `t_mm` is a subset of the registry at `prepare`, and because
   unregister strictly precedes close, any number still listed at `t_mm` had not
   been closed by `t_files < t_mm` — so it cannot have been foreign in the
   child's table. Breaking only the ordering breaks the defence: prepare-lock on,
   close-before-unregister with a 400 µs gap gave **818 in 30 000** against a
   rate-matched control (15 500 vs 15 446 registrations) at **0 in 30 000**.
   Both are single runs, and two narrower-gap arms of the same violation
   (0 µs and 50 µs) returned 0 — so the hazard is gap-dependent and the pair
   should be read as "rule (iii) is co-load-bearing", not as a rate. The plain
   store in the child is not optional: without it the child's first `register`
   deadlocks, observed deterministically.

   **So the buildable design is five rules, not one array:** (i) `swap`-clear in
   the child handler; (ii) a `prepare` spinlock excluding `register` across the
   fork, released by `parent` and by a plain store in `child`; (iii) `unregister`
   strictly before `close`; (iv) a bounded walk behind a high-water mark;
   (v) **the registry slot owns the descriptor and the handle owns the slot** —
   the handle stores its slot index and its `Drop` `swap`s that slot to `EMPTY`
   and closes only if it got a live fd back, so a handler that already took it
   makes the child's later `Drop` a no-op.

   **Rule (v) is where the integration cost is, and it is much larger than
   "register at four call sites".** `OwnedFd` and `File` cannot be that handle —
   their `Drop` closes unconditionally — so every field in question 2's
   enumeration changes type: `client::Attached{segment, socket}`,
   `lockfile::LockFile{file}` (which hands its `File` to `rustix`/`std` APIs),
   `server::OwnerServer{listener, shutdown}`, `ShutdownHandle{eventfd}`,
   `serve()`'s `epoll` fd and `clients: Vec<Option<(OwnedFd, u32)>>`,
   `mapped::MappedArena{fd}`, `open::Attachment::Joined{_socket}`.

   *Overflow.* At capacity `register` returns `Full` and leaves the array
   unchanged — no partial state. **Refuse, surfaced as an `OpenError` at
   attach**, and the cost stated: a process that *concurrently* attaches more
   trees than the registry holds cannot attach (the bound is concurrent, not
   cumulative). The alternatives were measured, not assumed. Silently skipping:
   the refused description survives into the child, so the hole reopens for
   exactly that fd with nothing to tell it from the pre-`0030` state. Growing: an
   allocation, plus a pointer the handler must read while another thread swaps it
   — a fifth unsafe boundary, so
   [`0007`](./0007-the-unsafe-budget-and-the-c-abi.md), not this record. Sizing
   follows question 2: five descriptions for a joiner, eight plus up to
   `MAX_PARTICIPANTS = 64` accepted sockets for an owner, so `CAP = 256` (1 KiB
   of BSS) covers a full owner with room over. Its price is on `fork`, measured
   over 3 000 forks per row: 154 µs empty → **161 µs at 8 entries** → 224 µs at
   256.

   **`fork.rs`'s module `// SAFETY:` block becomes false under this change and
   the amendment must say so.** It currently reads *"a `child` handler … and no
   `prepare` or `parent` handler"* and *"Its body is a single `fetch_add` on a
   `static AtomicU64`"* (`fork.rs:48-56`). Both stop being true. A handler that
   walks 256 atomics and calls `close(2)` is still async-signal-safe, but the
   invariant as written is a claim about the *body*. The analogue is already on
   the page one crate over: `MappedArena`'s `owner_pid` doc (`mapped.rs:86-107`)
   documents this exact hazard for `munmap` — *"nothing stops the kernel from
   placing a later mapping of the child's own into that hole … at a distance,
   with no diagnostic"* — guards it with `getpid`, and admits **"no test fails
   when this check is removed."** The fd side needs the same guard and the same
   honesty.

   *What this does not establish.* The prototype is standalone and **not in the
   repository; nothing runs it**, and `crates/tf_tree_bench/tests/fork.rs` — the
   verification `0028` step 7 asks for — was not extended by anyone. Nothing in
   the tf_tree workspace was built, tested or linted by either pass. x86-64 only,
   local `rustc 1.97.1` against CI's 1.98.0. It ran clean under
   `-Zsanitizer=thread` but for 400 forks, far below the rate at which either
   hazard appears, and none of it is a `loom` argument. The attribution of the
   skew to `copy_files()` preceding `copy_mm()` is a reading; the direction and
   rough width are the measurement. **Two failure modes of the `prepare` lock
   itself are unmeasured**: a thread preempted while holding it stalls every
   forking thread in a spin with no yield (plausible on an oversubscribed
   container), and a `fork` from a signal handler that interrupted `register`
   deadlocks in `prepare`.

2. **RESOLVED 2026-08-22: five descriptions for a joiner, those plus the accept
   set for an owner. This question already names two of them; what it does not
   name is the owner's listener and the memfd.** ~~Which descriptions must be
   registered? `0028` names the client socket and the participant lock byte.
   There are at least two more independent descriptions in a live tree — the
   claim lock file and, for an owner, the accept set — and whether they belong
   here or are separately harmless has to be worked out rather than assumed.~~
   Read at `7739805`. Every entry below is also a rule-(v) type change.

   **Every role, joiner and owner alike:**

   1. **The `Session`'s `LockFile`** — `tf_tree_ipc/src/open.rs:481`, opened at
      `:311`. Holds the **participant byte** (and byte 0 for an owner). This is
      the description `0028` piece 2 decides liveness from. **Register.**
   2. **The claim-lease `LockFile`** — `tf_tree/src/open.rs:67`, a *third,
      separate* description of the same file by deliberate construction. Holds
      every claim byte at `CLAIM_BASE + edge_id`. A forked child keeps the dead
      parent's leases, so no reaper collects the edge and a successor writer gets
      `ClaimApiError::LeaseContended` for the child's lifetime. Same shape as the
      participant byte and independent of it. **Register** — this question names
      it; the disposition is what is new.
   3. **The `LivenessProbe`'s `LockFile`** — `tf_tree/src/open.rs:117-121`. Takes
      **no** lock; `is_held` is `F_OFD_GETLK` only (`ofd.rs:162`). An inherited
      description that holds no lock releases nothing and keeps nothing alive.
      **Separately harmless**; register for symmetry or not at all.
   4. **The client socket** — `Attachment::Joined._socket`,
      `tf_tree/src/open.rs:410`. `0028`'s own case. **Register.**
   5. **The memfd** — `MappedArena.fd`, `tf_tree_arena/src/mapped.rs:81`; one
      description shared by every participant, because `SCM_RIGHTS` passes a
      reference rather than a copy. **Named nowhere in this record, and not
      harmless.** §3.9's "freed with its last mapping" (`PHASE2.md:880`, and
      ":554" says "the last mapping drops") is not the rule — a memfd inode lives
      while *any* reference does. Measured: a forked child holding only the
      inherited descriptor, with no mapping at all (`MADV_DONTFORK`), pinned
      **256 MiB of `Shmem`** after the parent had both `munmap`ped *and* closed;
      the same program without the fork returned to baseline immediately. Closing
      it in the child has no observable consequence there (nothing to unmap,
      `Tree` already poisoned) and it is not a liveness signal, so it cannot make
      anything read "alive". **Register — but the cost is a crate boundary**:
      `tf_tree_arena` depends on `bytemuck` + optional `rustix` and deliberately
      not on `tf_tree_ipc` (`mapped.rs:93-97`), so either the facade registers a
      number it does not own — breaking rule (v) — or that dependency is
      inverted. **That sub-decision is still open, and it is the one part of this
      answer that is not settled.**

   **Owner only:**

   6. **`OwnerServer.listener`** — `tf_tree_ipc/src/server.rs:60`. A child holding
      it keeps the socket *listening* after the owner dies, so §3.4 step 1's
      `connect()` **succeeds** and a fresh process "joins" a corpse and burns the
      handshake timeout instead of taking byte 0. Measured: `connect` returned 0
      with the handler off, `ECONNREFUSED` with it on. This is a *different*
      failure from the one this record describes — `0028`'s hole is "the owner
      never sees `HUP`"; this is the mirror, `ArenaHeldButUnreachable` arriving by
      a route nothing enumerates, reachable today. **Register.**
   7. **`OwnerServer.shutdown` and `ShutdownHandle.eventfd`** — `server.rs:61`
      and `:78`; the second is `fcntl_dupfd_cloexec` of the first, so **two
      descriptors, one description**, and closing one releases nothing. The
      reachable damage — a child's `Tree` drop shutting down the *parent's*
      server — is already blocked by `OwnerThread::fork_gen`
      (`tf_tree/src/open.rs:1093-1102`), which is a value check, not a descriptor
      one. **Register both numbers, or neither.**
   8. **The `epoll` set and the accept table** — `serve()`'s `ep`
      (`server.rs:237`) and `clients: Vec<Option<(OwnedFd, u32)>>`
      (`server.rs:256`). Both are **locals of the serving thread**, which does not
      exist in the child and whose stack nothing in the child can reach: a
      registry is the only way to close them at all. **Today they cost nothing** —
      no joiner ever watches its socket, `Attachment::Joined` holds it purely for
      `Drop` (`tf_tree/src/open.rs:400-410`) — but `tf_tree/src/open.rs:39` says
      §3.5 takeover *is* "a watcher on the client socket", so the day takeover
      lands an owner's forked child silently blocks every heir from noticing.
      **Register at accept; unregister before the `drop(sock)` in the hangup
      arm.**

   **A read-only attacher holds the same five as a read-write joiner, not
   fewer.** `register_at` takes a participant byte regardless of `AccessMode`
   (`tf_tree_ipc/src/open.rs:439-450`) and the `Joined` arm calls
   `use_claim_leases` unconditionally (`tf_tree/src/open.rs:970`), so a D18
   consumer holds a claim-lock description with no claims in it. D18 changes the
   mapping's `prot`, not the description set.

   **A `build_shared` / `attach_shared` tree holds only the memfd** —
   `attachment`, `claim_lock` and `ofd_probe` are all `None`
   (`tree.rs:1250-1260`) — and every `attach_shared` caller in the workspace
   reaches its fd through `exec` (`load_child` takes it from stdin), where the
   registry is a freshly zeroed static. So registering the memfd does not break
   the inheritance path.

3. **Does closing them in the child change what the child can observe?** The
   child's `Tree` is already poisoned, so it should not be able to tell. "Should
   not" is the part that needs a test.
4. **Does this interact with `0028` step 4's rebase?** The hangup fast path is
   the thing a fork currently defeats. If this record lands after `0028`, the
   test in question 3 has to be written against the rebased callback rather than
   the one at `adeb158`.

## What would make this `ready`

- ~~Question 1 answered with a prototype, not a sketch.~~ **MET, and the
  prototype refuted the sketch.** A bare `[AtomicI32; N]` is not merely
  incomplete: it double-closes, deterministically, because the child's own owning
  handles still hold the numbers the handler closed. That is a correctness
  regression worse than the hole it was to fix, and it was found by running the
  thing rather than by reading it.
- ~~Question 2 answered~~ **MET** — the descriptions are enumerated in question 2,
  and the enumeration turned up one the record did not name.
- Question 3 — **NOT MET.** Untouched by this work, and its "should not" is still
  the part that needs a test.
- The §11.2 or §11.3 walk that follows — **NOT MET**, and it is now larger than
  when this list was written, because the answer to question 1 puts a rule on
  every fd-holding field in the seam rather than on the handler alone.
- The §6.2/§7.3 amendment drafted and agreed, narrowly, against `0029`'s
  enumeration of which predicate applies to which tree — **NOT MET.** `0029` is
  still `draft`, and its question 1 moved on 2026-08-22, so the enumeration this
  amendment is supposed to be written against is itself in flux. **This record
  should not go `ready` before `0029` does**, and that ordering is now a
  dependency rather than a preference.

**Not rejected.** Question 1 was the one that could have closed this record as
*rejected*, leaving the fork hole a permanent documented limitation of `0028`.
It did not: the registry is buildable. What it cost is that the design is bigger
than a handler.
