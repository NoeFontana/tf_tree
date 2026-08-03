# 0019: One binary, and topology you can wait for

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** **steps 1–5 have landed. Steps 6–7 — the daemon — have not,
and are not scheduled.**

- **Steps 1–2** — **#139**, `feat/0019-await-and-ro-create`.
  `OpenError::ReadOnlyCannotCreate`, checked before `RuntimeDir::resolve()`, with
  `Open::new()`'s `create` default moved to `CreatePolicy::Never` in the same
  change; `Open::await_open(Duration)`, `Tree::await_frames([&str; N], Duration)`
  and `AwaitError`. `tf_tree_cli`'s `--create` was **kept and given
  `requires = "rw"`**, not deleted — step 1 offered both and the flag is what
  `tf_tree serve` grows into. The same PR carries `Open::require_create` +
  `OpenError::ArenaAlreadyLive`, which belongs to
  [`0015`](./0015-the-bridge-fills-a-shared-arena.md)'s step 0 rather than to any
  step here; it rode along because it rewrites the same twenty lines of `Open`.
- **Step 3** — split, and both halves are done. `RUNBOOK.md`'s
  `FrameNotDeclared` row — the live defect *Context* names — was fixed in
  **#137**, the PR that added this document, and `PHASE2.md` §13's runbook row
  was rewritten with it. `FrameNotDeclared`'s own message stopped naming `tf_treed`
  in **#139** (`crates/tf_tree_core/src/error.rs`), pinned by
  `crates/tf_tree/tests/await_frames.rs`, which asserts the rendered description
  offers `await_frames` as the remedy and does **not** contain the string
  `tf_treed`.
- **Steps 4–5** — **#137**. `PHASE2.md` §9's superseding amendment and §0.0's
  rows, `PROJECT.md`'s Phase 2 notes, `PHASE4.md`'s two references, `0009`'s §9
  amendment, `docs/decisions/README.md`, and `0015` moved to `ready`. `rg
  'tf_treed'` returns only references that name this record or describe it in the
  past tense.
- **Steps 6–7** — **not built.** `tf_tree serve`, `--metrics-port` and the
  systemd/container examples. §4 puts them outside 0.1.0 deliberately, and the
  plan's closing sentence is the disposition: *"if steps 1–3 remove the pain,
  they may never be urgent, which is the outcome this record is shaped to
  allow."* Nothing in the workspace has grown a `serve` subcommand, and nothing
  is waiting on one.

**So this record stays `ready` and is not moved to `implemented`.**
[`README.md`](./README.md) defines `implemented` as *"code shipped; PRs linked;
document frozen"* and makes it the immutability lock: a document marked
implemented is never edited to match reality, only superseded. Two of the seven
steps have shipped no code, and this record deliberately declines to say whether
they ever will — so freezing it now would mean that whoever eventually builds
`tf_tree serve` finds its specification in a document they may not amend, for
work this record scheduled and never cancelled. `ready` costs nothing and says
the true thing: the *Implementation plan* is still a work breakdown, and steps
6–7 are the part of it nobody has picked up.

## Context

Three documents currently describe three different answers to one question —
*who creates the shared arena, and how does its topology get declared?* — and one
of them tells an operator to run a program that does not exist.

- **`PHASE2.md` §9** specifies `tf_treed`, a ~400-line reference owner daemon,
  unimplemented since Phase 2. Its stated purpose is that it "holds no
  application logic, so it is the process least likely to crash."
- **[`0015`](./0015-the-bridge-fills-a-shared-arena.md)** (draft) gives the ROS
  bridge an `arena_name` so *it* creates the shared arena. That is the same job,
  reached from the ROS side, and `PHASE5.md` §9.1's benchmark cannot be completed
  without it.
- **`RUNBOOK.md`'s `FrameNotDeclared` row** — marked *(needs `tf_treed`)* — tells
  an operator to "pre-declare the static structure in `tf_treed`'s config". There
  is no `tf_treed` and no such config path. **That row is a live defect**, not a
  forward reference.

