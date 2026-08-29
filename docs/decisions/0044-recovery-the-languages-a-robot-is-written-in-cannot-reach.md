# 0044: recovery, in the languages a robot is written in

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** (filled in as work lands)

## Context

[`0037`](./0037-a-takeover-is-not-a-second-open.md) built ownership migration and
[`0043`](./0043-owner-lost-is-a-question-about-the-owner.md) made its trigger
answer the right question. **Neither is reachable from C, C++ or Python.**

```
$ grep -oE '\btft_[a-z0-9_]+\b' crates/tf_tree_c/include/*.h | sort -u | wc -l
58
$ ... | grep -E 'owner|inherit|reap|participant'
(nothing)
```

`PyTree`'s method list is `plan / publisher / freeze / span / frames / edges /
is_shared / is_writable / instance_uuid / lookup / at_extrapolating*` and
nothing else. `Tree::owner_lost`, `Tree::inherit_ownership`, `Tree::reap`,
`Tree::reap_participant` and `Tree::reap_participants` are all Rust-only.

**What that costs is stated in `docs/RUNBOOK.md` without being connected to
it.** An all-C++/Python fleet whose arena owner is `SIGKILL`ed cannot be
rejoined: the survivors keep their participant bytes, §3.4 step 4 refuses every
new create with `ArenaHeldButUnreachable`, and the one thing that ends that
state — a survivor calling `inherit_ownership` — exists in a language nobody in
the fleet is running. The documented recovery is to stop every attached process.
ROS 2 nodes are written in C++ and Python; `ros/tf_tree_ros` is `ament_cmake`.
So the fleet shape this engine was built for is exactly the shape that cannot
recover.

This is the third time this pattern has been found — after `0038` (the time
domain) and `0039` (extrapolation) — and it is the same pattern: **a capability
that is implemented, tested, documented, and callable by nobody who needs it.**

The reap half is **narrower than it was**, because `0043`'s successor closed the
common producer: the owner's hangup callback now revokes a dead participant's
claims, so an ordinary killed-and-restarted publisher needs no reaper at all. Two
producers remain and neither has a hangup — a dead **owner**, and a
`TreeBuilder::build_shared` participant with no socket — so `Tree::reap` is still
the only collector for them, and still Rust-only.

## Decision

**The three recovery entry points cross both boundaries, and the facade drops
the `&mut` that made that impossible.**

### 1. `inherit_ownership` takes `&self`

This is the load-bearing part and it comes first, because without it neither
binding can call the method at all. Both hold the tree in an `Arc` —
`tft_tree` an `Arc<TreeShare>`, `PyTree` an `Arc<Tree>` (the receiver type
`Tree::claim_owned` requires) — and `Arc::get_mut` fails whenever any plan or
publisher holds a clone, which in a binding is always.

`Tree::attachment` becomes `Mutex<Option<Attachment>>`. Six sites touch it, all
control plane; **`Plan::at` does not, and cannot** — the fold reads the mapping
and the `Guard` and nothing else, which is what `docs/PHASE2.md` §3.5's "lookups
do not stop, slow down, or observe anything during a takeover" rests on. So the
lock costs the hot path nothing, measured by the existing bench gate rather than
asserted.

Two consequences worth naming rather than discovering:

- **`&mut self` → `&self` is a relaxation, not a break.** Every existing caller
  still compiles.
- **§3.5's one caller-side qualification disappears.** That section says "
  `inherit_ownership` takes `&mut self`, so the inheriting handle's own
  `Guard<'_>` cannot be outstanding across the call". It can now, which removes
  the one place a control loop had to arrange its borrows around recovery.
- **Drop order is preserved.** Requirement 5 — serving stops before byte 0 is
  released — is expressed as field declaration order on `Attachment::Owner`
  (RFC 1857), and a `Mutex` drops its contents by the same rule.

### 2. The C ABI, in the unstable tier

The stable ABI is frozen at 1.0 (`docs/PHASE4.md` §7), so these are declared in
`tf_tree_unstable.h`, which is where a surface that may still move belongs:

```c
tft_status tft_tree_open_named(const char *name, bool read_write, tft_tree **out);
tft_status tft_tree_owner_lost(const tft_tree *tree, bool *out);
tft_status tft_tree_inherit_ownership(const tft_tree *tree, uint8_t *out);
tft_status tft_tree_reap_dead(const tft_tree *tree, uint32_t *out);
```

`tft_tree_reap_dead` is `Tree::reap_dead()` plus `Tree::reap_participants()`,
summed — a binding caller wants "collect what the dead left", not a choice
between two sweeps whose difference is which arena table they walk.

> **Amendment (2026-08-29), made while implementing: `tft_tree_open_named` was
> not in this record and the other three are decoration without it.**
> `tft_tree_open(tft_tree **out)` is the **entire** arena-opening surface of the
> C ABI, and it is `tf_tree::open()` — read-only, with the name taken from
> `$TF_TREE_ARENA`. `rg 'AttachMode::ReadWrite' crates/tf_tree_c/src/lib.rs`
> returns nothing. So a C or C++ consumer could only ever hold a read-only
> attachment, and `tft_tree_inherit_ownership` would answer `TFT_READ_ONLY`
> every single time: an owner writes the participant table on every grant and a
> `PROT_READ` mapping cannot, which is D18 working rather than failing.
>
> This record's Context said the recovery *methods* were unreachable. They were,
> and so was the only state from which any of them does anything. Found by the C
> test failing to compile against a signature that did not exist, which is where
> it should be found.
>
> `tft_tree_open_named` never creates (`CreatePolicy::Never`): creating needs a
> layout and there is no way to express one across this boundary — a C creator is
> `tft_bridge_create`, which brings its own topology.
>
> **`tft_inheritance` is a `typedef uint8_t`, not an enum, and it is not tiered
> as a symbol.** `xtask headers` tiers *functions*, and a bare type alias listed
> there is rejected as naming no exported function while an unlisted one lands
> in **both** generated headers — which §3.1 forbids, because the split is the
> stability promise. Listing the five constants is what pulls the typedef into
> the unstable header alone.

