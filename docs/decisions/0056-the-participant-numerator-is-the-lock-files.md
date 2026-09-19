# 0056: the participant numerator is the lock file's

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** (none yet)

## Context

`docs/PHASE5.md` §6 defines `TFT015` as *"arena occupancy > 80% (frames, edges,
**participants**)"*. The participants row does not ship; `Meta.notes` discloses
this through `checks::PARTICIPANT_OCCUPANCY_NOTE`. `checks::occupancy_of`'s doc
and the test `no_occupancy_row_is_permanently_zero` said the cause was
`ArenaHeader::participant_count` never being incremented and to restore the row
"in the same commit that makes the engine maintain the counter". That would
produce a wrong row:

- **A header counter cannot be maintained.** A killed participant cannot
  decrement it, so it drifts up for the life of the arena; that is why liveness
  is a lock byte the kernel releases (D17).
- **The arena participant table is not a numerator either.** A read-only
  attachment (D18's default, Python's) is `PROT_READ`: it takes a lock-file byte
  and writes **no arena record**. Measured, one publisher and one read-only
  consumer
  (`crates/tf_tree_cli/tests/attach.rs`,
  `the_two_participant_censuses_disagree_by_the_read_only_population`):

  | Census | Count |
  |---|---|
  | lock file, held participant bytes | **2** |
  | arena participant table, `LIVE` identities | **1** |

  The direction is the point: 60 read-only consumers and one publisher read 1/64
  from the arena table on an arena with three slots left.
- **The existing guard does not catch the substitution.**
  `no_occupancy_row_is_permanently_zero`'s fixture is an in-process arena whose
  own process holds a record, so an arena-table numerator still passes it.

**A byte-free `LIVE` record is not occupied.** [`0028`](./0028-the-slot-a-killed-participant-keeps.md)'s
slot assigner calls `reclamation_verdict`: a byte-free non-`FREE` record is
`Reclaimable` and is reclaimed and granted in the same handshake
(`crates/tf_tree/src/open.rs`). A numerator counting it over-reports (63 stale
records and one held byte read 64/64 and fire `TFT015` while all 63 are
grantable). Counting every row a census prints is worse: nothing ever clears a
lock-file identity record (`LockFile::write_identity` is the only writer;
`release_participant` only unlocks the byte), so the numerator is monotone in the
high-water mark of concurrent slots — a fleet that once peaked at 52 reads 81%
with two processes attached. (The owner's `epoll` delays *re-use*, which makes an
immediate-reuse assertion in a **test** a race; it says nothing about capacity.)

## Decision

**Draft — this record authorises nothing yet.** It settles that neither
arena-side source may be used, and states the candidates so the row is restored
once. "Counts" means *this numerator would report the slot as unavailable to a new
joiner*.

| # | Numerator | live, holds a byte | live, holds **no** byte | leaked record, byte freed | departed, stale identity |
|---|---|---|---|---|---|
| A | `ArenaHeader::participant_count` | no — counts nothing, ever | no | no | no |
| B | arena table `LIVE` identities | **no** — a read-only attach writes no record | yes | **yes**, wrongly | no |
| C | **held lock bytes** | yes | **no** | no — correctly | no |
| D | `state != Free \|\| byte == Held` | yes | yes | **yes**, wrongly | no |
| E | every row a merged census prints | yes | yes | **yes**, wrongly | **yes**, wrongly, for ever |

**The recommendation is C.** The question `TFT015` asks is *can another node
attach?*, whose one authoritative answerer is the owner's slot assigner; it grants
a slot whose byte is free, so the slots it would refuse are the **held bytes**.
D and E over-report and would fire a false alarm, which teaches people to ignore
the check.

**C's blind spot is stated, not patched.** A `TreeBuilder::build_shared` creator
holds a `LIVE` record and no byte (`lock_file: None`, `crates/tf_tree/src/tree.rs`),
and C does not count it; the assigner does not either and will reclaim its slot.
The `ReadWrite` fd-attach arms have refused since `0028` step 0b, and
[`0031`](./0031-the-participant-record-with-no-byte.md) put the served
`build_shared` arena **out of contract** on 2026-09-18, so this is a blind spot C
shares with every reclaimer, over a population the project does not support.

C is the cheapest: one `F_OFD_GETLK` per slot over 64 slots, which `top`'s row
producer and `doctor`'s `probe_lock_facts` already perform.

## Open questions

1. **Ratify C, or argue that the assigner is the wrong authority.** The
   alternative reading — *how much of the table is in use* — counts a
   `build_shared` participant and makes C wrong; it is rejected because it cannot
   be answered from outside the process.
2. **What the row does when no lock facts were asked.** `doctor --from-bag`,
   `--from-file` and the in-process fixture have no lock file, so every slot's
   `doctor::LockByte` is `Unknown` (evidence in neither direction, §6.2). The row
   must be **withheld**, not reported as 0% (`INVALID is not FAIL`), through
   `Meta.notes`, so the existing disclosure becomes conditional. Undecided:
   whether "some slots answered and some did not" is a third state.
3. **`ArenaHeader::participant_count`'s fate.** A pinned field
   (`offset_of!(ArenaHeader, participant_count) == 96`) that nothing writes and
   nothing should read. Deleting it is a **format break** for the ledger
   `docs/decisions/0032` part 2 opened; until then it carries a doc comment saying
   it is vestigial.

## Consequences

- The participants row stays absent; the disclosure names the right reason.
- One new test (a publisher and a real read-only attachment, so not the catalogue's
  in-process fixture) guards against the arena-table numerator shipping later.
- No engine change, no arena change, no new census.
