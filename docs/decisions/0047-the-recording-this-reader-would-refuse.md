# 0047: the recording this reader would refuse

**Status:** implemented (2026-09-09; the plan landed in #303 and the status line did not)
**Owner:** @NoeFontana
**Implementation:** #303 — all eight plan steps, verified against the tree on 2026-09-09: `docs/PHASE2.md` §10's DECLINED banner, §0.0's row split, the strike-throughs, §15's box, `docs/PROJECT.md`'s three sites, `docs/benchmarks/tf2.md`, and `crates/tf_tree_cli/tests/replay_bit_identity.rs`'s module doc and inline comment. Open question 1 stays open and is **not decision-affecting** — the record itself scopes it that way (*"nothing in the tree currently owes it"*), which this folder's `ready`/`implemented` bar now states explicitly.

## Decision

**`docs/PHASE2.md` §10(c) is met:** `crates/tf_tree_cli/tests/replay_bit_identity.rs` is the NORMATIVE test (run by `just shm-check`, not `just test`).

**§10(a) Record and §10(b) Replay are DECLINED, not deferred.** No `tf_tree_record` crate, no `record`/`replay` subcommand, and none is owed.

### 1. Its artifact is one nothing in this tree can read

`crates/tf_tree_ingest/src/source.rs` accepts a channel only by schema `tf2_msgs/msg/TFMessage` or `tf2_msgs/TFMessage` (`docs/PHASE5.md` §3.3); §10's channels carry neither, and fixing that needs a second MCAP reading path (`docs/PROJECT.md` §6).

### 2. Phase 2's Definition of Done does not ask for it

§15's only box naming §10 is the NORMATIVE test.

## Consequences

- A revival is `tf_tree record --format tf2_msgs/TFMessage` on the shipped binary, never a new crate or schema.
- `tf_tree_ingest`'s fill calls `builder.build()`, never `build_shared`, which is why the NORMATIVE test carries its own `replay()`.

## Implementation plan

1. `docs/PHASE2.md` §10 gains a `SUPERSEDED` banner.
2. `docs/PHASE2.md` §0.0's row splits into four; the recorder row says **declined**.
3. `docs/PHASE2.md`'s §0 deliverable row, §2 dependency row and Appendix A step get strike-through plus pointer.
4. `docs/PROJECT.md`'s Phase 2 paragraph, Status blockquote and §4 are corrected.
5. `docs/benchmarks/tf2.md`'s "tooling half" sentence.
6. `docs/PHASE2.md` §15's §10 box names `just shm-check`.
7. `replay_bit_identity.rs`'s module doc and inline comment.
8. `CHANGELOG.md` entry; the index row is added centrally.

## Open questions

1. Whether `tf_tree_ingest` should be able to build a mapped arena. Nothing owes it.
