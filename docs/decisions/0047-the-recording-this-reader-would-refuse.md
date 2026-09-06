# 0047: the recording this reader would refuse

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** (filled in as work lands)

## Context

`docs/PHASE2.md` §10 makes three promises under one crate name:

> - **Record.** A read-only participant tapping every edge, writing MCAP with
>   two channels: `tf_tree/topology` (declaration and mutation events) and
>   `tf_tree/samples` (`edge_id`, `stamp`, `pose`).
> - **Replay.** Reconstructs an arena from a recording and re-publishes
>   deterministically.
> - **The test that matters — NORMATIVE.** Replay one recording into a
>   `HeapArena` and a `MappedArena` […] and assert **bit-identical `f64`
>   results**.

There is no such crate:

```
$ grep -rn tf_tree_record --include=*.rs --include=*.toml .
$ echo $?
1
```

The name survives only in prose — `docs/PROJECT.md`, `docs/PHASE2.md`,
`docs/benchmarks/tf2.md` — while all three promises have been answered, in
three different places, by things that are not it.

**(c) is shipped and gated.** `crates/tf_tree_cli/tests/replay_bit_identity.rs`
builds a heap arena and a `build_shared` mapped arena from one `TreeBuilder`
shape, fills both with one `replay()` function from one `Vec<FixtureMessage>`,
runs an identical straddling query set, and compares raw `[u64; 7]` bits behind
two anti-vacuity guards. It is `#![cfg(all(feature = "shm", target_os =
"linux"))]`, so `just test` never reaches it — `cargo nextest list -p
tf_tree_cli` does not list it and the same command with `--features shm` does.
What runs it is `just shm-check` (the `cargo nextest run -p tf_tree_cli
--features shm --test replay_bit_identity` line inside that recipe), which
`.github/workflows/ci.yml` invokes as a step.

**The read half of (b) is shipped, one phase later and under another name.**
[`0006`](./0006-the-eight-phase-roadmap.md) (`ready`) re-cut the roadmap and
made the new Phase 5 *"bag ingestion"*; `tf_tree_ingest` is that, and it
reconstructs a `tf_tree::Tree` from an MCAP recording in two passes with
canonical declaration order. So **§10 and `0006`'s Phase 5 both claim
record/replay**, and nothing has said which one is in force. That is the
tension this record settles.

**The process shape (a) would need already exists, narrowly.** `tf_tree top` is
a long-lived, foreground, read-only-attached CLI subcommand with `--interval`
and an unbounded `--iterations 0` loop, which refuses `--rw` and writes no
participant record. What it proves is exactly that: *a long-lived read-only
poller needs no daemon in this architecture*. It proves nothing about capturing
poses — its own module doc says it performs **no lookups**, and that consecutive
captures often read the same sample.

No decision record covers §10. The absence was checked by subject rather than by
crate name — `grep -rn 'record/replay\|recorder' docs/decisions/` — because a
grep for `tf_tree_record` cannot establish it, and the subject grep is what found
`0006`.

## Decision

**§10(c) is met.** `crates/tf_tree_cli/tests/replay_bit_identity.rs` is the
NORMATIVE test, and `docs/PHASE2.md` §15's box for it is ticked.

**§10(a) Record and §10(b) Replay are DECLINED, not deferred.** There is no
`tf_tree_record` crate, no `tf_tree record` subcommand and no `tf_tree replay`
subcommand, and none is owed. §10's dependency-table row — a new crate carrying
`mcap` and `serde`, *"isolated here specifically so D14 holds for the core"* —
is retired with it: MCAP arrived in `tf_tree_ingest`, the isolation argument was
honoured, and no crate in this workspace declares `serde` at all (`grep -rn
'^serde' crates/*/Cargo.toml Cargo.toml` finds only `serde_json`).

Two arguments carry the decline. Each is stated at the strength it has.

### 1. Its artifact is one nothing in this tree can read

§10 specifies the channels `tf_tree/topology` and `tf_tree/samples`.
`crates/tf_tree_ingest/src/source.rs` accepts a channel only when its **schema**
is `tf2_msgs/msg/TFMessage` or `tf2_msgs/TFMessage`, and `docs/PHASE5.md` §3.3
is explicit that discovery is by schema and not by topic name. Neither of §10's
channels carries a `tf2_msgs` schema. So the recorder's own output would be
refused by the only MCAP reader this repository has, and closing that means a
second MCAP reading path beside the one that ships — which `CLAUDE.md`'s hard
rules and `docs/PROJECT.md` §6 both name as the design smell to avoid.

It would also be another spelling of the transform stream, in a tree that
already carries several. Enumerated rather than counted, because a count here is
a measurement with today's date on it and the first enumeration of this list
missed one:

