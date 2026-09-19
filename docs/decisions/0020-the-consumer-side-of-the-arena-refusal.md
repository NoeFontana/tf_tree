# 0020: The consumer side of the arena refusal

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** _(none — a `draft` authorizes no work; see
[`README.md`](./README.md)'s lifecycle)_


## Context

[`0015`](./0015-the-bridge-fills-a-shared-arena.md)'s *Failure* section gave the
bridge `TFT_ERR_ARENA_UNAVAILABLE` so an operator can tell "another bridge holds
this name" from "the runtime directory is on NFS" from "a bug". It shipped without
touching `tft_tree_open`, which still does
`Err(_) => set_error(TFT_ERR_INTERNAL, <fixed string>)`: the `OpenError` is never
inspected, so the C consumer gets one code and one sentence for every failure:

| What actually happened | What a caller should do |
|---|---|
| `IpcError::ArenaAbsent` / `ArenaHeldButUnreachable` | wait; the publisher is not up or is mid-start |
| `RuntimeDirUnusable` / `RuntimeDirNotADirectory` / `RuntimeDirForeignOwner` / `NetworkFilesystem` / `StatFsFailed` | fix the machine or deployment |
| `DomainNotAnInteger` / `NameInvalid` | fix `$TF_TREE_DOMAIN` / `$TF_TREE_NAME` |
| `HandshakeRejected { VersionMismatch \| LayoutMismatch }` | rebuild every participant together |
| `HandshakeRejected { NoParticipantSlots }` | find the leak; capacity is fixed at construction (`PROJECT.md` §5 D4, `tf_tree_arena::layout::DEFAULT_MAX_PARTICIPANTS`) |
| `OpenError::Map` | a bug, or a resource limit |

`Build`, `ReadOnlyCannotCreate`, `NoLayoutToCreate` and `ArenaAlreadyLive` are
unreachable here: `tf_tree::open()` is `Open::new().open()`, read-only, which
implies `CreatePolicy::Never` ([`0019`](./0019-one-binary-and-topology-you-can-wait-for.md)
§2a). A fresh `open()` in a fork child is an ordinary attach and succeeds.

The consumer is the process with no operator, and its likeliest failure - "the
arena is not there yet" - is not an error, just early. Both in-tree consumers work
around it: `ros/tf_tree_ros/test/test_shared_arena.cpp` asserts
`TFT_ERR_INTERNAL` for "correctly absent", and both `open_within` helpers
(there and in `ros/tf_tree_bench_ros/src/bench_consumer.cpp`) poll blind, so a
typo, an NFS runtime directory or a version disagreement each cost the whole
timeout. `Open::await_open` retries only `ArenaAbsent` and
`ArenaHeldButUnreachable` and returns every other error verbatim; C and C++
cannot express that partition.

[`API.md`](../API.md) §1 R5 makes the **code** the contract across FFI and
forbids matching on message text, so prose cannot close this. New public surface
on a frozen C ABI needs a record ([`API.md`](../API.md) §7).

## Decision

**Recommended; `draft` until ratified (see *Open questions*).**

### 1. One new status code, for the retryable class only

```c
/** No arena is serving this (domain, name) yet, or one is held and its owner
 *  did not answer. Retrying may succeed; every other failure of this call
 *  will not change on its own. */
#define TFT_ERR_ARENA_ABSENT (-43)
```

The partition is exactly `Open::await_open`'s
(`IpcError::ArenaAbsent | ArenaHeldButUnreachable` versus everything else;
`0019` §2b, `is_retryable` in `crates/tf_tree/src/open.rs`). Two codes and not one
per `IpcError` class: the granularity is `TFT_ERR_BAD_CONFIG`'s, one code per
class of caller action, cause in the message.

It is distinct from `TFT_ERR_ARENA_UNAVAILABLE` ("the arena you asked me to
*create* could not be created"): one code with two meanings depending on the
function is what `0015` question 3 and `PHASE5.md` §6's `TFT017`/`TFT018` refused.

### 2. The message stops being a constant

`Err(_)` becomes `Err(e)` and the `OpenError` (already `Display`) is rendered
into the bounded `tft_error` buffer. Diagnostic only; R5 still forbids matching.

### 3. The new code is reachable only from a new entry point

```c
/** Join the arena named by the environment, waiting up to timeout_ns for it
 *  to appear. timeout_ns == 0 makes exactly one attempt. */
tft_status tft_tree_open_wait(uint64_t timeout_ns, tft_tree **out);
```

`tft_tree_open` keeps its contract byte for byte, including `TFT_ERR_INTERNAL`.
Every minor bump rests on "an older caller cannot observe the change"; widening
`tft_tree_open`'s return set fails that (a `0.5` caller matching
`TFT_ERR_INTERNAL` for "no arena" silently stops matching), and `PHASE4.md` §3.6
puts that on the major. A new symbol restores the `3` -> `4` precedent, and
`tft_tree_open_wait(0, &tree)` is the one-shot call with the better code.
`TFT_ABI_VERSION_MINOR` 5 -> 6.

## Rationale

- **Not leaving it:** a consumer that starts before its publisher is the normal
  case on a robot, and today it can only poll to a timeout and say nothing useful.
- **Not exposing `await_open` alone:** expiry must return something, and
  `TFT_ERR_INTERNAL` would conflate "waited, publisher never came" with "your
  `$TF_TREE_NAME` has a space in it". The wait needs the code; the code wants the
  wait.
- **Timeout is `uint64_t` nanoseconds:** `API.md` §1 R3.
- **Rejected:** a `bool *retryable` out-parameter on `tft_tree_open` (signature
  change, major bump); documenting message text (R5); a richer C++ exception
  (the wrapper is header-only over the C codes and would parse the message).

## Consequences

Both `open_within` helpers collapse into one call, and the ROS test can assert
"the arena is absent". Cost: one more symbol and code in a frozen header, and
`tft_tree_open` / `tft_tree_open_wait` stay two spellings of one operation
forever. Python (binds Rust directly) and the Rust facade are unaffected. The
retryable set here and `is_retryable` must stay identical, asserted by a test.
`API.md` §6 gains a row when this record reaches `ready`, not while `draft`.

## Implementation plan

_Not to be started while this record is `draft`._

1. **`TFT_ERR_ARENA_ABSENT` and `tft_tree_open_wait`**, behind `shm`, minor 5 -> 6
   with its paragraph on `TFT_ABI_VERSION_MINOR`. Verified by a `tests/` case that
   a wait against no publisher returns `TFT_ERR_ARENA_ABSENT` inside the budget,
   and one that a `$TF_TREE_NAME` containing `/` returns terminally and
   immediately.
2. **`Err(_)` -> `Err(e)`** in `tft_tree_open` and the new function. Verified by
   the longest-name message test's sibling: a maximal name must not truncate the
   part that names the condition.
3. **Both `open_within` helpers deleted** (`ros/tf_tree_ros/test/`,
   `ros/tf_tree_bench_ros/src/`). Verified by `just ros-test`; `just dds-bench`
   still reports four arms at 0 % failure.
4. **A correspondence test** between the C partition and
   `tf_tree::open::is_retryable`, failing when either side is edited alone.

## Open questions

1. **RESOLVED 2026-08-22 - distinct code.** `doctor` and `top` cannot report
   either code: they never touch `tft_status`; every CLI attach is
   `AttachArgs::open` (`crates/tf_tree_cli/src/attach.rs:70-101`), returning a
   Rust `OpenError`. No consumer is obliged to distinguish the two today, so the
   objection stands on the constant alone: `TFT_ERR_ARENA_UNAVAILABLE`'s
   documented meaning includes "the rendezvous name is already held by a live
   arena" (`crates/tf_tree_c/src/error.rs:126-130`), the negation of this code's.
   Its **normative** sole-producer clause ("Returned only by `tft_bridge_create`,
   and only when `tft_bridge_options::arena_name` is non-NULL") is what made the
   `4` -> `5` bump provable, and is authored at one site
   (`crates/tf_tree_c/src/error.rs:141`). Reuse would be ABI-safe; the case for a
   distinct code is a per-code contract in a header that never withdraws a
   promise, not a diagnosis anyone would get wrong today.
2. **Does `tft_tree_open` stay unchanged forever, or is `TFT_ERR_INTERNAL` on it
   deprecated at the next major?** If `0.x` -> `1.0` is close, widening the
   existing function without a new symbol becomes available.
3. **RESOLVED 2026-08-22 - `0018` does not reach a userspace poll.** Its
   boundary is the arena: a waiter must register by writing a word, which a
   `PROT_READ` consumer cannot. `API.md` ("No blocking wait in the core") and
   `0019` §2b (`Open::await_open`, a `MIN_BACKOFF` -> `MAX_BACKOFF` loop) are the
   in-force citations; `tf_tree_c` already depends on the facade with
   `unstable`. Do not cite `0018` plan step 5: it is gated behind `PHASE7.md`
   §0.0. **Still open: the header tier.** No wait symbol exists in either header,
   so `tft_tree_open_wait` in the frozen `tf_tree.h` (Decision §3) is this
   record's choice, and moving it to `tf_tree_unstable.h` would change question
   1's minor-bump argument. It would be the C ABI's first blocking entry point.
