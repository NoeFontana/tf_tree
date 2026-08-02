# 0009: Descoping Phase 6 — covariance and CoW branches are cut, URDF leaves the engine

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** (filled in as work lands)

## Context

[`0006`](./0006-the-eight-phase-roadmap.md) cut the roadmap into eight phases and
gave Phase 6 the title *"remaining engine features"*, containing four items:

1. covariance with adjoint transport,
2. copy-on-write branches for loop-closure and multi-hypothesis evaluation,
3. cumulative B-spline interpolation with analytic derivatives,
4. URDF parsing and typed-frame codegen.

Phase 6 is the only phase in the roadmap that is not organised by
[`docs/PROJECT.md`](../PROJECT.md) §4's own stated principle — *"ordered by what
constrains what"*. Phases 1, 2, 3, 4, 5, 7 and 8 each carry a single thesis.
Phase 6 is the remainder, and a remainder is where scope drifts without anyone
deciding that it should.

**What forces the decision now rather than at Phase 6.** `PHASE5.md` §1 spent a
`FORMAT_VERSION` break (2 → 3) partly to pre-reserve Phase 6's layout, so that
"the break happens exactly once". Four header fields were committed at pinned
offsets:

| field | offset |
|---|---|
| `covariance_region_off: u32` | 160 |
| `covariance_stride: u32` | 164 |
| `spline_region_off: u32` | 168 |
| `spline_degree: u8` | 172 |

Removing a header field is cheap **today** — `layout_hash()` hashes region
strides and not header fields, the header carries ≥ 64 bytes still reserved, and
the covariance fields are written as `0` in exactly one place
(`crates/tf_tree_arena/src/heap.rs`) and read nowhere. Removing one **after
Phase 6 has populated it** is a `FORMAT_VERSION = 4` and a synchronised restart
of every participant on the robot — the precise cost §1 exists to avoid paying
twice.

Phases 4 and 5 are now closed, so this is the last moment at which the question
is free.

## Decision

**Phase 6 is renamed *continuous-time interpolation* and reduced to one item:
cumulative B-spline interpolation with analytic derivatives.** The other three
are disposed of as follows.

### 1. Covariance is descoped entirely — not deferred

`covariance_region_off` and `covariance_stride` are removed from `ArenaHeader`
and their twelve bytes returned to `_reserved_v3`. `PROJECT.md` §1's sentence
promising that "uncertainty, when it arrives in Phase 6, will be a marginal" is
replaced by a statement that `tf_tree` **does not carry uncertainty at all**, and
D2's "Uncertainty is a marginal" clause goes with it.

This is a *cut*, not a postponement. A reader must not be able to infer that
covariance is coming later.

### 2. Copy-on-write branches are cut

No arena change is required — CoW reserved no layout space. `PROJECT.md` §4 and
the "out of scope" tables in `PHASE4.md` and `PHASE5.md` stop naming it as a
Phase 6 item and name it as rejected, citing D2.

### 3. URDF leaves the engine and becomes a converter

URDF parsing is **not** a `tf_tree` engine feature. It becomes a separate,
optional tool that reads a URDF and emits the topology config format that
Phase 4 already shipped (`tf_tree topology --config`). It is explicitly *not* a
dependency of any engine crate, and **it is not built in this repository as part
of any phase** — it is listed as tooling a downstream user or a future
contributor may want.

Whoever builds it uses [`urdf-rs`](https://crates.io/crates/urdf-rs) (0.9.0,
Apache-2.0) rather than writing a parser. It is on `deny.toml`'s allow-list
already.

### 4. B-splines stay, and Phase 6 becomes about them

`spline_region_off` and `spline_degree` keep their reserved header fields and
their offsets. Phase 6's thesis is the §2 problem-table row it answers: *"no
derivatives, no continuous-time model → cannot serve as a VIO/SLAM trajectory
backbone"*.

## Rationale

### Covariance — the deciding question is not scope, it is correctness

Covariance has **no row in `PROJECT.md` §2**, the table of specific `tf2`
behaviours that define the design targets. Every other engine feature in this
project traces to a line in that table. Covariance traces to nothing: it is not
a `tf2` deficiency anyone reported.

