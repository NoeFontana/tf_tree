# 0037: a takeover is not a second `open()`

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** #275 (the deletion) and the §3.5 commit on
`feat/sota-runtime-hardening` (the replacement). The change that prompted this
record **removed** code rather than adding any — `tf_tree_ipc::Open::already_attached`
and the takeover arm it reached are deleted — and this record is what the
replacement was built from.

## Why it cannot be an `Open::open` call

A new file description cannot verify a claim about the caller's own locks. `Open::open` builds its own `LockFile`, and from a fresh description `F_OFD_GETLK` answers *"does anyone **else** hold this byte"* (`crates/tf_tree_ipc/src/lockfile.rs`'s module doc). The probe cannot distinguish a caller holding byte *n* on another description (declaration true) from a live peer holding it (declaration a lie). Every repair attempt produced an unsound state.

## The five unsound states

1. `register_any` handed back the first *free* byte, not the caller's (the #201 defect; `0035`).
2. Returning the declared slot unchecked gave `TookOver` over a **free** byte.
3. No range check: `already_attached_at(u32::MAX)` returned `Ok(4294967295)`.
4. A serving owner overrode the declaration, reproducing #201 on the join path.
5. Honouring the declaration on the join path stranded the owner's `granted` bit, wedging an arena after 64 such joins.

## Decision: a takeover is a method on the session that already holds the byte

`Session::take_over_ownership(&mut self) -> Result<(), IpcError>` `try_take_ownership()`s on the description the session already has and serves its *existing* fd. No registration, no second slot, nothing to verify. `PHASE2.md` §3.5's algorithm was right; the second `open()` was the defect. The heir never constructs an arena, so no path leads from inheriting to creating a forked segment.

The trigger is `tf_tree_ipc::peer_hung_up` and `tf_tree::Tree::owner_lost`, **caller-driven, deliberately** (`0019`: no background thread, no daemon). A fleet whose survivors never call it stays ownerless. Lookups do not stop, slow down, or observe anything during a takeover.

## Open questions

All five are answered.

1. **Where does the heir's arena come from?** `Tree::shared_fd` and the mapping were in reach; `Attachment::Joined` now retains the `Rendezvous` it used to drop.
2. **Who wins when two survivors race?** One wins the uncontended `F_OFD_SETLK`; the loser gets `Inheritance::Contended` **with its slot intact**. No arbitration.
3. **Does `OpenOutcome::TookOver` survive?** **No.** A takeover is not an outcome of `open()`. It is deleted with `tf_tree::OpenError::TakeoverUnsupported` and its refusal arm.
4. **What happens to `docs/PHASE2.md` §3.5 and `0005` step 5?** §3.5 is amended for its plumbing and trigger; its algorithm stands.
5. **What is `Session::release_ownership` for now?** The correct half of a pair with `take_over_ownership`; neither moves a slot.

## The `0028` step 9 obligation, and why it is not triggered

`0028` step 9's unit test that the `TookOver` arm refuses was retired with the arm. It is owed again only if `TookOver` ever gains a producer, before that producer lands. The arm is not replaced with `unreachable!`, because `TookOver`'s removal leaves no `pub` variant to panic on. The `#201` stress test's mutant recipe in `crates/tf_tree_ipc/tests/multiprocess.rs` (`register_creator` to `register_any`) can no longer be run.