**What has already changed under all three.** D16 said ownership is *configured,
not negotiated*, and a daemon existed to make configuring it trivial.
[`0005`](./0005-the-shared-memory-seam.md) §8 retired the "no takeover" half:
ownership is a role the kernel reassigns on an uncontended `F_OFD_SETLK`, and
`OpenOutcome::TookOver` ships. Liveness, reaping and owner death are all handled
with no daemon at all. **Most of §9's responsibilities are already discharged by
whichever process creates the arena.**

**What actually remains is narrower, and it is not a lifecycle problem.**
[`0004`](./0004-builder-time-edge-declaration.md) declares edges at build time,
D4 fixes capacity, D10 makes ids append-only — so **whoever creates the arena
fixes its topology and capacity permanently**. A tree's shape therefore depends
on which process started first. That is an unusual constraint (a `tf2` user has
no arena and no such moment), and it is what `FrameNotDeclared` is really
reporting.

**And the defaults describe a participant nobody wants.** `Open::new()` defaults
to `AttachMode::ReadOnly` *and* `CreatePolicy::IfAbsent`
(`crates/tf_tree/src/open.rs:226-227`) — a configuration that asks to create an
arena it cannot write.

> **Correction, and it repeals the sentence this paragraph used to carry.** The
> first revision of this record said that combination "creates an empty arena
> with a default layout, after which the publisher's `layout_if_creating` never
> runs and the topology is permanently wrong." **It does not.** `Open::open`
> demands a `TreeBuilder` before it creates anything
> (`crates/tf_tree/src/open.rs:337`), so `ro` + `IfAbsent` + no layout fails with
> `OpenError::NoLayoutToCreate`. Reaching the empty arena needs a caller that
> *also* passes `layout_if_creating`, and no such caller exists in the workspace.
> The claim was written from the shape of the defaults rather than from the code,
> and it was repeated verbatim into four other documents before anyone ran it.

So §2a removes a **latent** class rather than an observed failure, and the honest
justification is the simpler one: a builder whose own documented defaults are a
configuration no correct program wants is a defect on its own terms.
`CreatePolicy::Never`'s doc comment already names the hazard — *"a consumer that
creates an empty arena because the estimator has not started yet looks healthy
and publishes nothing"* — and nothing enforces it. The layout requirement is an
accident that happens to mask it, not a rule that forbids it.

**What forces the decision now.** `0015` is `draft` with three open questions,
`PHASE2.md` §9 is unimplemented, and both are being weighed for 0.1.0. Building
both would ship two answers to one question and leave `tf_treed` to be discovered
as the thing that also does what the bridge does.

## Decision

### 1. There is no `tf_treed` binary. The capability is `tf_tree serve`.

`PHASE2.md` §9's daemon becomes a subcommand of the binary that already ships:

```
tf_tree serve --config <topology.toml> [--domain <n>] [--name <n>]
              [--participants 64] [--metrics-port <p>]
```

Same responsibilities as §9 minus the ones `0005` already discharges: create and
seal the segment from the config, pre-declare frames and static edges, hold the
arena open, export metrics, and on `SIGTERM` drain and exit leaving the segment
alive for existing participants. It **must not** publish, claim any edge, or
interpret transforms.

`--config` takes the topology format `TopologyConfig` already parses and
`tf_tree topology --discover` already writes. `PHASE2.md` §9's `--lock` and
`--socket-mode` are dropped: the rendezvous owns both and neither was ever a
daemon-specific concern.

### 2. Startup ordering is fixed by three things that already exist, and a daemon is none of them — NORMATIVE

**a. A read-only attach implies `CreatePolicy::Never`.** Asking for `ro` *and* a
creating policy is a typed error at `Open::open`, not a silently-created empty
arena. A read-only creator is incoherent on its face — it would create an arena
it cannot write, which is by definition the empty one nobody wants — so this
removes the class by construction rather than by advice. This is `API.md` R6
carried one step further: read-only is not merely the default, it is the thing
that cannot create.