That alone would argue for deferral. What argues for a *cut* is that the data
structure cannot make the number correct. `PROJECT.md` §1 already concedes the
tree "cannot represent cross-correlation between sibling branches", so a
composed covariance is valid only when the composed edges are independent. On a
real robot they routinely are not: `map → odom` and `odom → base_link` are both
produced by one estimator, and their errors are correlated through it. Composing
their marginals as if independent understates the true covariance — it is not
merely imprecise, it is **optimistic in the direction that causes harm**, since
the consumer of a covariance is usually gating a decision on it.

`PROJECT.md` §1's mitigation was that `tf_tree` "should say so loudly in its
docs". That is a weaker guarantee than it appears: the caller who most wants a
covariance is the least likely to re-derive whether the independence assumption
holds for their particular chain, and a documented-but-wrong number is consumed
as a right one. **We cannot ship a correct number here, so we ship none.** A
user who needs joint uncertainty needs a factor graph, which is what D2 already
says.

Cost is a secondary argument but points the same way: a 6×6 marginal is 21 f64s
= 168 bytes against a 64-byte pose slot, and adjoint transport adds a 6×6
similarity per composition hop to a query path whose entire budget is
"*d* binary searches, *d* interpolations, *d−1* compositions".

**Alternative considered — keep the reserved header fields, decide at Phase 6.**
Rejected because reserving space is not free of meaning: `PHASE5.md` §1.2 and
`doctor --explain-version` both tell operators the field is there for Phase 6,
which is a promise. Removing it now costs twelve bytes of a reserved block;
removing it later costs a format break.

### CoW branches — they serve the use case D2 rejects, and contradict three invariants

D2 is explicit: *"Do not add multi-parent support to 'handle' loop closure —
that is a factor graph and a different project."* Copy-on-write branching is a
different mechanism from multi-parent edges, but it exists to serve the same two
use cases D2 names: loop closure and multi-hypothesis evaluation. Admitting the
use case through a second door makes D2 decorative.

It is also the most structurally invasive of the four. Multiple live versions of
one edge contradict, simultaneously:

- **fixed capacity with no growth or realloc** — branches need allocation;
- **one writer per edge**, enforced by the seqlock sequence number and the
  claim lease — two versions means two writers or a versioned slot;
- **append-only `FrameId`/`EdgeId`, tombstone and never recycle** — a branch
  that is discarded wants its ids back.

That is not a feature added to the storage layer; it is a second storage model
living beside it. Nothing in `PROJECT.md` §2 asks for it.

### URDF — no, not every user needs it, and no, we should not write it

Two questions decide this, and both point outward.

**Would every `tf_tree` user need it?** No. The §2 design targets are kilohertz
sensor edges, many readers, multi-process and multi-host. A user memory-mapping
a frozen `.tft` from a bag across sixteen dataloader workers — `PHASE5.md`'s
entire adoption wedge — never sees a URDF. Nor does a VIO backbone, nor any
non-ROS consumer. URDF is a ROS robot-description convention, and treating it as
core would make a ROS artifact load-bearing in a library whose distinguishing
claim is that it needs no middleware.

