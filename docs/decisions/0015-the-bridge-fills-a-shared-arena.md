# 0015: The bridge fills a shared arena

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** steps 0–2 landed on `feat/0015-bridge-shared-arena`
(`Open::require_create` + `OpenError::ArenaAlreadyLive`; the `struct_size` prefix
rule on `tft_bridge_options` with `arena_name` appended; `open_shared` refusing
rather than downgrading, in both the `shm` and the no-`shm` build). Steps 3–4 are
on `feat/0015-ros-arena-name` (`BridgeOptions::arena_name`, the `arena_name` node
parameter, `test_shared_arena.cpp`, and the `TFT_HAVE_SHM` probe step 4 turned
out to rest on — see the correction under step 4). **Steps 5–7 are outstanding**,
and so is the fork test the *Invariants to maintain* clause below demands — see
the note under it.

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

`tft_bridge_options` gains one field, at the end.

> **Correction — the prefix rule §3.6 describes is implemented for exactly one
> struct, and `tft_bridge_options` is not it.** This paragraph used to say the
> field was "guarded by the existing `struct_size` prefix rule … so a caller
> built against the previous header keeps working unchanged". `tft_bridge_create`
> validates with **exact equality** (`crates/tf_tree_c/src/bridge.rs:890-893`)
> and then reads the whole struct (`:896`). The rule exists for
> `tft_bridge_sample` alone — frozen shadow struct at `:293-301`, compile-time
> offset assertions at `:303-323`, bounded copy in `read_sample` at
> `:1368-1402`. `tft_bridge_outcome`, `tft_bridge_remap` and `tft_bridge_stats`
> are exact-equality too, and stay that way: they are `out` parameters, and
> accepting a short one means the callee must know which fields to skip writing,
> which is a different and larger design.
>
> So this record's **first** step is to *port* the rule, not to rely on it: a
> `tft_bridge_options_v1` shadow struct with the same offset assertions, and a
> `read_options` that narrows the copy to the declared size. **Relaxing `!=` to a
> length test without narrowing the read is an out-of-bounds read**, in the one
> crate whose entire `unsafe` budget is argument validation — `read_sample`'s own
> doc comment is explicit that the narrowed copy *is* the safety argument.
> `tft_bridge_create`'s safety contract widens from "whose `struct_size` is set"
> to "…and which has at least that many readable bytes", matching
> `tft_bridge_offer`'s.

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

`tft_bridge_create` routes the same builder through `tf_tree::Open` instead of
calling `build()`.

> **Correction — `build_shared(name)` alone cannot do this.** This paragraph used
> to say `tft_bridge_create` "selects `build_shared(name)` over `build()` on that
> field alone". `TreeBuilder::build_shared` **publishes no rendezvous**:
> `crates/tf_tree/src/tree.rs:373-376` is explicit that the name is a debug label
> that appears in `/proc/<pid>/fd`, and that *"segments are not discoverable by
> name — the fd is the capability"*. A second process could not find it. The path
> that publishes is `Open::open`'s `Created`/`TookOver` arm
> (`crates/tf_tree/src/open.rs:337-345`), which is `build_shared` **plus**
> `use_ofd_liveness`, `use_claim_leases`, `spawn_owner_server` and
> `hold_ownership`.

The call is:

```rust
Open::new().name(arena_name)?
    .mode(AttachMode::ReadWrite)
    .create(CreatePolicy::IfAbsent)
    .require_create(true)                              // see *Failure*
    .layout_if_creating(ingest.declared().builder())
    .open()
```

`layout_if_creating` is what preserves §5.6: the builder still comes from
`ingest.declared()` and never from `config`, so a `tf_prefix`-rewritten topology
sizes the arena, exactly as `bridge.rs:978-988` requires today.

**`arena_name` is the rendezvous *name*; the rendezvous *domain* is not
`tft_bridge_options.domain`.** That field is §5.5's *time* domain. The rendezvous
domain comes from `$TF_TREE_DOMAIN`, else `$ROS_DOMAIN_ID`, else 0 — which is
precisely [`0019`](./0019-one-binary-and-topology-you-can-wait-for.md) §3's
resolution of question 2, and why no derivation from `tf_prefix` is needed. Two
fields named "domain" in one header meaning different things is a documentation
obligation, not an accident to be discovered. Everything downstream — the claims, the ingest pipeline, the counters,
the outcome POD — is unchanged, because a `Tree` is a `Tree`.

### The rclcpp surface

`tf_tree_ros::BridgeOptions` gains `std::string arena_name` (empty = heap), and
`BridgeNode` a `arena_name` parameter, default `""`. §5.8's three deployment
forms all inherit it.