**b. A consumer waits rather than failing — and it is two waits, not one.** The
primitives already ship, so each is a caller-side loop, exactly the shape
[`0018`](./0018-blocking-waits-belong-in-the-shim.md) settled for data:

```text
// Two waits, because they are two different absences.
let tree = tf_tree::Open::new()
    .mode(AttachMode::ReadOnly)                      // implies CreatePolicy::Never
    .await_open(Duration::from_secs(5))?;            // wait for the arena
let [target, source] =
    tree.await_frames(["map", "base_link"], Duration::from_secs(5))?;
let plan = tree.plan(target, source)?;
```

> **`text`, not `rust`, deliberately.** The three calls yield `OpenError`,
> `AwaitError` and `LookupError`, and `LookupError` implements neither `Display`
> nor `Error` — `tf_tree_core` has no `Display` impl anywhere and no `thiserror`
> — so no single `?`-chain unifies them, not even into `Box<dyn Error>`. The
> first revision of this record printed this block as compilable Rust. It is
> not, and fixing that belongs to a separate decision about giving
> `tf_tree_core`'s errors real `Display` impls, not to this one.

**Why two calls follows from a decision already made.** `CreatePolicy::Never`
against an absent arena fails *fast* with `IpcError::ArenaAbsent` by design
(`crates/tf_tree_ipc/src/open.rs:330-335`, pinned by
`crates/tf_tree/tests/rendezvous.rs:127-140`), so a consumer racing the
publisher's *process start* never reaches a frames-only wait — it never obtains a
`Tree` to call one on. `Open::await_open` waits for the arena to exist;
`Tree::await_frames` waits for names to be interned into an arena that already
does. Folding them would mean making `Never` slow on an absent arena, repealing
the fail-fast property a supervised deployment depends on, or returning a `Tree`
the caller may not use until a second wait finished. Two absences, two names.

**The predicate is `ArenaView::find_frame`, and `await_frames` refuses two
handles outright.** `Tree::frame` is the wrong predicate in both modes, for opposite reasons:
on a read-only arena it answers `FrameError::ReadOnly` for an absent name, and on
a writable one it **interns and succeeds immediately**
(`crates/tf_tree/src/tree.rs:1166-1181`) — so a wait built on it would return
instantly and wrongly on exactly the tree a publisher holds. `find_frame` never
inserts. And because "does this name exist" has two defensible answers on a
writable tree, `await_frames` refuses one rather than picking silently — §3's
rule about one diagnostic with two meanings — and points a writable caller at
`Tree::frame`, which cannot fail for absence. **A frozen `.tft` is refused for
the sibling reason**: it is read-only *and* writer-free, so the poll could only
ever run the caller's whole budget and report a timeout for something that was
never coming. Two statically-futile handles, two distinct answers
(`WritableTree`, `FrozenTree`). `#[non_exhaustive]` keeps a later relaxation
available; refusal is the reversible direction.

The backoff pair is defined once **in the facade**, for both of its waits. An
earlier draft of this section said it was "the rendezvous' own" and widened
`tf_tree_ipc`'s constants to `pub` to share them — that coupling is decorative,
since the two are nested loops over different work, and it bought a crate's
public API for two numbers.

Both are convenience on the facade over a poll loop; **no arena primitive, no
notification mechanism, no futex** — `0018`'s argument applies unchanged and with
more force, because topology settles once at startup. A bounded backoff is
adequate — `MIN_BACKOFF` 200 µs doubling to `MAX_BACKOFF` 4 ms, the same two
values the rendezvous picked (`crates/tf_tree_ipc/src/open.rs:200`, `:203`),
restated in the facade rather than shared with it for the reason in the
paragraph above — and the budget is a `Duration`
the caller passes, matching `Open::timeout` rather than introducing this
workspace's first public `Instant` deadline. `await_open` clamps `Open::timeout`
to what is left, so one held-but-unreachable attempt cannot overrun the whole
wait.

