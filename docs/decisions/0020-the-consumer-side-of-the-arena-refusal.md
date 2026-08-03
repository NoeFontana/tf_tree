# 0020: The consumer side of the arena refusal

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** _(none — a `draft` authorizes no work; see
[`README.md`](./README.md)'s lifecycle)_

## Context

[`0015`](./0015-the-bridge-fills-a-shared-arena.md)'s *Failure* section spends
four paragraphs arguing that a bridge which cannot get the shared arena it asked
for must say **which** thing went wrong, and that collapsing those causes onto
one code "leaves an operator unable to tell 'another bridge holds this name'
from 'the runtime directory is on NFS' from 'a bug'". It gave the bridge
`TFT_ERR_ARENA_UNAVAILABLE` for exactly that reason, and named
`tft_tree_open` — *"what `tft_tree_open` does today"* — as the anti-pattern it
was refusing to copy.

It then shipped without touching `tft_tree_open`.

```rust
// crates/tf_tree_c/src/lib.rs:502-524
match tf_tree::open() {
    Ok(tree) => { /* ... */ TFT_OK }
    Err(_) => {
        set_error(
            TFT_ERR_INTERNAL,
            "could not join an arena; check $TF_TREE_DOMAIN, $TF_TREE_NAME \
             and that a publisher is running",
            |_| {},
        );
        TFT_ERR_INTERNAL
    }
}
```

**`Err(_)`.** The `OpenError` is not inspected, not rendered, and not carried
into the message — which is a fixed string. So the C consumer gets one code and
one sentence for every one of these:

| What actually happened | What a caller should do |
|---|---|
| `IpcError::ArenaAbsent` — nothing is serving this `(domain, name)` | wait; the publisher has not started |
| `IpcError::ArenaHeldButUnreachable` — a holder exists, its owner server did not answer | wait; it is mid-start |
| `IpcError::RuntimeDirUnusable` / `RuntimeDirNotADirectory` / `RuntimeDirForeignOwner` | fix the machine |
| `IpcError::NetworkFilesystem` / `StatFsFailed` — `$TF_TREE_RUNTIME_DIR` is on NFS | fix the deployment |
| `IpcError::DomainNotAnInteger` / `NameInvalid` — a typo in `$TF_TREE_DOMAIN` / `$TF_TREE_NAME` | fix the environment |
| `HandshakeRejected { VersionMismatch \| LayoutMismatch }` | rebuild every participant together |
| `HandshakeRejected { NoParticipantSlots }` | raise `--participants`, or find the leak |
| `OpenError::Map` — the segment arrived and could not be mapped | a bug, or a resource limit |

Three `OpenError` variants are *not* in that table and should not be:
`ReadOnlyCannotCreate`, `NoLayoutToCreate` and `ArenaAlreadyLive` are all
unreachable through this entry point, because `tf_tree::open()` is
`Open::new().open()` and `Open::new()`'s defaults are read-only plus
`CreatePolicy::Never` ([`0019`](./0019-one-binary-and-topology-you-can-wait-for.md)
§2a). Fork poisoning is not in it either, and that is worth stating because it
is easy to assume otherwise: poisoning marks handles a child *inherited*, and a
fresh `tf_tree::open()` in that child is an ordinary attach that succeeds —
which is precisely the remedy `docs/PHASE3.md` §8 tells a Python user to
apply.

**Why the consumer side is worse than merely inconsistent with the bridge
side.** The bridge is a supervised process with an operator and a log; it fails
once, at startup, and a human reads the line. The consumer is the process that
*has no operator looking at it* and whose single most likely failure — "the
arena is not there yet" — is the one that is **not an error at all**, just
early. It is the side that needs the actionable code more, and it is the side
that has none.

That is not a hypothetical reading of the code. Both consumers in this
repository already work around it, and both say so in their own comments:

- `ros/tf_tree_ros/test/test_shared_arena.cpp`'s `open_status_now` records that
  "arena absent" and "runtime directory unusable" are one code here, so its
  negative assertion is `ASSERT_EQ(open_status_now(), TFT_ERR_INTERNAL)` — an
  assertion that a *bug* and *the arena is correctly absent* look identical,
  written as the strongest thing available.
- `ros/tf_tree_bench_ros/src/bench_consumer.cpp`'s `open_within` polls **blind**:
  `if (tft_tree_open(&tree) == TFT_OK) return tree;` and otherwise sleeps 20 ms
  until a deadline. A `$TF_TREE_NAME` typo, an NFS runtime directory and a
  `FORMAT_VERSION` disagreement each cost the caller its entire timeout and then
  report as a timeout. `ros/tf_tree_ros/test/test_shared_arena.cpp`'s
  `open_within` is the same function for the same reason, and says so.

`Open::await_open` does not have this problem, and the contrast is the whole
point: it retries `ArenaAbsent` and `ArenaHeldButUnreachable` and **returns every
other error verbatim**, "because retrying cannot change a `FORMAT_VERSION`
disagreement, a layout hash mismatch or a missing runtime directory, and burning
the budget against one would replace a precise message with a timeout". Rust
consumers get that partition. C and C++ consumers cannot express it.

**Why prose does not close this.** `tft_last_error()` exists, and
[`docs/API.md`](../API.md) §1 R5 is NORMATIVE about what it is worth: *"Across an
FFI boundary the **code** is the contract and the message is a diagnostic"*, and
*"Message text is not [a compatibility promise], and no surface may document text
that a downstream caller could be tempted to match on."* A C caller that wants to
retry on absence and abort on a version mismatch is therefore not permitted to
read the string — and in this case there is nothing in the string to read, since
`Err(_)` threw the distinction away before `set_error` was reached.

**What forces the decision now.** This is new public surface on a frozen C ABI —
at minimum one `tft_status`, possibly one entry point — and an ABI minor bump.
`CLAUDE.md` routes that to a decision record rather than to a PR, and
[`API.md`](../API.md) §7 is the checklist a new entry point passes.

## Decision

**Recommended, and the reason it is written as a recommendation rather than as a
settled decision is in *Open questions*: this record is `draft`.**

### 1. One new status code, for the retryable class only

```c
/** No arena is serving this (domain, name) yet, or one is held and its owner
 *  did not answer. Retrying may succeed; every other failure of this call
 *  will not change on its own. */
#define TFT_ERR_ARENA_ABSENT (-43)
```

The partition is **not a new judgement**. It is exactly `Open::await_open`'s —
`IpcError::ArenaAbsent | IpcError::ArenaHeldButUnreachable` on one side,
everything else on the other — which `0019` §2b argued and
`crates/tf_tree/src/open.rs`'s `is_retryable` implements. Adopting it verbatim
means the two surfaces cannot drift, and it means this record settles a spelling
rather than a semantics.

Two codes and not more. `0015` set the granularity precedent explicitly:
`TFT_ERR_ARENA_UNAVAILABLE` is *"one code with a specific `tft_error` message, at
the granularity `TFT_ERR_BAD_CONFIG` already uses for every way a config can be
wrong"*. A code per `IpcError` class would re-export an internal enum across a
frozen ABI and buy a permanent compatibility obligation for each one.

**A distinct code, not a reuse of `TFT_ERR_ARENA_UNAVAILABLE`.** That code means
"the arena you asked me to *create* could not be created"; this one means "the
arena you asked me to *join* is not there". Spelling both the same makes the
meaning depend on which function returned it, which is one diagnostic with two
meanings — the thing `0015`'s question 3 and `PHASE5.md` §6's `TFT017`/`TFT018`
amendment each refused. See *Open questions* 1 for the counter-argument.

### 2. The message stops being a constant

`Err(_)` becomes `Err(e)` and the `OpenError` is rendered into the bounded
`tft_error` buffer. This is a diagnostic improvement and nothing more — R5 still
forbids a caller matching on it — but "the runtime directory
`/run/user/1000/tf_tree` is on NFS" is the difference between a five-minute
diagnosis and an afternoon. `OpenError` implements `Display` already, so this is
a formatting change, not a new capability.

### 3. The new code is reachable only from a new entry point

```c
/** Join the arena named by the environment, waiting up to timeout_ns for it
 *  to appear. timeout_ns == 0 makes exactly one attempt. */
tft_status tft_tree_open_wait(uint64_t timeout_ns, tft_tree **out);
```

`tft_tree_open` keeps its current contract byte for byte, including
`TFT_ERR_INTERNAL`.

**This is the half of the decision that is about the ABI rather than about
diagnosis, and it is load-bearing.** Every minor bump this ABI has taken is
documented on `TFT_ABI_VERSION_MINOR` and every one of them rests on the same
argument: *an older caller cannot observe the change*. `3` → `4` rests on an
older caller never calling the new functions; `4` → `5` rests, more tightly, on
an older caller being unable to *express* the request that produces the new code.
Widening `tft_tree_open`'s return set has **neither** property. A `0.5` caller
already calls `tft_tree_open`, and one that wrote `if (rc == TFT_ERR_INTERNAL)`
to mean "no arena" would silently stop matching. That is a change in the meaning
of an existing observable, which §3.6 puts on the major, not the minor.

Routing the new code through a new symbol restores the `3` → `4` precedent
exactly, and it is not a workaround: the wait is the thing both in-tree consumers
hand-rolled, and `tft_tree_open_wait(0, &tree)` is the one-shot call with the
better code for a caller who does not want to wait at all. One symbol pays for
both.

`4` → `5` → **`5` → `6`**: one appended entry point and the one status code only
it can return.

## Rationale

**Why not leave it.** The cost is paid by the process least able to report it. A
consumer that starts before its publisher — the *normal* case on a robot, where
launch order is not a total order — is indistinguishable from a consumer whose
build is wrong, and the only strategy available to it is to poll until a timeout
and then say nothing useful. Both in-tree consumers already demonstrate the
shape, and one of them is a *test* whose negative assertion is weaker than it
should be because of it.

**Why not a code per cause.** `0015` already answered this for the bridge side
and the answer transfers: the granularity that has aged well in this ABI is
`TFT_ERR_BAD_CONFIG`'s — one code per *class of caller action*, with the specific
cause in the message. There are exactly two actions here (retry, or stop), so
there are exactly two codes.

**Why not simply expose `Open::await_open` and stop.** Tempting, and it is half
of the recommendation. It is not all of it, because a wait must still return
something on expiry, and if that something is `TFT_ERR_INTERNAL` the caller is
back where it started: unable to tell "I waited five seconds and the publisher
never came" from "your `$TF_TREE_NAME` has a space in it and no wait will ever
fix that". The wait needs the code; the code, for ABI reasons, wants the wait.

**Why the timeout is `uint64_t` nanoseconds and not a `struct`.**
[`API.md`](../API.md) §1 R3 makes integer nanoseconds the stamp representation
across every surface; a duration in the same units needs no new convention and no
new type in the frozen header.

**Alternatives considered.**

*Return the retryable/terminal bit through a new out-parameter on
`tft_tree_open`* (`bool *retryable`). Rejected: it is an appended parameter on an
existing symbol, which is a signature change and therefore a major bump — worse
than what it avoids.

*Document that `tft_last_error()`'s text distinguishes the cases.* Rejected by
R5, in its own words.

*Give the C++ wrapper a richer exception and leave C alone.* Rejected: the C++
wrapper is header-only over the C codes and can only be as expressive as they
are; it would have to parse the message, which is R5 again.

## Consequences

**Easier.** Both in-tree `open_within` helpers collapse into one call. A ROS
node can wait for a bridge with a budget and report a *reason* on expiry.
`ros/tf_tree_ros/test/test_shared_arena.cpp`'s negative assertion gets to assert
"the arena is absent" rather than "something went wrong".

**Harder.** One more symbol and one more code in a frozen header, both permanent.
`tft_tree_open` and `tft_tree_open_wait` are two spellings of one operation for
as long as the ABI lives, and every later change to the join path has to be made
to both or deliberately not.

**Not affected.** Python binds Rust directly (`docs/PHASE3.md` §0) and already
receives `OpenError`; it needs nothing from this. The Rust facade is the
reference for the partition and does not change.

**Invariants to maintain.** The retryable set here and `is_retryable` in
`crates/tf_tree/src/open.rs` are one decision with two spellings, and they must
be kept identical — a test that asserts the correspondence, not a comment.
`API.md` §6 gains a row when this record reaches `ready`; it does not get one
while it is `draft`, because that column names where work is authorized and a
draft authorizes none.

## Implementation plan

_Not to be started while this record is `draft`._

1. **`TFT_ERR_ARENA_ABSENT` and `tft_tree_open_wait`**, behind `shm` like
   `tft_tree_open`, with `TFT_ABI_VERSION_MINOR` 5 → 6 and its paragraph on
   `TFT_ABI_VERSION_MINOR`'s doc comment. — verified by a `tests/` case that a
   wait against no publisher returns `TFT_ERR_ARENA_ABSENT` inside the budget,
   and one that a wait against a `$TF_TREE_NAME` containing `/` returns
   terminally and *immediately* rather than after the budget.
2. **`Err(_)` → `Err(e)`** in `tft_tree_open` and the new function; the
   `OpenError` rendered into the bounded error buffer. — verified by the existing
   longest-name message test's sibling: the message must survive a maximal name
   without truncating the part that names the condition.
3. **Both `open_within` helpers deleted** in favour of the new call, in
   `ros/tf_tree_ros/test/` and `ros/tf_tree_bench_ros/src/`. — verified by
   `just ros-test`; `just dds-bench` still reports four arms at 0 % failure.
4. **The correspondence test** between the C partition and
   `tf_tree::open::is_retryable`. — verified by its own failure when either side
   is edited alone.

## Open questions

1. **A distinct code, or `TFT_ERR_ARENA_UNAVAILABLE` reused?** The *Decision*
   argues distinct. The counter-argument is real and is not addressed by the
   "one diagnostic, two meanings" objection on its own: the two conditions are
   returned by *different functions*, so a caller can already tell them apart by
   which call it made, and "the arena you wanted is not available" describes
   both. Reusing costs one fewer permanent code. What has not been checked is
   whether `tf_tree doctor` or `tf_tree top` would ever have to report both from
   one place, which is where the ambiguity would actually bite.
2. **Does `tft_tree_open` stay unchanged forever, or is `TFT_ERR_INTERNAL` on it
   deprecated at the next major?** The *Decision* leaves it alone to protect the
   minor bump. If `0.x` → `1.0` is close enough, the simpler shape — widen the
   existing function, no new symbol — becomes available and the wait can be
   argued on its own merits instead of carrying the code.
3. **Should the wait be the only new thing, with the partition exposed some other
   way?** `tft_tree_open_wait` is a *policy* in the ABI (a poll loop with this
   crate's backoff), and `0018` is on record that blocking belongs in the caller.
   `0018`'s argument is about arena primitives and a `PROT_READ` consumer's
   inability to register on a futex, so it does not obviously reach a
   userspace poll in a wrapper — but "obviously" is doing work in that sentence
   and somebody should check it before this moves to `ready`.
