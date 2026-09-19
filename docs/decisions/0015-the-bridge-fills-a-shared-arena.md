# 0015: The bridge fills a shared arena

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** **all eight numbered steps have landed**, across four PRs.

Steps 0-7 landed (#139, #141-#143, #146); step 7's `PHASE4.md` §5.8 half in the
last PR. The record stays `ready`, not `implemented` (which is the folder's
immutability lock, [`README.md`](./README.md) *Gates*), for one owed item:
`docs/PHASE5.md` §9.2's *Scaling curve, N = 1...16* row for the new arm.
`ros/dds_bench.sh` defaults `CONSUMERS` to 4, the only value
`tf_tree.processes` has run at.

Moved `draft` -> `ready` by [`0019`](./0019-one-binary-and-topology-you-can-wait-for.md),
which resolved the three open questions and scoped this record: **the bridge owns
the arena when a ROS stack is the source of truth; `tf_tree serve` owns it when
nothing else is a natural owner. A deployment runs one or the other, never both.**

## Context

`docs/PHASE5.md` §9.1 compares N `tf2` consumers with **one bridge plus N
`tf_tree` consumers**, and §9.2 requires total RSS across N consumers. Those must
be separate processes reading what the bridge writes, but `tft_bridge_create`
builds a **heap** arena via `TreeBuilder::build()`, reachable only in-process, so
`just dds-bench` has a `tf_tree.processes` row that is "not measurable". That row
is the one the project's O(1)-in-consumers claim is about; nothing demonstrates it
for an arena a ROS bridge filled. Phase 2 already ships every mechanism
(`build_shared`, `attach_shared`, `tf_tree::open()`, rendezvous, fd passing,
leases, reaping, fork poisoning); what is missing is a way to ask the bridge for
one. New public surface on the C ABI's §5 seam, so a record.

## Decision

**Give `tft_bridge_options` an optional arena name. When set, the bridge builds a
shared arena under it instead of a heap one; when not, nothing changes.**

### The ABI

`tft_bridge_options` gains one field, at the end.

The `struct_size` prefix rule (§3.6) is implemented for one struct only,
`tft_bridge_sample`; `tft_bridge_create` validates `tft_bridge_options` by **exact
equality** (`crates/tf_tree_c/src/bridge.rs:890-893`) and reads the whole struct.
`tft_bridge_outcome`, `_remap` and `_stats` stay exact-equality (they are `out`
parameters). So step 1 *ports* the rule: a `tft_bridge_options_v1` shadow struct
with offset assertions and a `read_options` that narrows the copy to the declared
size. **Relaxing `!=` without narrowing the read is an out-of-bounds read**;
`tft_bridge_create`'s safety contract widens to "at least that many readable
bytes", as `tft_bridge_offer`'s does.

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

`build_shared(name)` alone cannot do this: it publishes **no rendezvous**
(`crates/tf_tree/src/tree.rs:373-376`: the name is a debug label). The path that
publishes is `Open::open`'s `Created`/`TookOver` arm
(`crates/tf_tree/src/open.rs:337-345`), which is `build_shared` plus
`use_ofd_liveness`, `use_claim_leases`, `spawn_owner_server` and
`hold_ownership`. So:

```rust
Open::new().name(arena_name)?
    .mode(AttachMode::ReadWrite)
    .create(CreatePolicy::IfAbsent)
    .require_create(true)                              // see *Failure*
    .layout_if_creating(ingest.declared().builder())
    .open()
```

`layout_if_creating` preserves §5.6: the builder comes from `ingest.declared()`,
never `config`, so a `tf_prefix`-rewritten topology sizes the arena.

`arena_name` is the rendezvous *name*. The rendezvous *domain* is not
`tft_bridge_options.domain` (§5.5's *time* domain): it is `$TF_TREE_DOMAIN`, else
`$ROS_DOMAIN_ID`, else 0 ([`0019`](./0019-one-binary-and-topology-you-can-wait-for.md)
§3, question 2), so nothing is derived from `tf_prefix`. Two "domain" fields
meaning different things is a documentation obligation.

### The rclcpp surface

`tf_tree_ros::BridgeOptions` gains `std::string arena_name` (empty = heap) and
`BridgeNode` an `arena_name` parameter, default `""`. Form 3 (no `BridgeNode`, no
parameters) inherits the *field* only, so `test_shared_arena.cpp` asserts both
paths. One rule at the parameter layer: an `arena_name` that is entirely
whitespace or has leading/trailing whitespace is **refused, not trimmed** (`""` and
`" "` are opposite instructions to the bridge; `" foo"` and `"foo"` are different
rendezvous, and neither shows in a log). Every other malformed name is
`tf_tree_ipc::ArenaName`'s to refuse, arriving as a `BridgeError` naming it.
`BridgeHandle` maps `""` to a NULL `arena_name`.

### What a consumer does

`tf_tree::open()` / `Tree::open()` / `tf_tree.open()` as for any shared arena. No
new consumer API.

### Failure

A shared build can fail where a heap build cannot (name taken, runtime directory
unwritable, `memfd_create` refused). Nothing existing means these
(`TFT_ERR_BAD_CONFIG` is topology text, `TFT_ERR_TIME_DOMAIN` is §5.5), and
collapsing onto `TFT_ERR_INTERNAL` - what `tft_tree_open` does today - leaves an
operator unable to tell "another bridge holds this name" from "the runtime
directory is on NFS" from "a bug". So **`TFT_ERR_ARENA_UNAVAILABLE`**: one code,
specific `tft_error` message, `TFT_ERR_BAD_CONFIG`'s granularity. A **minor bump**
under §3.6, on `TFT_ERR_BAD_STAMP`'s precedent, tighter here because it is
reachable only when `arena_name` is non-NULL, which a previous-layout caller
cannot set.

`CreatePolicy` has no "create, or refuse if one is live": with `IfAbsent` a second
bridge would join read-write and claim edges in another's arena (`0019` §3
question 3). `Never` forbids creating; `Always` is `--force-new`. Hence
`Open::require_create(bool)` + `OpenError::ArenaAlreadyLive`.

A `bridge`-without-`shm` build must **refuse, not ignore**: it carries
`arena_name` with no `tf_tree::Open` behind it and must return
`TFT_ERR_ARENA_UNAVAILABLE` naming the missing feature. `just shm-check` gains the
`--features bridge,shm` lines.

There is no runtime fallback to a heap arena: consumers see only "the rendezvous
is not there yet", so a silent downgrade presents as a bridge that never came up.

## Rationale

- **Option, not always shared:** a shared arena costs a `memfd`, a runtime-dir
  entry and a participant slot; §5.8's form 3 needs none, and unconditional would
  change every existing caller.
- **Name, not fd:** an fd makes every consumer's attach bridge-specific and grows
  the bridge a protocol; the rendezvous is `0005`'s and `tf_tree::open()` speaks it.
- **Not a second entry point:** two constructors differing in one field drift;
  `struct_size` exists for this.

## Consequences

`dds_bench` gains the `tf_tree.processes` arm and `MISSING_ARM` is deleted.
The bridge gains a startup failure mode and a name two bridges can collide on;
`tf_tree doctor` / `top` see a new participant (their tests pin the output).

**Invariants to maintain.** The bridge remains the single writer of every edge it
claims; consumers attach **read-only** and the ABI must not grow a way for them
not to. Fork poisoning, reaping and claim leases apply as to any participant, and
`0005` step 9's `atfork` rules apply unchanged and are tested, not assumed:

- `crates/tf_tree_bench/src/bin/fork_child.rs` has a fourth mode (behind an
  optional `tf_tree_c = { features = ["bridge", "shm"] }` edge on
  `tf_tree_bench`, landed #146) forking with `libc::fork`; it cannot live in
  `tf_tree_c`, which would add a second real `fork()` and `libc` to the C ABI
  (`0007` budget questions).
- The Rust-level claim is covered by `0017` step 4's `owned` mode. This mode
  covers the C layer: `tft_bridge_offer`, `tft_bridge_get_stats` and
  `tft_bridge_free` on an inherited handle in a forked child **come back at all**
  (no `SIGSEGV`, no `abort()`), and the parent's bridge still applies an offer
  and its arena is readable from a third process after the child exits.
- The panic guard is *not* the mechanism: `OwnedWriter::push` returns
  `Err(PushError::ChildDetached)`, which `publisher::map::push` maps. Only
  `tft_bridge_get_stats` returns the code directly; `tft_bridge_free` returns
  `void`; `tft_bridge_offer` returns `TFT_OK` with the detachment on the
  *outcome* (`action = TFT_BRIDGE_REJECTED`, `out.status =
  TFT_ERR_CHILD_DETACHED`).

## Implementation plan

0. **`Open::require_create` + `OpenError::ArenaAlreadyLive`** (landed with `0019`
   step 1). Verified by `just shm-rendezvous`: a second `require_create(true)`
   open against a live arena fails and leaves the first serving.
1. **Port the `struct_size` prefix rule to `tft_bridge_options`** (shadow struct,
   offset assertions, narrowing `read_options`, deleting the whole-struct
   `read_unaligned`), append `arena_name` defaulting to NULL, branch
   `tft_bridge_create` to the `Open` path. Verified by a
   `crates/tf_tree_c/tests/bridge.rs` case that the *previous* `struct_size` still
   gets a heap arena.
2. **Refuse rather than downgrade**, no heap fallback. Verified by creating a
   bridge under a held name and asserting the create fails and nothing is
   published.
3. **`BridgeOptions::arena_name` and the `arena_name` node parameter**, through
   §5.8's three forms. Verified by `test_node.cpp`; `just ros-test`.
4. **The ROS parameter reaches `tft_bridge_options::arena_name`.**
   `a_second_process_reads_what_the_bridge_wrote` in
   `crates/tf_tree_c/tests/bridge_shared.rs` already covers a second process, so
   `ros/tf_tree_ros/test/test_shared_arena.cpp` is a *comparison* (same node,
   topology and attach, without the parameter nothing is findable, with it the
   attach reads the static edge), plus form 3 through `BridgeOptions`, plus the
   held-name refusal as a `BridgeError`. A pure "attach and assert" test would
   pass against an implementation that publishes unconditionally.
   Prerequisite: `tf_tree.h` hides `tft_tree_open` behind `TFT_HAVE_SHM`, so
   `crates/tf_tree_c/CMakeLists.txt` probes **each** resolved library (`.a` and
   `.so` separately) with `nm` and propagates a per-target `TFT_HAVE_SHM=1`
   through `tf_treeConfig.cmake.in`; `ros/build.sh` builds `--features
   bridge,shm` and checks one symbol per feature.
5. **`dds_bench` grows a `tf_tree.processes` arm**; `bench_consumer --mode
   tf_tree_attach` calls `tf_tree::Tree::open()`. Verified by `just dds-bench`
   reporting four arms at 0 % failure.
6. **Delete `dds_report::MISSING_ARM`** and its mirrored paragraph in
   `docs/benchmarks/tf2.md`, adding the pin nothing had
   (`crates/tf_tree_bench/tests/dds_report_aggregate.rs`: four arm labels and no
   `NOT MEASURED`).
7. **Update `docs/PHASE4.md` §5.8 and `docs/PHASE5.md` §0.0's §9 row.**

## Open questions

All three are resolved by [`0019`](./0019-one-binary-and-topology-you-can-wait-for.md) §3.

1. **Resolved: refuse.** A live arena under this name with a different
   `layout_hash` is a startup refusal (`LayoutMismatch` names both values);
   `CreatePolicy::Always` is the operator's explicit act. Adding an edge restarts
   every participant (D4, `0004`).
2. **Resolved: no derivation.** The rendezvous is namespaced by `(domain, name)`
   and `domain_from_env` falls back `TF_TREE_DOMAIN` -> `ROS_DOMAIN_ID`;
   deriving from `tf_prefix` would couple what `PHASE4.md` §5.6 keeps apart.
3. **Resolved: beside §5.4, not inside it.** A second bridge on a held name is a
   *rendezvous* fault; §5.4 is two publishers on one *edge*. Folding them gives
   one diagnostic two meanings, which `PHASE5.md` §6's `TFT017`/`TFT018`
   amendment refused.
