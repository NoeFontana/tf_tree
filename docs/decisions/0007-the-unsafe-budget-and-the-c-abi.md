# 0007: The unsafe budget, restated — and where the C ABI lives

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** this record, plus the `CLAUDE.md` and `docs/PROJECT.md` edits it authorises

## Decision

**Replace `CLAUDE.md`'s crate enumeration with a rule about what makes `unsafe`
acceptable**, so `docs/PHASE4.md` §3's C ABI is admitted by criterion.

### The rule — NORMATIVE
1. **`unsafe` is permitted only at a boundary the compiler cannot see across.**
   Four kinds: the arena's raw memory, the OS, a foreign runtime that owns its own
   objects, and a foreign *caller* (the C ABI). A new kind needs a decision record.

   > **Amended by [`0048`](./0048-a-kind-is-not-a-crate-name.md).** The kinds are
   > **properties**, not crate names (those live in `scripts/unsafe-budget.txt`);
   > kind 3 widens to *"a foreign runtime or library"*; a fifth admits **our own C
   > ABI, called from Rust to exercise or measure it**; a sixth a trait the language
   > requires be implemented unsafely in a target that never ships. **The rule binds
   > every crate root, not every package.** `scripts/unsafe-budget.sh` is the gate.

2. **Everything else keeps `#![forbid(unsafe_code)]`** (`tf_tree_math`, `tf_tree_cli`).

   > **Amended by [`0017`](./0017-owned-handles-and-the-lifetime-rule.md):** `tf_tree`
   > is `#![deny(unsafe_code)]` with one `#[allow]` (`OwnedWriter`).

3. **Every `unsafe` block carries a `// SAFETY:` comment naming its invariant, and
   every crate with `unsafe` a module-level `// SAFETY:` block explaining the
   boundary.**

4. **A crate with `unsafe` must declare `#![deny(unsafe_op_in_unsafe_fn)]`.**

### Where the C ABI lives

`crates/tf_tree_c`, depending on `tf_tree` — not `tf_tree_core` — so it cannot
bypass the facade's fork-generation and detach checks. Its `unsafe` is confined to
**turning caller pointers into Rust references, once, at the entry point**:

```rust
#[no_mangle]
pub unsafe extern "C" fn tft_plan_at(plan: *const tft_plan, /* … */) -> tft_status {
    // SAFETY: `plan` is null-checked and its magic word validated before any
    // dereference; only `tft_plan_create` hands out a `tft_plan` (§3.2).
    guard(|| { /* safe from here down */ }) // `catch_unwind` wrapper, §3.4
}
```

### `cbindgen` is a tool, not a dependency — NORMATIVE

`cbindgen` is **MPL-2.0**, outside `deny.toml`'s allowlist, so it is no
`[build-dependencies]` entry. **It runs as an `xtask` step and the generated headers
are committed** (§3.1: "frozen and reviewed by hand"); CI runs
`cargo xtask headers --check`, which regenerates into a temp dir and diffs.

## Implementation plan

1. `CLAUDE.md` + `docs/PROJECT.md` §6 restated; `#![deny(unsafe_op_in_unsafe_fn)]`
   on `tf_tree_arena`, `_core`, `_ipc`, `_py`.
2. `crates/tf_tree_c` skeleton: ABI version, magic-word handle, `tft_last_error`,
   `guard` — a C test forcing a Rust panic asserts `TFT_ERR_INTERNAL` (§6.1).
3. `cargo xtask headers` + `--check` (added to `just lint`), headers committed.
4. The rest of §3.
