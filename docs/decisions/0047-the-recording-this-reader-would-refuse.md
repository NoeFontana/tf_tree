# 0047: the recording this reader would refuse

**Status:** implemented (2026-09-09; the plan landed in #303 and the status line did not)
**Owner:** @NoeFontana
**Implementation:** #303 — all eight plan steps, verified against the tree on 2026-09-09: `docs/PHASE2.md` §10's DECLINED banner, §0.0's row split, the strike-throughs, §15's box, `docs/PROJECT.md`'s three sites, `docs/benchmarks/tf2.md`, and `crates/tf_tree_cli/tests/replay_bit_identity.rs`'s module doc and inline comment. Open question 1 stays open and is **not decision-affecting** — the record itself scopes it that way (*"nothing in the tree currently owes it"*), which this folder's `ready`/`implemented` bar now states explicitly.

## Context

`docs/PHASE2.md` §10 promised (a) **Record**, a read-only participant writing MCAP channels `tf_tree/topology` and `tf_tree/samples`; (b) **Replay**; (c) the NORMATIVE test that one recording replayed into a `HeapArena` and a `MappedArena` gives **bit-identical `f64` results**. There is no `tf_tree_record` crate. (c) is shipped in `crates/tf_tree_cli/tests/replay_bit_identity.rs` (run by `just shm-check`, not `just test`); the read half of (b) shipped as `tf_tree_ingest` under `0006`'s Phase 5.

## Decision

**§10(c) is met.** `crates/tf_tree_cli/tests/replay_bit_identity.rs` is the NORMATIVE test, and `docs/PHASE2.md` §15's box for it is ticked.

**§10(a) Record and §10(b) Replay are DECLINED, not deferred.** There is no `tf_tree_record` crate, no `tf_tree record` subcommand and no `tf_tree replay` subcommand, and none is owed. §10's dependency-table row (a crate carrying `mcap` and `serde`) is retired with it.

### 1. Its artifact is one nothing in this tree can read

`crates/tf_tree_ingest/src/source.rs` accepts a channel only by **schema** `tf2_msgs/msg/TFMessage` or `tf2_msgs/TFMessage` (`docs/PHASE5.md` §3.3); §10's channels carry neither. The recorder's output would be refused by the only MCAP reader the repository has, and fixing that needs a second MCAP reading path, which `docs/PROJECT.md` §6 names as a design smell. It would also be one more spelling of the transform stream, beside `SampleRing`, `tf_tree_ingest`'s `TransformStamped`, `tf_tree_bridge`'s `Sample`, `tft_bridge_sample`, `tf_tree_bench`'s `Sample`/`TfStream` and `PushSample`, and `FixtureMessage`.

### 2. Phase 2's Definition of Done does not ask for it

§15 has one box naming §10 and it is the NORMATIVE test. This is corroboration only: §15 is prose and §0.0's old row said *"Not implemented"*, a status not a decline, so this record is the decision.

### What is deliberately *not* an argument here

Losslessness: §10 asks for coverage of the edge set, not every sample, and `SampleRing`'s `head` counts samples ever published, so a poller can report its own drops.

## Consequences

- Nobody can capture a live arena's push stream to a file, and no substitute is offered.
- §10's promised regression corpus and real robot data were never delivered (`docs/PHASE5.md` §0.0's §3 row); declining stops promising them.
- A revival is `tf_tree record --format tf2_msgs/TFMessage` on the shipped binary, never a new crate or schema; it cannot carry `tf_tree/topology`, so it is a narrower capability than §10's.
- `tf_tree_ingest`'s fill calls `builder.build()`, never `build_shared`, which is why the NORMATIVE test carries its own `replay()`.
- §15's §10 box names where its evidence runs (`just shm-check`, not `just test`).

## Implementation plan

1. `docs/PHASE2.md` §10 gains a `SUPERSEDED` banner, the shape §9 carries for `0019`.
2. `docs/PHASE2.md` §0.0's row splits into four; the recorder row says **declined**.
3. `docs/PHASE2.md`'s §0 deliverable row, §2 dependency row and Appendix A step get strike-through plus pointer.
4. `docs/PROJECT.md`'s Phase 2 paragraph, Status blockquote and §4 are corrected.
5. `docs/benchmarks/tf2.md`'s *"tooling half"* sentence.
6. `docs/PHASE2.md` §15's §10 box names `just shm-check` and its CI step.
7. `crates/tf_tree_cli/tests/replay_bit_identity.rs`'s module doc and inline comment say what the test does (write the recording, assert it exists, replay the in-memory fixture into both backends).
8. `CHANGELOG.md` entry; `docs/decisions/README.md`'s index row is added centrally.

No gate is proposed: `scripts/evidence-audit.sh`'s subject set (`bin`, `example`, `bench` targets) excludes an integration test's `//!`, so a check there would run over an empty set.

## Open questions

1. Whether `tf_tree_ingest` should be able to build a mapped arena. Nothing owes it; it stays open.
