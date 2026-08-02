# 0015: The bridge fills a shared arena

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** —

> **Moved `draft` → `ready` by
> [`0019`](./0019-one-binary-and-topology-you-can-wait-for.md), which resolves
> all three of the open questions below and scopes this record against the
> other answer to the same problem.** The *Decision* and the seven-step
> *Implementation plan* are unchanged.
>
> The scoping matters as much as the answers: **the bridge owns the arena when a
> ROS stack is the source of truth, and `tf_tree serve` owns it when nothing else
> is a natural owner.** A deployment runs one or the other, never both. Without
> that line this record and `PHASE2.md` §9 were two answers to one question, each
> written as though the other did not exist.

## Context

`docs/PHASE5.md` §9.1 specifies the benchmark artifact as *"N `tf2` consumers
versus **one bridge plus N `tf_tree` consumers**"*, and §9.2 requires **total RSS
across N consumers** as a row. Both sentences assume the N consumers are separate
processes reading what the bridge writes. They cannot be, today.

`tft_bridge_create` builds its arena with `TreeBuilder::build()` — an ordinary
**heap** arena — so nothing outside the bridge's own process can reach it. The
`BridgeHandle::tree()` accessor hands out a `tft_tree *` valid only in-process.

The consequence is not theoretical; it is the shape of the comparison that
shipped. `just dds-bench` runs three arms and **prints, above its own table on
every run**, that the fourth cannot exist:

| arm | procs | consumers | svc p50 | PSS |
|---|---|---|---|---|
| `tf2.processes` | 4 | 4 | 4.16 µs | 63.02 MiB |
| `tf2.composed` | 1 | 4 | 1.58 µs | 24.02 MiB |
| `tf_tree.composed` | 1 | 4 | 0.83 µs | 24.81 MiB |
| `tf_tree.processes` | — | — | **not measurable** | **not measurable** |

The missing row is the one the project's central claim is about.
`docs/PROJECT.md`'s argument is that tf_tree's cost is *O(1) in the number of
consumers* where `/tf` is O(consumers × edges × rate), and
`docs/PHASE5.md` §12's criterion 4 — *"16 workers sharing one `.tft`: total Pss
within 1.2× of one worker"* — calls the sharing claim "the wedge's central
claim". `crates/tf_tree_bench`'s `contended_scaling` and `mp_bench` both
demonstrate it, but they build their own shared arenas: **nothing demonstrates
it for an arena a ROS bridge filled**, which is the only way a real robot gets
one.

Phase 2 already built every mechanism this needs. `TreeBuilder::build_shared`,
`Tree::attach_shared`, `tf_tree::open()`, the rendezvous, fd passing, claims as
leases, reaping and fork poisoning all ship and are gated by `just shm-check`.
What is missing is a way to ask the bridge for one.

This is new public surface on the C ABI's §5 seam, so `CLAUDE.md` routes it here
rather than to a PR.

## Decision

**Give `tft_bridge_options` an optional arena name. When it is set, the bridge
builds a shared arena under it instead of a heap one; when it is not, nothing
changes.**

### The ABI

`tft_bridge_options` gains one field, at the end, guarded by the existing
`struct_size` prefix rule (§3.6) so a caller built against the previous header
keeps working unchanged:

```c
typedef struct {
  uint32_t struct_size;
  /* ... existing fields ... */

  /**
   * Rendezvous name for a SHARED arena, or NULL for a private heap arena.
   *
   * When non-NULL the bridge publishes its arena under this name, and any
   * process may attach read-only with tft_tree_open() / tf_tree::open().
   * NULL is the default and preserves the previous behaviour exactly.
   */
  const char *arena_name;
} tft_bridge_options;
```

`tft_bridge_create` selects `build_shared(name)` over `build()` on that field
alone. Everything downstream — the claims, the ingest pipeline, the counters,
the outcome POD — is unchanged, because a `Tree` is a `Tree`.

### The rclcpp surface

