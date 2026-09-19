# 0031: the participant record with no byte

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** steps 2, 3 and 4 landed with the promotion (#358); step 1
landed 2026-09-19. **Decided 2026-09-18, on the owner's delegation**: question 2
is answered *out of contract*, which selects the small branch this record
predicted for that answer, and questions 3 and 4 are answered with it.

**Frozen.** Corrections go in [`README.md`](./README.md)'s row for this record,
not in place.

## Context

`TreeBuilder::build_shared` registers a `LIVE` participant record
(`register_participant`, `crates/tf_tree/src/tree.rs`) and takes **no lock byte**,
because such an arena has no lock file: the fd is the capability
(`docs/PHASE2.md` §3.1). Every reclaimer
[`0028`](./0028-the-slot-a-killed-participant-keeps.md) shipped reads a `LIVE`
record over a permanently free byte as *dead*. Binding a rendezvous over such an
arena makes that verdict reachable.

## Decision

**A served `build_shared` arena is out of contract.**

### 1. Why out of contract

`Open::open`'s `Created` arm is `build_shared` **plus** OFD liveness, claim
leases, the owner server and ownership; it holds the ownership byte and
participant byte 0 before it builds. Serving a `build_shared` arena by hand is
that composition with the OFD-liveness half left out, and every consequence
measured is a consequence of leaving it out (`crates/tf_tree_c/src/bridge.rs`,
*Why `tf_tree::Open` and not `TreeBuilder::build_shared`*).

**The discriminator is the lock file, not the server.** The only `build_shared`
call in shipped library code is `Open::open`'s `Created` arm, served and
lock-file-holding. Every other call site is a test, bench or example: unserved
(no rendezvous, no lock file, no observer holding a probe, so a byte-less record
is unobserved, which is the design) or one of the two byte-less served
compositions in `crates/tf_tree/tests/rendezvous.rs`. No shipped path composes
the byte-less served shape.

**It reaches neither binding.** `tf_tree_py` exposes only `open_arena`
(`tf_tree::Open`); `build_shared` appears nowhere in `crates/tf_tree_py/src/`,
and in `crates/tf_tree_c/src/` only in doc comments.

### 2. What that selects

**The refusal cannot be mechanical.** `build_shared` cannot know a server will
later be bound over its fd, and `tf_tree_ipc` (`OwnerServer::bind_at` takes a
`SegmentDescriptor`; `serve` takes a `BorrowedFd`; dependencies are `rustix` and
`libc`) cannot map a participant table to see the absent byte.

So the answer is **prose plus a characterisation test**: the shape stays
*executed*, with its consequences asserted, so the boundary cannot rot into a
sentence nobody runs.

### 3. Why step 0b closed two byte-less entry points and this answer closes the third differently

`attach_shared(ReadWrite)` and `attach_shared_at(ReadWrite, slot)` attach to an
arena somebody else created and may be serving; a byte-less `LIVE` record is
then visible to probe-carrying observers, so the only stop is refusing the call
(`0028` step 0b). `build_shared` creates an arena nothing can find by name; the
wrong opinion needs a **second call by the same caller** (binding a rendezvous).
The defect belongs to a pair of calls, neither of which can see the other.

### 4. What is not decided

Giving the record a byte, and restoring a second liveness fact for byte-less
records, are **not taken and not refused** — moot, since the arena that produces
such a record is not a supported shape. #201 is untouched and keeps its own
narrower fix.

## Open questions

1. **Resolved by measurement: the failure is availability, not integrity.** D7 is
   never violated, but the eviction of a byte-less publisher from its edge is
   unbounded.
2. **Answered 2026-09-18: out of contract.** *Decision* parts 1 and 3.
3. **Answered 2026-09-18 by checking: no binding reaches it.** *Decision* part 1.
4. **Moot 2026-09-18:** *Decision* part 4.

## Implementation plan

1. **Say it where the call is.** `TreeBuilder::build_shared`'s rustdoc and
   `PHASE2.md` §3.1 state that binding a rendezvous over a `build_shared` arena is
   out of contract.
2. **Keep it executed.** Three tests in `crates/tf_tree/tests/rendezvous.rs` stay
   as characterisations of an unsupported composition, assertions unchanged:
   `a_byteless_creators_record_reads_dead_and_is_reaped_while_it_publishes`,
   `a_byteless_publisher_is_evicted_from_the_edge_it_is_publishing_to`, and the
   control `a_leased_publisher_keeps_its_edge_against_a_sweeper`.
3. **Remove the ledger row** from `PROJECT.md` §5.1.
4. **Update `decisions/README.md`'s row** for this record.