**c. Headroom covers frames that arrive later.** `frame_headroom` /
`edge_headroom` already exist and `PHASE7.md` §4 J3 already relies on them for
the shim. Exhaustion is a typed error naming the knob.

**`FrameNotDeclared`'s message names (a) and (b)**, and stops naming `tf_treed`.

### 3. `0015` proceeds, scoped as the ROS answer, and its three open questions are resolved here

The two are not competitors once scoped: **the bridge owns the arena when a ROS
stack is the source of truth; `tf_tree serve` owns it when nothing else is a
natural owner.** A deployment runs one or the other, never both.

`0015`'s open questions, answered:

1. **Sizing, and a live arena that is not this bridge's: refuse.** Confirming
   `0015`'s own leaning.

   > **Correction — the mechanism named here does not detect what the question
   > is about.** This resolution originally said "a live arena whose
   > `layout_hash` differs". `tf_tree_arena::layout_hash()`
   > (`crates/tf_tree_arena/src/layout.rs:433`) is a `const fn` over
   > `ArenaHeader`'s size and alignment and the region *strides* — it is
   > **independent of the declared topology**, so two arenas built from
   > different configs by the same binary hash identically. The conclusion
   > survives, by a better route: the bridge refuses to *join* at all
   > (`Open::require_create`), so it never reaches a hash comparison.
   > `LayoutMismatch` remains the right refusal for a *consumer* whose binary
   > differs, which is the case it was actually built for. What a stale
   > consumer should do about a restarted bridge is the instance UUID's job and
   > is not settled here. A silent replace would strand every consumer holding the
   old mapping, and `LayoutMismatch` already exists as an attach error naming
   both values. `CreatePolicy::Always` is the operator's explicit act and already
   documents itself as "never take this path automatically." Adding an edge to
   the config is a restart of every participant, and that is stated rather than
   engineered around — it is D4 and `0004` being what they are.
2. **No name derivation from `tf_prefix`.** The rendezvous is already namespaced
   by `(domain, name)`, and `domain_from_env` falls back `TF_TREE_DOMAIN` →
   `ROS_DOMAIN_ID` — which is precisely the convention two robots on one host
   already use. Deriving a name from `tf_prefix` would couple two things
   `PHASE4.md` §5.6 keeps apart *and* make the name unguessable for an operator
   who has to attach to it. A collision is the refusal in question 1.
3. **A second bridge on a held name is a rendezvous fault, not an authority
   fault.** It belongs beside `PHASE4.md` §5.4's machinery, not inside it. §5.4
   is about two publishers on one edge, with per-edge attribution; arena
   ownership is a different condition, and folding them together would give one
   diagnostic two meanings — the exact error `PHASE5.md` §6's `TFT017`/`TFT018`
   amendment refused when it declined to reuse an existing id. It reports as a
   startup refusal with its own message, and `doctor` reports it as a
   participant/rendezvous condition.

`0015` moves to `ready` with its Decision and its seven-step plan unchanged.

### 4. Neither is a 0.1.0 blocker; the `RUNBOOK` defect is fixed immediately

`tf_tree serve` blocks nothing that 0.1.0 leads with — the offline wedge (`.tft`,
bag ingest, `doctor --from-bag`, dataloaders) involves no shared arena at all, and
single-process embedding involves no rendezvous. §2's three items are the 0.1.0
scope; `tf_tree serve` and `0015` follow.

## Rationale

**Why a subcommand and not a second binary.** A second binary doubles the
packaging surface — crates.io, distro package, container image, systemd unit,
every install document — for ~400 lines. Users already have `tf_tree` and have
run `doctor`, `top` and `topology`; `tf_tree serve` is discoverable from
`--help`, and `tf_treed` is a separate thing they must learn exists. It also
composes: `tf_tree topology --discover` writes the config that `tf_tree serve`
consumes, using tools that already ship. `consul` is the precedent — one binary,
`consul agent` is the daemon and `consul members` is the diagnostic — and it has
aged well.

