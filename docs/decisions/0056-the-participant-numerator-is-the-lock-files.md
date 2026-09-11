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
comfortable 2% — on an arena with **three** slots left.

**The guard that exists does not catch the substitution.** Feed the row from the
arena table and `no_occupancy_row_is_permanently_zero` still passes: its fixture
is an in-process arena whose own process holds a record, so `used > 0` and the
row looks like a real measurement. Measured, not reasoned. The assertion written
*because* this row was untested does not reach the likeliest wrong answer to it,
which is the failure mode `anti-vacuity-checks-go-vacuous` names.

So the numerator is the lock file's. **Which lock-file population** was written
here as the open question, on an argument that was wrong in both directions and is
replaced below.

**The retracted argument, stated so it is not re-derived.** This section claimed
the lock file and the arena disagree symmetrically — a held byte with no record
being a read-only participant, and a record whose byte has been released being a
**leaked slot that is not yet re-attachable**, "because the byte frees on the
departing participant's own thread while the *index* is only recycled when the
owner's `epoll` gets round to it". The second half is false, and
[`0028`](./0028-the-slot-a-killed-participant-keeps.md) is what makes it false: the
owner's slot assigner calls `reclamation_verdict`, a byte-free non-`FREE` record
returns `Reclaimable`, and the assigner **reclaims that slot and grants it in the
same handshake** (`crates/tf_tree/src/open.rs`; that is `0028`'s fix for #184,
where 63 dead records wedged the arena). So a leaked slot *is* available, on the
next hello. What is true about the `epoll` is that *re-use is not instantaneous*,
which makes an immediate-reuse assertion in a **test** a race — and that is what
the note behind this claim was about. It says nothing about capacity, and turning
a fact about test timing into a fact about occupancy is the mistake.

**Two consequences, and they invert the recommendation this record first made.**
A numerator that counts a byte-free `LIVE` record as occupied **over**-reports: an
arena with 63 stale records and one held byte reads 64/64 and fires `TFT015`
saying no node can attach, while all 63 are grantable on the next hello. And
counting every row a census *prints* is worse still, because **nothing in the
workspace ever clears a lock-file identity record**: `LockFile::write_identity` is
the only writer, `release_participant` only unlocks the byte, and both row
builders keep a slot when `held || id.is_some()`. So one `tf_tree top --attach`
that has exited still produces a row for ever, and such a numerator is monotone in
the *high-water mark* of concurrent slots — a fleet that once peaked at 52
consumers reads 52/64 = 81% with two processes attached. That is the same defect
as the header counter, arrived at from the other end.

**What a byte-only numerator really misses** — the one case the retracted argument
should have named — is a **live participant that never takes a byte**.
`TreeBuilder::build_shared` calls `register_participant` and constructs its `Tree`
with `lock_file: None` (`crates/tf_tree/src/tree.rs`), so it holds a `LIVE` arena
record and no byte for its whole life; `Tree::attach_shared` over an existing
segment is the same shape. `CLAUDE.md` already names this pair as the two
producers of a stale claim that have no hangup. From outside, such a participant is
indistinguishable from a leaked record — which is precisely the ambiguity `0028`
resolves, by treating byte-free as collectable.

## Decision

**Draft — this record authorises nothing yet.** What it settles is that neither
arena-side source may be used, and it states the candidates so the row is
restored once rather than twice.

The numerator candidates. "Counts" below means *this numerator would report the
slot as unavailable to a new joiner*, which is the only reading under which an
occupancy warning says something an operator can act on:

| # | Numerator | live, holds a byte | live, holds **no** byte | leaked record, byte freed | departed, stale identity |
|---|---|---|---|---|---|
| A | `ArenaHeader::participant_count` | no — counts nothing, ever | no | no | no |
| B | arena table `LIVE` identities | **no** — a read-only attach writes no record | yes | **yes**, wrongly | no |
| C | **held lock bytes** | yes | **no** | no — correctly | no |
| D | `state != Free \|\| byte == Held` | yes | yes | **yes**, wrongly | no |
| E | every row a merged census prints | yes | yes | **yes**, wrongly | **yes**, wrongly, for ever |

**The recommendation is C, and it is derived rather than preferred.** The question
`TFT015` asks about participants is *can another node attach?*, and that question
has exactly one authoritative answerer: the owner's slot assigner. A numerator
that disagrees with the assigner is wrong by construction, whichever way it
disagrees. The assigner grants a slot whose byte is free, so the count of slots it
would refuse is the count of **held bytes** — candidate C, and nothing else.

That also disposes of the two unions. D over-reports every leaked record, and E
over-reports those plus every identity ever written, for the life of the arena.
Both would fire an operator-visible warning about an arena with free capacity —
the same class of wrong answer as A's silent `pass`, and harder to argue away,
because a false alarm teaches people to ignore the check.

**C's blind spot is stated rather than patched.** A `build_shared` or
`attach_shared` participant is alive, holds a `LIVE` record with a permanently free
byte, and C does not count it. That is not an inaccuracy this numerator
introduces: the assigner does not count it either, and will reclaim that
participant's record and grant its slot away. It is the protocol gap `CLAUDE.md`
already records, and a numerator that quietly compensated for it would report a
capacity the protocol does not honour — and would hide the gap.

**C is also the cheapest and the hardest to drift.** It is one `F_OFD_GETLK` per
slot over 64 slots, which `top`'s row producer and `doctor`'s `probe_lock_facts`
both already perform; neither needs a new census.

## Open questions

1. **Ratify C, or argue that the assigner is the wrong authority.** The
   recommendation above is a derivation from `0028`, so the way to disagree with it
   is to disagree that *can another node attach?* is the question `TFT015` asks
   about participants. There is a real alternative reading — *how much of the table
   is in use* — under which a `build_shared` participant counts and C is wrong.
   This record takes the first reading because the second cannot be answered from
   outside the process, for exactly the reason `0028` gives.

   **This replaces a question posed as "D or E", on the claim that the two differ
   only on an in-flight joiner.** They do not: they differ on every slot carrying a
   leftover identity record, which is every slot any participant has ever used.
   Both are now excluded.
2. **What the row does when no lock facts were asked.** `doctor --from-bag`,
   `--from-file` and the in-process fixture have no lock file, so every slot's
   `doctor::LockByte` is `Unknown` — which §6.2 already treats as evidence in
   neither direction. The row must then be **withheld**, not reported as 0%:
   `INVALID is not FAIL`, and a 0% occupancy row is a `pass`. The mechanism for
   withholding it is the one already in use — `Meta.notes` — so the existing
   disclosure does not disappear when the row lands; it becomes conditional. What
   is undecided is whether "some slots answered and some did not" is a third
   state or folds into one of the two. Under C the question is sharper rather than
   softer: the whole numerator is lock-file facts, so an arena with no lock file has
   no numerator at all and there is nothing to fall back on.
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
- No engine change, no arena change, no new census. C reads the same
  `F_OFD_GETLK` per slot that `doctor`'s `Snapshot::probe_lock_facts` and `top`'s
  row producer already take.
