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
  [`0029`](./0029-one-liveness-predicate-per-tree.md) is the record that
  enumerates which tree gets which fact.

## Open questions

1. **Is the lock-free fd registry buildable under the constraint?** Capacity,
   overflow, and a registration racing a fork in another thread. If the answer is
   no, the fork case stays a permanent documented limitation and this record
   closes as *rejected* rather than sitting open.
2. **Which descriptions must be registered?** `0028` names the client socket and
   the participant lock byte. There are at least two more independent
   descriptions in a live tree — the claim lock file and, for an owner, the accept
   set — and whether they belong here or are separately harmless has to be worked
   out rather than assumed.
3. **Does closing them in the child change what the child can observe?** The
   child's `Tree` is already poisoned, so it should not be able to tell. "Should
   not" is the part that needs a test.
4. **Does this interact with `0028` step 4's rebase?** The hangup fast path is
   the thing a fork currently defeats. If this record lands after `0028`, the
   test in question 3 has to be written against the rebased callback rather than
   the one at `adeb158`.

## What would make this `ready`

- Question 1 answered with a prototype, not a sketch.
- Questions 2 and 3 answered, with the §11.2 or §11.3 walk that follows.
- The §6.2/§7.3 amendment drafted and agreed, narrowly, against `0029`'s
  enumeration of which predicate applies to which tree.
