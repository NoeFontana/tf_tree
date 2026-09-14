# 0058: the fields a Python exception only printed

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** none yet. No step lands before
[`0059`](./0059-the-arena-errors-that-cannot-describe-themselves.md)'s
implementation (*Implementation plan*).

## Context

[`PHASE3.md`](../PHASE3.md) §4.4 says Rust's typed errors map to *"an exception
hierarchy carrying **structured attributes**, not just messages, so users can
program against them"*. No tf_tree exception has ever carried one. Every mapper
in `crates/tf_tree_py/src/errors.rs` builds its exception as
`XxxError::new_err(format!(..))` and nothing more, and has done so since the
module's first commit (`7c33b0e`). §4.4's 2026-09-14 amendment (#335) now lists
what ships instead: **ten classes, no attributes, `args == (message,)`**. It
leaves three questions to *"a decision record's rather than this section's"*:

- the attributes, including how an id-shaped one (`.edge`, `.target`,
  `.cut_at`) reaches a language that is never handed an id;
- the `KeyError` base;
- the four classes §4.4 names that were never built.

This is that record. §4.4 is not marked NORMATIVE (`PHASE3.md:9`), so nothing
here is overdue against a requirement. What is overdue is what a caller can do
today. **The information exists and the caller cannot reach it except by parsing
a sentence**, and [`API.md`](../API.md) R5 is NORMATIVE that a sentence is not a
promise.

### What a caller holds today, and what the other surfaces hold

A batch caller whose `plan.at(stamps)` raises `ExtrapolationError` cannot learn
which stamp failed or what window the edge retained. Both are in the Rust error
(`LookupError::Extrapolation { edge, requested, oldest, newest }`), and both are
formatted into the message and dropped. The nearest substitute is
`Tree.span(target, source)`, but it is a second call. It races a live writer, and
it answers for the whole path's intersected window rather than for the edge
that failed.

The other two bindings already hand the fields out. The frozen C ABI's
`tft_error` (`crates/tf_tree_c/include/tf_tree.h`) has `edge`, `frame_a`,
`frame_b`, `requested`, `oldest`, `newest`, `plan_generation` and
`current_generation`, and the C++ `Error` wrapper exposes each of them. Python is
the surface [`PHASE3.md`](../PHASE3.md) §0's out-of-scope table (and §13's first
bullet) says binds Rust directly because *"through C we would lose typed
errors"*, and it is the only one of the three that has none.

**The class half of the gap is also real, and it costs a retry loop.** Of the
failures a Python caller meets, these need opposite responses and today share the
base `TfTreeError`:

| Failure | Rust | Right response | Python class today |
|---|---|---|---|
| a slot stayed mid-write | `LookupError::SlotContended` | retry | `TfTreeError` |
| an intern is in flight | `FrameError::InternContended` | retry | `TfTreeError` |
| a lease is still held | `ClaimApiError::LeaseContended` | retry | `TfTreeError` |
| another writer holds the edge | `ClaimApiError::AlreadyClaimed` | wait for release or reap | `TfTreeError` |
| this writer's claim was revoked | `PushError::ClaimRevoked` | stop, re-claim | `TfTreeError` |
| a stamp older than the newest | `PushError::NonMonotonicStamp` | drop the sample | `TfTreeError` |
| the plan's domain is not the path's | `LookupError::TimeDomainMismatch` | pass `domain=` | `TfTreeError` |
| one edge's domain is not the rest of the path's | `LookupError::MixedTimeDomains` | none a query can pass; the path is refused | `TfTreeError` |
| nothing is serving yet | `IpcError::ArenaAbsent` | retry (`0019`'s `is_retryable`) | `TfTreeError` |
| a byte is held and nothing serves | `IpcError::ArenaHeldButUnreachable` | retry, then operator | `TfTreeError` |

#335 closed the one row that could end a retry loop on a handle that will never
work again (`ChildProcessDetachedError`, §8.1, NORMATIVE). The rest are this
record's.

### How the Rust errors changed under #339

#339 (merged as `0761583` on 2026-09-14) changed two of the variants this record
maps. Read at `d7868b5`:

- `PushError::NonMonotonicStamp { edge, last, got }`. `edge` is new, filled at
  `buffer.rs`'s producer, where the ring already holds its `EdgeId`.
- `ClaimApiError::AlreadyClaimed { edge, cause: ClaimError }`. It was a tuple
  variant around `ClaimError`. `From<ClaimError>` and the `ClaimErrorExt` shim
  are deleted, and `cause` is still `EdgeAlreadyClaimed { owner_slot }`.

The Python mapper only moved its match patterns
(`NonMonotonicStamp { last, got, .. }`, `AlreadyClaimed { cause, .. }`), and its
messages still use the names the caller typed. Before #339, those two variants
could only be labelled with the caller's own spelling. Several claim and push
variants already carried an `EdgeId` on `main` (`PushError::ClaimRevoked`,
`ClaimApiError::LeaseContended`, `LeaseUnavailable` and `ReapedDuringClaim`).

**One attribute this record decides needs #339, and #339 has merged.**
`AlreadyClaimed` is returned
by `Tree::claim` only after its `ParentMismatch` check
(`crates/tf_tree/src/tree.rs`, the early return before
`claim(claim_rec, self.participant)?`), and both binding call sites
(`Tree.publisher` and the module-level `push`) already hold the resolved
`FrameId`s they passed in. So when `AlreadyClaimed` fires, those two ids are
exactly the refused edge's stored `(parent, child)`, and the variant's own
`EdgeId` names the same edge. A push error has no such second route: #339's
`EdgeId` on `NonMonotonicStamp` is what `NonMonotonicStampError.edge`
(*Decision* §4) is resolved from.

### Where §4.4's attribute list has gone stale

- **`EdgeAlreadyClaimedError.owner_pid`.** Since amendment A3, a claim word
  names a participant **slot**. `ClaimError::EdgeAlreadyClaimed` carries
  `owner_slot: u32`, and `claimed_by`'s doc comment in `errors.rs` says why a
  field spelled `pid` would send an operator to `kill` an unrelated process.
- **`ArenaHeldButUnreachableError.holders -> [(pid, name), ...]`.** The Rust
  error carries `holder_slots: u64` (a bitmask), `first_slot: Option<u32>`,
  `first_pid: u32` (`0` when the record was never written) and
  `ownership_held: bool`. It has one pid, not one per holder, and no names. The
  cost of reading more is not what prevents `.holders`: the producer
  (`held_but_unreachable` in `tf_tree_ipc/src/open.rs`) already reads the lock
  bytes, one identity record and the ownership probe, and only after the open's
  deadline has passed. What prevents it is that **the Rust error carries one
  identity, and the binding holds no `LockFile` once `open` has returned**, so
  there is nothing to read the other holders' records from.
- **`DisconnectedError.target/.source/.cut_at` and every `.edge`** are
  `FrameId`/`EdgeId` in Rust. The binding never hands a Python caller an id
  and offers no way to invert one (`errors.rs` module doc, *"This module is
  `docs/API.md` R5's separate layer"*). So the question is broader than `.edge`:
  it is how any id-shaped value appears.
- **`TimeDomainMismatchError`** is not in the set `errors.rs` defers as
  *"decision-record material"*. That comment names `FrameOutOfRange`,
  `MissingEdge` and `MixedTimeDomains`. `plan_domain_err`'s doc already commits
  the plan-time and per-query refusals to **one** type, which is why both raise
  `TfTreeError` today and why a new class has to cover both. Both are reachable
  from Python: the plan-time one through `tree.plan(.., domain=)`, and the
  per-query one through `Tree.lookup(.., domain=)`, which
  `test_domains.py`'s `test_lookup_takes_the_domain_the_same_way` already
  raises (`LookupError::TimeDomainMismatch` from `Plan::check_domain_tag`,
  mapped by `lookup_err`).

### What `PHASE3.md` §7.2 does and does not reach

§7.2 is NORMATIVE: *"no `once_cell` singletons holding Python objects"*.
`create_exception!` expands to a `static TYPE_OBJECT: PyOnceLock<Py<PyType>>`
per class (pyo3 0.29.0 `src/exceptions.rs`), so the ten shipped classes are ten
such statics, and *Decision* §4 adds four. **That reading of §7.2 cannot be
the intended one**, because §7.1 is NORMATIVE too and requires `Tree`, `Plan`
and `Publisher` to be `#[pyclass]`es, and every `#[pyclass]` keeps its type
object in the same kind of process-global static
(`static TYPE_OBJECT: LazyTypeObject<#cls>`, `pyo3-macros-backend-0.29.0`
`src/pyclass.rs`). If §7.2's letter covered type objects, §7.1 could not be
satisfied, and moving all fourteen exception types into module state would not
make the module comply while the three pyclasses stayed. So §7.2 is read here as
being about **instances and caches**, and the new classes do not change the
module's standing under it. Step 1 adds one clarifying sentence to §7.2.
Separately, PyO3 0.29 refuses to initialise the module in a second interpreter
(`src/impl_/pymodule.rs`, *"PyO3 modules do not yet support subinterpreters"*).
That is the *"later"* in §7.2's PEP 734 motivation, not a reason to drop it.

### What was measured

All probes ran on the dev host, 2026-09-14. They are one-off probes in
`docs/benchmarks/EVIDENCE.md`'s sense: their scripts are under *Reproduction*,
and they are not registered there because step 2's measurement replaces the
only performance figures below. The tf_tree probes loaded the **pre-#335**
extension in the main checkout,
`python/tf_tree/_core.cpython-314-x86_64-linux-gnu.so` (built 2026-09-11). They
loaded it directly under the module name `_core`, because that build predates
`ChildProcessDetachedError` and the checkout's `__init__.py` refuses to import
it. The build profile of that `.so` was not recorded. Every figure below that
depends on it is marked.

**1. A pickle round trip keeps attributes set after construction. The premise
that it loses them is false for the design this record decides.**
`BaseException.__reduce__` returns `(type, args, __dict__)` whenever `__dict__`
is non-empty, and unpickling restores `__dict__` through `__setstate__`. The
matrix was run in pure Python on six interpreters: CPython 3.10.19, 3.11.14,
3.12.3, 3.13.12, 3.14.3 and 3.14.2 free-threaded. **All six gave identical
results except the `add_note` row, which needs 3.11 or later**
(`BaseException.add_note` does not exist on 3.10, so that row raises
`AttributeError` there and ran on the other five):

| Shape | What it is | Round trip, pickle protocols 0–5 |
|---|---|---|
| `e = Cls("msg"); e.requested = 5; e.edge = ("world", "base")` | set after construction, stored in `__dict__` | **kept**: `vars(back) == {'requested': 5, 'edge': ('world', 'base')}`, `back.args == ('msg',)`, and `str(back)` unchanged; `copy.copy` keeps them too |
| `Cls("msg", 5, 1, 4)` with properties reading `args[1:]` | fields carried in `args` | kept, **but `str(e)` is `"('msg', 5, 1, 4)"`**, because `BaseException.__str__` renders a multi-element `args` as a tuple |
| `__init__(self, msg, *, requested)` | a required keyword-only constructor | **fails at `loads`**: `TypeError: V3.__init__() missing 1 required keyword-only argument: 'requested'`, since `__reduce__` replays positional `args` only |
| the same with `requested=None` defaulted | a defaulted keyword | kept; the default is overwritten by the restored `__dict__` |
| `__slots__ = ("requested",)` | storage outside `__dict__` | **lost**: `vars(e) == {}`, and the unpickled instance has no `requested` |
| an attribute holding a lambda | a value that cannot pickle | **fails at `dumps`** with `PicklingError` |
| `e.add_note(..)` beside an attribute (3.11+) | a caller's note | kept, as `__notes__` in `__dict__` |

The slot row stands in for storing fields on a Rust struct, as
`#[pyclass(extends = PyException)]` with `#[pyo3(get)]` would. That storage is
not in `__dict__` either, so `__reduce__` cannot see it. It was not built and
measured, and step 2 does not need it.

**2. PyO3's exception types behave like the Python classes.** On the pre-#335
`_core`, a version-specific build for 3.14:

- `ExtrapolationError` is a heap type (`Py_TPFLAGS_HEAPTYPE` set) with
  `__dictoffset__ == 16` and MRO `[ExtrapolationError, TfTreeError, Exception,
  BaseException, object]`.
- An instance **raised from Rust** by `plan.at(99)` on a `[1000, 2000]` ring has
  `vars(e) == {}` and `args == ('edge "world" -> "base": stamp 99 ns is outside
  the retained history [1000, 2000] ns',)`.
- `e.requested = 99` on that raised instance round-trips through pickle as
  `{'requested': 99}`. The class needed `sys.modules["_core"]` for the round
  trip, because that build still declares `__module__ = '_core'`, which is the
  defect #335 fixed.

**This covers one of the builds a user installs, not the most common one.** The
published GIL wheel is `abi3-py39` (`.github/workflows/wheels.yml`) and serves
3.10–3.13, while `just py-test` builds version-specific extensions for 3.14 and
3.14t only, and nothing in CI runs pytest against the abi3 artifact. Measurement
1's pure-Python rows cover 3.10–3.13; the PyO3 rows do not.

**3. A `KeyError` base changes three things, and it can be built without
`unsafe`.** Pure Python, on all six interpreters again:

- `str(type("FrameNotDeclaredError", (TfTreeError, KeyError), {})(msg))` is
  `'no frame named "ghost" in this arena'`, with the quotes. The traceback's
  last line reads `FrameNotDeclaredError: 'no frame named "ghost" in this
  arena'`.
- Adding `"__str__": Exception.__str__` to the class dict restores the
  unquoted form in both places.
- The instance is caught by `except KeyError` and `except LookupError`. Today's
  shape is caught by neither.
- A `try` that guards a dict lookup with `except KeyError` swallows a
  `FrameNotDeclaredError` raised later in the same block. The probe
  demonstrates exactly that.
- The class pickles as itself.

On the PyO3 side, `type("FrameNotDeclaredError", (_core.TfTreeError, KeyError),
{"__str__": Exception.__str__, ...})` builds, has MRO `[..., TfTreeError,
KeyError, LookupError, Exception, ...]` and pickles. `create_exception!` takes
one base. `PyErr::new_type` (pyo3 0.29.0, `src/err/mod.rs:344`) documents *"or a
tuple of classes"* but its signature is `base: Option<&Bound<PyType>>`, so a
tuple needs an unchecked cast. It does take a class `dict`, and calling `type`
from Rust does not need `unsafe`. So the `scripts/unsafe-budget.txt` row that a
raw `PyErr_NewExceptionWithDoc` with a tuple of bases would need is avoidable.

**4. What attributes would add to the error path.** `taskset -c 3`, 3.14.3,
the best of 7 × 200 000 iterations, pre-#335 `_core`. A probe, superseded by
step 2's measurement:

| Operation | ns |
|---|---|
| success: `plan.at(1500)`, depth 1, *profile unrecorded* | 216.9 |
| `plan.at(99)` raising `ExtrapolationError`, caught | 459.8 |
| the same, plus `str(e)` | 500.4 |
| `ExtrapolationError("m")`, constructed in Python | 53.0 |
| the same, plus four Python-level `setattr` (a tuple and three ints) | 133.4 |

The last two rows are interpreter work, independent of the extension's build
profile. **They are not an upper bound on the Rust-side cost.** A PyO3
`setattr("requested", 99)` creates the name string and the integer object where
Python bytecode uses interned constants and cached small ints, so the Rust figure
may be higher. Step 2 measures the real thing. What does not depend on the
measurement is that the cost falls on a raise, not on a success. Why that holds
is under *Decision* §5.

**5. A stored name cannot be passed back.** On the same `_core`,
`t = build([("world", "sensor_" + "x" * 60)])` lists the child in `t.edges()`
at 48 bytes, and `t.plan("world", t.edges()[0][1])` raises
`FrameNotDeclaredError`, while `t.plan("world", <the 67-byte typed name>)`
succeeds. A truncated stored name hashes differently from the name that was
interned, so no entry point accepts it.

**6. The `None` arm of a resolved id has no known Python trigger.** A fork
child is the case `test_errors.py`'s
`test_a_stale_id_degrades_to_an_index_and_a_reason_not_to_a_debug_dump` names
as reachable. In a fork child of a process holding a shared arena, `plan.at(99)`,
a no-data `plan.at`, `tree.span` and `tree.edges()` all raised the detached
refusal before resolving any id, and `lookup_err`'s `UnknownEdge` arm returns
`detached_err()` on a detached tree. `named_frame_in`'s doc
(`crates/tf_tree_py/src/offline.rs`) says *"No id reaching here today comes from
outside `1..=frame_count`"*.

## Decision

The six questions this record opened with were decided on 2026-09-14 under the
owner's explicit delegation to *"choose the most desirable approach for the
library goals"*. Three reasons were weighed, in this order: typed correctness
for the handlers real programs write, no promise the plan cannot test, and the
smallest surface that does both. *Open questions* below gives each answer and
its reason. Against the draft, `ArenaHeldButUnreachableError.first_pid` is
dropped, `ClaimRevokedError` is deferred, `NonMonotonicStampError` is added,
`EdgeAlreadyClaimedError.owner_slot` is a plain `int` (with a constraint
found after the move, recorded in §4), and the stub stays
precise with no class-level defaults.

### 1. Attributes on the classes that exist

Each attribute below is set on **every** instance of its class that the library
raises, and on no instance of any other class. That rule also decides what is
left out (question 1). Names are lifted from `tft_error`
where it has one (`requested`, `oldest`, `newest`, `plan_generation`,
`current_generation`, `edge`). Elsewhere they come from the Rust field
(`target`, `source`, `cut_at`, `expected`, `got`, `owner_slot`). So each Python
name matches either `tft_error` or the Rust field, **not both**: the two surfaces
already disagree, and Python follows C where C has a name. C spells
`Disconnected`'s frames `frame_a`/`frame_b` and has no `cut_at`; Rust spells
`TopologyChanged`'s generations `plan`/`current`.

| Class | Attribute | Type | From |
|---|---|---|---|
| `ExtrapolationError` | `edge` | `tuple[str, str] \| None` | `Extrapolation.edge`, resolved (§2) |
| | `requested`, `oldest`, `newest` | `int`, nanoseconds | the variant |
| | `domain` | `int` | the query's time-domain tag (§3, question 3) |
| `DisconnectedError` | `target`, `source`, `cut_at` | `str \| None` | the variant's three `FrameId`s, resolved |
| `NoDataError` | `edge` | `tuple[str, str] \| None` | `NoData.edge`, and `span`'s own arm |
| `TopologyChangedError` | `plan_generation`, `current_generation` | `int` | `TopologyChanged { plan, current }` |
| `FrameNotDeclaredError` | `name` | `str \| None` | the name the caller typed; `None` from the `UnknownFrame { hash }` fallback, where no name survives |
| `DerivativesUnavailableError` | `edge` | `tuple[str, str] \| None` | the variant |
| `NoSegmentError` | `edge` | `tuple[str, str] \| None` | the variant |

**No attributes on** `TfTreeError`, `ChildProcessDetachedError` or `BufferError`.
The first two carry nothing. `BufferError` has 23 raise sites (22 in `tree.rs`,
one in `errors.rs`). About five carry numbers: `BufferTooSmall { need, got }`
counts elements, four `tree.rs` sites format an expected and an actual array
*shape* (`(4, 4)`, `({n}, 4, 4)`, `({n}, 7)`, and a layout's `want`), and one
formats a DLPack device type and id. A shape is not `BufferTooSmall`'s element
count, so no single attribute set covers more than a minority of the sites, and
an attribute present on some raises of a class and absent on others is not an
attribute of the class. A `shape`-valued attribute fails the same rule and is
not added (question 1).

`FrameNotDeclaredError.name` is not in §4.4's list. It is included because it
is the one fact the class exists to report, and `resolve_frame` already holds it
as a `&str`. **Omitted by the same rule:** `push_many`'s sample index, which `push_many`'s
raises would carry and `Publisher.push`'s raises of the same class would not.
**Omitted for a different reason:** `DerivativesUnavailableError.interp`. That
class has one raise site (`lookup_err`'s `DerivativesUnavailable { edge, interp }`
arm), so an `interp` would be on every instance and the rule does not exclude it.
It is left out because no handler branches on it: the remedy (a policy with an
exact derivative, or a layout that needs none) is the same for every policy the
refusal can name. Its value is also a raw `u8` discriminant, which is id-shaped
(§2 hands Python no integer id) and which `stored_interp` cannot always name, so
it would need a `str | None` arm to serve a branch no handler takes. That is the
minimal-surface cut.

### 2. An id reaches Python as the arena's names, or as `None`

An edge is its `(parent, child)` pair of **stored** frame names, and a frame is
its stored name. That is the shape `Tree.edges()` and `Plan.edges()` already
return, so there is one spelling of "an edge" in the Python surface. It is a
plain `tuple[str, str]`, as those return, and not a named tuple
(`edge.parent`, `edge.child`), which would be a new public type for a shape that
already has one. **`None`
when the arena has no usable record at that id** is the case where
`edge_label_in` falls back to `edge #7 (name unavailable: ...)`. The attribute
does not copy that fallback string, because it is prose. The id never appears
as an integer.

Claim and push errors resolve through the arena too, not through the names the
caller typed. `EdgeAlreadyClaimedError.edge` and `NonMonotonicStampError.edge`
are resolved from the variant's `EdgeId`, which #339 added to both; for
`AlreadyClaimed` the call site's two `FrameId`s name the same pair (*Context*).
Their **messages** keep the caller's spelling, for the reason `push_err`'s doc
gives.

**The two spellings differ only for a name longer than 48 bytes, and that case
has two costs this decision carries.** A stored name is truncated, so it
cannot be passed back to `plan`, `publisher`, `lookup` or `push` (measurement
5): an `.edge` from a claim or push error names the edge but cannot be acted on,
where the caller's typed pair could. And truncated stored names **collide**:
draft [`0027`](./0027-the-48-byte-frame-name-store.md) measured `frames()`
returning byte-identical names for two frames and `edges()` returning
self-loops. So `e.edge in tree.edges()` holds, but it does not identify the
edge in the one case where the choice matters. `0027` proposes that `intern`
refuse a name over 48 bytes. If that is adopted, stored and typed names never
differ, both costs disappear, and this section's choice stops mattering.
Question 1 records why the stored pair was chosen over the typed one.

### 3. Stamps carry their domain

[`API.md`](../API.md) R3 says a stamp is integer nanoseconds **carrying a
domain**. A pickled `ExtrapolationError` that reaches a parent process has left
behind the plan whose `domain` said which clock `requested` is on. So
`ExtrapolationError.domain` is the query's tag. It comes from the call site's
plan handle (or `Tree.lookup`'s `domain=`), where
[`0038`](./0038-the-domain-a-binding-cannot-name.md) put it. That is an `int`
on every raise, with no `None` arm. Every `lookup_err` call site whose Rust call
can return `Extrapolation` holds that tag: `PyPlan`'s methods in its `domain`
field, and `Tree.lookup` in its `domain=` argument. The three
call sites that hold none (`span_impl`'s two in `offline.rs`, over `Tree::plan`
and `Plan::span`, and `unknown_frame_err`'s, which is handed `UnknownFrame`) were
read on 2026-09-14 as unable to receive `Extrapolation`; step 2 re-reads them,
and no call site passes a tag it does not hold. Question 3 gives the reasons for
the call site's tag over the edge record's.

### 4. Four classes are built: three that §4.4 names, and `NonMonotonicStampError`

| Class | Base | Attributes | Raised from |
|---|---|---|---|
| `TimeDomainMismatchError` | `TfTreeError` | `expected: int`, `got: int` | both `LookupError::TimeDomainMismatch` (per query, `Tree.lookup(.., domain=)`) and `plan_domain_err` (at plan time), keeping one type for both. `expected` is the path's or the plan's tag, and `got` is the one the caller supplied |
| `EdgeAlreadyClaimedError` | `TfTreeError` | `edge: tuple[str, str] \| None`, `owner_slot: int` | `ClaimApiError::AlreadyClaimed { edge, cause }` whose `cause` is `ClaimError::EdgeAlreadyClaimed { owner_slot }`. `edge` is resolved from the variant's `EdgeId`. `ClaimError` is `#[non_exhaustive]`, and a later cause that carries no slot raises the base `TfTreeError` through `claim_err`'s bug-report arm rather than this class without an `owner_slot`, because `0` is a real slot (`claimed_by`) and an absent attribute would break §1's rule |
| `ArenaHeldButUnreachableError` | `TfTreeError` | `holder_slots: tuple[int, ...]` (the bitmask decoded, ascending), `ownership_held: bool` | `OpenError::Rendezvous(IpcError::ArenaHeldButUnreachable { .. })` in `open_err` |
| `NonMonotonicStampError` | `TfTreeError` | `edge: tuple[str, str] \| None`, `last: int`, `got: int` (nanoseconds) | `PushError::NonMonotonicStamp { edge, last, got }`, through `Publisher.push`, `push_many` and the module-level `push`. `push_class` gains the arm, and `edge` is resolved from #339's `EdgeId` |

§4.4's block is replaced, not annotated again. Its **`.owner_pid`** becomes
`.owner_slot`, its **`.holders`** is struck, its `KeyError` base is struck (§6),
**`ClaimRevokedError` is deferred** (below), and `NonMonotonicStampError`, which
it does not name, is added (question 4). `first_slot` is not carried, because it
is `holder_slots[0]` when the tuple is non-empty and a second copy of the same
fact otherwise.

**`owner_slot` is not always a slot. Review found this after the move to
`ready`, and it is not settled here.** `claim()` builds
`EdgeAlreadyClaimed { owner_slot: slot_of(held) }`
(`crates/tf_tree_core/src/edge.rs`), and `slot_of` returns `u32::MAX` when the
word the `compare_exchange` lost to is `CLAIMING`. That is a claim between its
`compare_exchange` and its owner store, or a claimer killed in that window,
whose word stays `CLAIMING` until a reaper clears it. `Tree::claim` maps the
error with no retry. So, as decided above, `EdgeAlreadyClaimedError.owner_slot`
is `4294967295` in that state, and a handler would pass that number to
`tf_tree participants` or `tf_tree doctor` as a slot. The facade has already
refused this sentinel twice: `ReparentError::LockContended.owner_slot` is an
`Option<u32>` because *"held by live participant slot 4294967295"* is what the
sentinel printed, and `IpcError::ArenaHeldButUnreachable.first_slot` is an
`Option` so that no consumer logs a slot that does not exist. The plain `int`
rested on the reading that only a later `ClaimError` variant could lack a slot,
and that reading missed this state. The two candidate answers are
`owner_slot: int | None`, with `None` for `u32::MAX` (no Python test can reach
that arm, so it would join the gap stated above the plan's steps), or routing
the `CLAIMING` state to another class or arm, with the reason for doing so.
**Step 5 does not start until the owner settles it.**

**`first_pid` is dropped.** The Rust field's doc calls it advisory (*"the lock
is the liveness, this is the name"*), and `identity.rs` records that a pid
written inside a container or `unshare --fork --pid` names a *different* process
when resolved against an observer's `/proc`
([`0033`](./0033-the-identity-record-cannot-name-a-namespace.md)). The error
carries no namespace inode that would let a caller tell. That is the hazard that
strikes `.owner_pid`, and it is sharper here, because `os.kill(e.first_pid, ..)`
is the natural use a Python supervisor would make of it, and it can signal an
unrelated process. There is a sharper case: `first_pid` is `0` when the identity
record was never written, and `os.kill(0, sig)` signals the caller's own process
group. `holder_slots` and `ownership_held` are what separate the remedies (the
Rust `Display` spends `ownership_held` to choose between its two). Turning a slot
into a process safely is `tf_tree doctor`'s job, because since `0033` it reads the
namespace a recorded pid was drawn from. `tf_tree participants`, which
`docs/RUNBOOK.md` reaches for first, prints the recorded pid raw with no namespace
check (`cmd_participants` in `crates/tf_tree_cli/src/lib.rs`), so its pid column
carries the same hazard. The class docstring points at `participants` for the list
of held slots and at `doctor` for the process behind one.

**The message still prints the pid.** `open_err` forwards `IpcError`'s `Display`,
which reads `(slot {slot}, pid {first_pid})` and *"Stop the process holding slot
0"*. So `str(e)` still hands a Python supervisor the number this record declines
to give it as an attribute, and R5 already forbids parsing it out. That text
belongs to `IpcError`, a sibling of `0033`'s scope, and this record does not
change it.

**`ClaimRevokedError` is deferred, not built.** No Python caller is known to be
able to make `PushError::ClaimRevoked` raise. Its two known producers are a
direct `reap` of the claim record in a core unit test (`tf_tree_core`'s
`tests.rs`) and one facade `shm` test (`rendezvous.rs`'s
`a_byteless_publisher_is_evicted_from_the_edge_it_is_publishing_to`), which needs
a `build_shared` creator with no lock file that no Python entry point builds,
while a rendezvous-joined publisher keeps its lease against a sweeper (that
test's control). A class with no trigger is a mapper arm no test can fail and no
mutant can be run against. `PHASE3.md` §4.4 is amended to say the class waits
for a Python-reachable trigger, and names draft
[`0031`](./0031-the-participant-record-with-no-byte.md) as what could create one,
since an answer to it could change which publishers are reaped. Until then the
failure raises the base `TfTreeError`, as it does today. What reopens it is
question 4's criterion.

**Every class is registered on every platform**, including
`ArenaHeldButUnreachableError`, which only a Linux `open` can raise. That way
`except tf_tree.ArenaHeldButUnreachableError` is valid code everywhere and
`_core.pyi` has one shape. Today `open_err` and the `OpenError` import are
`#[cfg(target_os = "linux")]`, but the class is not an `OpenError`.

### 5. How attributes are attached, and why a successful call does no new work

- **Set on the instance after construction, into `__dict__`; `args` stays
  `(message,)`.** This is measurement 1's first row: pickle, `copy` and
  `multiprocessing` keep the attributes with no `__reduce__` of ours, and
  `str(e)` does not change.
- **Attribute values are plain data**: `int`, `str`, `bool`, `None` and tuples
  of those. Never a `Tree`, a `Plan` or a `Publisher`. Measurement 1's lambda
  row is why, and [`API.md`](../API.md) §3.1 refuses pickling those types anyway.
- **The mappers take `py: Python<'_>`**: `lookup_err`, `push_err`/`push_class`,
  `claim_err`, `plan_domain_err`, `open_err`, `unknown_frame_err`, `span_impl`'s
  arm, and the three that produce every named `FrameNotDeclaredError`
  (`resolve_frame`, `frame_not_declared` and `unresolvable_name`). So does
  `push_many`'s inline `push_class(e)(format!(..))` arm, which has to set
  `NonMonotonicStampError`'s attributes too. That is 15 `lookup_err` call sites (`tree.rs`
  ×12, `offline.rs` ×2, and `errors.rs` ×1 in `unknown_frame_err`), 8
  `resolve_frame` call sites (`tree.rs` ×6,
  `offline.rs` ×2), plus the rest. **The token is what keeps attribute
  construction off a detached thread, and the compiler enforces it.**
  `Python<'py>` holds `PhantomData<NotSend>` (pyo3 0.29.0 `marker.rs:357-361`)
  and so is not `Ungil`, and `Python::detach` requires `F: Ungil`. A mapper that
  needs the token cannot be called inside a `py.detach` closure. Every call site
  today already maps after `detach` returns (for example `res.map_err(..)` after
  `py.detach(run)` in `at_extrapolating_into`).
- **Every attribute is computed inside the `Err` arm, from the error value** and
  the ids the call site already holds. No handle pre-computes an attribute "for
  when it fails". `PyPublisher` keeps holding only the `edge` label it builds at
  creation, and the pair for `NonMonotonicStampError.edge` is resolved from the
  variant's `EdgeId` when it is raised. The success path of `at`, `at_into`,
  `lookup`, `push` and `push_many` therefore executes no code this record adds.
  The per-raise cost is measurement 4's order of magnitude, and a batch raises
  once per call, not once per element.

### 6. `FrameNotDeclaredError` does not gain a `KeyError` base (question 2)

§4.4's `FrameNotDeclaredError(TfTreeError, KeyError)` is struck. The Python
surface has no mapping protocol: no `__getitem__` or `__contains__` on `Tree`,
and nothing that reads as `tree[name]`. So there is no call site where the
`KeyError` idiom is the one a caller reaches for. Measurement 3 prices what it
would cost. Every `except KeyError` and `except LookupError` that encloses a
tf_tree call starts catching a failure it never meant to, and `str(e)` gains
quotes unless `__str__` is overridden in a hand-built class.

### 7. Attributes are a compatibility promise on the class's terms

R5 says exception *types* are a compatibility promise and message *text* is not.
**An attribute's name, its presence on every instance of its class that the
library raises, and its value type belong to the type.** An attribute is what R5
lets a caller match on in place of the text. So removing or retyping one is
recorded like removing a class: a `CHANGELOG.md` entry with a migration note.
Under the `0.0.x` line, *every release may break every other*, and that does not
change.

The `None` arms are part of the type (`str | None`). **For resolved ids, no
Python trigger for `None` is known** (measurement 6), and
`FrameNotDeclaredError.name`'s `None` is reached only through an intern race
that no test can make happen on demand. The arms are typed so that a caller's
code is correct whenever one is met, not because a caller meets it today. Question 5 keeps the arm, and the plan records where no test reaches it.

**The promise is about raised instances, and a type checker cannot see that
line.** `_core.pyi` will annotate `requested: int` in the class body, which
pyright applies to every instance. An instance the caller constructs
(`tf_tree.ExtrapolationError("boom")`, a test double's `side_effect`) has no
attributes, and neither has one unpickled from a build that predates this record.
A handler reading `e.requested` then raises `AttributeError` inside its own
`except` block, and `pyright --strict` passes it. **Question 6 chose the precise
stub**: `requested: int`, not `int | None`; every class docstring states that
the attributes exist only on instances the library raises; and no class-level
`None` default is set. The consumer of an attribute is a handler of raised
errors, which would otherwise narrow, on every read, a value that is never
`None` on anything it catches. A test double built without the attributes fails
loudly at runtime, in the test that built it.

R5's sentence *"Name resolution against the arena is `Described`, a `Display`
wrapper, not a field"* **does not reach a Python exception, and D11 is why.** R5
cites D11 for it, and D11 states the rule as *"Errors are `Copy`,
allocation-free, and carry IDs; a `Display` wrapper resolves names against the
arena"*. That is a rule about the representation of a `Copy`, allocation-free
Rust error, and a Python exception is neither. The binding's prose layer already
resolves every name it prints (`edge_label` / `frame_label`), and handing that
resolution out as data puts no field on a Rust type. `PHASE3.md` §4.4's authors
held both readings at once: the same block that lists `.edge`, `.target`,
`.source` and `.cut_at` says *"`str(e)` uses the Rust `Described` wrapper"*. R5's
NORMATIVE paragraph is about types against text, not fields. Step 1 adds a
pointer from R5's sentence to D11's scope, so the unscoped *"not a field"* stops
reading as forbidding this.

## Rationale

**Why set attributes after construction, rather than pass them to it.**

- *Fields in `args`* (measurement 1, second row) change `str(e)` to a tuple repr
  unless every class overrides `__str__`. They also break the
  `args == (message,)` that §4.4's amendment tells a caller they can rely on.
- *A keyword constructor* (third row) makes `pickle.loads` fail with `TypeError`
  unless every keyword has a default. With defaults it works, but it needs a
  Python-level `__init__` in every class dict, built from Rust, to buy a
  constructor callers were never promised.
- *Rust-struct storage* via `#[pyclass(extends = PyException)]` (the slot row's
  analogue) is invisible to `BaseException.__reduce__`. It would put #335's
  pickling guarantee back at risk and replace `create_exception!` for every
  class.
- *A custom `__reduce__`* is not needed, because the default already carries
  `__dict__`. It would be a second list of attribute names that has to agree
  with the `setattr` calls, which is `PROJECT.md` §6's second-spelling smell
  inside one class.

**Why names and not ids.** An integer id is information a Python caller has no
way to use: no entry point accepts one and none returns one. `errors.rs`
already argues this for messages. An id-valued attribute would also be the
first place the Python surface hands out an id, which is a larger API change
than this record means to make.

**Why these four classes and not a wider set.** The precedent is not that a
spec named a class first. `NoSegmentError` got its class in a review commit
(`93e5fb5`) on R5's argument alone, that its remedy is distinct and telling the
remedies apart by text is what R5 forbids, and no spec named it until #335's
amendment. `DerivativesUnavailableError` landed in the same commit (`8282dec`)
as the `PHASE5.md` status text that names it. So R5's argument by itself would
admit every row of the Context table that has a distinct remedy, and a spec
having named a class is no criterion either: §4.4 named `ClaimRevokedError`,
which no Python test can make raise. **The criterion is two conditions, both
necessary and together not sufficient** (question 4): a Python handler needs to
branch on the failure, and a Python test can make it raise. The first is what
earns a class its surface; the second is what lets a test hold its mapper arm.
The four classes here meet both. Question 4 lists each deferred candidate with
what holds it back, and for `ArenaAbsent`, which meets both, that is a third
thing: its shape (a leaf, or a parent above `ArenaHeldButUnreachableError`) is
not settled.

**Why not leave §4.4 as amended and build nothing.** Because the gap the
amendment records is the one R5 says a caller must not bridge by parsing text.
§4.4's harm is concrete: a `multiprocessing` worker's `ExtrapolationError` now
reaches its parent intact (#335), and still cannot say which stamp it was about.

## Consequences

- A Python caller can branch on `e.requested`, `e.edge` and `e.owner_slot`
  instead of on a sentence. That makes [`API.md`](../API.md) §7 check 5
  (*"does any documentation invite a caller to match on message text?"*)
  satisfiable for Python for the first time.
- Four new classes, each a `TfTreeError` subclass, so every existing
  `except TfTreeError` still catches them. **Any caller who matched a
  `TfTreeError` message to tell those failures apart is already outside R5.**
  The change is still recorded in `CHANGELOG.md`, because such callers exist.
  `NonMonotonicStampError` is the one a working program meets most: a check of
  `type(e) is TfTreeError` around a push stops matching it.
- `_core.pyi` gains attribute annotations in class bodies. `test_stubs.py`
  collects only `FunctionDef` members of classes (`_stub_members`), so today it
  cannot see an attribute. Step 2 widens it, or the stub can drift silently.
- Every mapper gains a `py` parameter. That is mechanical, and it is the
  enforcement *Decision* §5 relies on.
- `EXPECTED_EXCEPTION_COUNT` in `test_errors.py` moves from 10 to 14. The class
  pickling test is built from `vars(tf_tree)`, so it covers the new classes
  without a new row.
- §4.4, §11.1 (*"every error type raised at least once with its attributes
  asserted"*) and Appendix B stop being prose about something absent, with four
  gaps recorded rather than closed: the `None` arm of every resolved id,
  `FrameNotDeclaredError.name`'s `None` arm, `TopologyChangedError`'s two
  attributes, and the deferred `ClaimRevokedError` (the gap stated above the
  plan's steps, and question 5).
- **`0059`'s implementation lands first.** Both records edit
  `crates/tf_tree_py/src/errors.rs`, and `0059` step 1(d) rewrites `open_err`'s
  `OpenError::Map` arm and its doc comment, in the function step 6 here gives a
  new arm.
- **The attribute round trip is gated on 3.14 and 3.14t only.** `just py-test`
  builds version-specific extensions for those two, and no job runs the abi3
  wheel that 3.10–3.13 users install. The attributes are plain `__dict__`
  entries and measurement 1 found the pure-Python behaviour identical from 3.10
  up, so a divergence is not expected, but it is not tested.
- **The attribute set becomes something to keep in step with the Rust
  variants.** A Rust field added to `LookupError::Extrapolation` does not
  appear in Python by itself. That is the same property the enumerated
  `lookup_err` match already has, and it is not new.
- **Not decided here, and named so that the deferral is not read as having
  checked it:** the cause that `PushError::ClaimRevoked`'s rustdoc, `push_msg`
  and `docs/RUNBOOK.md`'s *A writer stopped publishing* section give, that the
  process *"was judged dead while it was stopped or stalled"*. Under D17 and
  `0028` liveness is the lock byte, which a stopped process keeps, and the known
  producers are a direct reap and a byte-less participant. The draft's plan
  checked that sentence in the step that built `ClaimRevokedError`; with the
  class deferred, no step here touches it.

## Implementation plan

**Order.** No step lands before
[`0059`](./0059-the-arena-errors-that-cannot-describe-themselves.md)'s
implementation, which rewrites arms and comments in the same
`crates/tf_tree_py/src/errors.rs`. Then step 1, then step 2, which every later
step builds on (the `py` plumbing and the stub test). Steps 3 to 6 follow step 2
**one at a time, in any order**; each adds one class, moves
`EXPECTED_EXCEPTION_COUNT` up by one, and edits the `PHASE3.md` §4.4 amendment
bullet it makes false. Step 7 follows the last of them.

Each code step (2 to 6) lands as one PR with its own `CHANGELOG.md` entry
(`just artifact-versions` fails without it) and is gated by `just py-test`,
`just py-test-freethreaded`, `just py-lint` and `just lint`, whose `py-compile`
covers the excluded crate. Rows marked `@shm` in `tests/python` run under
`just py-test` and skip only on a build without shared memory. No step touches a
Rust `shm`-gated target, so `just shm-check`, which compiles and runs no Python,
is not part of any step's gate. **Every mutant below is applied, rebuilt and run,
and its PR quotes the failure it produced**; none is recorded on a predicted
failure.

**The gap §11.1 is left with, stated once for every step's tests.** §11.1 asks
for *"every error type raised at least once with its attributes asserted"*. Four
things this plan ships cannot meet it:

- the `None` arm of every resolved-id attribute, which has no known Python
  trigger (measurement 6);
- `FrameNotDeclaredError.name`'s `None` arm. It comes from `lookup_err`'s
  `UnknownFrame { hash }` fallback, which is not a resolved id: `Tree.plan` and
  `span_impl` resolve both names first, and `Tree.lookup` reaches the fallback
  only through `unknown_frame_err` when both names resolve on its second probe,
  which is a peer's intern landing between two reads;
- `TopologyChangedError.plan_generation` and `.current_generation`. The class
  ships, but no Python test can make it raise. The generation moves only on a
  topology publication, which is a reparent, and neither the binding
  (`offline.rs`: *"the binding exposes no `reparent`"*), the CLI nor the C header
  has one. `test_errors.py`'s class-pickling test says no row of `CASES` raises
  it. The nearest candidate trigger is `tf_tree`'s `tf_tree_rendezvous_child`
  binary, whose `join-reparent` mode reparents a shared arena from another
  process, but no Python test drives it and `just py-test` does not build it;
- `ClaimRevokedError`, which is not built (question 5).

No test below claims to reach any of them, no mutant is listed against any of
them, and step 7 writes all four into §11.1.

1. **The specs say what is decided** (docs only; one PR).
   - `PHASE3.md` §4.4's block is replaced by the hierarchy *Decision* §1 and §4
     decide, with `ClaimRevokedError` listed as waiting for a Python-reachable
     trigger and draft [`0031`](./0031-the-participant-record-with-no-byte.md)
     named as what could create one, marked `(draft)`. Its 2026-09-14 amendment
     stays, dated, as the account of what ships, and its closing sentence
     (*"Until one is `ready`, the list above is what a caller can rely on"*),
     which expired when this record moved to `ready`, is corrected to say the
     list stays what ships until this record's steps land, and that each step
     edits the bullet it makes false.
   - §7.2 gains one sentence: type objects, including those `#[pyclass]` and
     `create_exception!` create, are not the singletons it forbids (*Context*).
   - [`API.md`](../API.md) gets a pointer from R5's *"not a field"* to D11's
     scope (*Decision* §7).

   Verified by `just artifact-versions`: relative links, table rows, and the
   draft-citation check, which the `0031` citation passes only if it uses no
   settled verb. And by reading the new block against the amendment: every class
   and attribute in the block either ships today or is named by a step below,
   and every bullet of the amendment is still true of `main`.
2. **Attributes on the seven existing classes of *Decision* §1, the `py`
   plumbing, and the stubs.** Also in this PR: `errors.rs`'s module doc drops
   *"No exception here has an attribute"*, and the §4.4 amendment's *"No class
   carries an attribute"* bullet is corrected. Verified by:
   - a `test_errors.py` table asserting each attribute's **value** on a raised
     instance. The fixture values are pairwise distinct (`requested=99`,
     `oldest=1000`, `newest=2000`, a `target` that is not the `source`), so a
     swapped pair cannot pass. `TopologyChangedError` has no row, because
     nothing a Python test can do raises it (the gap above);
   - for every raised instance in `CASES`, `len(e.args) == 1` and
     `str(e) == e.args[0]`, which is what the `args` mutant below has to fail;
   - a `SIM_DOMAIN` row for `ExtrapolationError.domain`, so a tag hardcoded to
     `0` cannot pass;
   - `test_a_raised_exception_survives_a_pickle_round_trip` extended to
     `vars(back) == vars(e)`;
   - `test_a_stale_id_degrades_to_an_index_and_a_reason_not_to_a_debug_dump`
     extended to assert the **resolved** pair, `e.edge == ("chassis_b",
     "sensor_c")`-shaped and a member of `t.edges()`. That test's tree resolves
     every id, and asserting that no fallback happens is what it is for;
   - a stub test asserting that each class body's annotated names equal
     `set(vars(raised))` for every class a row of `CASES` raises, with
     `_stub_members` widened to see annotations. `TopologyChangedError`'s
     annotations are compared against *Decision* §1's table instead, which checks
     the stub and not the mapper;
   - question 6's pin: a caller-constructed `tf_tree.ExtrapolationError("m")`
     has no `requested` attribute, and the stub annotates `requested: int`
     without `None`.

   Mutants:
   - swap `oldest`/`newest` in the `setattr`;
   - hardcode `domain` to `0`;
   - resolve `NoDataError.edge` as `(child, parent)`;
   - swap `DisconnectedError.target` and `.source`;
   - pass `requested` in `args` while still setting the attribute, which the
     `len(e.args) == 1` / `str(e) == e.args[0]` assertion above must fail;
   - drop `oldest: int` from `_core.pyi`;
   - set a class-level `requested = None` on `ExtrapolationError` in
     `register()`, which question 6's pin must catch.

   **Not a mutant: falling back to `("", "")` instead of `None`.** No test can
   reach that arm (the gap above), so it could not be killed.

   **Success-path cost:** an interleaved, pinned A/B of scalar `plan.at` and
   `Publisher.push` between this branch and its base, on a `--release` build,
   n ≥ 30 rounds. It is reported, not gated. Beside it goes the measured
   per-raise increment for `ExtrapolationError`, which replaces measurement 4's
   Python-level proxy.
3. **`TimeDomainMismatchError`.** Verified by `test_domains.py`'s existing
   plan-time refusal and `test_lookup_takes_the_domain_the_same_way` for the
   per-query one, each asserting the exact class and `expected`/`got`, with the
   two tags distinct. Mutants: raise `TfTreeError` from `plan_domain_err` only,
   and separately from `lookup_err`'s `TimeDomainMismatch` arm only, so each
   direction of the one-type claim is killed by its own row; and swap `expected`
   and `got`.
4. **`NonMonotonicStampError`.** Verified by `test_errors.py`'s three existing
   rows, `_non_monotonic_push`, `_non_monotonic_push_many` and
   `_non_monotonic_module_push`, moved from `tf_tree.TfTreeError` to the new
   class (which `pytest.raises` does not match against a base `TfTreeError`),
   each asserting `last`, `got` and `edge` equal to the stored pair. Mutants:
   delete `push_class`'s new arm, which fails all three rows; set the attributes
   in `push_err` only, which should fail the `push_many` row while the scalar
   rows pass, because `push_many` builds its exception through `push_class`
   inline; swap `last` and `got`; and set `edge` from the caller's typed pair
   instead of the resolved `EdgeId`. The three rows use `_chain()`'s short
   names, where the typed and stored pairs are equal, so none of them can kill
   that last mutant. **A fourth row does**: a backwards push on an edge whose
   child name is longer than 48 bytes (measurement 5's `"sensor_" + "x" * 60`),
   asserting `e.edge == t.edges()[i]`, the truncated stored pair, and
   `e.edge != (typed_parent, typed_child)`. **That row depends on
   [`0027`](./0027-the-48-byte-frame-name-store.md)**: if `intern` comes to refuse
   names over 48 bytes, the two spellings never differ, the row cannot be built
   and the mutant is equivalent, and the change that lands `0027` deletes the row
   and says so.
5. **`EdgeAlreadyClaimedError`.** Does not start until *Decision* §4's
   `owner_slot` constraint is settled; if the answer is `int | None`, the `None`
   arm joins the gap above and gets no mutant. Verified by a test in which a subprocess
   creates the arena and claims nothing (slot 0, `CREATOR_SLOT` in
   `tf_tree_ipc/src/open.rs`), the test process joins read-write (slot 1: the
   first joiner of a fresh arena, which `rendezvous.rs` already relies on) and
   claims an edge, and a second participant's claim on that edge is refused. It
   asserts the exact class, `owner_slot == 1` and `edge` equal to the stored
   pair. Mutants: hardcode `owner_slot` to `0`, which a claim held by the creator
   could not tell apart and this one can; route the `EdgeAlreadyClaimed` cause to
   the bug-report arm, so the class is the base `TfTreeError` again; and set
   `edge` from the caller's typed pair. That last mutant is killed only by a
   second refused claim on an edge whose child name is longer than 48 bytes,
   asserting the truncated stored pair as step 4's fourth row does, and that row
   is deleted under the same `0027` condition.
6. **`ArenaHeldButUnreachableError`.** Verified inside
   `test_a_python_consumer_recovers_an_arena_whose_owner_died`. Between the
   owner's reap and `inherit_ownership`, the survivor holds its byte and nothing
   serves, so a fresh `tf_tree.open(mode="rw")` there is the trigger. Python's
   `open` takes no `timeout=`, so the row costs `DEFAULT_OPEN_TIMEOUT` (5 s,
   `tf_tree_ipc/src/open.rs`). Before the owner is killed, the test
   attaches a second participant from its own process
   (`tf_tree.open(mode="ro")`, which takes a lock-file byte, or a second
   `mode="rw"` handle if the implementing PR finds a read-only byte outside
   `held_participants`' mask), so at least two slots are held when the trigger
   fires, and releases it before the test's own `inherit_ownership` assertion.
   With one survivor `holder_slots` has one element, which is ascending and
   descending at once. The test asserts the exact class, `ownership_held is
   False`, `len(holder_slots) >= 2` and `holder_slots ==
   tuple(sorted(holder_slots))`, and that the instance has no `first_pid`. Mutants: `ownership_held` hardcoded `True`;
   `holder_slots` decoded descending; drop the `open_err` arm, so the error is
   the base `TfTreeError` again.
7. **The specs close, and status to `implemented`** (docs only; one PR; after
   steps 2 to 6).
   - `PHASE3.md` §4.4's amendment is marked historical: the block is what ships,
     except `ClaimRevokedError`.
   - §11.1 records the gap stated above the steps, and Appendix B follows.
   - [`API.md`](../API.md) gets its §6 row for the attributes and the four
     classes.

   Verified by `just artifact-versions`, and by `EXPECTED_EXCEPTION_COUNT` in
   `test_errors.py` reading 14, which it does only if steps 3 to 6 have all
   landed.

## Open questions

Resolved before status moves from `draft` to `ready`. A `ready` doc has none.

None. All six were answered on 2026-09-14, under the owner's explicit delegation
to *"choose the most desirable approach for the library goals"*; the date is
recorded once, here and in *Decision*, for all six. The reasons were weighed in
the order *Decision* gives: typed correctness for real handlers, no untested
promise, minimal surface. Each question keeps the draft's text, struck through,
above its answer, because the reasoning is what the answer rests on.

One constraint was found in review after the move to `ready`, and none of the six
answers covers it: `EdgeAlreadyClaimedError.owner_slot` is `u32::MAX` when the
claim word is `CLAIMING`. It is recorded where it applies, in *Decision* §4, and
it holds step 5 only.

### 1. ~~Is the attribute set right, and is `.edge` the arena's pair?~~ — Decision §1's set, less `first_pid`; the stored pair, as a plain tuple

~~- **The set.** *Decision* §1 adds `FrameNotDeclaredError.name` and
  `ExtrapolationError.domain`, neither of which is in §4.4. It omits
  `DerivativesUnavailableError.interp`, whose Rust field is a stored
  discriminant that `stored_interp` may not be able to name. It omits
  `push_many`'s sample index, which is binding context rather than a field of
  the Rust error, and a `shape` on the `BufferError` sites that format one.
  `ClaimRevokedError.edge` would be the first attribute on the push path
  resolved from an `EdgeId` rather than taken from the publisher's label.
  `ArenaHeldButUnreachableError.first_pid` could be dropped, keeping
  `holder_slots` and pointing at `tf_tree doctor`, which prints a slot's
  identity with the context a pid needs. Each of these is a separate yes or no.~~

~~- **`.edge` as stored names** (proposed) against **the caller's typed names**
  for claim and push errors. Stored names match `Tree.edges()`. Typed names are
  what the caller will search their source for, and they are the only spelling
  that can be passed back into an entry point (measurement 5). The alternative
  is typed names where the binding has them, which gives `.edge` two spellings
  for names over 48 bytes. Neither is unique: truncated stored names collide
  (`0027`), and `Tree.frames()`'s docstring says a stalled intern that is rescued
  can leave the same name at two ids. The only unique identity is the id, and
  *Decision* §2 refuses to hand it out. If `0027` is adopted, this bullet has no
  cost on either side.~~

~~- **Should `.edge` be a named tuple** (`edge.parent`, `edge.child`)? That would
  be a new public type, and `Tree.edges()` returns plain tuples.~~

- **The set is *Decision* §1's**, `FrameNotDeclaredError.name` included: it is
  the one fact that class exists to report.
- **Omitted:** `push_many`'s sample index and a `BufferError` shape. Neither is
  present on every instance of its class, which is §1's own rule for what an
  attribute of a class is. `DerivativesUnavailableError.interp` is on every
  instance of its class and is omitted for another reason: no handler branches
  on it, and its raw discriminant cannot always be named (*Decision* §1).
- **Dropped:** `ArenaHeldButUnreachableError.first_pid`. It is advisory and
  pid-namespace-local (`0033`), its natural use, `os.kill`, can signal an
  unrelated process, and at `0` (no identity record) it signals the caller's own
  process group. `holder_slots` and `ownership_held` separate the remedies, and
  `tf_tree doctor` is the namespace-aware way to turn a slot into a process;
  `tf_tree participants` prints the raw pid. The message still prints the pid,
  which is `IpcError`'s text and not this record's (*Decision* §4).
- **`.edge` is the arena's stored `(parent, child)` pair, as a plain tuple.** It
  is the one spelling `Tree.edges()` already returns, and a named tuple would add
  a public type for it. The typed pair's advantages, that a caller can grep for
  it and pass it back, exist only for a name over 48 bytes, and stored names
  collide there too, so neither spelling identifies that edge. If
  [`0027`](./0027-the-48-byte-frame-name-store.md) is adopted and `intern`
  refuses names over 48 bytes, the stored and typed pairs never differ and this
  choice stops mattering.
- `NonMonotonicStampError.edge` is now the first push-path attribute resolved
  from an `EdgeId`, the role the draft gave `ClaimRevokedError.edge`.

### 2. ~~`KeyError`: strike it from §4.4 (proposed), or build it?~~ — struck

~~Building it means a class made by calling `type(name, (TfTreeError, KeyError),
{"__str__": Exception.__str__, ...})` from Rust (measurement 3), stored the way
`create_exception!` stores its types. It widens `except KeyError`/`except
LookupError` for every caller. What it buys is a `KeyError` idiom the surface
has no call site for. If the answer is *build*, the class change needs its own
`CHANGELOG.md` migration note, because it changes which existing handlers catch
it.~~

**Struck from `PHASE3.md` §4.4** (*Decision* §6; step 1 makes the edit). A
`KeyError` base widens every `except KeyError` and `except LookupError` that
encloses a tf_tree call, which measurement 3's swallowed dict lookup
demonstrates, and the surface has no mapping protocol whose idiom it would serve.

### 3. ~~Does `ExtrapolationError.domain` belong, and from where?~~ — yes, from the call site's tag

~~The argument for including it is *Decision* §3: an integer stamp that has
crossed a process boundary without its clock is R3's float-stamp mistake in
another form. The argument against is that the caller holds the plan in the
process where it matters, and the C ABI's `tft_error` has no domain field.
There are two sources. The **call site's tag** (proposed) is always an `int`,
but it adds a parameter to `lookup_err`'s Extrapolation path. The **edge
record's `domain`** needs no parameter. It equals the plan's tag whenever
`Extrapolation` fires, because the plan-time and per-query checks run first,
but it is `None` when the record is unresolvable.~~

**Included, from the call site's tag** (*Decision* §3). It is always an `int`,
so the attribute has no `None` arm, where the edge record's `domain` would need
one for an unresolvable record. R3 says a stamp carries its domain, and a
pickled error crosses a process boundary without the plan that said which clock
`requested` is on. The extra `lookup_err` parameter is on the error path, which
is cold (*Decision* §5).

### 4. ~~Which classes beyond §4.4's four, if any?~~ — a criterion, and `NonMonotonicStampError`

~~Listed with what each costs. None is proposed in *Decision* §4:~~

~~- **`ArenaAbsent`.** `0019`'s `is_retryable` puts it on one branch with
  `ArenaHeldButUnreachable`. If only the latter gets a class, a Python loop
  waiting for a robot to start still has to match `ArenaAbsent`'s message. A
  shared parent (for example `ArenaUnavailableError`, with
  `ArenaHeldButUnreachableError` beneath it) would mirror that branch exactly.
  It would be a class §4.4 does not name, and a name close to C's
  `TFT_ERR_ARENA_UNAVAILABLE`, which covers **every** open failure, not the
  retryable two.~~
~~- **`NonMonotonicStamp`.** It is the commonest push failure (`push_msg`'s
  comment), and without a class it cannot be told apart from the other base
  `TfTreeError`s a push raises: `released()` from `Publisher.push` and
  `push_many` on a closed publisher, a poisoned publisher mutex, and, from the
  module-level `push`, every `claim_err` arm and `unresolvable_name`'s two arms.
  `.last`/`.got`, and an `.edge` resolved from #339's new `EdgeId`, would need a
  class to hang on.~~
~~- **The retryable family**: `SlotRecycled`, `SlotContended`, `InternContended`,
  `LeaseContended`, `ReapedDuringClaim`. One `RetryableError` parent or several
  leaves. The Context table is the argument for it. The cost is that
  "retryable" is a judgement per variant, and `SlotRecycled`'s own arm says a
  retry re-reads *a newer* window.~~
~~- **`MixedTimeDomains`.** Is it `TimeDomainMismatchError` (both are domain
  refusals) or not (one is the caller's `domain=`, the other a property of the
  path that no argument fixes)? `errors.rs` calls it *"arguably
  `Disconnected`-shaped"*.~~
~~- **`FrameOutOfRange` and `MissingEdge`**, which `errors.rs` calls *"arguably
  `FrameNotDeclaredError`"*.~~

**The criterion: a class is added only when a Python handler needs to branch on
the failure *and* a Python test can make it raise.** Both conditions are
necessary. They are not sufficient, and `ArenaAbsent` below is the one candidate
for which that matters.

**`NonMonotonicStampError(TfTreeError)` meets both and is added**, with `.edge`
(resolved from the `EdgeId` #339 added), `.last` and `.got`. It is the commonest
push failure (`push_msg`'s comment), and today a handler cannot tell it apart
from the other base `TfTreeError`s a push raises: `released()` from
`Publisher.push` and `push_many` on a closed publisher, a poisoned publisher
mutex, and, from the module-level `push`, every `claim_err` arm and
`unresolvable_name`'s two arms. A backwards push triggers it, and
`test_errors.py`'s three `_non_monotonic_*` rows already do.

**Deferred**, each with what holds it back. The criterion reopens the first
three; `ArenaAbsent`, which already meets it, reopens on a choice of shape:

- **The retryable family** (`SlotRecycled`, `SlotContended`, `InternContended`,
  `LeaseContended`, `ReapedDuringClaim`). No Python test makes any of them raise
  today, and the contended ones are races a test cannot make happen on demand.
  "Retryable" is also a judgement per variant: `SlotRecycled`'s own arm says a
  retry re-reads *a newer* window.
- **`MixedTimeDomains`.** No handler branch: no argument a query can pass fixes
  the path, so a class would not change what a handler does.
- **`FrameOutOfRange` and `MissingEdge`.** No Python trigger: both describe an
  arena that changed under a plan or lacks a record, and no Python entry point
  produces either on demand.
- **`ArenaAbsent`, and an `ArenaUnavailableError` parent above
  `ArenaHeldButUnreachableError`.** Deferred, but **not for want of the
  criterion**, which on this record's own evidence it meets: a Python `open` with
  nothing serving raises it (`test_api.py`'s
  `test_open_validates_interp_even_with_nothing_to_create` quotes the message in
  its mutant note), and Python's `open` takes no `timeout=`, so a supervisor loop
  waiting for a robot has to branch on it. What is unsettled is the shape: a leaf
  of its own, or the shared parent that mirrors `0019`'s `is_retryable` branch,
  whose natural name sits close to C's `TFT_ERR_ARENA_UNAVAILABLE`, which covers
  every open failure rather than the retryable two. Either is additive later:
  inserting a parent between `ArenaHeldButUnreachableError` and `TfTreeError`
  keeps every existing `except` clause matching. The shape is the third condition, and
  the only one holding it back: it reopens on a record that chooses the shape,
  not on the criterion.

### 5. ~~Does a class ship when no Python caller can make it raise?~~ — no; an attribute's `None` arm does

~~`ClaimRevokedError` is reachable only from Rust (step 2), and the `None` arm of
every resolved id attribute in step 1 has no known Python trigger either
(measurement 6). §11.1 asks for *"every error type raised at least once with its
attributes asserted"*. Three answers, each with its cost:~~

~~- **Ship the class and the arm, record the gap in §11.1.** A caller can write
  `except ClaimRevokedError`, or handle `e.edge is None`, for the day the
  failure becomes reachable (an answer to
  [`0031`](./0031-the-participant-record-with-no-byte.md) could change which
  publishers are reaped). The mapper arm has no test that can fail, and no
  mutant can be run against it.~~
~~- **Build a test-only trigger.** A `test-hooks`-style entry point into
  `tf_tree_py` that constructs the Rust error, or a stale id, and runs the
  mapper. That is new surface in a wheel whose every exported name the stub
  drift check holds to `_core.pyi`.~~
~~- **Do not build the class until a trigger exists.** §4.4 is not NORMATIVE, so
  this is allowed, and the spec is amended to say which classes wait and why.
  For an id attribute, this means typing it without `| None` and raising on the
  unreachable arm, which turns a promise nobody can test into a bug report.~~

**`ClaimRevokedError` is deferred, not built** (*Decision* §4). `PHASE3.md`
§4.4 is amended to say it waits for a Python-reachable trigger, citing draft
`0031` as what could create one; until then the failure raises the base
`TfTreeError`. **The resolved-id attributes keep their `| None` arm**: an arena
record the binding cannot resolve is a real state even with no known Python
trigger, and typing it away would make the stub wrong on the day one appears.
`FrameNotDeclaredError.name`'s `None` arm, which only an intern race reaches,
is kept on the same reasoning. So are `TopologyChangedError`'s two attributes,
although no Python test can make that class raise: the class already ships, a
correct program attached to a shared arena meets it whenever a peer reparents
(`lookup_err`'s comment on `TopologyChanged`), and C's `tft_error` already
carries both generations. The gap §11.1 is left with is recorded once, above the
plan's steps, and step 7 writes it into §11.1. The test-only trigger is not built, because it is new
surface in a wheel to test a state the wheel cannot otherwise reach.

### 6. ~~What does a caller-constructed instance carry?~~ — nothing: the stub stays precise

~~*Decision* §7 names the hazard: the stub annotates attributes every instance
lacks unless the library raised it. Two remedies:~~

~~- **(a) Class-level `None` defaults**, set on each type object in `register()`.
  An instance's `__dict__` still wins, so pickling a raised instance is
  unaffected (measurement 1, first row), and a caller-built one reads `None`
  instead of raising. The cost is that every stub attribute is typed `X | None`,
  so a handler that only ever sees raised instances must still narrow
  `e.requested` before arithmetic. The stub test gains a caller-built instance.~~
~~- **(b) Keep `requested: int` in the stub**, and say in every class docstring
  that the attributes exist only on instances the library raises. Correct code
  stays unannotated, and a test double without attributes still fails at
  runtime rather than at type-check.~~

**(b).** The stub types are precise (`requested: int`, not `int | None`), every
class docstring states that the attributes exist only on instances the library
raises, and no class-level `None` default is set. The consumer of these
attributes is a handler of raised errors, and under (a) every such handler would
narrow a value that is never `None` on anything it catches; a test double built
without the attributes fails loudly at runtime, where it was written. Step 2's
pin holds the choice.

## Reproduction

Three pure-Python or read-only scripts produced measurements 1–4. The two that
load tf_tree read the pre-#335 `.so` by path, so on a checkout whose extension
has been rebuilt, point `so` at a build of the same age or accept that the
`__module__` line differs. Measurements 5 and 6 are the inline snippets quoted
where they are reported; measurement 6 needs `TF_TREE_RUNTIME_DIR` set to a path
short enough for `sun_path` (108 bytes).

**`probe_portable.py`**, measurement 1's protocol sweep and measurement 3, no
tf_tree. Run as `python3.X probe_portable.py` under each interpreter:

```python
import pickle, sys, sysconfig
ft = bool(sysconfig.get_config_var("Py_GIL_DISABLED"))
print(f"{sys.version.split()[0]} free-threaded={ft}")

class TfTreeError(Exception): pass
class Ex(TfTreeError): pass

e = Ex("msg"); e.requested = 5; e.edge = ("world", "base")
kept = [p for p in range(pickle.HIGHEST_PROTOCOL + 1)
        if vars(pickle.loads(pickle.dumps(e, protocol=p))) == {"requested": 5, "edge": ("world", "base")}]
print("  setattr attrs survive pickle protocols:", kept, "of", list(range(pickle.HIGHEST_PROTOCOL + 1)))

class Slot(TfTreeError):
    __slots__ = ("requested",)
s = Slot("m"); s.requested = 5
print("  slot-stored attr survives:", hasattr(pickle.loads(pickle.dumps(s)), "requested"))

msg = 'no frame named "ghost" in this arena'
K = type("FrameNotDeclaredError", (TfTreeError, KeyError), {"__module__": __name__})
k = K(msg)
print("  KeyError base: MRO", [c.__name__ for c in K.__mro__])
print("  KeyError base: str(e) =", str(k))
KS = type("FrameNotDeclaredError2", (TfTreeError, KeyError), {"__module__": __name__, "__str__": Exception.__str__})
print("  with __str__ = Exception.__str__: str(e) =", str(KS(msg)))
def caught_by(exc, *classes):
    out = []
    for c in classes:
        try:
            raise exc
        except c:
            out.append(c.__name__)
        except BaseException:
            pass
    return out
print("  caught by:", caught_by(KS(msg), KeyError, LookupError, TfTreeError, Exception))
print("  today's shape caught by:", caught_by(Ex(msg), KeyError, LookupError, TfTreeError, Exception))
globals()["FrameNotDeclaredError2"] = KS
b = pickle.loads(pickle.dumps(KS(msg)))
print("  KeyError-based class pickles:", type(b) is KS, str(b))
frames = {"lidar": "base_link"}
def resolve(name):
    parent = frames[name]
    raise KS(f'no frame named "{parent}" in this arena')
try:
    resolve("lidar")
except KeyError:
    print("  hazard: `except KeyError` meant for the dict swallowed FrameNotDeclaredError")
import traceback
print("  traceback last line:", traceback.format_exception_only(K(msg))[-1].rstrip())
print("  traceback last line (__str__ restored):", traceback.format_exception_only(KS(msg))[-1].rstrip())
```

**`probe_pickle.py`**, measurement 1's shape matrix and measurement 2. The
pure-Python half runs under each interpreter (the `V7` line raises
`AttributeError` on 3.10); the PyO3 half needs the 3.14 `.so` and `numpy`. Run as
`python3.14 probe_pickle.py`:

```python
import copy, importlib.util, pickle, sys
print(sys.version)

class Base(Exception):
    pass

def rt(e):
    try:
        b = pickle.loads(pickle.dumps(e))
    except Exception as x:
        return f"FAILS: {type(x).__name__}: {x}"
    return b

class V1(Base): pass
e = V1("stamp 5 ns is outside [1, 4] ns"); e.requested = 5; e.oldest = 1; e.newest = 4; e.edge = ("world", "base")
b = rt(e)
print("V1 setattr-after-construction: __reduce__ =", e.__reduce__())
print("   round trip:", type(b).__name__, b.args, vars(b), "str:", str(b))
print("   copy.copy keeps:", vars(copy.copy(e)))

class V2(Base):
    @property
    def requested(self): return self.args[1]
e = V2("stamp 5 ns is outside [1, 4] ns", 5, 1, 4)
b = rt(e)
print("V2 args-carrying: str(e) =", repr(str(e)))
print("   round trip requested:", b.requested, "str:", repr(str(b)))

class V3(Base):
    def __init__(self, msg, *, requested):
        super().__init__(msg); self.requested = requested
e = V3("m", requested=5)
print("V3 required kw-only __init__: __reduce__ =", e.__reduce__())
print("   round trip:", rt(e))

class V4(Base):
    def __init__(self, msg, *, requested=None):
        super().__init__(msg); self.requested = requested
e = V4("m", requested=5)
b = rt(e)
print("V4 defaulted kw __init__: round trip requested:", b.requested)

class V5(Base):
    __slots__ = ("requested",)
e = V5("m"); e.requested = 5
b = rt(e)
print("V5 slot storage: vars(e) =", vars(e), "round trip has requested:", hasattr(b, "requested"))

class V6(Base): pass
e = V6("m"); e.tree = (lambda: None)
print("V6 unpicklable attribute value: round trip:", rt(e))

class V7(Base): pass
e = V7("m"); e.requested = 5; e.add_note("while sampling lidar")
b = rt(e)
print("V7 add_note + attr: round trip", vars(b))

so = "/home/dev/src/tf_tree/python/tf_tree/_core.cpython-314-x86_64-linux-gnu.so"
spec = importlib.util.spec_from_file_location("_core", so)
core = importlib.util.module_from_spec(spec); spec.loader.exec_module(core)
sys.modules["_core"] = core
X = core.ExtrapolationError
print("PyO3 type:", X, "__module__", X.__module__, "mro", [c.__name__ for c in X.__mro__])
print("   __dictoffset__", X.__dictoffset__, "flags heaptype", bool(X.__flags__ & (1 << 9)))
e = X("m"); e.requested = 5; e.oldest = 1; e.newest = 4; e.edge = ("world", "base")
b = rt(e)
print("   setattr then round trip:", type(b), b.args, vars(b))
tree = core.build([("world", "base")])
pub = tree.publisher("base", "world")
import numpy as np
pub.push(1000, np.array([1,0,0,0,0,0,0.0]))
pub.push(2000, np.array([1,0,0,0,0,0,0.0]))
plan = tree.plan("world", "base")
try:
    plan.at(99)
except core.ExtrapolationError as raised:
    print("   raised from Rust: vars =", vars(raised), "args =", raised.args)
    raised.requested = 99
    b = rt(raised)
    print("   raised + setattr, round trip:", vars(b))
```

**`probe_cost.py`**, measurement 4. Run as `taskset -c 3 python3.14
probe_cost.py`:

```python
import importlib.util, sys, timeit
import numpy as np
so = "/home/dev/src/tf_tree/python/tf_tree/_core.cpython-314-x86_64-linux-gnu.so"
spec = importlib.util.spec_from_file_location("_core", so)
core = importlib.util.module_from_spec(spec); spec.loader.exec_module(core)
sys.modules["_core"] = core

tree = core.build([("world", "base")])
pub = tree.publisher("base", "world")
pub.push(1000, np.array([1, 0, 0, 0, 0, 0, 0.0]))
pub.push(2000, np.array([1, 0, 0, 0, 0, 0, 0.0]))
plan = tree.plan("world", "base")
X = core.ExtrapolationError

def ok():
    plan.at(1500)

def fail():
    try:
        plan.at(99)
    except X:
        pass

def fail_and_read_message():
    try:
        plan.at(99)
    except X as e:
        str(e)

def setattrs():
    e = X("m")
    e.edge = ("world", "base"); e.requested = 99; e.oldest = 1000; e.newest = 2000

def construct_only():
    X("m")

N = 200_000
for name, fn in [("success plan.at(1500)", ok), ("raise+catch ExtrapolationError", fail),
                 ("raise+catch+str(e)", fail_and_read_message),
                 ("X('m') construct only", construct_only),
                 ("X('m') + 4 setattr (py-level)", setattrs)]:
    best = min(timeit.repeat(fn, number=N, repeat=7)) / N * 1e9
    print(f"{name:34s} {best:8.1f} ns")
```
