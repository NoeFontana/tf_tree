# 0058: the fields a Python exception only printed

**Status:** implemented (2026-09-16)
**Owner:** @NoeFontana
**Implementation:** steps 1–8 in #346, after
[`0059`](./0059-the-arena-errors-that-cannot-describe-themselves.md)'s
implementation (#345), as the plan orders. Step 2 did not take its fallback: the
reparent test drives `tf_tree_rendezvous_child join-reparent`, so
`TopologyChangedError`'s two generations are asserted rather than deferred.
#346's review round then made the tagless mappers their own entry point
(`lookup_err_untagged`), so an `Extrapolation` with no domain tag cannot reach
the class-changing fallback step 2 had left behind it.

## Context

Type objects are not the process-global singletons [`PHASE3.md`](../PHASE3.md) §7.2 forbids: every `#[pyclass]` and `create_exception!` class keeps its type object in a static, so §7.2 is about instances and caches.

## Decision

### 1. Attributes on the classes that exist

On every raised instance of the class and no other. `ExtrapolationError`: `edge`,
`requested`, `oldest`, `newest`, `domain`. `DisconnectedError`: `target`, `source`,
`cut_at`. `NoDataError`, `DerivativesUnavailableError`, `NoSegmentError`: `edge`.
`TopologyChangedError`: `plan_generation`, `current_generation`.
`FrameNotDeclaredError`: `name`.

### 2. An id reaches Python as the arena's names, or as `None`

An edge is its `(parent, child)` pair of stored names, a frame its stored name,
else `None`; never an integer id ([`0027`](./0027-the-48-byte-frame-name-store.md)).

### 3. Stamps carry their domain

`ExtrapolationError.domain` is the query's tag, always an `int`
([`0038`](./0038-the-domain-a-binding-cannot-name.md)).

### 4. Five classes are built

All subclass `TfTreeError`: `TimeDomainMismatchError` (`expected`, `got`);
`EdgeAlreadyClaimedError` (`edge`, `owner_slot`; no pid,
[`0033`](./0033-the-identity-record-cannot-name-a-namespace.md));
`ArenaHeldButUnreachableError` (`holder_slots`, `ownership_held`);
`NonMonotonicStampError` (`edge`, `last`, `got`); `ArenaAbsentError`.
`ClaimRevokedError` is deferred (draft [`0031`](./0031-the-participant-record-with-no-byte.md)).

### 5. How attributes are attached
Into `__dict__`, `args` left `(message,)`, so pickle and `copy` keep them.

### 6. `FrameNotDeclaredError` has no `KeyError` base
A `KeyError` base would widen every `except KeyError`.

### 7. Attributes are a compatibility promise
Removing or retyping one needs a `CHANGELOG.md` migration note.

## Open questions

### 6. What does a caller-constructed instance carry?
Nothing: no class-level `None` default (`test_errors.py`).

## Implementation plan

Steps 1–8 landed: specs, attributes, one step per §4 class, specs close.
