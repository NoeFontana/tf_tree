# 0019: One binary, and topology you can wait for

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** **steps 1–5 have landed. Steps 6–7 — the daemon — have not,
and are not scheduled.**

The record stays `ready`, not `implemented` (the immutability lock,
[`README.md`](./README.md)), so whoever builds `serve` can amend it.

## Context

`PHASE2.md` §9 specified `tf_treed`, a daemon never built;
[`0015`](./0015-the-bridge-fills-a-shared-arena.md) lets the ROS bridge create the
arena. Ownership is a kernel-reassigned role ([`0005`](./0005-the-shared-memory-seam.md)
§8; [`0037`](./0037-a-takeover-is-not-a-second-open.md)), so liveness, reaping and
owner death need no daemon. What remains: D4 and D10 ([`0004`](./0004-builder-time-edge-declaration.md))
mean **whoever creates the arena fixes its topology and capacity permanently**,
which is what `FrameNotDeclared` reports; and `Open::new()` defaulted to
`ReadOnly` *and* `CreatePolicy::IfAbsent`.

## Decision

### 1. There is no `tf_treed` binary. The capability is `tf_tree serve`.

`tf_tree serve --config <topology.toml> [--domain <n>] [--name <n>]
[--participants 64] [--metrics-port <p>]` creates and seals the segment from the
config (the `TopologyConfig` format `tf_tree topology --discover` writes),
pre-declares frames and static edges, holds the arena open, and on `SIGTERM`
drains and exits leaving the segment alive. It **must not** publish, claim any
edge, or interpret transforms.

### 2. Startup ordering - NORMATIVE

**a. A read-only attach implies `CreatePolicy::Never`.** `ro` plus a creating
policy is a typed error at `Open::open` (`OpenError::ReadOnlyCannotCreate`), not a
silently-created empty arena (`API.md` R6).

**b. A consumer waits rather than failing, and it is two waits**, each a
caller-side loop ([`0018`](./0018-blocking-waits-belong-in-the-shim.md)):
`Open::await_open` waits for the arena, then `Tree::await_frames` waits for names
interned into it. `CreatePolicy::Never` against an absent arena fails *fast* with
`IpcError::ArenaAbsent`, so a consumer racing the publisher never obtains a `Tree`
to call a frames wait on; folding the waits would repeal that fail-fast property.
Waiting beats pre-declaring because a config is a second source of truth.

**The predicate is `ArenaView::find_frame`, and `await_frames` refuses two handles
outright.** `Tree::frame` is wrong in both modes: read-only it answers
`FrameError::ReadOnly` for an absent name; writable it **interns and succeeds
immediately**. So a writable tree is refused (`WritableTree`), and so is a frozen
`.tft` (`FrozenTree`): the poll could only burn its budget.

Both waits are a poll loop: **no arena primitive, no notification mechanism, no
futex** (`0018`). Backoff is `MIN_BACKOFF` 200 us doubling to `MAX_BACKOFF` 4 ms;
`await_open` clamps `Open::timeout` to the remaining budget.

**c. Headroom covers frames that arrive later** (`frame_headroom` /
`edge_headroom`, `PHASE7.md` §4 J3); exhaustion is a typed error naming the knob.
`FrameNotDeclared`'s message names (a) and (b).

### 3. `0015` proceeds, scoped as the ROS answer, and its three open questions are resolved here

**The bridge owns the arena when a ROS stack is the source of truth; `tf_tree
serve` owns it when nothing else is a natural owner. A deployment runs one or the
other, never both.**

1. **Sizing, and a live arena that is not this bridge's: refuse.** The bridge
   refuses to *join* at all (`Open::require_create`); `layout_hash()` is
   independent of the declared topology, so `LayoutMismatch` is only the refusal
   for a consumer whose *binary* differs. `CreatePolicy::Always` is the operator's
   explicit act.
2. **No name derivation from `tf_prefix`.** The rendezvous is namespaced by
   `(domain, name)` and `domain_from_env` falls back `TF_TREE_DOMAIN` ->
   `ROS_DOMAIN_ID`. A collision is question 1's refusal.
3. **A second bridge on a held name is a rendezvous fault, not an authority
   fault**, beside `PHASE4.md` §5.4 (two publishers on one edge), not inside it
   (`PHASE5.md` §6's `TFT017`/`TFT018` amendment). It is a startup refusal with
   its own message.

### 4. §2's three items are the first-release scope; neither daemon step blocks it.

## Consequences

- **`PHASE2.md` §9 is superseded** by `tf_tree serve`; `--lock` /
  `--socket-mode` are retired.
- **`Open::new()`'s `create` default moves to `CreatePolicy::Never`**, so a
  read-only open with `IfAbsent`/`Always` and `layout_if_creating` now fails,
  superseding [`0005`](./0005-the-shared-memory-seam.md)
  §3.2's `// DEFAULT: IfAbsent` and `PHASE2.md` §3.2. Every workspace publisher
  passes `create` explicitly.
- **`Open::await_open`, `Tree::await_frames` and `AwaitError` are stable-tier
  API** (`API.md` §7: tier 1, never on `Plan`); `AwaitError` is facade-local and
  `Copy`.

## Implementation plan

1. **A read-only attach implies `CreatePolicy::Never`.** *Landed.* The test
   **supplies `layout_if_creating`** (else `NoLayoutToCreate` makes it vacuous).
2. **`Open::await_open` and `Tree::await_frames`.** `await_open` retries only
   `ArenaAbsent` and `ArenaHeldButUnreachable`. *Landed;* tests in
   `crates/tf_tree/tests/rendezvous.rs` and `crates/tf_tree/tests/await_frames.rs`.
3. **`FrameNotDeclared`'s message and `RUNBOOK.md`'s row** name steps 1 and 2.
   *Landed.*
4. **`PHASE2.md` §9 amendment** and dependent docs. *Landed.*
5. **`0015` to `ready`.** *Landed.*
6. **`tf_tree serve`** - `--config`, pre-declaration, `SIGTERM` drain. Verified by
   a read-only consumer planning a path present only in the config, and the
   segment outliving `SIGTERM`. **Not scheduled.**
7. **`--metrics-port`** and the systemd/container examples. **Not scheduled.**

