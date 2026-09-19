# 0037: a takeover is not a second `open()`

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** #275 (the deletion) and the §3.5 commit on
`feat/sota-runtime-hardening` (the replacement). The change that prompted this
record **removed** code rather than adding any — `tf_tree_ipc::Open::already_attached`
and the takeover arm it reached are deleted — and this record is what the
replacement was built from.

## Why it cannot be an `Open::open` call

A fresh file description cannot verify a claim about the caller's own locks: `F_OFD_GETLK` answers "does anyone **else** hold this byte" (`crates/tf_tree_ipc/src/lockfile.rs`'s module doc), so a caller holding byte *n* on another description looks like a live peer. Every repair attempt produced an unsound state.

## Decision: a takeover is a method on the session that already holds the byte

`Session::take_over_ownership(&mut self) -> Result<(), IpcError>` `try_take_ownership()`s on the description the session already has and serves its *existing* fd: no registration, no second slot. `PHASE2.md` §3.5's algorithm stands. The trigger is `tf_tree_ipc::peer_hung_up` and `tf_tree::Tree::owner_lost`, **caller-driven, deliberately** (`0019`).

## Open questions

1. **Where does the heir's arena come from?** `Attachment::Joined` retains the `Rendezvous` it used to drop.
2. **Who wins when two survivors race?** One wins the `F_OFD_SETLK`; the loser gets `Inheritance::Contended` with its slot intact.
3. **Does `OpenOutcome::TookOver` survive?** **No.** Deleted with `tf_tree::OpenError::TakeoverUnsupported`.
4. **What happens to `docs/PHASE2.md` §3.5 and `0005` step 5?** §3.5 is amended for its plumbing and trigger.
5. **What is `Session::release_ownership` for now?** The other half of a pair with `take_over_ownership`.

## The `0028` step 9 obligation

Retired with the `TookOver` arm; owed again only if `TookOver` ever gains a producer.
