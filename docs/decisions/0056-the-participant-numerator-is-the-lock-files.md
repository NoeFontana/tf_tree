# 0056: the participant numerator is the lock file's

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** (none yet)

## Context

`docs/PHASE5.md` §6's catalogue defines `TFT015` as *"arena occupancy > 80%
(frames, edges, **participants**)"*. Two of those three rows ship. The
participants row does not, and its absence is disclosed to the operator in
`Meta.notes` through `checks::PARTICIPANT_OCCUPANCY_NOTE`.

The disclosure is honest about the gap and, until this record, **wrong about the
remedy**. Both `checks::occupancy_of`'s doc and the anti-vacuity test
`no_occupancy_row_is_permanently_zero` explained the absence by
`ArenaHeader::participant_count` never being incremented, and the doc closed by
saying to *"restore the row in the same commit that makes the engine maintain the
counter"*. That instruction would produce a wrong row.

**A header counter cannot be maintained.** A participant that is killed cannot
decrement it, so it drifts up for the life of the arena and never comes back
down. That is the whole reason `docs/PHASE2.md` keeps liveness in a lock byte the
kernel releases on process death (D17) rather than in a count somebody has to
remember to undo. A counter would turn the one occupancy figure an operator
trusts into the one figure a crash inflates permanently.

**The arena participant table is not a numerator either, and it fails in the same
direction.** A read-only attachment is D18's default and Python's; its mapping is
`PROT_READ`, so it takes a lock-file byte and writes **no arena record at all**.
A fleet whose consumers are all read-only — the shape this project recommends —
fills the slot table while the arena table stays as empty as the counter.

Measured, one publisher and one read-only consumer
(`crates/tf_tree_cli/tests/attach.rs`,
`the_two_participant_censuses_disagree_by_the_read_only_population`):

| Census | Count |
|---|---|
| lock file, held participant bytes | **2** |
| arena participant table, `LIVE` identities | **1** |

The ratio is not the point; the *direction* is, and it grows with every consumer.
At 60 read-only consumers and one publisher, an arena-table row reads 1/64 — a
comfortable 2% — on an arena with four slots left.

**The guard that exists does not catch the substitution.** Feed the row from the
arena table and `no_occupancy_row_is_permanently_zero` still passes: its fixture
is an in-process arena whose own process holds a record, so `used > 0` and the
row looks like a real measurement. Measured, not reasoned. The assertion written
*because* this row was untested does not reach the likeliest wrong answer to it,
which is the failure mode `anti-vacuity-checks-go-vacuous` names.

So the numerator is the lock file's. **Which lock-file population is the open
question**, and it is a real one, because the lock file and the arena disagree in
*both* directions:

- a byte held with no arena record is a **read-only participant** — occupying a
  slot, invisible to the arena;
- an arena record whose byte has been released is a **leaked slot**
  (`docs/decisions/0028`, *The ordering*) — and it is not yet re-attachable,
  because the byte frees on the departing participant's own thread while the
  *index* is only recycled when the owner's `epoll` gets round to it.

## Decision

**Draft — this record authorises nothing yet.** What it settles is that neither
arena-side source may be used, and it states the candidates so the row is
restored once rather than twice.

The numerator candidates, and what each would claim:

| # | Numerator | Counts a read-only participant | Counts a leaked slot | Counts an in-flight joiner |
|---|---|---|---|---|
| A | `ArenaHeader::participant_count` | no | no — it counts nothing | no |
| B | arena table `LIVE` identities | **no** | yes | no (`RESERVED` withholds identity) |
| C | held lock bytes | yes | **no** | no (the byte is taken after the identity) |
| D | `state != Free \|\| byte == Held` | yes | yes | yes, via the record |
| E | every row the merged census prints | yes | yes | yes, via the identity |

**The recommendation is D or E, and the argument for preferring one is about what
an operator is being warned of.** `TFT015` warns that a fixed-capacity table is
nearly full — the consequence being that the next node cannot attach — so the
honest numerator is *slots that are not available*, which is the union and not
either side alone. A and B are excluded by the measurement above. C is excluded
by the leaked slot: a leak is precisely the state in which the byte is free and
the slot is unusable, so a byte-only numerator reports room that does not exist.

E has one property D does not: it is the **same list the participant pane
prints**, so an operator cannot be shown N rows beside a percentage derived from
some other N. That failure has already happened once in this codebase in the
adjacent direction — `top`'s occupancy pane compared with `>=` while `doctor`
compared with `>`, so the two disagreed at exactly 80.0 % (`top.rs`'s own
comment records it). A numerator that is definitionally the pane's row count
cannot drift from the pane.

D has the property E does not: it is computable from a `doctor::Snapshot` alone,
which is the type the check already runs against.

## Open questions

1. **D or E** — one predicate over the snapshot, or the merged census's length.
   They differ only on an in-flight joiner that has written its lock-file
   identity and not yet taken its byte (§3.3 writes the identity first), which E
   counts and D does not. That window is sub-millisecond; the question is whether
   a numerator should be defined by a predicate or by "what the pane shows".
2. **What the row does when no lock facts were asked.** `doctor --from-bag`,
   `--from-file` and the in-process fixture have no lock file, so every slot's
   `doctor::LockByte` is `Unknown` — which §6.2 already treats as evidence in
   neither direction. The row must then be **withheld**, not reported as 0%:
   `INVALID is not FAIL`, and a 0% occupancy row is a `pass`. The mechanism for
   withholding it is the one already in use — `Meta.notes` — so the existing
   disclosure does not disappear when the row lands; it becomes conditional. What
   is undecided is whether "some slots answered and some did not" is a third
   state or folds into one of the two.
3. **`ArenaHeader::participant_count`'s fate.** It is a field at a pinned offset
   (`offset_of!(ArenaHeader, participant_count) == 96`) that nothing writes and,
   after this record, nothing should ever read. Deleting it moves every offset
   after it, so it is a **format break** and belongs in the scheduled-break
   ledger `docs/decisions/0032` part 2 opened — not in the commit that restores
   the row. Until then it should carry a doc comment saying it is vestigial and
   why, so the next reader does not repeat the inference this record corrects.

## Consequences

- The participants row stays absent for now, and the disclosure an operator reads
  now names the *right* reason: participant occupancy lives in the lock file.
- One new test, which is the evidence for all of the above and the guard against
  the arena-table numerator being shipped later. It needs a live arena — a
  publisher and a real read-only attachment — so it cannot live beside the
  catalogue's in-process fixture.
- No engine change, no arena change, no new census. Whichever of D or E wins,
  the count comes from a census the code already builds: `doctor`'s
  `Snapshot::probe_lock_facts` for D, `top::Capture::merge_lock_rows` for E.