> **Correction — form 3 inherits the *field*, not the parameter.** Forms 1 and 2
> are `BridgeNode` and get it from the ROS parameter; form 3 never constructs a
> `BridgeNode` and has no parameters at all, so `BridgeOptions::arena_name` is
> its whole surface. That is why `test_shared_arena.cpp` asserts both paths
> separately: a `BridgeNode` that did something private with the name would
> leave form 3 — the form this project dogfoods — unable to publish an arena at
> all.
>
> One rule is added at the parameter layer and only one: an `arena_name` that is
> **entirely whitespace** is refused there, because empty means "no shared arena"
> and `""` and `" "` are the same string to an operator and opposite
> instructions to the bridge, while to C `" "` is an ordinary valid
> single-component name that `ArenaName` accepts (measured, not assumed). Every
> other malformed name — empty, over 64 bytes, `../escape` — is
> `tf_tree_ipc::ArenaName`'s to refuse, and it arrives as a `BridgeError` naming
> the name. A narrower rule at the ROS layer would make `tf_tree_ros` reject
> names `$TF_TREE_NAME` and `tf_tree serve` accept.

### What a consumer does

Exactly what it already does for any shared arena — `tf_tree::open()` in Rust,
`tf_tree::Tree::open()` in C++, `tf_tree.open()` in Python. **No new consumer
API**, which is the point: the bridge becomes an ordinary producer of the arena
Phase 2 already specified, rather than a special case.

### Failure

A shared build can fail where a heap build cannot — the name is taken, the
runtime directory is unwritable, `memfd_create` is refused. Those are startup
failures and join the ones `tft_bridge_create` already reports (domain, cycle,
claim). **They need one new status code, and the bridge needs one new builder
knob.**

> **Correction — "no new error class" does not survive contact.** Nothing
> existing means these: `TFT_ERR_BAD_CONFIG` is the topology *text*,
> `TFT_ERR_TIME_DOMAIN` is §5.5's domain agreement, and the claim family is
> per-edge with `frame_a`/`frame_b` in its detail. Collapsing them onto
> `TFT_ERR_INTERNAL` — what `tft_tree_open` does today — leaves an operator
> unable to tell "another bridge holds this name" from "the runtime directory is
> on NFS" from "a bug", which is the diagnosis this section exists to protect.
>
> So: **`TFT_ERR_ARENA_UNAVAILABLE`**, one code with a specific `tft_error`
> message, at the granularity `TFT_ERR_BAD_CONFIG` already uses for every way a
> config can be wrong. Under §3.6 that is a **minor bump**, on the precedent
> `TFT_ABI_VERSION_MINOR`'s own documentation sets for `TFT_ERR_BAD_STAMP` — and
> the argument is tighter here, because the code is reachable only when
> `arena_name` is non-NULL, which a caller whose `struct_size` names the previous
> layout cannot set.
>
> **And `CreatePolicy` has no "create, or refuse if one is already live"
> variant.** With plain `IfAbsent` a second bridge takes the *join* path,
> attaches read-write, and starts claiming edges in somebody else's arena — the
> fault [`0019`](./0019-one-binary-and-topology-you-can-wait-for.md) §3's
> question 3 closes. `Never` forbids creating; `Always` is `--force-new` and
> documents itself as "never take this path automatically". So
> `Open::require_create(bool)` + `OpenError::ArenaAlreadyLive`, in
> `crates/tf_tree/src/open.rs` where the session is already in hand.
>
> **A `bridge`-without-`shm` build must refuse, not ignore.** `bridge` and `shm`
> are independent cargo features and **no recipe builds both together**;
> `build_shared` and `tft_tree_open` are `#[cfg(feature = "shm")]`. Such a build
> carries `arena_name` in its header with no `tf_tree::Open` behind it, and must
> return `TFT_ERR_ARENA_UNAVAILABLE` naming the missing feature. Ignoring the
> field is precisely the silent downgrade the rest of this section forbids,
> reached by a *build configuration* rather than a runtime fault — and it is the
> more likely of the two. `just shm-check` gains the `--features bridge,shm`
> lines that make the combination compile at all.

There is no runtime fallback to a heap arena. **A bridge asked for a shared arena that
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