**Is implementing it better than an existing implementation?** No.
[`urdf-rs`](https://crates.io/crates/urdf-rs) 0.9.0 is Apache-2.0, maintained,
and already the base of the `k` kinematics crate. Writing a URDF parser means
writing XML handling plus schema quirks — `<xacro>` expansion, `package://`
resolution, the `mimic`/`safety_controller` corners — with **no differentiation
whatsoever** from an existing crate. It is the opposite of the argument that
justified writing this engine at all.

There is also a hard constraint: the dependency budget (`tf_tree_core` = libm +
bytemuck + blake3, `tf_tree_arena` = bytemuck + optional rustix) forbids an XML
parser in either crate, so URDF could never have lived in the engine regardless
of the scope question.

**Alternative considered — a `tf_tree urdf` CLI subcommand in this repository.**
Rejected as a *phase* item, because it would make URDF support something the
project owes rather than something a user may add. Nothing prevents a
contributor from writing it later against the stable topology config format;
that is the point of emitting a config rather than a `TreeBuilder`.

### B-splines — the one item with a mandate

Kept because it is the only one of the four that answers a `PROJECT.md` §2 row,
and because it extends an axis the architecture already has rather than adding
one. Interpolation is already pluggable (ScLerp, LerpSlerp), Phase 4 already
shipped `sample_with_derivatives` — pulled forward from Phase 6 precisely
because ScLerp already computes the twist — and a spline evaluation needs a
wider bracket read, not a new region shape. Its reserved header fields are
therefore earned and stay.

## Consequences

- **Phase 6 becomes a single-thesis phase**, consistent with §4's ordering
  principle. It is also much smaller, which should be stated rather than hidden.
- **`FORMAT_VERSION` stays 3.** Removing header fields does not move
  `layout_hash` (which hashes region strides), and the freed twelve bytes go
  back to `_reserved_v3`, whose "≥ 64 bytes still reserved" assertion gets
  *easier* to satisfy, not harder. **The pinned offsets of the remaining fields
  must not move** — `spline_region_off` and `spline_degree` keep 168 and 172, and
  the freed range 160..168 becomes reserved in place.
- **`tf_tree` now states plainly that it carries no uncertainty.** This is a
  capability we are declining, and the docs must not leave a reader expecting it
  later. Anyone needing joint uncertainty is directed to a factor graph, per D2.
- **D2 is amended**: its "Uncertainty is a marginal" clause is replaced, because
  a marginal is no longer what we store — we store nothing.
- **The `tf_treed --config <file.toml|urdf>` surface in `PHASE2.md` §9 loses its
  `urdf` half.** That section is unimplemented tooling, so this costs no code;
  it does remove a promise.

  > **The surface moved, and this retraction moved with it.**
  > [`0019`](./0019-one-binary-and-topology-you-can-wait-for.md) supersedes
  > `PHASE2.md` §9: there is no `tf_treed`, and the capability is
  > `tf_tree serve --config <topology.toml>`. It takes the topology format and
  > nothing else, so the `urdf` half stays retracted — this bullet is unchanged
  > in substance, only in where it applies.
- **A future contributor may still add URDF**, against the topology config
  format, without a decision record — because it is then an ordinary tool, not
  an architectural change. That is the intended effect.

## Implementation plan

1. **`docs/decisions/0009`** (this document) and the `README.md` state table —
   verified by the record existing at `ready` and listed.
2. **`PROJECT.md`** — §1's non-goal paragraph, §2's problem-table row for
   derivatives (drop nothing; the B-spline half stands), §4's Phase 6 sentence,
   D2's uncertainty clause, and D22's "Phase 6 covariance and spline regions"
   first-consumer note. Verified by `grep -i covarian docs/PROJECT.md` returning
   only the *rejection* rationale.
3. **`crates/tf_tree_arena/src/header.rs`** — remove `covariance_region_off` and
   `covariance_stride`, extend the reserved block over 160..168, keep
   `spline_region_off` at 168 and `spline_degree` at 172. Verified by the
   existing `offset_of!` assertions for the spline fields being **unchanged**,
   by `size_of::<ArenaHeader>() == 320`, and by the `_reserved_v3 >= 64`
   assertion.
4. **`crates/tf_tree_arena/src/heap.rs`** — drop the two zeroing writes and the
   `assert_eq!(h.covariance_region_off, 0, "Phase 6, absent")`. Verified by
   `cargo nextest run -p tf_tree_arena`.
5. **`crates/tf_tree_cli/src/lib.rs`** — `doctor --explain-version`'s text stops
   promising a covariance region. Verified by the CLI's own snapshot test and by
   running `tf_tree doctor --explain-version`.
6. **`PHASE5.md`** §1.2's field table and byte-count note, and its "out of
   scope" row — verified by the numbers in §1.2 matching the header after step 3.
7. **`PHASE4.md`** §0's out-of-scope table row and the `Sample::accel` comment —
   verified by `grep -n covarian docs/PHASE4.md`.
8. **`PHASE2.md`** §9's `tf_treed --config <file.toml|urdf>` — verified by grep.
9. Full gate: `just lint`, `just test`, `just shm-check`.

Steps 3–5 are one PR (they must land together or the arena does not build);
1–2 and 6–8 may travel with it, since a doc that describes a header the code
does not have is the failure mode this record exists to prevent.

## Open questions

None. `PHASE1.md` §3.1's note that the twist convention "is what
`docs/PHASE1.md` §3.1 fixes for covariance" is left in place deliberately: the
right-perturbation convention is load-bearing for B-spline derivatives too, and
rewording it is a separate, purely editorial change.
