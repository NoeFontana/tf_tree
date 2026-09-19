# 0045: the spin that can starve what it waits for

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** *Implementation plan* below — five steps. **Decided
2026-09-19, on the owner's delegation**: both halves are taken, the yield is
`wait_for_publish`'s alone, and A8's invariant is untouched by the bound.


## Context

`FrameTable::wait_for_publish`, the handshake a name resolution goes through when
another participant is mid-intern, ends in an unconditional `spin()`, and
`sync::spin` is `core::hint::spin_loop()` with no `sched_yield`.
`INTERN_SPIN_LIMIT`'s doc says the bound is a *liveness-poll interval*, not a
timeout: a claimant the predicate reports alive is waited on **without limit**.

Amendment **A8** covers a claimant that dies (proven dead, entry taken over). It
did not consider one that is **neither running nor dead**: `SIGSTOP`, a debugger,
a frozen cgroup, a paused container. Its record reads `LIVE`, `/proc` reports it,
the OFD byte is held, and it will not publish until resumed. Two consequences:

- *The wait is unbounded.* `Tree::lookup`, `tft_plan_create` and `tree.plan()` all
  reach it and none documents a wait.
- *The spin can prevent the resume it waits for.* On a shared core (a Jetson-class
  part, a two-thread `cpuset`, an `isolcpus`-pinned RT consumer) a higher-priority
  spinning waiter stops the claimant being scheduled: priority inversion. Every
  other spin in the engine waits on a **store** a running peer is instructions
  from making; this one waits on a peer that may not be running.

`Tree::reparent` already answers this for the topology byte
([`0029`](./0029-the-topology-lock-is-a-kernel-lock.md)):
`Tree::take_topology_lease` polls `TOPO_BYTE_ATTEMPTS` times and returns
`ReparentError::LockContended`, because a `SIGSTOP`ped holder "is alive and D17
forbids distinguishing by timeout". Interning has no byte, and still spins.

## Decision

Two separable changes; the second is a protocol change.

### 1. The spin yields

The waiter yields after a short pure-spin prefix. The yield reaches
`tf_tree_core` as a **passthrough feature**, defaulted off in `tf_tree_core` and
enabled unconditionally in `tf_tree`'s `[dependencies]`, as `counters`,
`pure-hash` and `crash-points` are wired (`crates/tf_tree/Cargo.toml`): every
shipped `tf_tree` user gets it, and a bare-metal `tf_tree_core`-only consumer stays
`no_std` and pure-spins. It costs no public surface; a hook parameter on
`ArenaView` would change `pub` items (`InternTable`, `intern_core`, `find_core`)
and break `API.md` §7 and `0.0.x`.

### 2. The wait is bounded