`tf_tree_ros::BridgeOptions` gains `std::string arena_name` (empty = heap), and
`BridgeNode` a `arena_name` parameter, default `""`. §5.8's three deployment
forms all inherit it.

### What a consumer does

Exactly what it already does for any shared arena — `tf_tree::open()` in Rust,
`tf_tree::Tree::open()` in C++, `tf_tree.open()` in Python. **No new consumer
API**, which is the point: the bridge becomes an ordinary producer of the arena
Phase 2 already specified, rather than a special case.

### Failure

A shared build can fail where a heap build cannot — the name is taken, the
runtime directory is unwritable, `memfd_create` is refused. Those are startup
failures and join the ones `tft_bridge_create` already reports (domain, cycle,
claim), with the existing status codes; there is no new error class and no
runtime fallback to a heap arena. **A bridge asked for a shared arena that
cannot make one must refuse to start**, because a silent downgrade would leave
every consumer waiting on a rendezvous that will never appear, which is the
failure mode hardest to diagnose from the consumer's side.

## Rationale

**Why an option rather than always shared.** A shared arena costs a `memfd`, a
rendezvous entry in the runtime directory, and a participant slot; a bridge
composed into the same process as its only consumer needs none of them, and
§5.8's form 3 exists precisely for that deployment. Making it unconditional
would also change the behaviour of every existing caller, which the
`struct_size` rule exists to avoid.

**Why the name, and not a file descriptor.** Handing back an fd would work and
is strictly more flexible, but it makes every consumer's attach path
bridge-specific: the consumer would need the bridge's fd, which means a socket,
which means the bridge grows a protocol. The rendezvous is the protocol
`docs/decisions/0005` already specified and `tf_tree::open()` already speaks.

**Why not a second bridge entry point** (`tft_bridge_create_shared`). Two
constructors that differ in one field is the shape that drifts: every later
option has to be added to both, and one of them is eventually forgotten. §3.6's
`struct_size` mechanism exists for exactly this and is already load-bearing
elsewhere in the ABI.

**Why refusing beats falling back.** A fallback is attractive — the bridge keeps
running, the robot keeps moving — and it is the wrong trade here. The consumers
are separate processes whose only signal is "the rendezvous is not there yet",
which is indistinguishable from "the bridge has not started yet". A bridge that
downgraded silently would present as a bridge that never came up, on the
consumer side, forever.

## Consequences

**Easier.** §9.1's comparison becomes complete: `dds_bench` grows a
`tf_tree.processes` arm and `dds_report`'s `MISSING_ARM` sentence is deleted
rather than reworded. §9.2's *total RSS across N consumers* row becomes
measurable for a bridge-filled arena, and §12 criterion 4's claim becomes
demonstrable on the online path and not only the frozen one. A robot can run one
bridge and N nodes without composing them into one process.

**Harder.** The bridge acquires a failure mode at startup it did not have, and
an operational surface — a name that two bridges can collide on. `tf_tree
doctor` and `tf_tree top` gain a participant they did not previously see, which
is a *benefit* for diagnosis and a change in their output that their tests pin.

**Invariants to maintain.** The bridge remains the single writer of every edge it
claims; consumers attach **read-only** and the ABI must not grow a way for them
not to. Fork poisoning, reaping and the claim leases apply to the bridge exactly
as to any other participant, and the bridge's ingest thread is the one that
created the arena — so `docs/decisions/0005` step 9's `atfork` rules apply to it
unchanged and must be tested, not assumed.

## Implementation plan

1. **`tft_bridge_options.arena_name`**, `struct_size`-guarded, defaulting to
   NULL; `tft_bridge_create` branches to `build_shared`. — verified by a new
   `crates/tf_tree_c/tests/bridge.rs` case asserting a caller passing the
   *previous* `struct_size` still gets a heap arena, plus the existing 52 cases
   staying green.
2. **Refuse rather than downgrade**: a shared build failure is a startup status,
   with no heap fallback. — verified by a test that creates a bridge under a name
   already held and asserts the create fails and no arena is published.
