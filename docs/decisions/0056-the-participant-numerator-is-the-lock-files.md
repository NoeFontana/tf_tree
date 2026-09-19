# 0056: the participant numerator is the lock file's

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** (none yet)

## Context

`docs/PHASE5.md` §6 defines `TFT015` as *"arena occupancy > 80% (frames, edges,
**participants**)"*. The participants row does not ship; `Meta.notes` discloses
this through `checks::PARTICIPANT_OCCUPANCY_NOTE`. Restoring it from an arena-side
source would produce a wrong row:

- **A header counter cannot be maintained** (D17).
- **The arena participant table is not a numerator.** A read-only attachment
  (D18's default) takes a lock-file byte and writes **no arena record**: with one
  publisher and one read-only consumer the lock file counts 2 and the table 1
  (`the_two_participant_censuses_disagree_by_the_read_only_population` in
  `crates/tf_tree_cli/tests/attach.rs`).

**A byte-free `LIVE` record is not occupied.**
[`0028`](./0028-the-slot-a-killed-participant-keeps.md)'s slot assigner reclaims
and re-grants it in the same handshake; nothing clears a lock-file identity record
(`LockFile::write_identity` is the only writer).

## Decision

**Draft — this record authorises nothing yet.** "Counts" means *this numerator
would report the slot as unavailable to a new joiner*.

| # | Numerator | live, holds a byte | live, holds **no** byte | leaked record, byte freed | departed, stale identity |
|---|---|---|---|---|---|
| B | arena table `LIVE` identities | **no** — a read-only attach writes no record | yes | **yes**, wrongly | no |
| C | **held lock bytes** | yes | **no** | no — correctly | no |
| D | `state != Free \|\| byte == Held` | yes | yes | **yes**, wrongly | no |

**The recommendation is C.** `TFT015` asks *can another node attach?*; the
assigner refuses exactly the held bytes. D over-reports; `ArenaHeader::participant_count` counts nothing (a killed participant cannot decrement it, D17); a merged census never clears. C costs one
`F_OFD_GETLK` per slot, which `doctor`'s `probe_lock_facts` already performs.

**C's blind spot:** a `TreeBuilder::build_shared` creator holds no byte and is not
counted, as the assigner does not count it;
[`0031`](./0031-the-participant-record-with-no-byte.md) put it **out of contract**.

## Open questions

1. **Ratify C, or argue that the assigner is the wrong authority.**
2. **What the row does when no lock facts were asked** (`--from-bag`,
   `--from-file`, the in-process fixture: every `doctor::LockByte` is `Unknown`).
   It must be **withheld**, not reported as 0%, through `Meta.notes`. Undecided:
   whether "some slots answered and some did not" is a third state.
3. **`ArenaHeader::participant_count`'s fate.** Pinned at offset 96 and never
   written; deleting it is a **format break** for the ledger `0032` part 2 opened.

Until ratified the participants row stays absent; a test with a real read-only
attachment must guard against the arena-table numerator.