Both roles count **every** liveness round (not only `CLAIM_UNRECORDED` /
`CLAIM_ANONYMOUS`) and return `Wait::Contended` past a limit, surfacing as the
existing `FrameError::InternContended`. A8 constrains *takeover* ("a slow interner
is never stolen from"), and a refusal steals nothing; "without limit" is
`INTERN_SPIN_LIMIT`'s doc comment, not amendment text. A caller can now be told
"someone holds this name and I cannot say when" instead of waiting.

## Rationale

- **Yield alone** fixes starvation, not unboundedness: a stopped claimant with
  spare cores still never publishes.
- **Not a kernel lock:** interning is per name; there is no byte, and one would
  mean a byte per hash slot. `0029` worked because A2's lock is one arena-wide word.
- **`Wait::Contended`, not a longer spin:** a larger limit moves the failure;
  only a typed refusal separates "taking a while" from "will not finish".
- **Round count, not duration:** `tf_tree_core` is `no_std` with no clock (D14).

## Consequences

- **`Tree::await_frames` is broken by the bound unless changed** (step 5): it maps
  every `find_frame` error to terminal `AwaitError::Frame`, on the reasoning that
  it "will not change on its own", false for a claimant that resumes on
  `SIGCONT`. The core answers; the layer owning a deadline retries
  `InternContended` within its own timeout.
- `Tree::lookup`, `tft_plan_create` and `tree.plan()` gain a failure mode; their
  docs must say so. On `Tree::lookup` it arrives as `LookupError::UnknownFrame {
  hash }`, because the facade's resolver (`find` in `crates/tf_tree/src/tree.rs`)
  maps every `find_frame` error to it; `# Errors` names a write-free way to tell
  the cases apart. A distinct variant is a record of its own.
- **A8's text does not change.** `docs/PHASE2.md` §1 gains a note, as §3.5's
  amendment did, that a claimant neither running nor dead is a class A8 did not
  consider.
- The `loom` models gain a reachable `Contended` arm; the new bound needs the same
  `cfg(loom)` shrink as `INTERN_SPIN_LIMIT` (2) and its own control.
- **`just bench-check` cannot measure the yield:** no gated bench reaches
  `wait_for_publish` (`benches/lookup.rs` resolves frames in setup), and the yield
  fires only after `INTERN_SPIN_LIMIT` iterations on a claimed-but-unpublished
  slot. Step 1 owes a bench reaching the contended path.

## Implementation plan

1. **`spin` splits; the yield arrives through the passthrough feature.**
   `sync::spin` stays pure for the four store-waiters (`buffer::read_slot`,
   `plan.rs`'s two generation retries, `topology.rs`'s A2 acquire); a second
   function, used by `frame::wait_for_publish` alone, yields after a pure-spin
   prefix, and pure-spins with the feature off. Both keep yielding under
   `cfg(loom)`.
   - **The prefix is per liveness round and must be well under
     `INTERN_SPIN_LIMIT`.** `wait_for_publish` resets its spin counter every round;
     a prefix at or above the limit means the yield **never fires**, and
     lengthening it to chase a `bench-check` regression would silently restore the
     inversion with every gate green. Per round, the prefix is re-spent each
     boundary, entering the scheduler less often than a per-wait prefix.
   - **Verified by** `just bench-check`, `just loom`, existing frame tests
     unchanged, and **a test that observes a yield on a `tf_tree` tree** (proving
     the feature is on is not proving the yield runs).
   - **Owed: a `no_std` compile gate for `tf_tree_core`.** Nothing has one:
     `stable-tier-check` compiles `-p tf_tree`, and the closest pass
     (`clippy -p tf_tree_core --no-default-features --features crash-points`)
     pulls in `std`.
   - A hosted direct consumer of `tf_tree_core` (one of the five publishing
     crates) keeps the unbounded non-yielding spin unless it installs the feature.
2. **Both roles count every liveness round; past N, `Wait::Contended` ->
   `FrameError::InternContended`.** N = 8 (see *Open questions* 2). **Report the
   measured round cost (spin and probe syscalls) on both gated architectures.**
   The measurement can falsify the premises only: if a round costs far more than
   ~0.4 ms because the `/proc` fallback dominates, N must come down toward the
   floor; if the healthy-intern margin is under two orders of magnitude, the
   floor's justification is wrong.
   - **Rewrite `a_claimant_that_cannot_be_proven_dead_is_never_stolen_from`.** It
     asserts the waiter is still blocked after 250 ms (`rx.recv_timeout(250ms)`),
     false by design under the bound (and its message would misdiagnose the
     refusal), and its later `rx.recv().unwrap().unwrap_err() ==
     FrameHashCollision` would **hang** because the spawned interner already
     returned. Assert "never stolen from" structurally (`claiming[i]` still names
     the original claimant, no id published), which holds under any N. Do not
     inflate N (~625) to keep the timeout.
   - **Verified by** that control plus a new test staging a claimant that reads
     alive and never publishes, asserting `InternContended` inside the bound.
3. **Docs.** `docs/PHASE2.md` §1 gains the A8 note; `Tree::lookup`, the C entry
   point and the Python method document the refusal. **Eleven shipped sites say
   the wrong thing** for the new producer (anonymous / "transient - retry" /
   "may have died"): `FrameError::InternContended`'s variant doc and `Display`;
   `LookupError::UnknownFrame`; `find_core`'s and `ArenaView::find_frame`'s
   `# Errors`; `Tree::lookup`'s and `Tree::frame`'s `# Errors`; `intern_core`'s
   `# Errors` (silent on the variant); `AwaitError::Frame`'s doc;
   `tf_tree_py`'s `unresolvable_name` ("... Retry", which relocates the unbounded
   wait into the caller); and `TFT_ERR_UNKNOWN_FRAME` in
   `crates/tf_tree_c/include/tf_tree.h` (generated: `cargo xtask headers`,
   gated by `just c-header-check`). A distinct `Copy` identifier for "claimant
   will not progress" was costed and not taken (new public surface, `API.md` §7).