3. **`tf_tree_ros::BridgeOptions::arena_name` and the `arena_name` node
   parameter**, wired through §5.8's three forms. — verified by
   `ros/tf_tree_ros/test/test_node.cpp` gaining a parameter case; `just ros-test`.
4. **A second process attaches to a bridge-filled arena and reads what the
   bridge wrote.** — verified by a new ctest that spawns the shipped
   `tf_tree_bridge` executable, publishes `/tf`, attaches with `tft_tree_open`
   and asserts a lookup matches; `just ros-test`.
5. **`dds_bench` grows a `tf_tree.processes` arm**; `bench_consumer` gains
   `--mode tf_tree_attach` that calls `tf_tree::Tree::open()` instead of hosting
   a bridge. — verified by `just dds-bench` reporting four arms at 0 % failure.
6. **Delete `dds_report::MISSING_ARM`** and the paragraph in
   `docs/benchmarks/tf2.md` it mirrors, replacing both with the measured row. —
   verified by the `crates/tf_tree_bench` test that pins the report's required
   sections.
7. **Update `docs/PHASE4.md` §5.8 and `docs/PHASE5.md` §0.0's §9 row.** —
   verified by review.

## Open questions

**All three are resolved by
[`0019`](./0019-one-binary-and-topology-you-can-wait-for.md) §3, which is why
this record is `ready`.** They are kept below as written, with the answer under
each, because the reasoning that produced the question is worth more than the
answer alone.

> **1 — Resolved: refuse.** The leaning below was right. A live arena under this
> name with a different `layout_hash` is a startup refusal; `LayoutMismatch`
> already exists as an attach error naming both values, and `CreatePolicy::Always`
> is the operator's explicit act and already documents itself as "never take this
> path automatically". That adding an edge restarts every participant is stated
> rather than engineered around — it is D4 and `0004` being what they are.
>
> **2 — Resolved: no derivation.** The rendezvous is already namespaced by
> `(domain, name)`, and `domain_from_env` falls back `TF_TREE_DOMAIN` →
> `ROS_DOMAIN_ID` — precisely the convention two robots on one host already use.
> So the collision this question worried about is already handled one layer down,
> and deriving from `tf_prefix` would both couple what `PHASE4.md` §5.6 keeps
> apart and make the name unguessable for the operator who has to attach to it.
>
> **3 — Resolved: beside §5.4, not inside it.** A second bridge on a held name is
> a *rendezvous* fault; §5.4 is about two publishers on one *edge*, with per-edge
> attribution. Folding them together would give one diagnostic two meanings —
> the error `PHASE5.md` §6's `TFT017`/`TFT018` amendment refused when it declined
> to reuse an existing id.

1. **Who sizes the arena, and can it be resized without a restart?** The
   topology config fixes capacity at build time (`0004`, D4: fixed capacity, no
   growth), so a shared bridge arena is sized by the same file. That is
   consistent, but it means adding an edge to the config is a restart *of every
   consumer*, not just of the bridge — the arena is a new `memfd` and the old
   mapping is stale. Phase 2's rendezvous has an instance UUID for exactly this;
   what is unresolved is whether the bridge should refuse to start when a live
   arena with the same name has a different `layout_hash`, or replace it and let
   the reapers clean up. **Leaning: refuse, and make `--force` the operator's
   explicit act.**

2. **Does the bridge need `--arena-name` to imply anything about `tf_prefix`?**
   Two robots on one host each running a bridge will collide on a default name.
   A name derived from `tf_prefix` would avoid it automatically and would also
   couple two things §5.6 keeps separate. **Leaning: no derivation, and a
   collision is the refusal in question 1 — but this needs a look at what
   `docs/PHASE2.md` §3.3's rendezvous already does about namespacing.**

3. **Should `Strict` authority interact with a shared arena?** A second bridge
   attaching to a name already held is a different fault from two publishers on
   one edge, and it is not obvious whether it belongs in §5.4's conflict
   machinery or beside it. Probably beside it, but the diagnostic wording should
   be settled before the code is.
