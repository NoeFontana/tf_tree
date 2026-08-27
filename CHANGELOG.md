# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

**Versioning, stated before anything else, because `0.0.x` is not ordinary
semver.** Cargo treats every `0.0.x` release as incompatible with every other:
`^0.0.1` — which is what a bare `tf_tree = "0.0.1"` means — matches `0.0.1` and
nothing else. That is deliberate and it is the whole promise. **Nothing in this
release is stable. Every release may break every other**, in the public Rust
API, in the Python API, in the C ABI, and in the arena format. Pin exactly.

Two consequences worth naming rather than leaving to be discovered:

- **PyPI does not have that rule.** PEP 440 gives `0.0.x` no special meaning, so
  `pip install -U transform_tree` will move you from `0.0.1` to `0.0.2` without asking.
  Pin the wheel yourself if the promise above matters to you.
- **`SUPPORT.md`'s "an MSRV bump is a minor-version bump pre-1.0" rule does not
  apply on the `0.0.x` line** — there is no minor slot for it to occupy. The
  argument, and why the resolver already enforces what that rule was written to
  enforce, is in the root `Cargo.toml`'s comment on `[workspace.package]
  version` and in `SUPPORT.md`'s MSRV section.

The single source of truth for what is implemented is the status tables in
`docs/` — `## 0.0 Implementation status` at the head of `PHASE2.md`, `PHASE4.md`
and `PHASE5.md`, and `## 0.0 Status` in `PHASE7.md`. `PHASE1.md` has none because
Phase 1 is implemented whole; `PHASE3.md` has none because it records deviations
inline, in the section each one belongs to — §5.5's buffer-protocol steps are the
example. Where this file and one of those disagree, they are right and this file
is a bug.

---

## [Unreleased]

### Changed — breaking