4. **A `loom` model reaches the bound**, with a control that fails when it is
   unreachable, and a second control that fails when the reader's
   `CLAIM_UNRECORDED` abandon path is unreachable (what an N shrunk below
   `READER_UNRECORDED_ROUNDS` would cause).
5. **`Tree::await_frames` retries `InternContended` within its own timeout**, and
   `AwaitError::Frame`'s doc stops saying the error "will not change on its own".
   **Verified by** a test staging a contended name and asserting `await_frames`
   waits to *its* deadline, and existing await tests unchanged.

## Open questions

All three answered 2026-09-19 on the owner's delegation.

1. **ANSWERED: yes, and A8's invariant is untouched.** A8 (*"Interning must not
   spin forever on a **dead** claimant"*) is about a claimant that dies at
   `intern.after_hash_cas_before_id_store`, and its safety property is "a slow
   interner is never stolen from". Returning `Wait::Contended` CASes nothing and
   writes no record, so the property holds. What is owed is the note in step 3.
2. **ANSWERED as a rule, not a number: N = 8.**
   - **Ceiling:** the refusal must arrive while a control loop can absorb it as a
     dropped cycle: single-digit milliseconds on x86. Not "inside one period",
     which the floor alone (N >= 5, ~2 ms) already exceeds at 1 kHz.
   - **Floor (structural):** N > `READER_UNRECORDED_ROUNDS` (4). A reader waits
     that many rounds on `CLAIM_UNRECORDED` before concluding a name is absent,
     since one round made `find_frame` report a live in-flight name as missing.
     A global bound N <= 4 would fire first and answer `InternContended` where
     the answer is *not there*. The two constants move together.
   - N = 8 leaves room for the floor to grow, ~3.2 ms of spinning on x86 (one
     `INTERN_SPIN_LIMIT` round = 10 000 spins, ~0.4 ms at 3 GHz, three orders
     above a healthy intern, so health does not constrain N).
   - **Probes are syscalls not counted in N x `INTERN_SPIN_LIMIT`:** each round
     calls `claimant_alive` (`F_OFD_GETLK`, possibly a `/proc/<pid>/stat` read of
     hundreds of microseconds on a loaded host). Step 2 measures the round cost.
   - **`loom` cannot simply shrink N:** `INTERN_SPIN_LIMIT` is 2 under
     `cfg(loom)` but `READER_UNRECORDED_ROUNDS` has no `cfg`; both shrink
     together or neither does (step 4's control).
   - **aarch64:** `pause` is ~140 cycles on recent x86-64 and `isb` tens on
     aarch64, so a round is several times shorter there. The ceiling gains
     headroom; the health margin erodes from ~three orders to ~two (~0.4 ->
     ~0.05 ms). The floor counts rounds, so it is untouched.
3. **ANSWERED: only here; `spin` splits.** Four of `sync::spin`'s five production
   call sites wait on a store a running peer is about to make or a decision they
   reach themselves (`read_slot`'s seqlock bounded by `SEQ_RETRY_LIMIT`, A2's
   acquire by `TOPO_LOCK_SPIN_LIMIT`, `plan.rs`'s generation retries); yielding
   there trades a sub-microsecond wait for a scheduler round trip on the hot read
   path. Only `frame::wait_for_publish` waits on whether a peer is **scheduled**.
   The `loom` arm already yields for all five, for interleaving, and must stay so.
