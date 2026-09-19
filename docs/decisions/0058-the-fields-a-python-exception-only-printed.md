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

[`PHASE3.md`](../PHASE3.md) §4.4 promises exceptions carrying structured
attributes; [`API.md`](../API.md) R5 makes text not a promise. **Type objects are
not the singletons `PHASE3.md` §7.2 forbids**: every `#[pyclass]` (§7.1
requires three) and every `create_exception!` class keeps its type object in a
process-global static, so §7.2 is about instances and caches.

## Decision

### 1. Attributes on the classes that exist

Each attribute is set on **every** instance of its class that the library raises,
and on no instance of any other class. Names follow `tft_error` where it has one,
else the Rust field.

| Class | Attribute | Type | From |
|---|---|---|---|
| `ExtrapolationError` | `edge` | `tuple[str, str] \| None` | `Extrapolation.edge`, resolved (§2) |
| | `requested`, `oldest`, `newest` | `int`, nanoseconds | the variant |
| | `domain` | `int` | the query's time-domain tag (§3) |
| `DisconnectedError` | `target`, `source`, `cut_at` | `str \| None` | the variant's three `FrameId`s, resolved |
| `NoDataError` | `edge` | `tuple[str, str] \| None` | `NoData.edge`, and `span`'s own arm |
| `TopologyChangedError` | `plan_generation`, `current_generation` | `int` | `TopologyChanged { plan, current }` |
| `FrameNotDeclaredError` | `name` | `str \| None` | the name the caller typed; `None` from the `UnknownFrame { hash }` fallback |
| `DerivativesUnavailableError` | `edge` | `tuple[str, str] \| None` | the variant |
| `NoSegmentError` | `edge` | `tuple[str, str] \| None` | the variant |

No attributes on `TfTreeError`, `ChildProcessDetachedError` or `BufferError`
(its 23 raise sites share no attribute set). `push_many`'s sample index and
`DerivativesUnavailableError.interp` are omitted: the first is on some raises of
a class and not others, the second is a raw `u8` no handler branches on.

### 2. An id reaches Python as the arena's names, or as `None`

