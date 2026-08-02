# 0019: One binary, and topology you can wait for

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** _(filled in as work lands)_

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

**And the default makes it worse.** `CreatePolicy::IfAbsent` is the default, so a
read-only consumer that starts before any publisher **creates an empty arena with
a default layout** — after which the publisher's `layout_if_creating` never runs
and the topology is permanently wrong. `CreatePolicy::Never`'s own doc comment
already names the failure: *"a consumer that creates an empty arena because the
estimator has not started yet looks healthy and publishes nothing."* The remedy
exists; the default is the trap.

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

**b. A consumer waits for topology rather than failing.** The primitives already
ship — `Tree::frames`, `Tree::edges`, `tree.frame(name)`, and the topology
generation — so the wait is a caller-side loop, exactly the shape
[`0018`](./0018-blocking-waits-belong-in-the-shim.md) settled for data:

```rust
let tree = tf_tree::Open::new().mode(AttachMode::ReadOnly).open()?;   // no create
let (target, source) = tree.await_frames(["map", "base_link"], deadline)?;
let plan = tree.plan(target, source)?;
```

`Tree::await_frames` is a convenience on the facade over that loop; **no arena
primitive, no notification mechanism, no futex** — `0018`'s argument applies
unchanged and with more force, because topology settles once at startup. A
bounded backoff is adequate and the deadline is the caller's.

**c. Headroom covers frames that arrive later.** `frame_headroom` /
`edge_headroom` already exist and `PHASE7.md` §4 J3 already relies on them for
the shim. Exhaustion is a typed error naming the knob.

**`FrameNotDeclared`'s message names (a) and (b)**, and stops naming `tf_treed`.

### 3. `0015` proceeds, scoped as the ROS answer, and its three open questions are resolved here

The two are not competitors once scoped: **the bridge owns the arena when a ROS
stack is the source of truth; `tf_tree serve` owns it when nothing else is a
natural owner.** A deployment runs one or the other, never both.

`0015`'s open questions, answered:

1. **Sizing, and a live arena whose `layout_hash` differs: refuse.** Confirming
   `0015`'s own leaning. A silent replace would strand every consumer holding the
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
- **A read-only open can now fail where it previously succeeded** — a consumer
  relying on `ro` + `IfAbsent` to bootstrap an empty arena breaks. That is
  intended and is the point; the crate is private and no such consumer exists in
  the workspace. `tf_tree_py`'s `mode="ro"` default and the C ABI's
  `tft_tree_open` both inherit the rule and must be checked, not assumed.
- **`Tree::await_frames` is new public API on the stable tier**, and is checked
  against `API.md` §7 like any other: it is tier 1 (attach), allocates, and must
  never appear on `Plan`.
- **Two owners remain possible and that is not prevented in code** — a
  deployment that runs both a bridge with `arena_name` and `tf_tree serve` on the
  same `(domain, name)` gets question 3's refusal. Documented, not designed
  against.

## Implementation plan

1. **A read-only attach implies `CreatePolicy::Never`** — `Open::open` returns a
   typed error when `mode` is read-only and `create` is not `Never`. Verified by
   a test asserting the error, and by one asserting that `ro` + default policy no
   longer creates an arena where none exists. **Mutant:** allow the combination
   ⇒ the second test finds a freshly created empty arena.
2. **`Tree::await_frames(names, deadline)`** on the facade, over the existing
   `tree.frame` / generation primitives; no arena change. Verified by a test
   where a publisher creates the arena `N` ms after the consumer starts waiting,
   asserting the consumer resolves and that it returns before the deadline.
   **Mutant:** ignore the deadline ⇒ a no-publisher case hangs instead of
   returning `Timeout`.
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