- **`tf_tree_ipc::Open::already_attached(bool)` is now `already_attached_at(u32)`,
  and the takeover arm it reaches registers no participant** (closes #201).

  The arm called `register_any`, which takes the first **free** lock byte. A
  survivor holding byte 5 with arena record 5 was therefore handed a session on
  byte **0** — executed, on the arm itself:
  `outcome=TookOver  session slot=0  but the caller's arena record is 5`. Every
  liveness predicate in the facade indexes the lock byte and the arena record
  with one integer (`docs/PHASE2.md` §5.1), so that session reports a running
  process as **dead**, which §6.2 calls the corrupting direction.

  **The correct answer was already decided and the deferral was a typo.**
  [`0035`](docs/decisions/0035-the-creators-slot-is-taken-not-found.md) closed
  #201's creator path and left this arm open, pinning it to *"`0029` question
  3"*. That answer is `0028` question 3, **resolved 2026-08-20**: the heir keeps
  its existing slot, byte and arena record, because the slot is baked into every
  claim it already holds and a heir with a second slot would arrange for its own
  live claims to be reaped. `0029` corrected the misdirected citation on
  2026-08-25, by which time `0035` was frozen.

  So the arm now returns the slot the caller declares and takes no participant
  byte, and **`register_any` — whose only caller it was — is deleted**.
  `OpenError::ParticipantSlotDiverged` stays as an assertion neither path can now
  trip.

  **The declaration is a precondition, and `open()` checks it** — once, before
  anything else: one `F_OFD_GETLK` on the declared byte, refusing with the new
  **`IpcError::NotAttachedAt`** if nobody holds it, and range-checking on the way.
  Carrying a `u32` instead of a `bool` removes the *arm's* freedom to pick a slot
  but does not make the caller's declaration true, and review executed three ways
  it was not: a declaration nobody backed minted `TookOver slot=0` over a **free**
  byte, `already_attached_at(u32::MAX)` returned `Ok(4294967295)`, and a
  **serving** owner overrode the declaration with `Joined slot=1` — #201's own
  divergence on the join path, in the §3.5 race this arm exists for. With the
  check, and with every arm honouring the declaration, no value of that argument
  produces a session whose slot the caller did not choose, and none at all unless
  that slot's byte is held.

  `LockFile::take_any_participant` survives with no production caller and a
  caution on it: a participant's byte is never free to choose, so every caller
  that thought it needed "any free byte" already knew its slot.

- **`tf_tree_core::edge::ClaimRecord::last_push_nanos` is now
  `clock_offset_nanos`, and holds `wall clock - stamp` rather than the wall
  clock** ([`0036`](docs/decisions/0036-the-receipt-time-the-format-already-reserved.md)).
  A `pub` field that has shipped since `0.0.1` *unwritten* — nothing could have
  read it for its value, since it was always zero — but a field path or a
  `ClaimRecord { .. }` literal naming it will not compile. The Added entry below
  is the whole argument.

### Added

- **A per-publisher clock offset, from a field that has been in every shipped
  arena since it was declared** ([`0036`](docs/decisions/0036-the-receipt-time-the-format-already-reserved.md)
  steps 1–2). `ClaimRecord::last_push_nanos` had four references in the whole
  workspace — two struct definitions and two zero-initialisers — while
  `docs/PHASE2.md` §6.4 said normatively that it was *"bumped on every push"*.
  Nothing wrote it and nothing read it, and that is why `TFT004` (clock skew)
  reported *"cannot detect anything in any configuration"*: the check needs a
  per-publisher offset against each publisher's header stamp.

  **BREAKING: the field is now `ClaimRecord::clock_offset_nanos` and holds
  `wall clock - stamp`, not the wall clock.** A receipt time cannot be paired
  with a stamp by any reader — the write is sampled, so the ring's newest stamp
  belongs to a later push than the receipt does. Measured on a 10 Hz publisher
  whose clock is *exact*: `receipt - newest_stamp` reads **+3 µs on the sampling
  push and −900 ms nine pushes later**, decided by nothing but when the reader
  arrives. That is a ±1 s noise floor under a signal `TFT004` must resolve at
  tens of milliseconds, and question 1 made the interval ~1 s for *every*
  publisher, so it does not cancel in a fleet comparison either. The writer is
  the only party holding both sides at one instant, so the writer subtracts. No
  new field, no format change, no extra clock read. `0` still means *no sample
  yet*, which now also swallows a genuine offset of exactly zero — one sample in
  ~10⁹, overwritten by the next.

  The rename is a breaking change to a `pub` field that has shipped since
  `0.0.1`, unwritten. **Nothing could have been reading it for its value** — it
  was always zero — but a `ClaimRecord { .. }` literal or a field path naming it
  will not compile. Leaving the old name over the new quantity was the
  alternative, and it is the exact doc-versus-code drift that kept `TFT004`
  blind for the life of the project.

  **`tf_tree`'s `EdgeWriter::push` now records it, sampled rather than on every
  push.** A wall-clock read is **38.4 ns** against a **~4.9 ns** push, measured,
  so unconditional was never the trade; the interval is derived once per claim
  from `EdgeRecord::nominal_rate_mhz` as `max(mhz / 1000, 1)`, which makes the
  *offset sample rate* the constant instead of the push interval — a 1 kHz IMU
  and a 10 Hz localiser each yield about one offset per second of published
  data, and each pays one clock read per second. An edge that declares no rate
  samples every 1024 pushes rather than never, because a tree built without a
  topology file is the common case and not an exotic one.

  **A claim's first push samples, and a claim clears the offset it inherits.**
  Both matter more than they look. Starting the countdown a full interval away
  would leave a 10 Hz undeclared edge reading `0` — indistinguishable from the
  state this release is fixing — for its first 102 seconds, and a 0.2 Hz one for
  85 minutes. And **nothing in the system resets the field** — not
  `release`, not the reaper, not `tf_tree_core::edge::claim` — so without the
  clear, a replacement writer publishes under the departed writer's number and
  a future `TFT004` bills a dead publisher's skew to this one.

  **No arena byte moves and `FORMAT_VERSION` is untouched** — the field is
  already part of `layout_hash`, and `layout_hash` is a stride table, so neither
  the rename nor the change of meaning perturbs it. **That is worth naming as a
  cost and not only as a convenience:** eight bytes changed interpretation with
  nothing able to detect the difference, so two processes built either side of
  the amendment would read each other's writes in their own units. It is
  harmless *here* — both commits are inside this same `[Unreleased]` section, so
  no published artifact ever wrote a receipt time — and it is the reason a
  future change of meaning to a live field is not automatically free.

  **The engine crate changes only in the field's name and doc.** The clock is
  read in the facade, *after* `Publisher::push` returns, which keeps it outside
  the seqlock window; inside it, a writer's diagnostic would become every
  reader's `SlotContended` retries.

  **What it costs — measured, and not where the record predicted.** `push` goes
  from **4.85–5.0 ns to 5.87–6.1 ns**: **+1.0–1.1 ns, about +21%**, on every
  push, re-derived on the amended sampler and not carried over from the first
  one.
  That is a *paired* delta, both arms in one process, five sittings
  (`just push-sampler-cost`, `benches/push_sampler.rs`) — because this host
  fails `bench_report`'s fitness probe and an unpaired before/after across two
  `cargo bench` runs said **+47%**, which was drift. `just bench-check` passes.

  **Almost none of it is the clock read — at that interval.** `SystemTime::now()`
  is 38.4 ns here, which at the 1024-push default is 0.04 ns amortised, **3% of
  the 1.1 ns**. The rest is the counter, which `0036` described as *"a
  non-atomic counter increment and a compare against a value in a register"* and
  priced at nothing. But the 3% is a property of **1024**, not of the design:
  the cost is `counter + 38.4 / sample_every`, so at a declared 10 Hz the clock
  is 78% of a ~4.9 ns overhead. What stays bounded is the cost **per second of
  publishing** — `38.4 + rate × 1.06` ns, under a microsecond at 1 kHz and ~49 ns
  at 10 Hz. `docs/PHASE1.md` §11.2 tabulates both ends and marks the one measured
  row.

  **The alternative `0036` proposed was built, and it is slower.** Sampling off
  the arena's `heartbeat` with a mask — the counter the push path already
  maintains — reads **+1.4 ns against +1.1 ns**, forces `sample_every` to a
  power of two, and cannot sample a new writer's first push, because `heartbeat`
  belongs to the edge and not to the claim.

  What stays arithmetic is the *tail*: a 1 kHz publisher's p99.9 push is the
  sampled one, ~38 ns above its neighbours, and `publish_to_visible` — the row
  that would say whether that reaches a consumer — is `unavailable` on this host
  (4 physical cores against the 17 it needs, and no ROS 2). It ships unmeasured,
  and `docs/PHASE1.md` §11.2 says so in its own terms.

  **`TFT004` now reads it** — see the entry below — so it still skips only where
  it has no evidence, and
  `docs/PHASE5.md` §0.0's *"sixteen detect"* becomes seventeen.

- **`TFT004` detects clock skew** — the first check to move out of §0.0's
  *"cannot detect anything in any configuration"* group since the catalogue was
  written ([`0036`](docs/decisions/0036-the-receipt-time-the-format-already-reserved.md)
  steps 3–4, closing that record). `docs/PHASE5.md` §6 calls it *"the check most
  likely to find something nobody knew"*: on a multi-machine robot with imperfect
  time sync, clock error presents as intermittent extrapolation failures on an
  edge whose publisher is fine, and nothing else in a ROS 2 stack points at it.

  **What it finds is narrower than §6 asks for, and that is a finding rather than
  a shortfall.** A recorded offset is the publisher's clock error *plus* its
  stamp-to-push latency, and **one sample cannot separate them** — a localiser
  that stamps with the capture time of the scan it matched legitimately sits tens
  of milliseconds above an odometry publisher. A fleet-relative rule would report
  that healthy difference as skew however well its threshold were calibrated,
  because the quantity it compares is not the quantity it names. So `TFT004`
  fires only past a bound **no publish pipeline could account for** (ten
  seconds): a machine whose NTP never came up or whose RTC is dead. That is a
  physical argument, not a tuned constant.

  **The fleet spread is reported as a note** — the offsets, their median and
  their range, with the caveat attached so a reader does not chase a pipeline
  difference as skew. That is the useful half today, and it is what `0036`
  question 3 ratified.

  **What separates clock error from latency is drift**, which needs a series;
  `tf_tree top` polls and `doctor` does not, so the fleet-relative rule is owed
  and recorded as a `top` feature — in `top.rs`'s own module header, where
  whoever builds it will be, rather than only in a decision record they would
  have no reason to open.

  **Four skips, each with its own reason**, and two are about where the arena
  came from rather than what is in it: a **replayed** source (`--from-bag`
  records ingest-time offsets against a recording's stamps — two years for a 2024
  bag read in 2026), an arena **at rest** (a frozen `.tft` carries offsets from
  whenever it was frozen), `TFT005`'s epoch condition, and nothing sampled yet.

### Fixed

- **`Tree::reparent` no longer steals A2's topology lock from a live mutator
  that `/proc` misreports** (#213, [`0029`](docs/decisions/0029-the-topology-lock-is-a-kernel-lock.md)).
  It was the last place in the system where one process could destroy another's
  exclusive state on an *inference*: the steal was authorised by the
  `(pid, start_time, boot_id)` triple, even on a tree holding an `F_OFD_GETLK`
  probe, and `/proc` has two measured ways to call a running process dead — a
  PID-namespace collision (`Known(st) != stored`, which is not `ENOENT`-shaped,
  so the bias against proving death does not fire) and a same-user but
  **non-dumpable** target under `hidepid`. A false "dead" here puts two live
  processes in the topology critical section, which `docs/PHASE2.md` §6.2 calls
  the corrupting direction.

  **`reparent` now takes an exclusive OFD lock on the lock file's byte 1 before
  it touches the arena word**, and releases them in the other order — the move
  §6.1 already made for claims and §5.1 for participant records. A live holder is
  refused by the kernel before any inference runs; a dead one has its byte
  released by the kernel with no cooperation and no timeout, so nothing wedges.
  Byte 1 was reserved by §3.3 and is unused, so **no arena byte changes** and
  `FORMAT_VERSION` is untouched.

  **`reparent` keeps the patience it always had.** The byte is re-attempted 32
  times before contention is reported — sized from measurement (an uncontended
  `reparent` is 2.94 µs, one contended `fcntl` 791 ns, the arena word's existing
  1024-spin budget 30.29 µs), so a brief overlap is still absorbed rather than
  returned to the caller as an error.

  **The cost, measured rather than waved at:** an uncontended `reparent` goes
  from 1.011 µs to 2.96 µs (**+193%**), which is two `fcntl`s. `reparent` is off
  the query path (D3) and topology is near-static after startup, so the absolute
  number is what decides; lookups are untouched.

  A tree with **no lock file** — a heap tree, a directly-called
  `TreeBuilder::build_shared`, an `attach_shared` over an inherited fd — is
  unchanged in both directions: there is no byte for anyone to take, so the
  `/proc` predicate is still the whole answer there. Nothing acquires a new way
  to be stolen from.

### Added

- **`tf_tree_ipc::LockFile::try_take_topology` / `release_topology`** — byte 1 of
  the lock file, A2's topology mutation lock. The §3.3 byte table gains a row and
  `bytes 1–15 reserved` becomes `bytes 2–15`. There is deliberately **no
  `probe_topology`**: nothing reads it, and an unused `pub fn` on a published
  crate is surface with no consumer.

### Changed — breaking

- **`tf_tree::ReparentError::LockContended`'s `owner_slot` is `Option<u32>`, not
  `u32`** (#213). Breaking for anything that destructures it. It is `None` when
  the observation could not name the holder — it took the lock file's topology
  byte and had not yet published its slot into the arena word, or it released the
  word between the load and the `compare_exchange`. Both are nanoseconds wide and
  both mean the same thing to a caller: *a live peer is mutating topology,
  retry.*

  **The old `u32` carried `tf_tree_core`'s `u32::MAX` sentinel straight through
  to the message**, which then read *"the topology lock is held by live
  participant slot 4294967295"* — a sentence an operator has to already know a
  magic number to disbelieve. `docs/API.md` R5 makes the field the contract and
  the message a diagnostic, which requires the field to be the thing that is
  true. `tf_tree_core::topology::TopoLockError` keeps its sentinel; the
  translation happens once, in `tf_tree`'s `From<TopoLockError>`.

- **`tf_tree_ipc::LockRole` gains `Topology`** (#213). That enum is **not**
  `#[non_exhaustive]`, so this breaks any downstream exhaustive `match` on it —
  which is why it is here rather than under *Added*, where it was first filed.
  Nothing in this workspace matches on `LockRole` outside `tf_tree_ipc`, and that
  is not the standard a published crate is held to.

  Whether the error-shaped enums in `tf_tree_ipc` should carry
  `#[non_exhaustive]` at all is a **question this release does not answer**: the
  crate uses it in `wire.rs` and not on `LockRole` or `IpcError`, both of which
  have now grown breakingly more than once. It belongs with `API.md` §7 rather
  than in a change about the topology lock.

- **`tf_tree::ReparentError` gains `TopologyLease { raw_os_error: i32 }`**
  (#213). The enum is `#[non_exhaustive]`, so a downstream `match` with a
  catch-all is unaffected. Deliberately **not** folded into `LockContended`: both
  refuse, but only one means a peer is doing something, and only one is worth
  retrying. A refusal that names a live peer when the cause is `EBADF` sends an
  operator to look for a process that is not there.

- **`tf_tree_core::MAX_DEPTH` is 32, not 16, and it now means what its
  documentation always said** (#251, `0034`). It bounds the *compiled* plan —
  `Plan`'s `[Step; MAX_DEPTH]` array, counted **after** adjacent static links
  fold into one step. It used to be enforced on the **raw walk** instead, which
  is a different quantity: a 17-link rigid chain that compiles to a *single*
  constant was refused at exactly the length a 17-joint arm was. A new
  **`tf_tree_core::MAX_PATH_EDGES` = 64** bounds the walk. Both are `pub const`
  and re-exported by `tf_tree`, so on the `0.0.x` line this is a semver-relevant
  change to a value **and** to a meaning, on five published crates.

  **Two bounds because one number cannot price both slots.** A compiled slot is
  a `Step` — **128 bytes**, measured — carried by value in every `Plan`, in a
  16-slot thread-local plan cache, and behind every Python `Plan`. A raw slot is
  a `u32` edge id in `compile`'s stack frame: 4 bytes, paid once, on a call D3
  already places off the query path.

  **The values come from a survey, not from an intuition.** 91 distinct real
  robot structures from 26 repositories, and the binding quantity is the graph
  **diameter** in joints — up to the common ancestor and back down, which is
  what a lookup walks — not root-to-leaf depth: max 30, p95 24, median 10.
  Root-to-leaf depth maxes at 18, so a survey that measured *that* would have
  concluded 24 was ample. At `MAX_DEPTH = 16` the old engine refused at least one
  frame pair on **26 of the 91**. `MAX_PATH_EDGES = 64` is ~1.9× the floor a
  deployment sets (a 30-joint diameter plus `map → odom → base_footprint`), and
  deliberately not 256: this constant sets the worst *accepted* compile — 1.09 µs
  at 64 against 3.97 µs at 256 — and a refused pair is not cached (#259), so it
  recompiles on every lookup with nothing amortising it.

- **`LookupError::TreeTooDeep { depth }` reports one quantity per bound, and the
  two are disjoint** (`0034`). It reported three different things: the bound for
  a one-sided chain, the truth for a balanced two-sided path, and neither for a
  lopsided one. Now `MAX_PATH_EDGES + 1` means the **walk** refused — the walk
  stops when it runs out of buffer and never learns the real length, so that is
  the only value above the bound this field takes — and anything at or below
  `MAX_PATH_EDGES` is the **exact** folded step count. No new variant and no new
  `tft_status`: the C ABI's status table is frozen, and `TFT_ERR_TREE_TOO_DEEP`
  still covers both. Its header prose no longer names `TFT_MAX_DEPTH`, a macro
  referenced in two places and **defined nowhere** since Phase 4; it is still not
  defined, because `0034` split the quantity it was vaguely about into two and
  freezing that one name now would make it ambiguous rather than merely absent.

- **Error precedence on a too-long path.** `fold` now runs before the compiled
  bound is checked, so `UnknownEdge` and `MixedTimeDomains` are raised for a
  defect that sits **past** the bound rather than being hidden behind the path's
  length; `MissingEdge` is unchanged and still wins by its position in the walk.
  Nothing in the workspace pinned precedence, which is why this was invisible;
  `error_precedence_over_defect_kind_position_and_foldability` is the table that
  pins it now, verified by building the cheaper implementation and watching the
  two discriminating rows go red.

  The Rust and Python `TreeTooDeep` messages are rewritten with it. The Rust
  facade rendered **"path depth 16 exceeds the maximum of 16"** — self-
  contradictory, and shipped for the whole of Phases 1–5 because nothing
  asserted the text — and now names which bound refused and, for the compiled
  one, `TreeBuilder::static_edge` as a remedy. Python's does **not** name that
  remedy: `tf_tree.build` declares every edge dynamic, so a static edge is
  unreachable from Python and `docs/API.md` R5 puts a binding-specific remedy in
  the binding's own prose layer. Both renderings are now asserted
  (`crates/tf_tree/tests/lookup.rs`, `tests/python/test_errors.py`).

### Fixed

- **A `Tree::lookup` that cannot be planned recompiled on every call, forever**
  (#259). `with_plan` stored a compiled `Plan` and propagated a *failed* compile
  with `?` — which returns before the store — so a frame pair that cannot be
  planned paid the full compile per lookup for the life of the process. The
  cache now holds the **result** of compiling a key, refusal included.

  The pairs this covers are the ones whose *topology* refuses: `Disconnected`
  (a declared frame whose parent link was never established, or one `reparent`ed
  out of the queried subtree), `MissingEdge` (a link carrying the `0` edge
  sentinel), `TreeTooDeep`, and a defective edge anywhere on the path. It is
  **not** a typo'd frame name — `Tree::lookup` resolves names with a lookup, not
  an intern, so an undeclared name is `UnknownFrame` before any compile — and it
  is not an edge that simply has no samples yet, which compiles fine and fails
  in `Plan::at` with `NoData`.

  It was invisible while it was cheap. At `MAX_DEPTH = 16`, checked *during* the
  walk, a 40-edge path was refused after 16 edges. `0034` separated the raw-walk
  bound from the compiled-plan bound, so a path that is going to be refused is
  now walked to its full length and — under the fold that gives `0034` its
  stated error precedence — folded in full as well. Measured on a 60-edge chain
  that walks inside `MAX_PATH_EDGES` and folds past `MAX_DEPTH`, medians of 5
  rounds of 20 000 reps, `taskset -c 2`, builds interleaved: **579.0 ns →
  291.5 ns, −49.6%**, ranges [576-585] against [291-297], landing on top of what
  a *shallow* refusal costs — what is left is resolving the two names. A third
  build carrying only #264's change moved this metric −0.9%, so the win is this
  one's.

  Safe for the same reason a cached plan is: the key carries the topology
  generation, and every refusal `compile` can produce is a verdict on the reads
  that key fixes. The two that are not — `Plan::at`'s `NoData`/`Extrapolation`
  and `Tree::plan`'s post-`fork` `ChildDetached` — are respectively out of reach
  by construction and declined explicitly. Nothing is stored unless the
  generation is still the key's when the compile returns, which makes that an
  invariant the code checks rather than an argument three facts deep. It costs
  no memory: `Result<Plan, LookupError>` is `size_of::<Plan>()`, the `Err`
  variant riding a niche in the `[Step; MAX_DEPTH]` array, pinned by a test. The
  successful-hit path is unchanged and measured flat (+0.1%, inside its range).

- **`Tree::plan` moved a `Plan`-sized array three times, none of it
  proportional to the path** (#264). Disassembled at `MAX_DEPTH = 32`, all three
  copies had survived optimisation: `fold` returning its `[Step; MAX_DEPTH]` out
  through the caller's `sret` buffer (4096 B), `Plan::new` copying that same
  array from its by-value parameter into `self.steps` (4096 B), and `compile`'s
  `Plan` into `Tree::plan`'s `sret` slot (4160 B) — **12 352 bytes of `memcpy`
  to compile a plan that is usually six steps long.**

  The array is now written once, in place: `Plan::identity` makes the buffer and
  `fold_into` fills it through `&mut Plan`, writing the steps *and* the four
  fields that are a function of them (`len`, `domain`, `dyn_count`, `first_dyn`),
  accumulated as the steps are appended rather than by a second pass over what
  was just written. That deletes the first two copies — re-disassembled,
  `fold_into` calls no `memcpy` at all. A 6-step `Tree::plan` goes
  **265.0 ns → 118.7 ns, −55.2%** (same protocol as above; ranges [261-267]
  against [114-122]). Refused paths barely move, because a refusal returns `Err`
  and never constructs a `Plan`.

  `Plan::identity` is the only constructor, so a `Plan` is a complete value the
  moment it exists and there is no half-built state a later arm could forget to
  complete — which would otherwise answer `Iso3::IDENTITY` for every stamp where
  a refusal belongs.

  The third copy stays: it is `compile` returning by value, and removing it means
  an out-parameter on a `pub` function. Marking `fold_into` `#[inline]` was
  measured and changes nothing.

- **`docs/decisions/README.md` carried an unresolved merge conflict on `main`,
  and every gate was green on it.** `<<<<<<< Updated upstream`, `||||||| Stash
  base`, `=======` and `>>>>>>> Stashed changes` sat in the middle of the
  decision status table with three copies of the `0033`/`0034`/`0035` rows, two
  of them stale. The cause was a `git rebase` on a dirty worktree: the autostash
  popped into a conflict *after* the rebase had already printed "Successfully
  rebased", and `git status` was clean afterwards because the markers were inside
  a file staged in the same command.

  All eighteen CI checks passed on it, `just lint` included.
  `scripts/artifact-versions.py` reads that very table on every run and counts
  cells per row against the header — and a conflict marker is not a table row,
  while the duplicated rows it *did* see were well-formed. Nothing else in the
  workspace reads a Markdown table for anything but its shape.

  Resolved by keeping `main`'s `0033` and `0035` rows, which were newer than the
  stash base, and the stash's `0034` row, which was the edit that pull request was
  making.

  `just no-conflict-markers` is the new gate, fifth in the family that starts
  with `just msrv`'s third arm and now runs first in `just lint` beside
  `no-build-output`. Three markers and not four: `=======` alone is half of every
  conflict and also a Markdown setext heading underline, and this repository is
  more prose than code — every conflict git writes carries the
  `<<<<<<<`/`>>>>>>>` pair, so dropping the ambiguous one costs no coverage.
  Measured against the whole tracked corpus before it was written, per
  `no-build-output`'s standard: the three matched the one corrupted file and
  nothing else. It fails on the parent commit and passes on this one.

- **`doctor`'s `TFT014` no longer calls a healthy participant in another PID
  namespace a fork inheritor and tell the operator to stop it** (#239, `0033`).
  The two faults have opposite remediations and, until this, the same sentence:
  a live participant inside `unshare -U --fork --pid`, seen from the host,
  rendered *"slot 1 pid 1, byte still HELD: a fork inheritor — byte still HELD,
  recorded pid gone … Stop the child"* — text **byte-identical** to a genuine
  surviving fork inheritor's, 1092 bytes each once the slot number and the
  interpolated pid are normalised. The accused process was alive, `state=S`, and
  owned by the user reading the report.

  The cause is that a recorded pid is namespace-local while `/proc` is not. A
  namespaced participant records pid 1, and pid 1 on the host is `systemd` with
  a different start time, so `recorded_given`'s *"the number is in use and the
  start time differs, therefore the pid was recycled"* arm fires — an arm that
  is correct for the case it was written for. Nothing in the identity record
  could tell: `boot_id` is identical across every namespace on one host and the
  kernel has no per-namespace boot id.

  **The namespace is recorded at registration, not derived at diagnosis**, and
  that is the whole design. Probing `/proc/<recorded_pid>/ns/pid` fails *open*:
  measured, it read an unrelated same-uid process at the recorded number, found
  a matching namespace, and would have *confirmed* the fork verdict with false
  confidence — the same successful-read-of-the-wrong-process class as the bug
  this classifier was last fixed for. So `Identity` carries the writer's own
  `/proc/self/ns/pid` inode and `doctor` compares it against its **own**.

  Four arms staged, all four firing before and only the right one after:
  **A** a namespaced participant seen from the host (`unshare -U --fork --pid`,
  unprivileged); **B** a host participant seen from a container over a
  bind-mounted runtime dir, which reaches the same verdict through `ENOENT`
  instead — which is why the guard sits before the whole `match probe` and not
  as an arm ahead of one branch; **C** a genuine surviving fork inheritor, the
  true positive, which fires before and after; **D** participant *and* `doctor`
  inside one bare `unshare -U --fork --pid`, where every namespace matches and
  `doctor` reported **its own participant slot** — *"slot 1 pid 1, byte still
  HELD … The record is FREE (no arena record: a read-only participant, D18)"* —
  telling the operator to stop the process printing the report. D needs a second
  guard, `readlink("/proc/self")` against `getpid()`, because there the pids are
  namespace-local and the `/proc` resolving them is the parent's; the first
  guard is structurally blind to it. Measured separately: with only the
  namespace guard, A, B and C behave and D still fires; with only the `/proc`
  guard, D behaves and A and B still fire. Neither carries the other's arms.
  Real stagings ran `6 passed, 5 fired` before and `7 passed, 4 fired` after on
  A, B and D, against isolating host controls that were `7 passed, 4 fired`
  throughout; C stayed `6 passed, 5 fired`. `tests/attach.rs`'s
  `tft014_namespace_arm_*` are the in-tree four, behind `just shm-check`, and
  arm D skips loudly where `unshare -U` is refused.

  **What this does not fix**, said here because it will otherwise be read as
  fixed: the arena's `ParticipantRecord` gains no namespace discriminator, so
  the three paths `docs/PHASE2.md` §0.0 already calls corrupting still resolve a
  namespace-local pid against the observer's `/proc`. Those are arena fields and
  a `FORMAT_VERSION` bump; this is the lock file, and neither `FORMAT_VERSION`
  nor `layout_hash` moved. One verdict does move that was not the point: a slot
  with a non-`FREE` record, a *free* byte, and a recorded process that read
  `Running` now reads `Unknown` and so becomes a *byte free* report. Reaching it
  needs the host process at the recorded namespace-local pid to have a matching
  start time, which is the pid-reuse collision the identity triple exists to
  exclude — accepted, and named so it is not a surprise. **The second guard is
  wider than that**, and the record did not price it: where the first degrades
  one record, the `/proc`-is-mine check degrades *every* slot in the file at
  once, because if `/proc` is not the observer's namespace's then no recorded
  pid in it is comparable — including the observer's own. Still the right
  trade, since on such a `/proc` the alternative reading of that same slot is an
  accusation, but it is a difference and it is written onto `recorded_given`'s
  doc rather than left to be found.

  **Two assertions here exist because a review proved the fix was otherwise
  ungated, and both were mutated to check.** Replacing the production writer's
  namespace read with a literal `0` — the fix recording nothing, in the field it
  exists to fill — left `tf_tree_ipc` 91/91, `tf_tree_cli --features shm --lib`
  124/124, `--test attach` 16/16 and `--test rendezvous` 31/31 green, because
  every `TFT014` arm hand-writes the field into a synthetic record and
  `of_self` (the constructor that *was* pinned) has one caller in the workspace
  and it is a test. Reverting `name_str`'s fallback to `unwrap_or(32)` was
  likewise green everywhere, because the compatibility record's `"node"` has a
  NUL at byte 36 and never reaches the fallback. `best_effort_never_fails` now
  asserts the recorded inode against a fresh read, and the compatibility test
  gained a sixteen-byte name with no NUL — which is what an 18-to-20-byte
  pre-`0033` name leaves in `32..48`, and which panics on the old spelling.

- **Stopping and continuing an owner — Ctrl-Z then `fg`, or a `gdb -p` attach
  and detach — no longer strands its arena.** `OwnerServer::serve`'s
  `epoll_wait` propagated `EINTR` like any other
  errno. `signal(7)` lists `epoll_wait` among the interfaces that fail with
  `EINTR` after a stop signal followed by `SIGCONT` **with no signal handler
  installed anywhere**, so "this crate installs none" was never a reason it could
  not happen.

  What it cost is out of all proportion to the cause. `serve` returned `Err`, the
  server's `Drop` unlinked the published socket, and the process *lived on*
  holding participant byte 0 and the ownership byte. §3.4 then has no exit for
  anybody: nothing serves, so no new process can join, and step 4 refuses to
  create a second arena because a participant byte is held by a process that
  genuinely is alive. The arena was permanently unreachable and the only remedy
  was killing an otherwise-healthy publisher. Nothing reported it either — the
  facade's owner thread discards the result (`let _ = server.serve(...)`).

  Measured on a staged owner: `SIGSTOP` + `SIGCONT` took it from two threads to
  one, `default.sock` disappeared, and a join returned *"an arena is alive but
  unreachable: participant slots 0x1 still hold their lock bytes (slot 0, pid
  of a living process)"*. Three controls say it is the stop/continue **pair**
  and not the act of signalling: no signal for the same interval, three
  `SIGWINCH`es
  (default-ignored), and a bare `SIGCONT` to a never-stopped owner each left
  two threads, the socket in place, and the join succeeding.

  **Two triggers are measured, and the obvious third is not.** Ctrl-Z + `fg`
  (`SIGTSTP` + `SIGCONT`) reproduces it. A debugger reproduces it too, but by a
  different mechanism and only one way round: `PTRACE_ATTACH` + `PTRACE_DETACH`
  over every tid in `/proc/<pid>/task`, which is what `gdb -p` does, wedges the
  pre-fix build, while attaching to the main thread alone does not — that tracee
  sits in ptrace-stop and the syscall restarts. A container freeze/thaw is the
  one everybody will assume and **nobody has run**: `signal(7)`'s list is scoped
  to stop signals resumed by `SIGCONT`, and a cgroup freezer is a different
  mechanism, so it is left as a suspicion rather than written down as a cause.

  The fix retries that one errno and returns every other, so an `EBADF` is still
  loud rather than an infinite spin. The regression test is
  `a_stopped_and_continued_owner_still_serves_the_rendezvous`
  (`just shm-rendezvous`): it stops and continues a real
  owner and then has a second process join, and on the parent commit it fails on
  both halves independently — the thread count (`left: Some(1), right: Some(2)`)
  and the join.

  **Not fixed, and deliberately out of scope:** what an owner should *do* when
  its server dies for a reason that is not `EINTR`. It still keeps its bytes
  silently. That is a protocol question, not a retry.

- **A creator now takes participant slot 0 atomically, so it cannot end up
  holding one integer while its arena record holds another** (#201, `0035`).
  §3.4 step 4's split-brain scan and step 5's slot acquire were two passes over
  the same bytes. `any_participant_held` probes byte 0 **first** and then up to
  63 more before returning, so byte 0 could change hands for the rest of that
  scan — and the facade indexes the lock byte and the arena record with one
  number, so a creator on any other byte hands out a tree whose liveness
  predicates disagree with themselves.

  Measured, 4000 iterations of exactly the two calls steps 4 and 5 make, against
  a second open file description toggling byte 0: **2242 took a non-zero byte**.
  Control with the racer off: 4000 took byte 0, none diverged.

  No in-tree production path occupies that window today. It is fixed anyway
  because `LockFile::try_take_participant` is public API on a published crate, so
  a downstream consumer can — and "no caller in our own tree does this" is not an
  invariant a library can offer.

  The fix is smaller and faster than what it replaces: one `F_OFD_SETLK` on byte
  0 instead of a scan, so the check and the take are a single kernel-atomic
  operation and there is no window because there is no gap. Losing that acquire
  is not a new failure mode — it is step 4's condition arriving late, and takes
  step 4's existing branch.

  Since `0028` this was **detected** rather than silent: the facade refused with
  `OpenError::ParticipantSlotDiverged`, which is not in `is_retryable`, so a
  transient race became a permanent failure (measured end to end: 210 of 400).
  That path is now unreachable from a create. The guard stays where it is, as an
  assertion.

  `--force-new` (`CreatePolicy::Always`) is deliberately not exempted. It skips
  step 4, so a contended byte 0 there means a *live* participant holds it, and
  forcing a fresh arena past one is the split brain the flag exists to resolve.
  Byte 0 is free in exactly the case the hatch is for because it is the
  **owner's** slot, held for the owner's whole life while joiners are assigned
  `>= 1` — not, as this entry first said, because the wedged arena's participants
  are dead. That was false in both directions and is corrected below (#257).

  **Not fixed:** the takeover arm (`Open::already_attached`) still reaches
  `register_any` and can still produce the divergence. Its correct slot is
  `0028` question 3, RESOLVED 2026-08-20 — the heir keeps its existing slot,
  byte and arena. This entry read "`0029` question 3, which is `draft`" and was
  wrong twice: `0029`'s question 3 was about the socket-hangup callback, and the
  question meant here was already answered when this was written.

### Changed — breaking

- **`tf_tree_ipc::self_comm` returns `[u8; 16]`, not `[u8; 32]`, and
  `tf_tree_ipc::Identity` gains `pid_ns_inode: u64` while `name` narrows to
  `[u8; 16]`** (#239, `0033`). This is a public break on a **publishing** crate,
  taken on the `0.0.x` line where every release may break every other, and said
  here rather than left to a compile error downstream. In-tree there are exactly
  three callers of `self_comm` and every `Identity` literal is a compile error
  until it names the new field, so nothing about this is silent — except two
  sites that are not, and both are in `tf_tree_ipc` itself: `name_str`'s
  `unwrap_or(32)` becomes an out-of-bounds slice on a `pub` method, and
  `to_bytes`'s `out[32..64].copy_from_slice(&self.name)` is slice-to-slice, so
  it type-checks and then panics on *every* registering `open()`. Both now spell
  the bound `self.name.len()`.

  **The on-disk record did not grow and did not move.** `name` is `32..48`,
  `pid_ns_inode` is `48..56`, the 64-byte stride is unchanged, and the second
  page of the lock file is still exactly one page. It is free because the kernel
  caps `comm` at 15 bytes plus its NUL (`TASK_COMM_LEN`) — a real record written
  by a process whose binary basename is 52 characters used 15 of the 32, with
  `47..64` zero — so the eight bytes taken were padding in every record ever
  written. In both directions the change is compatible without a version field:
  every reader NUL-trims, so an old decoder reads a new record's name correctly
  and never sees the inode, and a new decoder reads `0` in an old record, which
  is already this field's *unknown namespace*.

  `tf_tree`'s handshake is **not** affected and pads back to 32:
  `HelloRequest::client_name` is wire bytes `56..88` of an 88-byte datagram,
  pinned by `the_byte_layout_is_pinned` and by `docs/PHASE2.md` §3.7. The two
  32s were never the same 32, which is why that one site looks redundant and is
  not.

- **`IpcError::ArenaHeldButUnreachable` gains an `ownership_held: bool`, and its
  message now tells an operator which of three states they are in** (#257).
  `--force-new` (`CreatePolicy::Always`) is not the empty promise the issue
  assumed and not the unconditional remedy `RUNBOOK.md` offered. Measured, it
  creates iff **nothing is serving**, **the ownership byte is free** and
  **participant byte 0 is free** — and since byte 0 is the creator's slot
  (`0035`), held by the owner for its whole life while joiners are assigned
  `>= 1`, that reduces to *the owner is gone and non-owner participants survive*,
  which is exactly the stranded-participant case `PHASE2.md` §3.4 offers it for.
  `the_escape_hatch_creates_over_a_stranded_participant` has pinned it working
  since it was written.

  Until now the error could not tell those states apart. A live byte 0 and a
  stranded byte 3 produced the same sentence, differing only in a slot number
  whose meaning appeared nowhere in the message — so the honest answer ("stop
  that process; no force can pass it") and the useful one ("force will create
  here") were indistinguishable. The bool is read with one `F_OFD_GETLK` at the
  deadline, next to the identity record already read there, and is advisory in
  the same way: it says what was true at that instant. `Display` spends it on
  three arms, and splits the pre-existing empty-mask arm in two — its text
  claimed the ownership byte was "held for the whole open timeout", which the
  probe cannot say when it comes back free, so that case now gets its own
  sentence. That split is the one branch in this change nothing stages: reaching
  it needs a holder that lets go between the last acquire attempt and the probe,
  and its reachability is read rather than measured.

  The slot-0 arm's remedy branches on the rest of the mask, and it has to. "Stop
  that process and an ordinary open will create" is true only when byte 0 is the
  *only* byte held; with a joiner still on byte 2, stopping the byte-0 holder
  leaves `IfAbsent` refusing and makes the forced create the one thing that
  works — the opposite advice. Measured, and pinned by the `0b101` arm of
  `a_live_byte_0_refuses_both_policies_and_says_no_force_can_pass`.

  **This is a breaking change on a published crate**: `IpcError` is not
  `#[non_exhaustive]`, so a downstream `match` that destructures this variant
  field-by-field stops compiling until it adds the field or a `..`. Taken
  deliberately — every `0.0.x` is incompatible with every other, which is this
  file's opening promise, and the alternative is an error that recommends a
  recovery that cannot work.

  Two new tests pin the boundary from both sides, each with the control that
  makes it non-vacuous:
  `a_live_byte_0_refuses_both_policies_and_says_no_force_can_pass` (both policies
  return the *same* error; move the held byte to 3 and `Always` creates) and
  `a_held_ownership_byte_refuses_the_hatch_and_freeing_it_lets_one_through`
  (release only the ownership byte and the same forced create succeeds).

- **A false sentence about `--force-new` is corrected in the four live places it
  was copied to** (#257): `PHASE2.md` §3.4, `Open::register_creator`'s doc
  comment, `docs/decisions/README.md`'s `0035` row, and the `[Unreleased]` entry
  above. Each said the wedged arena the hatch exists for has *dead* participants
  whose bytes the kernel released when they died. It
  is false, and backwards: a wedge **requires** a live holder, because an arena
  all of whose holders are dead holds no participant byte at all, so §3.4 step 4
  never fires and an ordinary `CreatePolicy::IfAbsent` open already creates with
  no force involved. §3.4 also contradicted itself — the paragraph four lines
  below described the wedge as a `SIGSTOP`ped participant, which is alive and
  holding its byte.

  A fifth site said something different and equally wrong:
  `CreatePolicy::Always`'s own doc comment described what it abandons as "an
  arena whose holders are alive and whose owner is not serving", which is
  precisely the state it cannot abandon — the holder of byte 0 *is* the owner
  that is not serving. It now says: an arena whose owner is gone and whose
  non-owner holders are alive.

  `docs/decisions/0035` is `implemented` and is left alone: it quotes the
  sentence in order to retract it, and `PHASE2.md` §0.0's `--force-new` row is
  where this project corrects a frozen record's copies. That row now also records
  that `0035`'s own correction over-reached — its "None of the three delivers the
  documented escape hatch" generalises from one staged state, a live holder of
  byte 0, and is refuted by the stranded-participant test passing at this
  revision.

- **`RUNBOOK.md`'s `ArenaHeldButUnreachable` section stops telling operators to
  expect an error that path can no longer return** (#257). It said a forced
  create against a live holder of byte 0 returns
  `OpenError::ParticipantSlotDiverged`; since `0035` the create never gets that
  far, and the operator sees `ArenaHeldButUnreachable { first_slot: Some(0), .. }`
  instead — so anyone grepping their logs for the string the runbook named found
  nothing. Its escape-hatch recipe also offered `CreatePolicy::Always`
  unconditionally; it now states the two states in which that is refused.

### Added

- **`tf_tree_py` gains a `pure-hash` passthrough, and `just py-cross-check`
  compiles it** (#180). `macos-15-intel` is the last x86_64 macOS image GitHub
  Actions will offer and it disappears in August 2027; the fallback is
  cross-building x86_64 from the arm64 runner, and that was written off because
  blake3's C backend needs a target C toolchain.

  `pure-hash` (#243) removed that for `tf_tree_core` and `tf_tree` but was never
  forwarded to the binding, so a cross build of the *wheel* still died in
  blake3's build script before pyo3 was reached. Measured both ways from Linux:
  without the feature, `cargo check --target x86_64-apple-darwin` exits 101 in
  `cc` on `-arch x86_64`; with it, all three of `{x86_64,aarch64}-apple-darwin`
  and `x86_64-pc-windows-msvc` exit 0. A full `cargo build --lib` for
  `x86_64-apple-darwin` compiles all 178 objects and stops at exactly one step —
  the final link, wanting an Apple linker driver and the macOS SDK.

  So option 2 needs seven things and **six are now true**; the seventh is whether
  the arm64 runner's SDK carries x86_64 slices, which is a five-minute check on a
  real runner and cannot be answered from Linux.

  Two findings from the same experiment worth recording. **libpython is a
  non-issue** — maturin writes its own cross PyO3 config from the abi3 feature and
  passes `-C link-arg=-undefined dynamic_lookup`, so no target interpreter is
  needed. And **`shm` never enters a macOS build at all**: `tf_tree` declares
  `tf_tree_ipc` under a target table, so it is absent from the non-Linux
  dependency graph. The macOS wheel has always been deliberately different and
  says so at runtime through `has_shared_memory()` and a refusing
  `#[cfg(not(target_os = "linux"))]` arm on `open_arena` — nothing was quietly
  different.

### Fixed

- **A stamp far from the origin overflowed two arithmetic sites, and a release
  build answered wrongly rather than panicking.** An all-static path is
  answerable at *any* stamp — `Plan::span` returns `Ok(None)` and says so — which
  makes `i64::MIN` and `i64::MAX` ordinary arguments rather than pathological
  ones. Two places subtracted them signed:

  * `plan::subdivide`'s segment width. `at_adaptive(i64::MIN, i64::MAX)` on an
    all-static plan panicked in a checked build. In a release build the width
    wrapped negative, failed the `> 1` split test, and returned a two-knot
    straight line: measured on a dynamic path spanning ±2^62, endpoints -0.4989
    and -0.9953 against a true midpoint of -17.2030, for a requested tolerance of
    1e-6. No error, no panic.
  * `sample.rs`'s interpolation parameter, on the hot path, for two bracketing
    samples more than `i64::MAX` apart. A wrapped denominator makes `s` negative,
    so `Interp::eval` runs backwards past the older sample and returns a pose from
    outside the bracket entirely. Measured in release: `t.x = -4.55e43` where the
    two samples were 0 and 10, returned as `Ok`.

  Every stamp difference in `sample.rs` now goes through one `span_ns` helper that
  subtracts in `u64`, which is exact for every ordered `i64` pair rather than a
  truncation of it. Five call sites; the ordering each relies on is established
  immediately above it. This is not an endorsement of 292-year sample gaps — if a
  segment-width bound is wanted it belongs in `push` as an error naming the edge
  (R5), not in an accident of two's complement.

  Found by measuring the static/dynamic isolation guarantee rather than by reading
  the code, and the second site was found by the first one's skeptic after the
  original report scoped it to "all-static plans only" on a control that varied
  two things at once.

- **The stamp-independence of an all-static path is now pinned** (`wide_stamps.rs`).
  It was true and untested at the extremes: perturbing a folded static step for
  `t != 0` is caught near the origin by three existing tests in `lookup.rs`, and
  by nothing at `i64::MIN`. The new test asserts bit-identical results across the
  full range.

- **358 MiB of committed cargo build output is untracked, and a gate now makes
  the class unmergeable.** Four `CARGO_TARGET_DIR` siblings — `target-p`,
  `target-x`, `target-miri`, `target-stable`, 1386 files — were committed and
  merged across #237, #242 and #243. `.gitignore`'s `/target/` is *anchored*, so
  it matched a child named `target/` and none of its siblings; `git status`
  stayed clean because the files were tracked, and no test, lint or release gate
  looks at what is tracked. A fresh clone cost 112 MiB instead of 5 MiB, and the
  v0.0.4 GitHub source tarball 119 MiB instead of 9 MiB.

  **No published artifact was affected.** `cargo publish` and `maturin sdist`
  both package from a crate root, and the junk sat above every one of them —
  verified against the registries: the five 0.0.4 crates are 0.06–0.23 MiB each
  and the `transform_tree` 0.0.4 sdist is 0.67 MiB with zero `target-*` entries.

  History was left intact deliberately, so every published SHA and the `v0.0.4`
  tag still resolve. The 358 MiB therefore remains reachable in the history and a
  clone still pays it; removing it needs a force-push that would move the release
  tag, and that trade was declined.

  Two fixes, and the second is the one that matters: `.gitignore` now says
  `/target*/`, and `just no-build-output` rejects any tracked file carrying a
  cargo build-output signature (`CACHEDIR.TAG`, `.fingerprint/`,
  `.rustc_info.json`, the two lock files) regardless of what the directory is
  called. `.gitignore` had already been patched twice for this same trap, once
  per spelling — no ignore rule anticipates the next one.

- **`CLAUDE.md` and `ci.yml` both said `just lint` runs six clippy passes; it
  runs eight.** `pure-hash` (#243) added two and neither prose site was updated —
  including a comment that explicitly claimed to have counted rather than
  remembered.

### Added

- **`just artifact-versions` now reads the five crates.io front pages** (#238).
  The gate is "one release, one version", and it could not see a version written
  in prose — so four of the five publishable crates' `README.md` opened their
  Version section with `0.0.1` for three releases while the workspace was `0.0.3`.
  `tf_tree_math` had hit it first and fixed it the right way, by deleting the
  number and recording why, and **that fix reached one crate of five**: a lesson
  written into prose only propagates if the next person reads the prose.

  The rule is narrower than the obvious one, because the obvious one flaps. "No
  version-shaped literal" fails 57 times today on correct prose (`MSRV is
  **1.87**`, `Apache-2.0`, `tf_tree_math`'s SE(3) examples); a bare
  three-component rule still fails 9 times, and all 9 are deliberate — including
  the very sentence #236 added to record this bug. What ships is **a
  three-component `v?X.Y.Z` outside an inline code span**, which is 0 hits today
  and exactly 4 against `abd2fd9^`, on exactly the four defective files, with
  `tf_tree_math` silent. The exemption is safe because the defect was never in
  code: it was bold. Fenced blocks stay in scope, so a future versioned install
  snippet is covered.

  Scope is derived rather than listed — `PUBLISHABLE` plus each manifest's
  `[package] readme` — so option 2's stated cost of encoding "these five files"
  is not paid, and a crate that publishes without a `readme` key fails.

  **It has nothing to check today**, and that is the honest description: after
  #236 no tracked Markdown file makes a live claim about the current version.
  It is a regression guard.

- **`just msrv`'s prose arm now covers those same five front pages.** All five
  state `MSRV is **1.87**` and none was checked. Found while closing #238, in the
  same five files, in the same class. That arm remains a *presence* test — a
  document stating the right floor and a wrong one alongside it still passes,
  which is a separate gap and is now written down next to the loop.

---

## [0.0.4] — 2026-08-22 (the slot a killed participant keeps)

**One defect, present for the project's whole life, is the reason this release
exists.** A shared arena granted participant slots and took one back only when
the process holding it ran `Drop`. A `SIGKILL`ed writer does not, so sixty-four
abnormal read-write exits — over an arena's *whole life*, not sixty-four at once
— wedged it at `NoParticipantSlots` permanently. It hid because **a wedged arena
scores perfectly**: a ring outlives the process that filled it, so every composed
read still succeeds, off samples nobody is refreshing.

`docs/decisions/0028` is the record and this release is its whole plan. Three
collectors now reclaim a dead participant's slot — the owner's slot assigner on
the next grant that walks past it, the owner's socket-hangup callback, and
`Tree::reap_participants()` from any read-write participant, which is the only
one that can reach the owner's own slot. All three share **one** liveness
predicate and **one** `ParticipantTable::reclaim`, and that predicate is the OFD
lock byte — the only fact `docs/PHASE2.md` §5.1 permits to answer the question,
in as many words: *"Any code deciding liveness from `state` or `heartbeat` is a
bug."*

**Two things to read before upgrading.** A read-write attach over a bare file
descriptor is now *refused* — see *Changed — breaking*, which carries the port.
And `Open::await_open` returned immediately for any whole-second budget in
`0.0.3`; if you called it that way, you never waited.

### Added

- **`ParticipantTable::reclaim`** in `tf_tree_core` — free a participant slot
  whose process is gone, guarded by the state word the caller observed rather
  than by an incarnation a reaper has no way to hold. `ParticipantTable::release`
  stays the clean-detach path; this is the path for a slot whose process never
  ran `Drop`, and it also accepts `RESERVED` — the word a process killed inside
  the two-phase publication leaves behind — which `release` cannot name at all.
  `docs/decisions/0028`, plan step 1.

  **It is listed here because it is public surface, not because anything ships
  on it yet.** `tf_tree_core` is one of the five crates that publish, so a new
  `pub fn` is an API change on the `0.0.x` promise. The pieces that make #184's
  leaked slots actually get reclaimed — the liveness predicate, the owner's
  assign-time sweep, and `Tree::reap_participants()` — land on top of it.

  **The liveness verdict is not taken here.** `docs/PHASE2.md` §5.1 is normative
  that whether a participant is alive is a kernel fact; this is only the guarded
  store that acts on the decision, and it reads no `heartbeat`. A caller must
  observe the state word **before** it probes the OFD lock byte — the doc comment
  carries that obligation, and its loom model ships with two runnable controls
  that erase a live participant's record without it.

- **`Tree::reap_participants()`** — reclaim the participant records of processes
  the kernel says are gone, and return how many were collected. Sweeps the
  arena's participant table, asks the OFD lock byte about each slot that holds a
  record, and frees the ones whose byte the kernel has released.
  `docs/decisions/0028`, plan step 5.

  **It is not owner-only, and that is the point of it.** `docs/PHASE2.md` §6.3:
  *"reaping must not be owner-only — an owner-only design leaks every claim held
  at the moment the owner died."* The owner's socket-hangup reap cannot reach the
  owner's **own** slot — the owner registers itself and no socket of its own
  closes, so no hangup ever fires for it — which is why a `SIGKILL`ed owner used
  to leave a `LIVE` record over a released lock byte for the life of the segment.
  Any surviving read-write participant can now collect it.

  **Refused on a read-only tree**, returning `0` rather than reaping: reclaiming
  is a `compare_exchange` and a `PROT_READ` mapping answers one with `SIGSEGV`
  (`docs/API.md` R6, D18). `Tree::is_writable()` is how a caller tells that `0`
  from "there was nothing to collect". A tree that did not come from
  `tf_tree::open` reaps nothing either: liveness is a kernel fact about a lock
  byte (`docs/PHASE2.md` §5.1), and a heap tree has no lock file to ask.

  **It never collects its own slot**, and it does not decide from `state`: the
  word selects which slots are candidates, the kernel decides which of those are
  dead, and the word is observed *before* the byte is probed so the two cannot be
  read out of order.

  **This does not make reclamation automatic.** It runs when a participant calls
  it. The owner-side collectors that reclaim without being asked are the other
  two in this release — the slot assigner (step 3) and the socket-hangup
  callback (step 4) — and all three share this one predicate and one
  `ParticipantTable::reclaim`. The `fork` case is deliberately out of reach of
  every one of them: a forked child keeps the parent's open file description, so
  the kernel reports the byte held and they correctly decline to act.

### Changed — breaking

- **`Tree::attach_shared` and `Tree::attach_shared_at` now refuse
  `AttachMode::ReadWrite`**, with the new `ShmError::ReadWriteNeedsRendezvous`.
  A read-write attach over a bare file descriptor is a compile-time-fine,
  run-time-refused combination as of this release; `AttachMode::ReadOnly` is
  unchanged on both, and so is every path through `tf_tree::Open`.

  **A writer joins through `tf_tree::Open`.** The reason is not style. A
  read-write attach *registers a participant record* in the arena, and the
  rendezvous takes an OFD lock byte for that slot before writing the record — so
  on every rendezvous path the byte is a complete answer to "is this participant
  alive?". A descriptor carries no lock file, so a writer that arrived that way
  held a `LIVE` record with a permanently free byte: indistinguishable, by the
  byte alone, from a slot leaked by a `SIGKILL`ed process. That ambiguity is what
  stopped anything from reclaiming leaked slots, which is the wedge
  `docs/decisions/0028` exists to fix — 64 abnormal read-write exits over an
  arena's whole life wedge it at `NoParticipantSlots` permanently.

  **Both entry points, because closing one closes nothing.** `attach_shared_at`
  takes a slot number, but being *told* a slot index is not the same as having
  been granted one, and a `pub` function cannot tell the two apart.
  `tf_tree::Open`'s joiner now registers through a crate-private path whose
  precondition is the byte it is already holding.

  **Porting.** A consumer that read: attach `AttachMode::ReadOnly`, unchanged. A
  consumer that published over an inherited or `SCM_RIGHTS`-passed fd: build it
  on `tf_tree::Open` — the descriptor stops being the whole capability, and the
  process needs a runtime directory (`$TF_TREE_RUNTIME_DIR`,
  `$XDG_RUNTIME_DIR/tf_tree`, `/run/tf_tree`, or `/tmp/tf_tree-<uid>`) it shares
  with the arena's creator. The Python `mode="rw"`, the C ABI and the ROS 2
  bridge are all unaffected: none of them exposes an attach-from-descriptor
  surface, and Python's read-write mode already built a `tf_tree::Open`.

  This is a breaking change to the Rust facade, and on the `0.0.x` line that is
  what every release is permitted to be — see the note at the head of this file.
  It is a refusal rather than a deprecation because the shape being removed is
  unsound rather than merely discouraged: a deprecation warning would leave the
  slot-leak ambiguity in place for as long as anyone ignored it.

### Fixed

- **A `SIGKILL`ed read-write participant kept its arena slot for ever, and the
  owner's slot assigner decided liveness from a word it is normative not to**
  (issue #184). The assigner skipped any slot whose `ParticipantRecord::state`
  read `LIVE`, and `docs/PHASE2.md` §5.1 says that in as many words: *"Any code
  deciding liveness from `state` or `heartbeat` is a bug."* A process killed
  without running `Drop` never clears its record, so sixty-four abnormal
  read-write exits wedged an arena permanently — a budget a crash-looping node
  spends in minutes. The assigner now takes its verdict from the participant's
  OFD lock byte, through the single `reclamation_verdict` predicate, and
  **reclaims the dead record before granting the slot** — deciding without
  reclaiming would change nothing, because `fill_slot` fills only a `FREE` slot
  and the joiner would be refused the slot just judged collectable.
  `docs/decisions/0028` plan step 3.

  **The owner's hangup callback is rebased onto the same operation** (plan
  step 4): it loads the `state` word once and hands it to
  `ParticipantTable::reclaim` rather than reading an incarnation out of
  `identity()` and calling `release`. For a live word the guard is identical —
  `live_word` packs the incarnation into the word — and the callback now also
  collects `RESERVED`, which `identity()` reports as `None` and `release` could
  therefore never name: a process killed inside the two-phase publication used
  to lose its slot to everybody, for ever. **That widening is the headline of
  these two steps, so it is pinned rather than asserted**: one test per
  collector stages the `RESERVED` word — `0028` open question 4 measured that
  publication window at ~12 ns, so it is staged rather than raced, and the tests
  say so — and narrowing either caller back to `LIVE` fails exactly one of them,
  while both fail at the commit before this change.

  **No format change and nothing on the hot path.** `FORMAT_VERSION` and
  `layout_hash` are untouched, `Plan::at` is not involved, and the cost is at
  most one `F_OFD_GETLK` per slot the assigner would otherwise have skipped, on
  a handshake already measured at 97.5 µs p50. No public API changes.

  **What is still not reclaimed**, so that a green run is not read as more than
  it is: a slot whose lock byte is held by a `fork`ed child that inherited the
  descriptor is **deliberately** left alone — the kernel says the byte is held,
  and §6.2 says that is the truth (`docs/decisions/0030`). A full sweep of every
  slot, rather than of the slots one grant walks past, is
  `Tree::reap_participants()` above — the third collector, added in this same
  release, and the only one that can reach the owner's own slot.

- **A process's participant lock byte and its arena participant record could
  carry different indices** (issue #201), which every liveness predicate in the
  engine assumes cannot happen. `Open::attempt` now compares the two where they
  are paired — the single `hold_ownership` call site, the one place in the
  workspace that can see both, since `tf_tree_ipc` chooses the byte and has no
  arena dependency — and refuses with the new
  `OpenError::ParticipantSlotDiverged` instead of returning a `Tree` whose every
  liveness answer is about another process. `docs/decisions/0028` plan step 0c.

  **`CreatePolicy::Always` is the only policy that can reach it**, because §3.4
  step 4 refuses to create while any participant byte is held and the escape
  hatch skips that check by design. The state it lands on is a live *non-owner*
  holding byte 0, which `tf_tree_ipc::Session::release_ownership` produces from
  a documented §3.5 call — the route `0.0.3`'s *Known issues* entry said was not
  known. A refusal costs the caller nothing: the check runs before the owner
  server binds, so no peer ever saw the arena, and both the participant and
  ownership bytes are released with the session. Stop the process still holding
  the byte, or open with `CreatePolicy::IfAbsent`.

  **New public API** on the `0.0.x` promise: `OpenError` gains a variant. It is
  `#[non_exhaustive]`, so a caller matching with a wildcard arm is unaffected;
  both bindings forward it by `Display` and needed no change.

- **`Open::await_open` failed immediately for any whole-second budget.** It
  derives each attempt's socket timeout by subtracting elapsed time from the
  caller's budget, so `await_open(Duration::from_secs(1))` reached the handshake
  as `999.999_9xx ms`. Converting that to `SO_RCVTIMEO_NEW`'s
  `(tv_sec, tv_usec)` rounds the sub-microsecond tail up without carrying into
  `tv_sec`, producing `tv_usec == 1_000_000`, which the kernel rejects with
  `EDOM` — surfaced as `IpcError::ClientSocketSetup`, which is classified
  **terminal**, so the call returned in microseconds with a local-resource error
  rather than waiting out its budget. The per-attempt timeout is now truncated to
  whole microseconds, the ceiling counterpart of the `MIN_BACKOFF` floor that was
  already there for the `EINVAL`-on-zero case at the other end. Every existing
  test used a sub-second budget, whose remainder never reaches the last
  microsecond of a second, which is why all fifteen rendezvous tests were blind
  to it.

### Known issues

These are the two things `docs/decisions/0028` does **not** fix, plus one of the
same shape one layer up. Each has a home; none is a surprise waiting to be found.

- **A `fork`ed child keeps its parent's participant slot alive, and the kernel
  agrees with it.** The client socket is `CLOEXEC` and `fork` does not `exec`, so
  the child keeps the connection's open file description and the owner never sees
  `HUP`; and by `docs/PHASE2.md` §6.2 the child holds the participant **lock
  byte** by the same mechanism. So the byte — which this release makes the whole
  liveness predicate — answers *alive* for a process that provably cannot
  participate: no mapping, poisoned `Tree`. None of the three collectors touches
  it, deliberately, because reclaiming a slot the kernel calls held is the
  corrupting direction. Reclamation waits for the last inheritor to exit, which
  for a `multiprocessing` worker pool is the pool's lifetime. `tf_tree doctor`
  reports this case under `TFT014` with its own message, because the operator
  response is the opposite of a free byte's. Whether a child-side
  `pthread_atfork` handler may close the inherited descriptors is
  `docs/decisions/0030`, which is `draft` — and whose first open question can
  close it as *rejected*, making this a permanent documented limitation rather
  than an open hole.

- **An arena whose owner has died cannot be rejoined.** `docs/PHASE2.md` §3.5
  takeover is unwired, and as of this release the arm that would have reached it
  refuses with the new `OpenError::TakeoverUnsupported` instead of doing the
  wrong thing quietly: it `memfd_create`d a *fresh* segment, so a "taker-over"
  got an empty arena with the same name and none of the state it was inheriting.
  Recovery is unchanged and is not a signal — every mapping has to go so the
  segment is freed. `docs/RUNBOOK.md` carries the procedure where an operator
  will look for it.

- **`Tree::reap_participants` will free the record of a live process that
  created its arena with `TreeBuilder::build_shared`.** That call registers a
  `LIVE` participant record and takes **no lock byte**, because such an arena has
  no lock file at all — the fd is the capability. Every collector in this release
  keys on the byte, so every one of them reads that record as dead. Measured: a
  `build_shared` creator served through `tf_tree_ipc::OwnerServer` is reported
  `alive false` by an ordinary joiner, and the sweep CASes its record to `FREE`
  while it is still publishing.

  **Who is affected is narrow, and worth stating precisely.** It needs a
  *rendezvous* over a `build_shared` arena — that is, `OwnerServer::bind_at` and
  `serve` called by hand — because a peer only forms an opinion about a record if
  it joined and took a probe. An arena created through `tf_tree::Open` is
  unaffected (its creator holds byte 0), and so is the ordinary `build_shared`
  deployment that passes `Tree::shared_fd` to children and stands up no
  rendezvous: no peer there carries a probe. Nothing in this workspace composes
  it the affected way. **Until `docs/decisions/0031` is answered, do not call
  `Tree::reap_participants` in a process tree where anything served a
  `build_shared` arena by hand.**

- **`Tree::reparent` decides topology-lock liveness from `/proc` even when the
  tree holds an OFD probe** (issue #213) — the same §5.1 shape this release fixed
  for participant slots, one layer up, on the one path that takes A2's topology
  lock. `docs/decisions/0029` is the record. **It was `draft` when this entry
  was written and is `implemented` as of #269** — re-scoped to *the topology lock
  is a kernel lock*, and the fix is not the one this entry's phrasing implies:
  `reparent` does not consult the probe, it takes a kernel lock so that no
  inference decides the steal.

## [0.0.3] — 2026-08-19 (first with a source distribution)

**This release exists partly to prove one thing.** `0.0.2` has eleven wheels and
no sdist, because PyPI accepted the wheels and then refused the twelfth file with
`400 License-File LICENSE-APACHE does not exist in distribution` — a partial
upload no registry lets you re-cut. The metadata declared three licence files the
tarball did not carry, and `pyproject.toml`'s `[tool.maturin] include` closed
that. The gate that keeps it closed lives in the `sdist` job, before `publish`,
so a build that cannot be uploaded fails where it was built rather than after
eleven wheels have gone to the index. **This is the first release where that runs
in CI rather than locally** (issue #179).

### Known issues

- **A process's lock byte and its arena participant record can carry different
  indices** (issue #201), which every liveness predicate in the engine assumes
  cannot happen. Reproduced and pinned by
  `defect_201_a_forced_creators_record_reads_dead_while_it_is_publishing`: with a
  live holder of participant byte 0 and a `CreatePolicy::Always` creator at arena
  record 0, `Tree::participant_alive(0)` answers about the *holder's* byte, and
  the verdict flips from alive to dead when only that holder leaves — about a
  process that never stopped publishing.

  **How that state arises is not known.** The escape-hatch route is measurably
  not it: an owner plus survivors holds bytes `[0, 1, 2]`, killing the owner
  leaves `[1, 2]`, and the forced creator then takes byte 0 against record 0,
  which agree. Two attempts to construct a live non-owner holder of byte 0
  through `tf_tree::Open` alone failed — a failed construction, not an
  unreachability argument. Nothing acts on the verdict destructively today, so
  the harm is conditional on a future rescuer; `docs/decisions/0028` question 3
  is where it is settled.

- **`Tree::reparent` decides topology-lock liveness from `/proc` even on a tree
  holding an OFD probe** (issue #213), and it is the only topology-lock path in
  the facade. Recorded in `docs/PHASE2.md` §0.0 as the third place the identity
  triple is still correctness-critical.


### Fixed

- **`Tree::lookup`'s per-thread plan cache could serve one tree's compiled plan
  to another** (issue #196). The cache is `thread_local!` and shared by every
  `Tree` on the thread, but its key was `(target, source, generation)` — and all
  three agree across trees as a matter of course, because `FrameId`s are handed
  out in interning order and a built tree's generation is its declared edge
  count. Two trees built from the same names in the same order therefore
  collided, and so did a tree rebuilt after its predecessor was dropped. The key
  now carries an arena identity: a shared segment uses its existing
  `instance_uuid`, so two handles onto one segment still share plans, and every
  other backing takes a process-local counter. **No arena field was added** and
  the per-thread cache's footprint is unchanged — the extra `u64` fits in the
  entry's existing padding, measured at 2176 bytes per entry either way.

  The failure was a wrong number, never a bad read: a stolen plan naming an edge
  index the other arena does not have is refused by `ArenaView::edge`'s bounds
  check with `LookupError::UnknownEdge`.

  **`tf_tree_py` inherited this and is fixed by the same change** — its `lookup`
  calls the Rust facade and keeps no cache of its own, so any Python process
  holding two `tf_tree.Tree` objects on one thread was exposed. The C ABI never
  was: it exposes no collapsed lookup, only `tft_plan_create` and `tft_plan_at`.

- **The liveness predicate could report a running process dead** (issue #194).
  Its own doc comment states the bias it is built on — every ambiguity resolves
  to *alive*, because a false "dead" lets a rescuer take an entry from a running
  process, which is corruption, while a false "alive" only delays recovery — and
  two of its branches produced the other direction.

  A `/proc` that is not mounted fails every open with `ENOENT`, the same errno a
  genuinely dead pid produces, and that was read as proof of death: on such a
  host every participant read dead at once. The classification now depends on a
  one-shot probe of `/proc/self`, because a process that cannot see its own entry
  learns nothing from the absence of anybody else's. The `ENOENT` that remains
  ambiguous is `hidepid=2` hiding another user's process, which `docs/PHASE2.md`
  §3.10's same-user participants rule out — a dependency the code now names.

  Separately, a registrant that could not read its own start time stored `0`, and
  `0` compares unequal to every real start time, so the first reader that *could*
  read `/proc` declared that registrant dead while it was running. A stored `0`
  is now read as *unknown*, and `process_start_time` returns an `Option` so the
  sentinel is written deliberately rather than returned as though it were a
  measurement.

  **No arena field was added.** The record's `start_time` is unchanged and `0`
  was already what a fresh arena's participant region holds, so this is a
  read-side reinterpretation: `FORMAT_VERSION` and `layout_hash` are untouched.
- **`RUNBOOK.md`'s recovery from `ArenaHeldButUnreachable` told an operator to
  use a flag that does not exist** (issue #189). `--force-new` is `PHASE2.md`
  §3.4's name for the escape hatch, and the escape hatch shipped as
  `CreatePolicy::Always` on `tf_tree::Open` — a policy on whoever creates the
  arena, never a command-line flag. `tf_tree_cli` cannot usefully carry one:
  it supplies no `layout_if_creating`, so the create path it would reach ends in
  `OpenError::NoLayoutToCreate`, and it exits, so the arena would not outlive the
  command. The runbook now names the policy and shows the call; `PHASE2.md` §0.0
  carries the status row the prose is read against.

  The same entry described the wrong cause. It blamed a stopped or wedged
  participant; on this build the ordinary cause is an owner that exited while a
  *healthy* survivor kept the arena mapped, because §3.5's takeover has no
  trigger — and the wedge then lasts as long as any survivor does. The recovery
  is to stop every participant, which the entry now says.

- **`PHASE2.md` §5.1 said the identity triple is off the correctness path while
  three paths decided liveness from it** (issue #205). All three are in
  `crates/tf_tree/src/tree.rs`: the OFD probe's fallback, which hands the
  question to `record_is_alive` whenever `F_OFD_GETLK` declines to answer;
  `liveness_for`, which *is* the whole predicate for any tree that never got a
  probe — a heap tree, or one from `Tree::attach_shared`; and `Tree::reparent`,
  which steals A2's topology lock on `participant_is_alive` and never consults
  the probe even when the tree has one. The third was found verifying the first
  two.

  Recorded as a §0.0 status row, not as surgery on §5.1's NORMATIVE prose, on
  the same ground as #189's row: §0.0 outranks prose, and §5.1's wording is
  `0028`'s claimed ground. The row also names §3.10 as the reason a `hidepid=2`
  `ENOENT` is survivable — a dependency #204 put in a code comment and nowhere
  in the spec, which was #194's own complaint one layer down. **No code
  changed**: moving those paths off the triple is a design decision and belongs
  in a record.

- **`tests/frozen.rs`'s litter check failed about files it had not produced.**
  It scanned the shared `std::env::temp_dir()` for any entry whose name
  contained this process's id as a substring, so an unrelated process's scratch
  directories tripped it — roughly two runs in five, making `just shm-check`
  intermittently red. It now matches the `.{stem}.tmp.` prefix `freeze_to`
  actually writes.

### Added

- **`tf_tree_math::slerp` is public**, the shortest-arc quaternion
  interpolation `LerpSlerp` evaluates. It was reachable only by building two
  `Iso3` with throwaway zero translations, which the optimizer does not fold
  away: both arms end in the *same* out-of-line `slerp`, and what the `Iso3`
  one puts in front of it is 256 bytes of stack, two 64-byte isometries written
  out field by field, and a lerp of one zero translation into another. Measured
  as exported `extern "C"` arms at `opt-level = 3` on x86-64, that prologue is
  45 instructions bare (7 against 52) and 28 through the consumer's own
  `nalgebra` adapter (41 against 69). `ScLerp`'s
  kernel `dualquat::screw_pow` has been public since Phase 1; this removes the
  asymmetry rather than opening a new surface.

  **Two behavioural differences from the `Iso3` round trip, at the endpoints
  only** — both because `LerpSlerp::eval` answers `s = 0` and `s = 1` from a
  shortcut that never reaches the kernel:

  - At `s = 1` with `qa·qb < 0` the bare function returns `-qb` where `eval`
    answers `qb`. Same rotation, opposite components. Callers comparing
    quaternion components rather than rotations at `s = 1` will see it.
  - Inside the `1e-6`-rad LERP fallback band the bare function *renormalizes*
    at both endpoints, where `eval`'s shortcut returns its input exactly — a
    departure of about `2.7e-16` for any input whose components do not happen
    to square to exactly `1.0`.

  (The count was **one** here when this entry first landed, while `slerp`'s
  rustdoc, `docs/API.md` §6 row 16 and
  `endpoints_lose_bit_exactness_only_in_the_lerp_fallback` all said two. A third
  difference exists and is not counted with these because no rotation can reach
  it: a `-0.0` component comes back `+0.0` from the kernel and survives `eval`'s
  shortcut. It needs a hand-built `Quat`; `slerp`'s *Endpoints and degenerate
  inputs* section states it.)

- **`tf_tree` re-exports `slerp`.** The commit above made
  `tf_tree_math::slerp` public for a consumer of the *engine*, and the facade
  did not carry it — so that consumer still had to add `tf_tree_math` as a
  second direct dependency and pin it in lockstep with `tf_tree` on a line
  where every release breaks every other, which is worse than the `Iso3` round
  trip they were told to abandon. `tf_tree::slerp` is now the same item as
  `tf_tree_math::slerp`, and **`tf_tree::dualquat`** carries `ScLerp`'s kernel
  beside it — the first revision re-exported only `LerpSlerp`'s and so
  reproduced the same asymmetry one layer up, leaving an `ScLerp` consumer in
  exactly the two-dependency position this closes. The module is re-exported
  rather than the function, so `tf_tree::dualquat::screw_pow` is the same *path*
  as `tf_tree_math::dualquat::screw_pow` and not a second spelling of it
  (`PROJECT.md` §6). On the facade's **stable** tier (no `unstable`
  feature), checked by `tf_tree/tests/math_reexports.rs`, which compiles only
  if the two paths resolve to one function. `ScLerp`'s kernel is deliberately
  not re-exported beside it: `screw_pow` is reached through
  `tf_tree_math::dualquat`, and a bare name at the facade root would be a
  second spelling of that path rather than the same one.

- **`just artifact-versions` checks every Markdown table's rows against its
  header.** `docs/API.md` row 16 rendered with a phantom cell from #198 to #209 —
  and the commit that claimed to fix it escaped four of six pipes — while
  `PHASE2.md` §12.2 hid two rows' measured results entirely, because GFM discards
  every cell past the header count. Nobody proofreads what they cannot see. The
  check is escape-aware, skips fenced code, HTML comments and indented code, and
  is verified in both directions: a ragged row fails with its location, and the
  three constructions GFM does *not* render as a table stay silent.
- `just shm-check` runs `cargo nextest run -p tf_tree --features shm --lib`. The
  recipe named integration targets individually and had no `--lib` line, so an
  `shm`-gated unit test in the facade was compiled by clippy and executed by
  nothing.

- `just py-compile` gates the workspace-excluded `tf_tree_py` on a clean
  checkout. It skipped whenever there was no `.venv` — which is every CI runner
  and every first clone — so the dependency `just lint` carries covered nothing
  in the one configuration CI runs. It now falls back to the interpreter on
  `PATH` (PyO3 executes a Python to configure itself; it includes no header, and
  the fallback compiles on a host with no `Python.h`), prefers `.venv` where
  there is one, and skips only where there is no Python at all. It also carries
  `cargo fmt --check` for that crate, which `cargo fmt --all` does not reach
  because the crate is not a workspace member — and `just fmt` gained the
  matching write-mode line, so the formatting `just lint` now enforces has a
  recipe that fixes it. `ci.yml`'s `bindings` job invokes the recipe instead of
  re-spelling its two lines.
- **`just gate4-python` re-derives PHASE5 §12 gate 4's Python arm**, which had
  been a recorded 1.785× that no recipe regenerated — the shape
  `docs/benchmarks/EVIDENCE.md` exists to catch. `frozen_workers` gained
  `--python <interpreter>`, and `crates/tf_tree_bench/python/gate4_worker.py` is
  the worker it drives: same fixture, same stamp grid, same barrier, so the two
  arms differ in the worker's language and nothing else. It **reports** — the
  criterion is stated over the Rust worker and the recipe exits 0 on the FAIL it
  prints. Both arms now name their worker in the verdict line, so a pasted
  transcript carries the qualification the amendment asks for.
- **`tf_tree doctor`'s `TFT014` reports the participant-slot leak its catalogue
  row is named for** (issue #190, `docs/decisions/0028` plan step 6). A
  participant killed without running `Drop` leaves a `LIVE` record over a free
  lock byte; the owner's slot assigner skips such a record, and 64 of them wedge
  the arena at `NoParticipantSlots`. `doctor` now names each one, with the pid
  and how much of the fixed budget is spent. Severity stays **warn**, and
  **nothing is reclaimed** — `0028` is a draft record that exists so no
  reclamation lands before its predicate is settled.

  **It is not the shape #184 measured, and the finding says so.** #191 gave the
  owner a socket-hangup reap (`0028`'s *candidate B*), so a rendezvous joiner
  killed under a running owner has its record released — measured with an
  owner, a read-write joiner and a third observing process: with the joiner
  `SIGKILL`ed, its `state` word had gone `0x6` (`LIVE`) to `0x0` (`FREE`) by the
  observer's first poll 50 ms later. Seeing this finding means the slot was one
  that reap cannot reach, and the message names the five: the owner's own slot,
  a client its `epoll` never watched, an `attach_shared` participant, a
  takeover, and an owner that died inside the callback. Two leave the owner
  dead, so `doctor --attach` cannot be pointed at them at all — the check's doc
  comment and `PHASE5.md` §6's amendment both say which is which.

  **The claim half was blind in that same state and is fixed with it.** It fired
  on `owner_pid == 0`, resolved through `ParticipantTable::identity`, which
  answers for any record whose `state` word reads `LIVE` — which a `SIGKILL`ed
  writer's does until something clears it. `PHASE2.md` §5.1 forbids deciding
  liveness from `state` in as many words. A `Snapshot` now carries the
  participant table with `Tree::participant_alive` already applied — the
  kernel's `F_OFD_GETLK` on the slot's lock byte — and both halves read that one
  answer.

  Three conditions in the title stay undetected, and `PHASE5.md` §6's amendment
  says why each is a gap in the available evidence rather than in the check: a
  `RESERVED` record, which the predicate cannot tell from a healthy joiner
  mid-attach; the fork case, where the kernel truthfully reports the inherited
  byte as held; and a claim whose slot has since been re-granted, where the
  claim word carries the `ClaimRecord`'s own epoch rather than the participant's
  incarnation and so joins to whoever holds the slot now.

  **`TFT014` now skips on `doctor --from-file`, where it reported `pass`.** A
  frozen `.tft` is a byte copy of the whole arena, participant records
  included, so every slot in one names a process that exited when the freeze
  finished — and a file has no assigner for a leaked slot to wedge. A `--json`
  consumer reading `TFT014` on a `.tft` sees `skipped` with a reason where it
  saw `pass`; the `pass` was an all-clear about a question the file cannot be
  asked, and it held only because the old predicate was reading `state`.

## [0.0.2] — 2026-08-17 (wheels, no sdist)

### Fixed

- **The Python binding did not compile on macOS or Windows.** `tf_tree_py`
  imported `OpenError` and `AttachMode` unconditionally and named
  `Open`/`CreatePolicy` in `open_arena`, all of which the facade gates on
  `#[cfg(all(feature = "shm", target_os = "linux"))]`. Since the binding always
  enables `shm`, it is the *target* that decides, and four `wheels.yml` rows —
  both Windows, both macOS — failed with `E0432`/`E0433`. `open_arena` now keeps
  a paired non-Linux arm that refuses with a message naming the platform, on the
  argument `offline.rs` already records for `.tft`: a missing attribute makes a
  portable script fail with `AttributeError` somewhere unrelated.

### Note on 0.0.2's PyPI upload

**0.0.2 published eleven wheels and no source distribution**, and the sdist for
it can never be added under that version — PyPI refused it and the version is
now partially published. `pip install transform_tree` works on every platform
the wheels cover; a platform they do not cover has no source fallback until
0.0.3.

The sdist declared `License-File: LICENSE-APACHE` (and `LICENSE-MIT`, `NOTICE`)
in its `PKG-INFO` and did not contain them. maturin auto-detects those files at
the project root for PEP 639 but its sdist selection follows the cargo package
rules, which never reach the workspace root — the tarball held only
`Cargo.toml`, `PKG-INFO`, `pyproject.toml` and `README.md`. PyPI validates the
pair and returns `400 License-File LICENSE-APACHE does not exist in
distribution`. The wheels were never affected; only the sdist selection is.

`[tool.maturin] include` now names the three files, and `twine check` passes.

### Note on 0.0.1

**0.0.1 is a crates.io-only release.** All five crates are published at that
version and stay published; no wheel exists for it and none can, because the
commit it was cut from is the one that did not compile off Linux. The bug was
invisible until the `v0.0.1` tag ran `wheels.yml` for the first time in the
project's life — no workflow had run here between 2026-07-23 and 2026-08-16, and
that one triggers only on a `v*` tag.

0.0.2 is the first release published from a tag to **both** registries, and the
first to use Trusted Publishing on either: crates.io by OIDC through
`release.yml`, PyPI through its pending publisher in `wheels.yml`. Neither uses
a stored token.

## [0.0.1] — 2026-08-17 (crates.io only)

First publish: the five publishable crates went to crates.io at 00:33 UTC on
2026-08-17. **No wheel** — PyPI refused `tf_tree` as too close to an existing
project, and the distribution was renamed to `transform_tree` for `0.0.2`, so
that is the first version installable with `pip`.

Everything below is *Added*, because there is no previous release for anything
to be changed, deprecated, removed or fixed relative to.

### Added — the engine (Phase 1, `docs/PHASE1.md`)

- A pointer-free, fixed-capacity, `#[repr(C)]` transform arena. No growth, no
  realloc, no `Arc`/`Box`/`Vec` inside an arena structure; `FrameId` and
  `EdgeId` are append-only and tombstoned rather than recycled.
- Lock-free reads: per-edge seqlock sample rings, one writer per edge.
- Compiled lookup plans — `Plan` is resolved once and evaluated many times, and
  evaluation allocates nothing, takes no lock and converts nothing.
- `tf_tree_math`: `no_std`, `#![forbid(unsafe_code)]` SE(3)/SO(3), quaternion and
  dual-quaternion math. Two interpolators, ScLerp (default) and LerpSlerp.
- Integer-nanosecond stamps carrying a clock domain in the type, so mixing a
  sensor clock with a host clock is a compile error rather than a silent wrong
  answer. Cross-domain lookup is an error until Phase 8 supplies alignment.
- `Copy` error types that name the offending edge. No `String` in an error type
  or on a hot path anywhere in the workspace.
- Builder-time edge declaration
  (`docs/decisions/0004-builder-time-edge-declaration.md`), which is what sizes
  the arena.

### Added — shared memory between processes, Linux only (Phase 2, `docs/PHASE2.md`)

Behind the default-off `shm` feature; see *Absent* for the one part of this that
is specified and not wired up.

- A `memfd_create`-backed, sealed, `MAP_SHARED` arena, and a zero-diff read path
  proven by a relocation gate — the same bytes answer the same at a different
  address in a different process.
- Discovery and rendezvous: `tf_tree::open()`, a lock file whose byte 0 is
  ownership, and an attach protocol over `SOCK_SEQPACKET` + `SCM_RIGHTS` served
  by a thread rather than a daemon.
- A participant registry, liveness derived from `F_OFD_GETLK`, edge claims held
  as OFD leases so a dead writer's claim is observable, and reaping by any
  read-write participant.
- Fork poisoning: a `pthread_atfork` counter, with five destructors guarded, so
  a tree inherited across `fork()` refuses rather than corrupting.
- Per-edge page population at take-up. `docs/PHASE2.md` §0.0 records the
  measurement: 66.3 MiB → 3.8 MiB resident on an over-provisioned arena.

### Added — Python bindings (Phase 3, `docs/PHASE3.md`)

Published as the `tf_tree` wheel. The bindings go straight to Rust through PyO3,
not through the C ABI, because typed errors and zero-copy buffers do not survive
a C boundary (`docs/PHASE3.md` §0).

- `tf_tree.open()`, batch lookup with NumPy in and NumPy out and no intermediate
  allocation, and `at_into` writing directly into a caller-owned NumPy array
  (only a NumPy array — see *Partial*).
- The GIL is released above a measured work threshold rather than always or
  never.
- Free-threaded CPython is supported: the module is `#[pymodule(gil_used =
  false)]`, which is what emits the `Py_MOD_GIL_NOT_USED` slot, so importing it
  does not silently re-enable the GIL for the whole process — the failure mode
  that has no error, no warning and no failed import. Wheels are `abi3` for GIL
  builds plus version-specific `cp314t`; `3.13t` is deliberately absent and
  `abi3t` awaits CPython 3.15.
- Hand-written `.pyi` stubs and `py.typed`, checked under strict pyright.

### Added — C, C++ and ROS 2 (Phase 4, `docs/PHASE4.md`)

Of this group, only `at_with_derivatives` reaches a published artifact — it is
part of the `tf_tree` crate. The C ABI, the C++ wrapper and the ROS 2 package are
build-from-source; see *What is and is not published*.

- A two-tier C ABI: `tf_tree.h` (stable) and `tf_tree_unstable.h` (opt-in behind
  `#define TFT_ENABLE_UNSTABLE`), at ABI version 0.5, which is versioned
  independently of this crate version. Handle model, `Copy` error identifiers, a
  panic guard at every boundary, and all five pose layouts in both directions —
  reading needs matrix→quaternion, which needs Shepperd's four-branch method and
  a determinant check, because a reflection and a scaled rotation both convert
  silently into a *valid, different* answer.
- A header-only C++ wrapper, `tf_tree.hpp`, with Eigen and Sophus interop, both
  error modes (exceptions and `-fno-exceptions`), and a CMake package a consumer
  reaches with `find_package(tf_tree CONFIG)`.
- `sample_with_derivatives` / `at_with_derivatives`, pulled forward from Phase 6
  because ScLerp already computes the twist.
- `ros/tf_tree_ros`: a one-way `/tf` and `/tf_static` → arena ingest bridge, as
  an `ament_cmake` package. Ingress only — **it never writes to `/tf`**, which is
  the Phase 7 line and not an oversight.

### Added — offline, observability (Phase 5, `docs/PHASE5.md`)

- **`FORMAT_VERSION = 3`**, `layout_hash 0x3D104195` (`tf_tree doctor
  --explain-version` prints both). The Phase 6 spline regions are already
  declared in the header, absent, so that format break happens once rather than
  again in Phase 6.
- **Frozen `.tft` arenas**: `Tree::freeze_to`, `Tree::open_frozen`, `tf_tree
  freeze`, and `Tree.freeze()` from Python. The arena bytes as a memory-mapped
  file, so sixteen dataloader workers share one copy. A frozen tree answers
  bit-for-bit identically to the live one it came from, and `docs/PHASE5.md` §2
  records that as tested rather than intended — by `crates/tf_tree/tests/frozen.rs`,
  which is compiled only under `--features shm` and so runs in `just shm-check`,
  not in `just test`.
- **Bag ingestion, MCAP only** — `tf_tree ingest --bag`, `tf_tree freeze
  --from-bag`, and `tf_tree_ingest` as a library. Two passes with a
  spill-to-run-file for recordings larger than the memory cap; zstd and lz4
  chunks decode through pure-Rust codecs, so a rosbag2 or Foxglove recording
  ingests with no C build step. A truncated recording is read up to the cut and
  reported as truncated rather than refused, because a SIGKILLed recorder is how
  bags in the field end.
- **Offline Python API**: `tf_tree.open_file()` returns the ordinary `Tree`, so
  there is no parallel offline surface to learn.
- **Diagnostic counters**, default-on behind the `counters` feature, with the
  arena regions declared whether or not the feature is compiled in — so turning
  them off does not fork `layout_hash`.
- **A diagnostics catalogue, `TFT001`–`TFT019`**, behind `tf_tree doctor`, with
  `--json` (schema `tf_tree.doctor/1`), `--exit-code`, `--suppress`, and
  `--from-bag` / `--from-file` so a recording or a frozen index can be diagnosed
  without a running robot. Sixteen of the nineteen can detect something; the
  other three say they cannot rather than passing (see *Partial*).
- **`tf_tree top`**: attaches read-only, refuses `--rw`, and renders per-edge
  rate/staleness/occupancy/writer, the participant list, a rolling event feed and
  a per-edge inter-arrival histogram. `--web` serves the same data over a
  hand-rolled HTTP/1.1 loop on `std::net::TcpListener` with a `default-src 'none'`
  CSP — no new dependency, no CDN.

### What is and is not published

**Published to crates.io:** `tf_tree`, `tf_tree_core`, `tf_tree_math`,
`tf_tree_arena`, `tf_tree_ipc`. **Published to PyPI:** the `tf_tree` wheel.

**Everything else in the repository is `publish = false`**, and two of those are
worth calling out because a reader will otherwise assume they arrive with the
crates:

- **`tf_tree_cli` — the `tf_tree` / `tft` binary — is not published.** So
  `tf_tree tree`, `echo`, `doctor`, `top`, `bench`, `ingest` and `topology` are
  build-from-source in this release, and `freeze` and `participants` are
  build-from-source *with* `--features shm` on Linux, which is the only
  configuration in which those two subcommands exist at all.
  `tf_tree` on crates.io is a library: `cargo install tf_tree` installs no
  command. It does not fail, either — on cargo 1.95.0 it exits **0** after
  `warning: none of the package's binaries are available for install using the
  selected features`, so a script that installs the crate and then looks for a
  `tf_tree` on `PATH` gets a green build and a missing command.
- **`tf_tree_c` — the C ABI — is not published either**, so the C header, the
  C++ wrapper and the CMake package are build-from-source too, as are both
  `ros/` packages (they need `rclcpp`, which only exists inside `docker/tf2`).

**And in the other direction: two of the published crates do carry a binary,
both multi-process test helpers rather than anything to run.** `cargo install
tf_tree_ipc` installs `tf_tree_ipc_child`; `cargo install tf_tree --features
shm` installs `tf_tree_rendezvous_child`. (The plain `cargo install tf_tree`
above installs neither — that target is behind the feature.) Each is named for
its crate because the names they had, `ipc_child` and `rendezvous_child`, are
names neither crate has any business owning in a shared `~/.cargo/bin`, and
crates.io is one-way: a version can be yanked, never deleted. That they install
at all is a residue rather than an oversight — each is spawned by its own test
through `CARGO_BIN_EXE_*`, a compile-time guarantee, and every way to install
*nothing* trades that for a path resolved at run time, in the two most
process-dependent test suites in the repository. Both manifests carry the
measurements.

### Absent, deliberately

Each of these is a decision with an argument behind it, not a gap waiting to be
filled in a patch release.

- **A `tf2_ros::Buffer`-compatible shim, and arena → `/tf` egress.** That is
  Phase 7, and D21 gates it on operating evidence rather than scheduling it.
  `docs/PHASE7.md` §0.0 lists four gates and none is met. That document is a
  requirements artifact; its existence is not permission to build it.
- **Cross-host operation.** Phase 8. Interest-based replication, a delta-coded
  wire format, and clock-domain alignment with reported uncertainty. A
  cross-domain lookup is an error today for exactly this reason.
- **Continuous-time interpolation — cumulative B-splines with analytic
  derivatives.** Phase 6. The arena header already reserves its regions
  (`docs/PHASE5.md` §1.2), which is why this release's format break is the only
  one Phase 6 needs.
- **Visualization.** `docs/PHASE5.md` §8 is a section about not building
  something, with the argument recorded. This is the finished state, not a gap.
- **ROS 1.** There is no path and there will not be one.
- **rosbag2 `.db3` (sqlite3) ingestion.** Refused on a measured dependency
  finding — `rusqlite` vendors C, `prsqlite` records no licence on the crates.io
  index so `cargo deny` refuses it outright, and the remaining readers are header
  parsers. A `.db3` handed to `tf_tree ingest` is *diagnosed* as one, with the
  `ros2 bag convert` remedy, rather than reported as a corrupt MCAP.
- **`tf_tree serve`, and a recorder.** `docs/PHASE2.md` §9 is superseded by
  `docs/decisions/0019`; keeping an owner alive is the operator's job in this
  release, which is what D16 says it should be.

### Partial, and stated rather than papered over

- **Ownership migration when the arena's owner dies (`docs/PHASE2.md` §3.5).**
  The lock-file protocol exists and is tested; **nothing triggers it.** No
  participant watches its client socket for `HUP`. Observable consequence: kill
  the owner and every already-attached process keeps serving lookups exactly as
  specified, but **no new process can join** — it wins the ownership byte, meets
  the split-brain check against the survivors, backs off, and times out with
  `ArenaHeldButUnreachable`, for as long as any survivor lives.
- **Three diagnostic checks cannot detect anything in any configuration, and
  report that instead of passing:** `TFT002` and `TFT003` (owned by
  `tf_tree_bridge::StaticStore`, whose state is process-local) and `TFT004` (no
  arena receipt time is recorded). Eight more skip conditionally, on evidence
  rather than on capability, and each says which condition it hit.
- **`at_into` acquires its output buffer by casting to `numpy.ndarray`, not
  through the buffer protocol** (`docs/PHASE3.md` §5.5's status note). So a
  `memoryview`, or a pinned `torch`/`cupyx` allocation that is not a NumPy array,
  is **refused whatever its layout** — with an error saying so and suggesting
  `np.asarray(...)`, not a wrong answer.
- **`freeze_from_arrays`** — building a frozen index straight from NumPy arrays
  — is not implemented. Unlike the `.db3` row above this is a schedule and not a
  decision: it needs no dependency and no format change.
- **Phase 4's exit criterion is operational and is open.** A real node on real
  hardware for a sustained period, and a written log of every surprise. No amount
  of code closes it, and it is what gates Phase 7.
- **`docs/PHASE4.md` §7's benchmark gate criterion 1** is measured as four
  quotients on an interleaved ladder rather than the single ratio its own table
  names, because the single ratio's denominator moved 43% on edits that did not
  touch it. That section records all four as passing, over twelve pinned runs;
  `docs/decisions/0023` is the record that would change the wording.

### Platforms

- **Linux `x86_64`** is where this is developed and gated.
- **Linux `aarch64`** is a target and not yet evidence: the CI matrix rows exist
  and have never executed.
- **macOS and Windows** get a Python wheel with the single-process engine only —
  no shared memory, no `.tft`, no `tf_tree top` — and nothing tests it. See
  `SUPPORT.md`.

### A note on the gate

**GitHub Actions produced no run for this repository between 2026-07-23 and
2026-08-16**, so most of this release was verified by the `just` recipes run
locally on `x86_64` Linux, and by the `## 0.0 Implementation status` tables
those runs are recorded in — which is what the note at the top of this file
means by naming those tables as the source of truth.

Making the repository public restored CI, and the first runs found three latent
defects that had been invisible for the project's whole life: `c_char`
signedness on `aarch64`, an exported symbol in no header tier, and a container
with no writable `ROS_HOME`. The `ubuntu-24.04-arm` rows now execute and pass
for the first time.
`.github/workflows/ci.yml`'s header carries the evidence for the outage and the
diagnosis. Do not read a green check as verification.

<!-- The tag does not exist until the release commit creates it, so this link
     404s until then. That is the accurate state, not a broken link to fix. -->
[0.0.1]: https://github.com/NoeFontana/tf_tree/releases/tag/v0.0.1