| where | what |
|---|---|
| `crates/tf_tree_core/src/buffer.rs` | `SampleRing` — the arena's own ring |
| `crates/tf_tree_ingest/src/cdr.rs` | `TransformStamped` — the MCAP/CDR read path |
| `crates/tf_tree_bridge/src/lib.rs` | `Sample` — the ROS-independent bridge |
| `crates/tf_tree_c/src/bridge.rs` | `tft_bridge_sample` — the same, across the C ABI, with its own `struct_size` versioning |
| `crates/tf_tree_bench/src/replay.rs` | `Sample` / `TfStream` — the `.tfstream` ASCII corpus |
| `crates/tf_tree_bench/src/fixture.rs` | `PushSample` — the observed push stream the doctor checks consume |
| `crates/tf_tree_ingest/src/fixture.rs` | `FixtureMessage` — the synthetic MCAP writer, test-only, behind the default-off `fixture` feature |

Two of those differ only in whether a field is typed and whether an ABI size
travels with it. That is the shape a recorder would add one more of.

### 2. Phase 2's Definition of Done does not ask for it

`docs/PHASE2.md` §15 has one box naming §10, it is the NORMATIVE test, and it is
ticked. No box asks for the record/replay tooling.

**This argument runs against `CLAUDE.md`'s own precedence rule and cannot stand
alone.** That rule says a spec's §0.0 status table is the source of truth over
its own prose — and §15 *is* prose. §0.0's own row said *"Not implemented"*,
which is a **status and not a decline**: it recorded that nothing was built and
left open whether something was owed. So §15 cannot retire §10 by itself, and
the honest reading is that the DoD is corroboration for a decision taken here
rather than the decision itself. Rewriting §0.0's row is part of this record's
implementation plan for exactly that reason.

### What is deliberately *not* an argument here

**Losslessness.** It is tempting to decline §10 on the ground that a read-only
tap cannot be lossless — [`0018`](./0018-blocking-waits-belong-in-the-shim.md)
forbids any notification primitive in the arena, and the ring retains
`capacity - 1` samples, so a poller can never recover a sample it missed.

**§10 does not ask for losslessness.** Its Record bullet says *"tapping every
edge"*, which is coverage of the edge set, not capture of every sample; the
section contains no completeness word at all. And `SampleRing`'s `head` is
documented as a *"monotone count of samples ever published"*, so a poller can
report its own drop count exactly. What that reasoning establishes is a
constraint on the **shape** any such tool would have to take — a self-reporting
lossy sampler — which belongs in *Alternatives*, not in the case for declining.
Declining a section for failing a requirement it never made would be the same
defect this record is written to stop.

## Consequences

- **Nobody can capture a live arena's push stream to a file**, and this record
  does not offer a substitute for it.
- **§10's second payoff is retired with it, and it was never delivered.** §10
  promised *"a regression corpus"* for subsequent phases and *"§12 real robot
  data instead of synthetic input"*. `docs/PHASE5.md` §0.0's §3 row is what to
  read before assuming otherwise: *"Nothing in this repository is a rosbag2
  bag"*, `/testdata/bags/` is `.gitignore`d, the nearest thing to real data is
  `testdata/tfstream/indoor_atelier.tfstream` — which no ingest test reads and
  which is not an MCAP — and *"what every §3 test reads is a recording this
  crate wrote"*. The one committed `.mcap`,
  `crates/tf_tree_ingest/testdata/zstd_conformance.mcap`, is decoder-conformance
  evidence and its own attribution says so. Declining §10 therefore does not
  cost a corpus that exists; it stops promising one that never did.
- **A revival has a shape, and it carries less than §10.** See *Alternatives*
  (ii). Recorded so a successor does not re-derive it, and so that the loss is
  visible before the work starts.
- **Two narrower gaps are real and are not closed by declining the recorder.**
  `tf_tree_ingest`'s fill calls `builder.build()` and never `build_shared`, so
  the shipped ingest cannot produce the mapped half the NORMATIVE test needs —
  which is why that test carries its own `replay()`. And the test's module doc
  and one inline comment claim a round trip through MCAP that does not happen.
  The second is corrected by this record's plan; the first is an open question
  below, because nothing owes it.
- **§15's §10 box gains a clause naming where its evidence runs**, which it does
  not today while its neighbours do. At least one other box in that list has the
  same omission — the one citing
  `another_process_reads_the_same_arena_bit_identically` — so this is a fix to
  one row and not a sweep.

## Alternatives considered

### (i) Build it as specified

Rejected on argument 1: a new crate, a new schema, and a second MCAP reading
path to make the new schema readable.

### (ii) `tf_tree record --format tf2_msgs/TFMessage`, so the output ingests

**This is the honest revival path** if a live tap is ever needed, and it should
be a subcommand on the binary that already ships — `tf_tree top`'s shape — never
a new crate and never a new schema.