An edge is its `(parent, child)` pair of **stored** frame names, a plain
`tuple[str, str]` as `Tree.edges()` returns; a frame is its stored name. `None`
when the arena has no usable record at that id. An id never appears as an
integer. Claim and push errors resolve through the arena too
(`EdgeAlreadyClaimedError.edge` and `NonMonotonicStampError.edge` from the
variant's `EdgeId`); their **messages** keep the caller's spelling. The two
differ only for a name over 48 bytes, where a stored name is truncated, cannot be
passed back, and can collide ([`0027`](./0027-the-48-byte-frame-name-store.md)).

### 3. Stamps carry their domain

`ExtrapolationError.domain` is the query's tag from the call site's plan handle
(or `Tree.lookup`'s `domain=`), per [`0038`](./0038-the-domain-a-binding-cannot-name.md):
an `int` on every raise, no `None` arm. No call site passes a tag it does not hold.

### 4. Five classes are built

| Class | Base | Attributes | Raised from |
|---|---|---|---|
| `TimeDomainMismatchError` | `TfTreeError` | `expected: int`, `got: int` | both `LookupError::TimeDomainMismatch` (per query) and `plan_domain_err` (plan time): one type for both |
| `EdgeAlreadyClaimedError` | `TfTreeError` | `edge: tuple[str, str] \| None`, `owner_slot: int \| None` | `ClaimApiError::AlreadyClaimed { edge, cause }`; `owner_slot` is `None` exactly when the Rust field is `u32::MAX`, the `CLAIMING` sentinel. A later `ClaimError` cause with no slot raises base `TfTreeError` through `claim_err`'s bug-report arm |
| `ArenaHeldButUnreachableError` | `TfTreeError` | `holder_slots: tuple[int, ...]` (bitmask decoded, ascending), `ownership_held: bool` | `IpcError::ArenaHeldButUnreachable` in `open_err` |
| `NonMonotonicStampError` | `TfTreeError` | `edge: tuple[str, str] \| None`, `last: int`, `got: int` | `PushError::NonMonotonicStamp`, via `Publisher.push`, `push_many` and module-level `push` |
| `ArenaAbsentError` | `TfTreeError` | none | `IpcError::ArenaAbsent` in `open_err` |

- `.owner_pid` becomes `.owner_slot`; `.holders` and `first_slot` are not
  carried; `first_pid` is dropped (a pid can name an unrelated process across
  namespaces, [`0033`](./0033-the-identity-record-cannot-name-a-namespace.md), and
  `0` means "never written"). The message still prints the pid; that text belongs
  to `IpcError`.
- `ArenaAbsentError` and `ArenaHeldButUnreachableError` are leaves with no shared
  parent (`ArenaUnavailableError` would collide with C's
  `TFT_ERR_ARENA_UNAVAILABLE`); a caller waiting for either writes
  `except (ArenaAbsentError, ArenaHeldButUnreachableError)`. The class inherits
  `is_retryable`'s classification of every producer of the variant.
- **`ClaimRevokedError` is deferred**: no Python caller can make
  `PushError::ClaimRevoked` raise, so its mapper arm could not be tested. Until a
  trigger exists (draft [`0031`](./0031-the-participant-record-with-no-byte.md))
  the failure raises `TfTreeError`.
- Every class is registered on every platform.

### 5. How attributes are attached

- Set on the instance after construction, into `__dict__`; `args` stays
  `(message,)`, so pickle, `copy` and `multiprocessing` keep them and `str(e)` is
  unchanged. No `__reduce__`, no keyword constructor, no `__slots__`.
- Values are plain data: `int`, `str`, `bool`, `None`, tuples of those.
- The mappers take `py: Python<'_>`, which the compiler keeps out of
  `py.detach` closures.
- Every attribute is computed inside the `Err` arm from the error value; no
  handle pre-computes one, so a successful call executes no new code.

### 6. `FrameNotDeclaredError` has no `KeyError` base

The surface has no mapping protocol, and a `KeyError` base would widen every
`except KeyError` / `except LookupError` around a tf_tree call.

### 7. Attributes are a compatibility promise

An attribute's name, its presence on every raised instance of its class, and its
value type belong to the type: removing or retyping one needs a `CHANGELOG.md`
entry with a migration note. R5's *"not a field"* is D11's rule for a `Copy`
Rust error and does not reach a Python exception.

## Open questions

### 6. What does a caller-constructed instance carry?

Nothing: the stub stays precise (`requested: int`, not `int | None`), each class
docstring says the attributes exist only on raised instances, and no class-level
`None` default is set (`test_errors.py`).

## Consequences

- `EXPECTED_EXCEPTION_COUNT` in `test_errors.py` is 15; the pickling test is
  built from `vars(tf_tree)`.
- `_core.pyi` annotates attributes in class bodies; `test_stubs.py` checks each
  against `set(vars(raised))`.
- `just py-test` and `just py-test-freethreaded` first run
  `cargo build -p tf_tree --features shm --bin tf_tree_rendezvous_child`.
- The round trip is gated on 3.14 and 3.14t only; no job runs the abi3 wheel.

## Implementation plan

The gap `PHASE3.md` §11.1 is left with: the `None` arm of every resolved id,
`FrameNotDeclaredError.name`'s `None` arm, `EdgeAlreadyClaimedError.owner_slot`'s
`None` arm (no §11.3 crash site inside the `CLAIMING` window), and the deferred
`ClaimRevokedError`.

1. **The specs say what is decided.** `PHASE3.md` §4.4, §7.2 and a pointer from
   `API.md` R5 to D11's scope.
2. **Attributes on the seven existing classes, the `py` plumbing, and the
   stubs.** Values asserted in `test_errors.py`'s `CASES` table; the `@shm`
   reparent test in `test_shared.py` holds `TopologyChangedError` and fails
   rather than skips when the child binary is missing. Measured:
   `#[cold]` + `#[inline(never)]` on the scalar `plan.at` mapper cost nothing
   on the success path; the other mappers showed no regression and carry neither.
3. **`TimeDomainMismatchError`.** `test_domains.py`.
4. **`NonMonotonicStampError`.** `test_errors.py`'s push rows.
5. **`EdgeAlreadyClaimedError`.** `test_shared.py`.
6. **`ArenaHeldButUnreachableError`.** `test_shared.py`.
7. **`ArenaAbsentError`.** `test_shared.py`, `test_api.py`.
8. **The specs close, and status to `implemented`.**
