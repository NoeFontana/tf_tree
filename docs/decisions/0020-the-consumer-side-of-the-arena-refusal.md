# 0020: The consumer side of the arena refusal

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** _(none — a `draft` authorizes no work; see
[`README.md`](./README.md)'s lifecycle)_

## Context

[`0015`](./0015-the-bridge-fills-a-shared-arena.md) gave the bridge
`TFT_ERR_ARENA_UNAVAILABLE`, but `tft_tree_open` still does
`Err(_) => set_error(TFT_ERR_INTERNAL, <fixed string>)`: the `OpenError` is never
inspected. A caller's action differs by class: wait (`IpcError::ArenaAbsent` /
`ArenaHeldButUnreachable`); fix the deployment (`RuntimeDir*`, `NetworkFilesystem`);
fix `$TF_TREE_DOMAIN` / `$TF_TREE_NAME`; rebuild every participant
(`HandshakeRejected`); find the leak (`NoParticipantSlots`, `PROJECT.md` §5 D4).
The create-side errors are unreachable: `tf_tree::open()` is read-only, which
implies `CreatePolicy::Never` ([`0019`](./0019-one-binary-and-topology-you-can-wait-for.md)
§2a).

The consumer's likeliest failure, "not there yet", is not an error, yet both
in-tree consumers poll blind (`open_within`), so a typo costs the whole timeout.
`Open::await_open` retries only `ArenaAbsent` and `ArenaHeldButUnreachable`; C
cannot express that partition, and [`API.md`](../API.md) §1 R5 makes the code the
contract across FFI.

## Decision

**Recommended; `draft` until ratified.**

### 1. One new status code, for the retryable class only

```c
/** No arena is serving this (domain, name) yet, or one is held and its owner
 *  did not answer. Retrying may succeed; every other failure of this call
 *  will not change on its own. */
#define TFT_ERR_ARENA_ABSENT (-43)
```

The partition is exactly `Open::await_open`'s (`0019` §2b, `is_retryable` in
`crates/tf_tree/src/open.rs`); cause stays in the message. It is distinct from
`TFT_ERR_ARENA_UNAVAILABLE` (a *create* failure): one code with two meanings is what
`0015` question 3 and `PHASE5.md` §6 refused.

### 2. The message stops being a constant

`Err(_)` becomes `Err(e)`: the `OpenError` is rendered into `tft_error`. Diagnostic
only; R5 forbids matching.

### 3. The new code is reachable only from a new entry point

```c
/** Join the arena named by the environment, waiting up to timeout_ns for it
 *  to appear. timeout_ns == 0 makes exactly one attempt. */
tft_status tft_tree_open_wait(uint64_t timeout_ns, tft_tree **out);
```

`tft_tree_open` keeps its contract byte for byte, including `TFT_ERR_INTERNAL`: a
widened return set belongs on the major (`PHASE4.md` §3.6).
`TFT_ABI_VERSION_MINOR` 5 -> 6. The timeout is `uint64_t` nanoseconds (R3).

## Implementation plan

_Not to be started while this record is `draft`; `API.md` §6 gains a row at `ready`._

1. **`TFT_ERR_ARENA_ABSENT` and `tft_tree_open_wait`**, behind `shm`, minor 5 -> 6.
   Test: a wait against no publisher returns `TFT_ERR_ARENA_ABSENT` inside the
   budget; a `$TF_TREE_NAME` containing `/` returns terminally at once.
2. **`Err(_)` -> `Err(e)`** in both functions; a maximal name must not truncate the condition.
3. **Both `open_within` helpers deleted**; `just ros-test`.
4. **A correspondence test** between the C partition and `is_retryable`.

## Open questions

1. **RESOLVED - distinct code.** `TFT_ERR_ARENA_UNAVAILABLE`'s documented meaning
   includes "the rendezvous name is already held by a live arena" (`tft_status`'s
   doc in `crates/tf_tree_c/src/error.rs`), the negation of this code's.
2. **Does `tft_tree_open` stay unchanged forever, or is `TFT_ERR_INTERNAL` on it
   deprecated at the next major?**
3. **RESOLVED - `0018` does not reach a userspace poll**: its boundary is the
   arena, and a `PROT_READ` consumer cannot write a waiter word. `0019` §2b is the
   in-force citation. **Still open: the header tier.** `tft_tree_open_wait` in the
   frozen `tf_tree.h` is this record's choice; `tf_tree_unstable.h` would change
   question 1's minor-bump argument.