**It cannot carry §10's `tf_tree/topology` channel.** `tf2_msgs/TFMessage` is a
transform-list schema; it has no representation for a declaration or a mutation
event. So a recorder emitting a schema `tf_tree_ingest` reads is a *different
capability*, narrower than §10's, and not a cheaper spelling of it. Anyone
reviving this should decide the topology half explicitly rather than discovering
it is missing.

It is also still one more spelling of the push stream, and *"it round-trips"* is
not by itself a reason to have it.

### (iii) Leave §10 standing and do nothing

Rejected. §0.0's row is stale, `docs/PROJECT.md` propagates it in three places
and `docs/benchmarks/tf2.md` in a fourth, and a NORMATIVE section that nobody
intends to build is the state
[`0019`](./0019-one-binary-and-topology-you-can-wait-for.md)'s Context describes:
several documents describing several answers to one question.

## Implementation plan

1. **`docs/PHASE2.md` §10** gains a `SUPERSEDED` blockquote banner immediately
   under the heading, above the surviving original body — the shape §9 already
   carries for `0019`. — verified by reading the section against §9's.
2. **`docs/PHASE2.md` §0.0's `tf_tree serve` / `tf_tree_record` / `/tf` ingest /
   diagnostics row splits into four**, because three of its four items are
   stale: `/tf` ingest is `docs/PHASE4.md` §0.0's *"Done, except §5.9's affinity
   knobs and §6.3's replay rows"*, diagnostics are `docs/PHASE5.md` §0.0's *"§7
   `tf_tree top` — Done, both halves"* plus this table's own CLI-adoption row,
   and `tf_tree_record` is declined here. The recorder row says **declined**
   rather than *"not implemented"*, on §15's own precedent that *"an unticked
   box reads as work owed, and this is work declined"*, and records that it
   moved. — verified by reading each cited row.
3. **`docs/PHASE2.md`'s §0 deliverable row and §2 dependency row get the
   strike-through-plus-pointer treatment** the `tf_treed` row directly above the
   first already carries; Appendix A's implementation-order step likewise. —
   verified by `grep -n tf_tree_record docs/PHASE2.md` showing every remaining
   hit carrying a pointer at this record.
4. **`docs/PROJECT.md`**: the Phase 2 paragraph stops listing `tf_tree_record`
   as a ship, its Status blockquote's *"tooling half"* sentence is corrected in
   all three items — the recorder is declined, `/tf` ingest shipped, and the
   long-running fault harness is `shm_torture`, whose §0.0 row is *"Done, and
   since 2026-09-04 it kills the rendezvous owner"* **with the qualifier that
   row instructs a quoter to carry** — and §4's *"MCAP record/replay lands early
   in Phase 2"* is corrected in place. — verified by
   `grep -n 'tf_tree_record' docs/PROJECT.md` and `grep -n 'record/replay'
   docs/PROJECT.md`.
5. **`docs/benchmarks/tf2.md`**'s *"the tooling half (`tf_tree_record`, the
   long-running fault harness)"* sentence, which repeats both stale claims. —
   same verification, over that file.
6. **`docs/PHASE2.md` §15's §10 box** gains a clause naming `just shm-check` and
   the CI step that runs it, and saying plainly that `just test` does not. —
   verified by `cargo nextest list -p tf_tree_cli` with and without
   `--features shm`.
7. **`crates/tf_tree_cli/tests/replay_bit_identity.rs`'s module doc and inline
   comment** say what the test does: it writes the recording and asserts it
   exists, then replays the in-memory fixture into both backends. **Not** by
   adding a read-back — §15's box records that widening the query set past the
   fixture's `stamp_ns == 0` statics is a change with a decision in it. —
   verified by `cargo nextest run -p tf_tree_cli --features shm --test
   replay_bit_identity` and by `just lint`.
8. **`CHANGELOG.md`** entry. **`docs/decisions/README.md`**'s index row is
   added centrally rather than on this branch — four branches editing one index
   is a conflict with no useful three-way merge — so it is the one step here
   that this record does not land itself.

**No gate is proposed for any of this**, and the reason is worth stating rather
than leaving as an omission. The defect in step 7 is a doc comment that
contradicts the code beneath it, and `scripts/evidence-audit.sh` derives its
entire subject set from `cargo metadata`'s `bin`, `example` and `bench` targets
— an integration test's `//!` is outside that set by construction. A check
bolted on there would run over an empty subject set and pass. **An empty subject
set is not a pass**, so the honest answer is that this class has no mechanical
guard here and the correction is held by review.

## Open questions

1. **Should `tf_tree_ingest` be able to build a mapped arena?** Its fill calls
   `builder.build()`; nothing calls `build_shared`. Declining the recorder does
   not settle it, and nothing in the tree currently owes it — the NORMATIVE test
   works around it with its own `replay()`. It is recorded here so the next
   reader finds it rather than rediscovering it, and it stays **open**.