> **The fork test does not exist yet, and this is what it has to be.** Steps 0–2
> landed without it, so the sentence above is currently an assumption — which is
> the thing it forbids. Written out rather than left as a clause, because the
> reason it did not land is a real constraint and not an oversight.
>
> *Half of it is already covered.* `BridgeInner` holds exactly two guarded
> shapes: `Arc<Tree>` and one `tf_tree::OwnedWriter` per declared dynamic edge
> (`tft_bridge_create` claims through `Tree::claim_owned`, per `0017`). That is
> the same pair `crates/tf_tree_bench/src/bin/fork_child.rs`'s **`owned` mode**
> already forks and checks — `0017` step 4 — so the Rust-level claim, that a
> forked child is refused rather than reading a `MADV_DONTFORK` hole and that its
> destructors do not release the parent's OFD lease, holds for the bridge by
> construction.
>
> *The uncovered half is the C ABI layer above it*, and it is the half a ROS
> node actually reaches: that `tft_bridge_offer`, `tft_bridge_get_stats` and
> `tft_bridge_free` called on an inherited handle in a forked child **return a
> status** — §3.4's panic guard turning `ChildDetached` into
> `TFT_ERR_CHILD_DETACHED`, never a `SIGSEGV` and never an `abort()` — and that
> the parent's bridge still applies an offer, and its arena is still readable
> from a third process, after that child has exited.
>
> **It cannot live in `tf_tree_c`.** What has to be produced is `fork()` without
> `exec` (`std::process::Command` always `exec`s, and a thread is not a process),
> and the only primitive for that is `libc::fork`, which `tf_tree_c` does not
> depend on. Adding it would put a second real `fork()` in the workspace against
> `0005`'s recorded single exception, and add `libc` to the C ABI's dependency
> graph — both of which are `0007` budget questions and therefore a decision
> record, not a PR.
>
> **So it belongs in `crates/tf_tree_bench`**, as a fourth mode of the existing
> `fork_child` binary — the file `0005` already grants the exception to and which
> already carries the scratch rendezvous, the `exited`-versus-`signalled`
> protocol and the parent re-validation this needs. The cost is one new crate
> edge: an optional `tf_tree_c = { features = ["bridge", "shm"] }` behind a
> `bridge` feature on `tf_tree_bench`, and a line in `just shm-check` beside the
> `fork_child` build it already has. That edge is what makes this its own commit
> rather than a rider on step 2.

## Implementation plan

0. **`Open::require_create` + `OpenError::ArenaAlreadyLive`** — not in this
   record's original seven, and required by them (see *Failure*). — verified by
   `just shm-rendezvous` with a case asserting a second `require_create(true)`
   open against a live arena fails and leaves the first serving.
1. **Port the `struct_size` prefix rule to `tft_bridge_options`** — shadow
   struct, offset assertions, a `read_options` that narrows the copy, and
   *deletion* of the whole-struct `read_unaligned` — then append `arena_name`
   defaulting to NULL; `tft_bridge_create` branches to the `Open` path of
   *The ABI* above. — verified by a new
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

   > **Correction — this step names the wrong property, and a prerequisite it
   > does not mention.**
   >
   > *The property.* "A second **process** reads what the bridge wrote" is step
   > 2's, and step 2 discharged it:
   > `crates/tf_tree_c/tests/bridge_shared.rs`'s
   > `a_second_process_reads_what_the_bridge_wrote` spawns a real child that
   > links no `tf_tree_c` at all and compares the bytes. Spawning
   > `tf_tree_bridge` from a ctest would re-run that across a middleware and an
   > ament install tree, and it would still not test the thing this step is
   > actually for — **that the ROS parameter reaches
   > `tft_bridge_options::arena_name`**. A bridge whose parameter was dropped on
   > the floor publishes no rendezvous, so a spawned-executable test fails, but
   > so does a much cheaper one. What no version of "attach and assert a lookup
   > matches" catches on its own is the reverse: it passes just as well against
   > an implementation that publishes *unconditionally*.
   >
   > So `ros/tf_tree_ros/test/test_shared_arena.cpp` is a **comparison**: the
   > same node, topology and attach, once without the parameter (nothing is
   > findable under the name) and once with it (the attach succeeds and reads
   > the topology's static edge). Plus the same thing through
   > `BridgeOptions::arena_name` for §5.8's form 3, which has no parameters at
   > all, and the held-name refusal crossing `BridgeHandle`'s promise as a
   > `BridgeError`.
   >
   > *The prerequisite.* `tf_tree.h` hides `tft_tree_open` behind
   > `#if defined(TFT_HAVE_SHM)` and **nothing in the CMake package defined it**,
   > so no `find_package(tf_tree CONFIG)` consumer — this ctest, `ros/tf_tree_ros`,
   > `just cmake-check` — could call the entry point this record's consumers
   > exist to call, except by hand-typing the macro against an archive that may
   > not have the feature in it. `crates/tf_tree_c/CMakeLists.txt` now probes the
   > resolved library with `nm` and propagates `TFT_HAVE_SHM=1` through
   > `tf_treeConfig.cmake.in`, and `ros/build.sh` builds `--features bridge,shm`
   > and checks one symbol per feature. None of that is in the seven steps; it is
   > what step 4 turned out to rest on.
5. **`dds_bench` grows a `tf_tree.processes` arm**; `bench_consumer` gains
   `--mode tf_tree_attach` that calls `tf_tree::Tree::open()` instead of hosting
   a bridge. — verified by `just dds-bench` reporting four arms at 0 % failure.
6. **Delete `dds_report::MISSING_ARM`** and the paragraph in
   `docs/benchmarks/tf2.md` it mirrors, replacing both with the measured row. —
   **the test this step used to name does not exist.** `dds_report.rs` has no
   `mod tests` and nothing under `crates/tf_tree_bench/tests/` references
   `MISSING_ARM`; the `REQUIRED_ROWS` machinery that sounds like it belongs to
   `bench_report`, a different binary. The sentence is pinned by nothing, so
   deleting it is unverified by construction. This step therefore *adds* the pin
   as well: a test over `aggregate`'s rendered output asserting four arm labels
   and the absence of `NOT MEASURED`.
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
