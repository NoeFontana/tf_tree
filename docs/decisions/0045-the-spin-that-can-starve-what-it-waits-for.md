# 0045: the spin that can starve what it waits for

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** *Implementation plan* below — five steps. **Decided
2026-09-19, on the owner's delegation**: both halves are taken, the yield is
`wait_for_publish`'s alone, and A8's invariant is untouched by the bound.

## Context

`FrameTable::wait_for_publish`, the handshake a name resolution goes through when
another participant is mid-intern, ends in an unconditional `spin()`
(`core::hint::spin_loop()`, no `sched_yield`). A claimant the predicate reports
alive is waited on **without limit**. Amendment **A8** covers a claimant that
dies, not one that is **neither running nor dead** (`SIGSTOP`, a debugger, a
frozen cgroup). Two consequences:

- *The wait is unbounded.* `Tree::lookup`, `tft_plan_create` and `tree.plan()` all
  reach it and none documents a wait.
- *The spin can prevent the resume it waits for* (priority inversion on a shared core).

Bounds are in rounds, not duration: `tf_tree_core` is `no_std` with no clock (D14).

## Decision

### 1. The spin yields

The waiter yields after a short pure-spin prefix. The yield reaches
`tf_tree_core` as a **passthrough feature**, defaulted off there and enabled
unconditionally in `tf_tree`'s `[dependencies]` (as `counters`, `pure-hash` and
`crash-points` are wired), so a bare-metal `tf_tree_core`-only consumer stays
`no_std` and pure-spins.

### 2. The wait is bounded

Both roles count **every** liveness round and return `Wait::Contended` past a
limit, surfacing as the existing `FrameError::InternContended`. A8 constrains
*takeover*; a refusal steals nothing.

## Consequences

- **`Tree::await_frames` is broken by the bound unless changed** (step 5): it maps
  every `find_frame` error to terminal `AwaitError::Frame`. The layer owning a
  deadline retries `InternContended` within its own timeout.
- `Tree::lookup`, `tft_plan_create` and `tree.plan()` gain a failure mode and
  must document it; on `Tree::lookup` it arrives as `LookupError::UnknownFrame {
  hash }`. A distinct variant is a record of its own.
- **A8's text does not change.** `docs/PHASE2.md` §1 gains a note that a claimant
  neither running nor dead is a class A8 did not consider.
- The `loom` models gain a reachable `Contended` arm; the new bound needs the same
  `cfg(loom)` shrink as `INTERN_SPIN_LIMIT` (2) and its own control.

## Implementation plan

1. **`spin` splits; the yield arrives through the passthrough feature.**
   `sync::spin` stays pure for the four store-waiters (`buffer::read_slot`,
   `plan.rs`'s two generation retries, `topology.rs`'s A2 acquire); a second
   function, used by `frame::wait_for_publish` alone, yields after a pure-spin
   prefix. Both keep yielding under `cfg(loom)`.
   - **The prefix is per liveness round and must be well under
     `INTERN_SPIN_LIMIT`**, or the yield never fires.
   - **Verified by** `just bench-check`, `just loom`, and a test that observes a
     yield on a `tf_tree` tree.
2. **Both roles count every liveness round; past N, `Wait::Contended` ->
   `FrameError::InternContended`.** N = 8 (*Open questions* 2). Report the measured
   round cost on both gated architectures.
   - **Rewrite `a_claimant_that_cannot_be_proven_dead_is_never_stolen_from`**: its
     250 ms `recv_timeout` is false by design under the bound and its later
     `recv()` would hang. Assert "never stolen from" structurally (`claiming[i]`
     still names the original claimant, no id published). Do not inflate N.
   - **Verified by** that control plus a new test with a claimant that reads alive
     and never publishes, asserting `InternContended`.
3. **Docs.** `docs/PHASE2.md` §1 gains the A8 note; `Tree::lookup`, the C entry
   point and the Python method document the refusal. Eleven shipped sites say the
   wrong thing for the new producer, among them `FrameError::InternContended`'s
   doc and `Display`, `LookupError::UnknownFrame`, the `# Errors` of `find_core`,
   `ArenaView::find_frame`, `Tree::lookup`, `Tree::frame` and `intern_core`,
   `AwaitError::Frame`, `tf_tree_py`'s `unresolvable_name`, and
   `TFT_ERR_UNKNOWN_FRAME` in `crates/tf_tree_c/include/tf_tree.h` (generated:
   `cargo xtask headers`, gated by `just c-header-check`).
4. **A `loom` model reaches the bound**, with a control that fails when it is
   unreachable, and a second that fails when the reader's `CLAIM_UNRECORDED`
   abandon path is unreachable.
5. **`Tree::await_frames` retries `InternContended` within its own timeout**, and
   `AwaitError::Frame`'s doc stops saying the error "will not change on its own".
   **Verified by** a test asserting `await_frames` waits to *its* deadline.

## Open questions

1. **ANSWERED: yes, and A8's invariant is untouched.** A8's safety property is
   "a slow interner is never stolen from"; `Wait::Contended` CASes nothing and
   writes no record. What is owed is the note in step 3.
2. **ANSWERED as a rule, not a number: N = 8.**
   - **Ceiling:** single-digit milliseconds on x86, so a control loop can absorb
     the refusal as a dropped cycle.
   - **Floor (structural):** N > `READER_UNRECORDED_ROUNDS` (4), or the global
     bound would answer `InternContended` where the answer is *not there*. The two
     constants move together.
   - **`loom`:** `INTERN_SPIN_LIMIT` is 2 under `cfg(loom)` but
     `READER_UNRECORDED_ROUNDS` has no `cfg`; both shrink together or neither.
3. **ANSWERED: only here; `spin` splits.** The other four call sites wait on a
   store a running peer is about to make; yielding there taxes the hot read path.
   The `loom` arm already yields for all five and must stay so.