### 3. Python

`Tree.owner_lost() -> bool`, `Tree.inherit_ownership() -> str` (the variant
name, which is what a Python caller branches on), `Tree.reap_dead() -> int`.

## Rationale

**Why not leave it Rust-only and tell integrators to write a Rust supervisor.**
That is [`0019`](./0019-one-binary-and-topology-you-can-wait-for.md)'s
prohibition wearing a different hat: every process a user is *required* to run is
a place adoption dies, and "add a Rust process to your C++ fleet so the arena can
recover" is a larger ask than a daemon, not a smaller one.

**Why the unstable tier rather than the frozen one.** `docs/API.md` §7's
checklist asks what a new surface commits us to. This one commits us to a
*protocol* — §3.5's inheritance — that shipped four days ago and has one
consequence its own record did not predict (`0043`'s three-outcome table). The
stable header is a promise about a decade; this is a surface that should be
allowed to move once. `tft_tree_plan_in_domain` went into the stable header
because it is a query shape, not a protocol.

**Why a `Mutex` and not a `RefCell` or an atomic swap.** `Tree` is `Sync` — it is
shared across threads through an `Arc` by both bindings and by
`docs/API.md` §2.2's own embedding idiom — so `RefCell` does not compile.
An `ArcSwap`-shaped alternative is D4's forbidden pattern and would not help: the
attachment is taken, mutated and put back as a unit, which is a critical section
rather than a pointer swap.

**Why `reap_dead` sums the two sweeps.** They differ in which table they walk —
claim records and participant records — and a caller in C has no basis to choose.
The Rust surface keeps both separately, because a Rust caller reaping in a hot
supervisory loop may well want only one.

**Why not expose `reap_participant(slot)` too.** The slot is the owner's fast
path from `EPOLLHUP`, and a binding has no `EPOLLHUP`. Exposing an index a
caller cannot obtain is surface for its own sake.

## Consequences

- The binding surface grows by three C functions, one C enum and three Python
  methods, and `docs/API.md` §3 and §4 record them. That is API growth in a
  project trying to shrink, accepted for the same reason `0038`'s was: the
  alternative is a fleet shape that cannot recover.
- **`docs/RUNBOOK.md`'s owner-death section stops being Rust-only advice.** Its
  "is any survivor read-write?" checklist gains a fourth question that was
  previously unaskable — *can any survivor call it?* — with the answer now
  "yes, in whichever language it is written in".
- The attachment becomes lock-protected, so a future caller could deadlock by
  calling `inherit_ownership` from inside something that already holds it. There
  is one such path today and it is internal (`crate::open::Open::attempt`); the
  `Mutex` is `pub(crate)` and every acquisition is in `tree.rs` or `open.rs`.
- **`Inheritance` becomes part of two more surfaces**, so adding a variant is
  now breaking in three places. It is already `#[non_exhaustive]` on the Rust
  side; the C enum carries an explicit "a value you do not know means *not the
  owner*" rule so a new variant degrades safely rather than falling off a
  `switch`.

## Implementation plan

1. `Tree::attachment` becomes `Mutex<Option<Attachment>>`; `owner_lost`,
   `inherit_ownership`, `attachment_ref`/`take_attachment`/`put_attachment`,
   `hold_attachment` and `hold_ownership` move onto it, and
   `inherit_ownership` takes `&self`. Verified by the existing rendezvous suite
   passing unchanged — this step is behaviour-preserving — and by `just
   bench-check` against the committed baseline, reported rather than assumed.
2. A test that a `Guard` may be outstanding across `inherit_ownership`, which is
   the §3.5 qualification this removes and does not compile before step 1.
3. **`tft_tree_open_named`** plus the three recovery entry points and the five
   `tft_inheritance` constants, in the unstable header. Verified by
   `crates/tf_tree_c/tests/recovery.rs`, which spawns an owner process, joins it
   **read-write from C**, kills it, and recovers — the thing that fails today,
   and fails at the point where the symbols do not exist. Also by
   `just c-header-check`.

   One assertion in that test was wrong on the first run and the code was right:
   after the owner dies, `tft_tree_reap_dead` collects **one** record — the dead
   owner's own. Nothing hangs up on an owner, so its `LIVE` record over a
   kernel-released byte is one of the exactly two states the hangup callback
   cannot reach, and it is the case this entry point exists for. The test
   asserts `1` and then `0` rather than "some number".
4. The three Python methods. Verified by a pytest reproducing step 3 through
   Python.
5. `docs/API.md` §3/§4, `docs/PHASE2.md` §3.5's qualification, and
   `docs/RUNBOOK.md`'s owner-death section. Verified by `just artifact-versions`
   and by re-reading the runbook against the header.

## Open questions

None. Two were resolved while writing:

- *Stable or unstable tier?* Unstable. The protocol is four days old and has
  already produced one unpredicted outcome; the stable header is a longer promise
  than that.
- *One `reap_dead` or two sweeps?* One across the boundary, two in Rust. A C
  caller has no basis to choose between them; a Rust supervisor might.
