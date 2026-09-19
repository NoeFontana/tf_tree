# 0048: a kind is not a crate name

**Status:** implemented (2026-09-09) — D1–D6 are in force and gated: `0007` rule 1 carries this record's amendment, `scripts/unsafe-budget.txt` is the index, and `just unsafe-budget` runs inside `just lint`. Step 4, the last outstanding one, landed on 2026-09-09. Both open questions are scoped by this record as not decision-affecting.
**Owner:** @NoeFontana
**Implementation:** #303 (steps 1–3 and the CHANGELOG); step 4 in the wave 6 backlog PR

## Context

[`0007`](./0007-the-unsafe-budget-and-the-c-abi.md) rule 1: `unsafe` is permitted only at a boundary the compiler cannot see across; a new kind needs a decision record. Its parentheticals named a crate beside each kind and were read as the criterion, so the budget was overtaken in several roots with every recipe green. `scripts/unsafe-budget.sh` and `0007`'s amendment are the remedy; the census figures live in the script and register.

## Decision

Rule 1's *criterion* is untouched.

### D1 — the kinds are properties; the crate names become a non-normative index

The list of where each kind currently lives moves to `scripts/unsafe-budget.txt`, a snapshot that may go stale without invalidating the rule, because a check recomputes it.

### D2 — kind 3 widens to "a foreign runtime **or library** that owns its own objects"

`tf_tree_tf2_sys` joins the index under it (`tf2::BufferCore` owns its frame table, as `tf_tree_py`'s objects are owned). It was never a fifth kind.

### D3 — a fifth kind: our own C ABI, called from Rust to exercise or measure it

Eligible in **any non-`lib` target of a `publish = false` package** whose purpose is exercising, forking or measuring `tf_tree_c`'s `extern "C"` surface. This includes `tf_tree_c`'s own `tests/` and `examples/`, where most of the kind's population lives. It is not kind 4 (a foreign caller): a domestic Rust caller of an `unsafe extern "C"` surface pays the same cost.

### D4 — the budget's subject is the CRATE ROOT, not the package

Every crate root that contains `unsafe` declares its posture explicitly rather than inheriting rustc's default `allow`, and carries a module-level `// SAFETY:` block naming its kind. `#![forbid(unsafe_code)]` on a `src/lib.rs` governs that root and no bin, test, bench or example of the package.

### D5 — a sixth kind: a trait the language requires be implemented unsafely, in a target that never ships

`unsafe impl GlobalAlloc` for an allocation counter and `unsafe impl Arena` for a relocation harness. Not folded into D3, so D3's sentence keeps its meaning.

### D6 — the register, and a check that recomputes it

`scripts/unsafe-budget.txt` is the index. `scripts/unsafe-budget.sh` takes the census and compares the **file set** in both directions; `just lint` depends on it.

## Rationale

- **`publish = false` is a scoping device in D3 and D5, never a licence.** `tf_tree_c` and `tf_tree_py` are `publish = false` and both ship.
- **A measurement harness is not a kind**: it would authorise a place, not a property.
- **The check pins a FILE set.** `--force-warn unsafe_code` carries no kind, so the register's `kind` column is maintained by a human and D1 stays a review rule. The covered roots are `crates/` and `xtask/`; a path prefix is a hand-maintained list and a third root would need adding.
- **The empty-subject shape.** A collapsed census makes `census − register` empty and a naive comparison green. The script therefore checks floors on the selector and site counts before any comparison, compares in both directions, and fails when `cargo check` fails. `bash scripts/unsafe-budget.sh --self-test` covers each verdict, including an empty census.
- **The `mem::zeroed()` conveniences are deleted, not authorised.** `tft_error::blank()`, `tft_extrapolated::blank()`, `tft_bridge_outcome::blank()` and `tft_bridge_stats::blank()` replace them; the strings in `tft_bridge_outcome` are `EMPTY`, never null. `#[derive(Default)]` is unavailable for these types.
- **Out-parameter tests must not seed `blank()`**, because it equals what the callee writes on the in-window path. `crates/tf_tree_c/tests/live.rs` seeds `by_ns: i64::MIN`.

## Consequences

- `0007` rule 1 gains its first amendment; `0005` is amended for its count only.
- `just lint` gains a compiling dependency, last in the chain.
- `tf_tree_py` (`just py-compile`) and `tf_tree_tf2_sys` (container-only `just tf2-check`) are outside the census's reach; their register rows say so.
- Adding a field to a `blank()` type is a compile error rather than a silently zeroed field.

## Implementation plan

1. `scripts/unsafe-budget.sh`, `scripts/unsafe-budget.txt` and `just unsafe-budget` in `lint`'s chain.
2. The `core::mem::zeroed()` sites replaced by the `blank()` constructors.
3. D4 postures on the `tf_tree_bench` bins that carry `unsafe`, and on `tf_tree_tf2_sys` in its manifest `[lints.rust]` (workspace-excluded, cannot inherit `[workspace.lints]`).
4. D4 applied to `tf_tree_c`'s own `tests/` and `examples/` roots and to `tf_tree_bench`/`tf_tree_bridge`'s test roots: **kind 5** on the eight `tf_tree_c` targets; **kind 6** on `zero_alloc.rs`, `relocation.rs` and `steady_state_alloc.rs`; **kind 1** on `heap_alignment.rs`. Posture only; the register already covered these files.
5. `docs/decisions/README.md`'s index row.

## Open questions

1. Whether the register's `kind` column should be mechanised. Left open; the script's *What it does NOT prove* section is the position.
2. Whether D3 wants an upper bound. Nothing proposes one.
