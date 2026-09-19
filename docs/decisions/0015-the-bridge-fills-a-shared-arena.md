# 0015: The bridge fills a shared arena

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** **all eight numbered steps have landed**, across four PRs.

The record stays `ready`, not `implemented` (the folder's
immutability lock, [`README.md`](./README.md) *Gates*), for one owed item:
`docs/PHASE5.md` §9.2's *Scaling curve, N = 1...16* row for the new arm
(`ros/dds_bench.sh` defaults `CONSUMERS` to 4).

Scope ([`0019`](./0019-one-binary-and-topology-you-can-wait-for.md)): **the bridge
owns the arena when a ROS stack is the source of truth; `tf_tree serve` owns it
when nothing else is. A deployment runs one or the other, never both.**

## Context

`docs/PHASE5.md` §9.1/§9.2 need N `tf_tree` consumers in separate processes
reading what the bridge writes, but `tft_bridge_create` builds a **heap** arena,
reachable only in-process.

## Decision

**Give `tft_bridge_options` an optional arena name. When set, the bridge builds a
shared arena under it instead of a heap one; when not, nothing changes.**

### The ABI

`tft_bridge_options` gains `const char *arena_name` at the end: the rendezvous
name for a SHARED arena, or NULL for a private heap arena (previous behaviour).
Step 1 *ports* the §3.6 `struct_size` prefix rule to it (`tft_bridge_create` had
validated by exact equality): a `tft_bridge_options_v1` shadow struct with offset
assertions and a `read_options` that narrows the copy to the declared size.
**Relaxing the equality check without narrowing the read is an out-of-bounds
read**; `tft_bridge_create`'s safety contract widens to "at least that many
readable bytes".

`build_shared(name)` alone publishes **no rendezvous**; `Open::open`'s `Created`
arm does. So:

```rust
Open::new().name(arena_name)?
    .mode(AttachMode::ReadWrite)
    .create(CreatePolicy::IfAbsent)
    .require_create(true)                              // see *Failure*
    .layout_if_creating(ingest.declared().builder())
    .open()
```

The builder comes from `ingest.declared()`, never `config` (§5.6). The rendezvous *domain* is not `tft_bridge_options.domain` (§5.5's *time* domain):
it is `$TF_TREE_DOMAIN`, else `$ROS_DOMAIN_ID`, else 0
([`0019`](./0019-one-binary-and-topology-you-can-wait-for.md) §3, question 2).

### The rclcpp surface

`tf_tree_ros::BridgeOptions` gains `std::string arena_name` (empty = heap) and
`BridgeNode` an `arena_name` parameter, default `""`. An `arena_name` that is
entirely whitespace or has leading/trailing whitespace is **refused, not
trimmed**; every other malformed name is `tf_tree_ipc::ArenaName`'s to refuse,
arriving as a `BridgeError`. `BridgeHandle` maps `""` to NULL.

### Failure

A shared build can fail where a heap build cannot (name taken, runtime directory
unwritable, `memfd_create` refused). So **`TFT_ERR_ARENA_UNAVAILABLE`**: one code,
specific `tft_error` message, `TFT_ERR_BAD_CONFIG`'s granularity. A **minor bump**
under §3.6, reachable only when `arena_name` is non-NULL.

With `CreatePolicy::IfAbsent` a second bridge would join read-write and claim
edges in another's arena (`0019` §3 question 3); hence `Open::require_create(bool)`
+ `OpenError::ArenaAlreadyLive`.

A `bridge`-without-`shm` build must **refuse, not ignore**: it returns
`TFT_ERR_ARENA_UNAVAILABLE` naming the missing feature. There is no runtime
fallback to a heap arena.

## Consequences

**Invariants to maintain.** The bridge remains the single writer of every edge it
claims; consumers attach **read-only** and the ABI must not grow a way for them
not to. Fork poisoning, reaping and claim leases apply as to any participant, and
`0005` step 9's `atfork` rules apply unchanged:

- `crates/tf_tree_bench/src/bin/fork_child.rs`'s fourth mode: `tft_bridge_offer`,
  `tft_bridge_get_stats` and `tft_bridge_free` on an inherited handle in a forked
  child **come back at all**, and the parent's bridge still applies an offer and
  its arena is readable from a third process. It cannot live in `tf_tree_c`
  (`0007` budget).
- The panic guard is *not* the mechanism: `OwnedWriter::push` returns
  `Err(PushError::ChildDetached)`, which `publisher::map::push` maps. Only
  `tft_bridge_get_stats` returns the code directly; `tft_bridge_free` returns
  `void`; `tft_bridge_offer` returns `TFT_OK` with the detachment on the *outcome*
  (`action = TFT_BRIDGE_REJECTED`, `out.status = TFT_ERR_CHILD_DETACHED`).

## Implementation plan

0. **`Open::require_create` + `OpenError::ArenaAlreadyLive`** (with `0019` step 1).
   *Landed.*
1. **Port the `struct_size` prefix rule to `tft_bridge_options`**, append
   `arena_name`, branch `tft_bridge_create` to the `Open` path. *Landed;* a
   `crates/tf_tree_c/tests/bridge.rs` case pins that the *previous* `struct_size`
   still gets a heap arena.
2. **Refuse rather than downgrade.** *Landed.*
3. **`BridgeOptions::arena_name` and the `arena_name` node parameter**, through
   §5.8's three forms. *Landed.*
4. **The ROS parameter reaches `tft_bridge_options::arena_name`.** *Landed;*
   `ros/tf_tree_ros/test/test_shared_arena.cpp` compares with and without it.
   `crates/tf_tree_c/CMakeLists.txt` sets a per-target `TFT_HAVE_SHM=1`.
5. **`dds_bench` grows a `tf_tree.processes` arm.** *Landed.*
6. **Delete `dds_report::MISSING_ARM`.** *Landed;* pinned by
   `crates/tf_tree_bench/tests/dds_report_aggregate.rs`.
7. **Update `docs/PHASE4.md` §5.8 and `docs/PHASE5.md` §0.0's §9 row.** *Landed.*

## Open questions

All three are resolved by [`0019`](./0019-one-binary-and-topology-you-can-wait-for.md) §3.

1. **Resolved: refuse** a live arena under this name (`LayoutMismatch`);
   `CreatePolicy::Always` is the operator's act.
2. **Resolved: no derivation** from `tf_prefix`.
3. **Resolved: beside §5.4, not inside it.** A second bridge on a held name is a
   *rendezvous* fault (`PHASE5.md` §6's `TFT017`/`TFT018` amendment).
