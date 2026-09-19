# 0044: recovery, in the languages a robot is written in

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** #284, #285

## Decision

**The three recovery entry points cross both boundaries, and the facade drops the `&mut` that made that impossible.**

### 1. `inherit_ownership` takes `&self`

Both bindings hold the tree in an `Arc`, where `Arc::get_mut` fails whenever a plan or publisher holds a clone. `Tree::attachment` becomes `Mutex<Option<Attachment>>`; `Plan::at` never touches it (`docs/PHASE2.md` §3.5).

### 2. The C ABI, in the unstable tier

`tft_tree_open_named(name, read_write, out)` (the only read-write attachment C has; never creates), `tft_tree_owner_lost`, `tft_tree_inherit_ownership` and `tft_tree_reap_dead` (`Tree::reap_dead()` plus `Tree::reap_participants()`, summed) are declared in `tf_tree_unstable.h`. `tft_inheritance` is a `typedef uint8_t` not tiered as a symbol (`xtask headers`; §3.1 forbids a type in both headers); an unknown value means *not the owner*.

### 3. Python

`Tree.owner_lost() -> bool`, `Tree.inherit_ownership() -> str`, `Tree.reap_dead() -> int`.

## Consequences

- Recorded in `docs/API.md` §3 and §4 and `docs/RUNBOOK.md`'s owner-death checklist.
- Every `Mutex` acquisition is in `tree.rs` or `open.rs`.

## Implementation plan

1. `Tree::attachment` becomes a `Mutex`; `inherit_ownership` takes `&self`.
2. A test that a `Guard` may be outstanding across `inherit_ownership`.
3. The C entry points, verified by `crates/tf_tree_c/tests/recovery.rs` and `just c-header-check`.
4. The three Python methods, with a reproducing pytest.
5. `docs/API.md` §3/§4, `docs/PHASE2.md` §3.5, `docs/RUNBOOK.md`.

## Open questions

None.
