# 0048: a kind is not a crate name

**Status:** implemented (2026-09-09) — D1–D6 are in force and gated: `0007` rule 1 carries this record's amendment, `scripts/unsafe-budget.txt` is the index, and `just unsafe-budget` runs inside `just lint`. Step 4, the last outstanding one, landed on 2026-09-09. Both open questions are scoped by this record as not decision-affecting.
**Owner:** @NoeFontana
**Implementation:** #303 (steps 1–3 and the CHANGELOG); step 4 in the wave 6 backlog PR

## Context

[`0007`](./0007-the-unsafe-budget-and-the-c-abi.md) rule 1: `unsafe` only at a boundary the compiler cannot see across. Its parentheticals named a crate beside each kind and were read as the criterion; the *criterion* is untouched.

## Decision
- **D1 — the kinds are properties;** where each lives is `scripts/unsafe-budget.txt`, a non-normative index a check recomputes.
- **D2 — kind 3 widens to "a foreign runtime **or library** that owns its own objects"** (`tf_tree_tf2_sys`).
- **D3 — a fifth kind: our own C ABI, called from Rust to exercise or measure it.** Eligible in any non-`lib` target of a `publish = false` package, including `tf_tree_c`'s own `tests/` and `examples/`. Not kind 4: a domestic Rust caller pays the same cost.
- **D4 — the budget's subject is the CRATE ROOT, not the package.** Every crate root containing `unsafe` declares its posture explicitly and carries a module-level `// SAFETY:` block naming its kind. `#![forbid(unsafe_code)]` on a `src/lib.rs` governs that root and no bin, test, bench or example of the package.
- **D5 — a sixth kind: a trait the language requires be implemented unsafely, in a target that never ships** (`GlobalAlloc` counters, an `Arena` relocation harness).
- **D6 — the register and its check.** `scripts/unsafe-budget.sh` compares the **file set** of the compiler census with the register in both directions; `just lint` depends on it.
**Notes.** `publish = false` scopes D3 and D5 and is never a licence. The check pins a FILE set (`crates/` and `xtask/` only), so the `kind` column is human-maintained; floors keep a collapsed census from passing (`bash scripts/unsafe-budget.sh --self-test`). Out-parameter tests must not seed `blank()` (`crates/tf_tree_c/tests/live.rs` seeds `by_ns: i64::MIN`).

## Implementation plan

1. `scripts/unsafe-budget.sh`, `scripts/unsafe-budget.txt`, `just unsafe-budget` in `lint`'s chain; `blank()` replaces `core::mem::zeroed()`.
2. D4 postures on the `tf_tree_bench` bins and `tf_tree_tf2_sys` (manifest `[lints.rust]`).
3. The `README.md` index row.
4. D4 applied to `tf_tree_c`'s `tests/` and `examples/` and the `tf_tree_bench`/`tf_tree_bridge` test roots: **kind 5** on the eight `tf_tree_c` targets; **kind 6** on `zero_alloc.rs`, `relocation.rs`, `steady_state_alloc.rs`; **kind 1** on `heap_alignment.rs`.

## Open questions

Whether the register's `kind` column should be mechanised, and whether D3 wants an upper bound; neither is decision-affecting.
