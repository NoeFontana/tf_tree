# 0044: recovery, in the languages a robot is written in

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** #284, #285

## Context

`0037`'s ownership migration and `0043`'s trigger were Rust-only, so an all-C++/Python fleet whose owner is `SIGKILL`ed could not recover (`docs/RUNBOOK.md`). The owner's hangup callback now revokes a dead participant's claims; two producers of stale records remain without a hangup (a dead **owner**, and a `TreeBuilder::build_shared` participant with no socket), so `Tree::reap` is still their only collector.

## Decision

**The three recovery entry points cross both boundaries, and the facade drops the `&mut` that made that impossible.**

### 1. `inherit_ownership` takes `&self`

Both bindings hold the tree in an `Arc`, where `Arc::get_mut` fails whenever a plan or publisher holds a clone. `Tree::attachment` becomes `Mutex<Option<Attachment>>`. Six control-plane sites touch it; **`Plan::at` does not** (`docs/PHASE2.md` §3.5: lookups do not stop, slow down, or observe anything during a takeover). `&mut self` to `&self` is a relaxation, and a `Guard` may now be outstanding across the call. Drop order (serving stops before byte 0 is released) stays field declaration order on `Attachment::Owner`.

### 2. The C ABI, in the unstable tier

The stable ABI is frozen at 1.0 (`docs/PHASE4.md` §7), so these are declared in `tf_tree_unstable.h`:

```c
tft_status tft_tree_open_named(const char *name, bool read_write, tft_tree **out);
tft_status tft_tree_owner_lost(const tft_tree *tree, bool *out);
tft_status tft_tree_inherit_ownership(const tft_tree *tree, uint8_t *out);
tft_status tft_tree_reap_dead(const tft_tree *tree, uint32_t *out);
```

`tft_tree_reap_dead` is `Tree::reap_dead()` plus `Tree::reap_participants()`, summed.

`tft_tree_open_named` is the only read-write attachment C has: `tft_tree_open` is read-only, and a read-only tree answers `TFT_READ_ONLY` to inherit (D18). It never creates (`CreatePolicy::Never`); a C creator is `tft_bridge_create`.

`tft_inheritance` is a `typedef uint8_t`, not an enum, and is not tiered as a symbol: listing its five constants is what pulls the typedef into the unstable header alone (`xtask headers`; §3.1 forbids a type in both headers). An unknown value means *not the owner*.

### 3. Python

`Tree.owner_lost() -> bool`, `Tree.inherit_ownership() -> str` (the variant name), `Tree.reap_dead() -> int`.

## Rationale

- A Rust supervisor for a C++ fleet is `0019`'s prohibition in another form.
- Unstable tier: the protocol is young (`0043`'s three-outcome table was unpredicted); `tft_tree_plan_in_domain` is stable because it is a query shape.
- `Mutex`, not `RefCell` (`Tree` is `Sync`) or `ArcSwap` (D4; the attachment is taken, mutated and put back as a unit).
- `reap_dead` sums the two sweeps because a C caller cannot choose; Rust keeps both. `reap_participant(slot)` is not exposed: a binding has no `EPOLLHUP` to obtain a slot.

## Consequences

- Three C functions, one C typedef and three Python methods, recorded in `docs/API.md` §3 and §4.
- `docs/RUNBOOK.md`'s owner-death checklist asks whether any survivor can call recovery, in any language.
- The `Mutex` is `pub(crate)` and every acquisition is in `tree.rs` or `open.rs`; the one internal re-entry path is `crate::open::Open::attempt`.
- `Inheritance` is on three surfaces; it is `#[non_exhaustive]` in Rust.

## Implementation plan

1. `Tree::attachment` becomes `Mutex<Option<Attachment>>`; `inherit_ownership` takes `&self`. Verified by the unchanged rendezvous suite and `just bench-check`.
2. A test that a `Guard` may be outstanding across `inherit_ownership`.
3. `tft_tree_open_named`, the three recovery entry points and the five `tft_inheritance` constants in the unstable header, verified by `crates/tf_tree_c/tests/recovery.rs` (owner process, read-write join from C, kill, recover; `tft_tree_reap_dead` returns `1` then `0`) and `just c-header-check`.
4. The three Python methods, with a pytest reproducing step 3.
5. `docs/API.md` §3/§4, `docs/PHASE2.md` §3.5's qualification and `docs/RUNBOOK.md`'s owner-death section.

## Open questions

None.