**Why the daemon is an escalation and not a prerequisite.** Every process a user
is *required* to run is a place adoption dies. `tf2`'s story is "already in your
ROS install"; if ours becomes "install a library, run a daemon, and write a config
file", the people who would have tried it do not. §2 makes the zero-config path
correct on its own, which is what makes the daemon optional — and an optional
daemon can be built late, or never, without anyone being stuck.

**Why the read-only/create interaction is a hard error rather than a lint.** It
is the difference between a footgun documented in a doc comment nobody reads at
3 a.m. and a class that cannot be expressed. The existing comment on
`CreatePolicy::Never` proves the hazard was understood and that understanding it
was not sufficient.

**Why waiting beats pre-declaring.** Pre-declaration requires a config file that
is a second source of truth about topology, which then drifts from what the
publishers actually declare — and `PHASE5.md` §6's `TFT007` amendment already
records what it costs when a *measured* thing and a *declared* thing are confused
for each other. Waiting requires nothing of the deployment and no second
artifact. Pre-declaration remains available through `tf_tree serve` for operators
who want determinism, which is the right place for it: an explicit choice, not
the price of admission.

**Alternatives considered.**

*Build `tf_treed` as specified.* Rejected: it makes a daemon the answer to a
problem `CreatePolicy::Never` plus a wait loop already solves, and charges every
user the packaging cost for it.

*Build neither, and leave `layout_if_creating` as the only answer.* Rejected:
it leaves the `RUNBOOK` defect, leaves the `ro`-creates-empty-arena footgun, and
leaves `PHASE5.md` §9.1's benchmark arm unmeasurable.

*Make the bridge the only owner and drop `tf_tree serve` entirely.* Tempting, and
correct for every ROS deployment. Rejected because it makes a non-ROS user's only
path "run the ROS bridge", which contradicts the project's position that it is not
a ROS component — `PHASE4.md`'s bridge is one ingest path, not the product.

*A notification primitive in the arena so consumers learn of topology changes.*
Rejected by `0018` for data and rejected here for the same reason: a `PROT_READ`
consumer cannot register on one without giving up D18's boundary.

## Consequences

