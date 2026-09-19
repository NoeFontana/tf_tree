# 0019: One binary, and topology you can wait for

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** **steps 1–5 have landed. Steps 6–7 — the daemon — have not,
and are not scheduled.**

Steps 1-5 landed (#137, #139; `Open::require_create` rode with #139);
`tf_tree_cli`'s `--create` was kept with `requires = "rw"`. Steps 6-7 are unbuilt
and nothing waits on them. The record stays `ready`, not `implemented` (the
immutability lock, [`README.md`](./README.md)), so whoever builds `serve` can amend
it. Read "first release" wherever a release number appears: the boundary decided
is which steps ship before the first tag.

## Context

Three documents gave three answers to *who creates the shared arena, and how does
its topology get declared?*:

- **`PHASE2.md` §9** specifies `tf_treed`, a ~400-line owner daemon, never built.
- **[`0015`](./0015-the-bridge-fills-a-shared-arena.md)** gives the ROS bridge an
  `arena_name` so *it* creates the arena.
- **`RUNBOOK.md`'s `FrameNotDeclared` row** told operators to pre-declare in
  `tf_treed`'s config, which does not exist (a live defect; fixed in #137).

[`0005`](./0005-the-shared-memory-seam.md) §8 retired D16's "no takeover":
ownership is a role the kernel reassigns on an uncontended `F_OFD_SETLK`
(`Session::take_over_ownership` + `Tree::inherit_ownership`, caller-driven;
`OpenOutcome` is `Joined | Created`, [`0037`](./0037-a-takeover-is-not-a-second-open.md)
question 3; `docs/PHASE2.md` §0.0 is authoritative). Liveness, reaping and owner
death need no daemon, so most of §9 is discharged by whichever process creates
the arena.

What remains: [`0004`](./0004-builder-time-edge-declaration.md), D4 and D10 mean
**whoever creates the arena fixes its topology and capacity permanently**, so a
tree's shape depends on which process started first. That is what
`FrameNotDeclared` reports.

The defaults were also wrong: `Open::new()` defaulted to `ReadOnly` *and*
`CreatePolicy::IfAbsent` (`crates/tf_tree/src/open.rs:226-227`). It did not create
an empty arena, since `Open::open` demands a `TreeBuilder` first
(`crates/tf_tree/src/open.rs:337`) and fails with `NoLayoutToCreate`; §2a removes a
**latent** class, because documented defaults no correct program wants are a
defect, and `CreatePolicy::Never`'s doc names the hazard without anything
enforcing it.

## Decision

### 1. There is no `tf_treed` binary. The capability is `tf_tree serve`.

```
tf_tree serve --config <topology.toml> [--domain <n>] [--name <n>]
              [--participants 64] [--metrics-port <p>]
```

`PHASE2.md` §9's responsibilities minus what `0005` discharged: create and seal
the segment from the config, pre-declare frames and static edges, hold the arena
open, export metrics, on `SIGTERM` drain and exit leaving the segment alive. It
**must not** publish, claim any edge, or interpret transforms. `--config` is the
`TopologyConfig` format that `tf_tree topology --discover` writes; §9's `--lock`
and `--socket-mode` are dropped (the rendezvous owns both).

### 2. Startup ordering is fixed by three things that already exist - NORMATIVE

**a. A read-only attach implies `CreatePolicy::Never`.** `ro` plus a creating
policy is a typed error at `Open::open` (`OpenError::ReadOnlyCannotCreate`), not a
silently-created empty arena (`API.md` R6 carried on: read-only cannot create).

**b. A consumer waits rather than failing, and it is two waits.** Each is a
caller-side loop, the shape [`0018`](./0018-blocking-waits-belong-in-the-shim.md)
settled:

```text
// Two waits, because they are two different absences.
let tree = tf_tree::Open::new()
    .mode(AttachMode::ReadOnly)                      // implies CreatePolicy::Never
    .await_open(Duration::from_secs(5))?;            // wait for the arena
let [target, source] =
    tree.await_frames(["map", "base_link"], Duration::from_secs(5))?;
let plan = tree.plan(target, source)?;
```

(`text` because this block is history; the errors unify into `Box<dyn Error>`
since [`0040`](./0040-the-error-that-cannot-be-returned.md).)

`CreatePolicy::Never` against an absent arena fails *fast* with
`IpcError::ArenaAbsent` (`crates/tf_tree_ipc/src/open.rs:330-335`, pinned by
`crates/tf_tree/tests/rendezvous.rs:127-140`), so a consumer racing the
publisher's process start never obtains a `Tree` to call a frames wait on.
`Open::await_open` waits for the arena to exist; `Tree::await_frames` waits for
names interned into one that does. Folding them would slow `Never` on an absent
arena, repealing the fail-fast property supervised deployments depend on.

**The predicate is `ArenaView::find_frame`, and `await_frames` refuses two handles
outright.** `Tree::frame` is wrong in both modes: on a read-only arena it answers
`FrameError::ReadOnly` for an absent name; on a writable one it **interns and
succeeds immediately**. Since "does this name exist" has two defensible answers on
a writable tree, `await_frames` refuses (`WritableTree`, pointing at
`Tree::frame`) rather than picking silently (§3's one-diagnostic-two-meanings
rule). A frozen `.tft` is refused (`FrozenTree`): read-only and writer-free, the
poll could only burn its budget. `#[non_exhaustive]` keeps relaxation available.

Both waits are convenience over a poll loop: **no arena primitive, no
notification mechanism, no futex** (`0018` applies with more force). Backoff is
`MIN_BACKOFF` 200 us doubling to `MAX_BACKOFF` 4 ms, the rendezvous' values
(`crates/tf_tree_ipc/src/open.rs:200`), defined once in the facade. The budget is
a caller-passed `Duration`; `await_open` clamps `Open::timeout` to what is left.

**c. Headroom covers frames that arrive later** (`frame_headroom` /
`edge_headroom`, `PHASE7.md` §4 J3); exhaustion is a typed error naming the knob.
**`FrameNotDeclared`'s message names (a) and (b)** and stops naming `tf_treed`.

### 3. `0015` proceeds, scoped as the ROS answer, and its three open questions are resolved here

**The bridge owns the arena when a ROS stack is the source of truth; `tf_tree
serve` owns it when nothing else is a natural owner. A deployment runs one or the
other, never both.**

1. **Sizing, and a live arena that is not this bridge's: refuse.** The bridge
   refuses to *join* at all (`Open::require_create`), so it never reaches a hash
   comparison. (`tf_tree_arena::layout_hash()`,
   `crates/tf_tree_arena/src/layout.rs:433`, is a `const fn` over header size and
   region strides, **independent of the declared topology**, so it cannot detect
   a different config; `LayoutMismatch` remains the right refusal for a consumer
   whose *binary* differs. What a stale consumer does about a restarted bridge is
   the instance UUID's job, unsettled.) `CreatePolicy::Always` is the operator's
   explicit act. Adding an edge restarts every participant (D4, `0004`).
2. **No name derivation from `tf_prefix`.** The rendezvous is namespaced by
   `(domain, name)` and `domain_from_env` falls back `TF_TREE_DOMAIN` ->
   `ROS_DOMAIN_ID`. Deriving from `tf_prefix` would couple what `PHASE4.md` §5.6
   keeps apart and make the name unguessable. A collision is question 1's refusal.
3. **A second bridge on a held name is a rendezvous fault, not an authority
   fault**, beside `PHASE4.md` §5.4 (two publishers on one edge), not inside it:
   one diagnostic with two meanings is what `PHASE5.md` §6's `TFT017`/`TFT018`
   amendment refused. It is a startup refusal with its own message.

`0015` moves to `ready` with its Decision and seven-step plan unchanged.

### 4. Neither is a first-release blocker; the `RUNBOOK` defect is fixed immediately

The offline wedge (`.tft`, bag ingest, `doctor --from-bag`) and single-process
embedding need no shared arena. §2's three items are the first-release scope.

## Rationale

A subcommand avoids doubling the packaging surface for ~400 lines and composes
with `tf_tree topology --discover` (precedent: `consul agent`). The daemon is an
escalation, not a prerequisite: every required process is where adoption dies.
Waiting beats pre-declaring because a config is a second source of truth that
drifts from what publishers declare; pre-declaration stays available through
`tf_tree serve`. Rejected: `tf_treed` as specified; building neither; the bridge
as sole owner (a non-ROS user's only path would be ROS); an arena notification
primitive (`0018`: a `PROT_READ` consumer cannot register without giving up D18).

## Consequences

- **`PHASE2.md` §9 is superseded**; its responsibilities and definition-of-done
  items move to `tf_tree serve`; `--lock` / `--socket-mode` are retired. `0009`'s
  §9 amendment follows: `serve --config` takes the topology format only.
- **A read-only open can now fail where it succeeded:** `ro` + `IfAbsent`/`Always`
  **and** `layout_if_creating`. `Open::new()`'s `create` default moves to
  `CreatePolicy::Never`, superseding [`0005`](./0005-the-shared-memory-seam.md)
  §3.2's `// DEFAULT: IfAbsent` and `PHASE2.md` §3.2 (`0005` is immutable, so this
  record carries it). Every workspace publisher passes `create` explicitly.
- **`Open::await_open`, `Tree::await_frames` and `AwaitError` are new stable-tier
  API** (`API.md` §7: tier 1, never on `Plan`). `AwaitError` is facade-local,
  `Copy` and `String`-free; a `Timeout` variant on `tf_tree_core`'s `LookupError`
  would put a wall clock in a `no_std` crate. Neither allocates.

## Implementation plan

1. **A read-only attach implies `CreatePolicy::Never`**: `Open::open` returns
   `OpenError::ReadOnlyCannotCreate` before `RuntimeDir::resolve()`;
   `Open::new()`'s `create` default becomes `Never`. *Landed.* Verified by a test
   that **supplies `layout_if_creating`** (else it fails with `NoLayoutToCreate`
   and is vacuous), asserting the variant, then that a `Never` re-open gets
   `IpcError::ArenaAbsent`. **Mutant:** allow the combination.
2. **`Open::await_open(Duration)` and `Tree::await_frames(names, Duration)`.**
   `await_open` retries only `ArenaAbsent` and `ArenaHeldButUnreachable` and
   returns every other error verbatim (retrying cannot change them; the budget
   would replace a precise message with a timeout). `await_frames` polls
   `find_frame`, never `Tree::frame`. *Landed.* Verified by: an arena starting
   200 ms late; no publisher, giving up in bounded time; a frame interned after
   the arena exists (child arm with `frame_headroom`); a writable-tree refusal
   **outside** `rendezvous.rs`. **Mutants:** `ArenaAbsent` terminal; ignore the
   deadline; predicate on `Tree::frame`.
3. **`FrameNotDeclared`'s message and `RUNBOOK.md`'s row** name steps 1 and 2.
   *Landed.*
4. **`PHASE2.md` §9 amendment**, §0.0 rows, `PROJECT.md`, `PHASE4.md`,
   `docs/decisions/README.md`. *Landed.*
5. **`0015` to `ready`.** *Landed.*
6. **`tf_tree serve`** - `--config`, pre-declaration, `SIGTERM` drain. Verified by
   a test that starts it, attaches a read-only consumer that plans a path present
   only in the config, and asserts the segment outlives `SIGTERM`. **Not
   first-release scope; not scheduled.**
7. **`--metrics-port`** and the systemd/container examples. **Not first-release
   scope; not scheduled.**

Steps 6-7 are deliberately last: if 1-3 remove the pain, they may never be
urgent, which is the outcome this record allows.

## Open questions

None.