- **`PHASE2.md` §9 is superseded.** Its responsibilities move to `tf_tree serve`
  and its definition-of-done items ("ships with a systemd unit and a container
  example") move with them. Its `--lock` / `--socket-mode` flags are retired.
- **`0009`'s amendment to §9 survives and follows it.** URDF is still not owed by
  any phase; `tf_tree serve --config` takes the topology format only.
- **`tf_tree_cli` acquires a long-running mode**, which it did not have. That is
  a real change in what that crate is: `doctor` and `top` are bounded or
  interactive, `serve` is a supervised process. It gains a signal-handling path
  and a metrics endpoint, and its tests gain a process-lifetime dimension.
- **A read-only open can now fail where it previously succeeded** —
  specifically a consumer passing `ro` + `IfAbsent`/`Always` **and**
  `layout_if_creating`. Without the layout that combination already failed, with
  `NoLayoutToCreate`, so the observable change is narrower than this record
  first claimed. `Open::new()`'s `create` default also moves to
  `CreatePolicy::Never` (see the plan's step 1), which supersedes
  [`0005`](./0005-the-shared-memory-seam.md) §3.2's `// DEFAULT: IfAbsent` and
  `PHASE2.md` §3.2 on that one point — `0005` is `implemented` and immutable, so
  this record carries the supersession. Every workspace publisher already passes
  `create` explicitly, so none changes.
- **`Open::await_open`, `Tree::await_frames` and `AwaitError` are new public API
  on the stable tier**, checked against `API.md` §7 like any other: tier 1, and
  never on `Plan`. `AwaitError` is facade-local, `Copy` and `String`-free in the
  shape of `OpenError` — *not* a `Timeout` variant on `tf_tree_core`'s
  `LookupError`, which would put a wall-clock concept in a `no_std` crate `0018`
  deliberately keeps free of one and add an unreachable variant to every
  hot-path read's return type. Neither allocates: `await_frames` is
  `[&str; N] -> [FrameId; N]`, so the earlier note that it "allocates" is
  withdrawn.
- **Two owners remain possible and that is not prevented in code** — a
  deployment that runs both a bridge with `arena_name` and `tf_tree serve` on the
  same `(domain, name)` gets question 3's refusal. Documented, not designed
  against.

## Implementation plan

1. **A read-only attach implies `CreatePolicy::Never`** — `Open::open` returns
   `OpenError::ReadOnlyCannotCreate` when `mode` is read-only and `create` is not
   `Never`, checked before `RuntimeDir::resolve()` so a misconfiguration reports
   as itself rather than as a missing runtime directory. `Open::new()`'s `create`
   default becomes `Never` in the same change, so the builder's defaults are the
   *consumer* and the error is reachable only by writing both halves explicitly.
   The alternative — leaving the default at `IfAbsent` and patching the free
   `tf_tree::open()` — ships a builder whose documented defaults are an error,
   and is rejected for that reason. `tf_tree_cli`'s `--create` gains
   `requires = "rw"` or is deleted; `attach.rs` never passes a layout, so it
   cannot create anything today either.
   Verified by a test that **supplies `layout_if_creating`** — without it the
   combination already fails with `NoLayoutToCreate` and the assertion is
   vacuous — asserting the new variant, then asserting the machine is still empty
   by re-opening with `CreatePolicy::Never` and getting `IpcError::ArenaAbsent`.
   **Mutant:** allow the combination ⇒ the first open returns `Ok` and the second
   finds a freshly created empty arena instead of `ArenaAbsent`.
2. **`Open::await_open(Duration)` and `Tree::await_frames(names, Duration)`** on
   the facade; no arena change. `await_open` retries only `ArenaAbsent` and
   `ArenaHeldButUnreachable` — the publisher-mid-start window — and returns every
   other error verbatim, because retrying cannot change them and burning the
   budget would replace a precise message with a timeout. `await_frames` polls
   `ArenaView::find_frame`, never `Tree::frame`, and refuses a writable tree.
   Verified by: a consumer waiting for an arena that starts 200 ms late; a wait
   with no publisher at all, asserting it gives up inside a bounded elapsed time;
   a frame interned after the arena exists (needs a child arm with
   `frame_headroom`, since the existing fixture has none); and a writable-tree
   refusal test **outside** `rendezvous.rs`, so plain `just test` gates it.
   **Mutants:** classify `ArenaAbsent` as terminal ⇒ the late-start test returns
   immediately; ignore the deadline ⇒ the no-publisher case hangs rather than
   giving up; build the predicate on `Tree::frame` ⇒ the read-only wait never
   resolves *and* the writable case returns `Ok` with a freshly interned id for a
   name nobody declared.
3. **`FrameNotDeclared`'s message and `RUNBOOK.md`'s row** name steps 1 and 2 and
   stop naming `tf_treed`. Verified by review and by the runbook's own check that
   every row names a real remedy.
4. **`PHASE2.md` §9 amendment**, `PHASE2.md` §0.0's status rows, `PROJECT.md`'s
   Phase 2 note, `PHASE4.md`'s two references, and `docs/decisions/README.md`.
   Verified by `rg 'tf_treed'` returning only historical references that name
   this record.
5. **`0015` to `ready`** with its questions marked resolved here. Verified by
   review; its seven-step plan is unchanged and lands separately.
6. **`tf_tree serve`** — the subcommand, `--config`, pre-declaration, `SIGTERM`
   drain. Verified by a test that starts it, attaches a read-only consumer that
   plans a path present only in the config, and asserts the segment outlives
   `SIGTERM` for that consumer. **Not 0.1.0 scope.**
7. **`--metrics-port`** and the systemd/container examples. **Not 0.1.0 scope.**

Steps 1–5 are 0.1.0 work and are independent of 6–7. Steps 6–7 are the daemon and
are deliberately last: if steps 1–3 remove the pain, they may never be urgent,
which is the outcome this record is shaped to allow.

## Open questions

None.
