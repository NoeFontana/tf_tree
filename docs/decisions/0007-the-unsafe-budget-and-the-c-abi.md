# 0007: The unsafe budget, restated — and where the C ABI lives

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** this record, plus the `CLAUDE.md` and `docs/PROJECT.md` edits it authorises

## Context

`docs/PHASE4.md` §3 requires a C ABI, whose `extern "C"` entry points take raw
pointers from a caller the compiler cannot see. `CLAUDE.md`'s budget enumerated
crates, which read literally forbids it and had already been overtaken, without amendment, by
`tf_tree_ipc::fork`, `tf_tree_py` and `tf_tree_bench::bin::fork_child`. An
enumeration of crate names goes stale whenever the project grows a boundary.

## Decision

**Replace the enumeration with a rule about what makes `unsafe` acceptable.** The
discipline is unchanged; only the bookkeeping changes.

### The rule — NORMATIVE

1. **`unsafe` is permitted only at a boundary the compiler cannot see across.**
   Today there are exactly four kinds: the arena's raw memory, the OS, a foreign
   runtime that owns its own objects, and a foreign *caller* (the C ABI). Anything
   else is not eligible, and a new kind needs a decision record.

   > **Amended by [`0048`](./0048-a-kind-is-not-a-crate-name.md).** The kinds
   > survive; **the crate-name parentheticals do not** — downstream readers copied
   > the crate name instead of the criterion. The kinds are **properties**; crate
   > names live in a non-normative index at `scripts/unsafe-budget.txt`; kind 3
   > widens to *"a foreign runtime or library"*; a fifth admits **our own C ABI,
   > called from Rust to exercise or measure it** (not kind 4, which is a *foreign*
   > caller); a sixth covers a trait the language requires be implemented unsafely
   > in a target that never ships. **The rule binds every crate root, not every
   > package**: `#![forbid(unsafe_code)]` on a `src/lib.rs` governs that root and no
   > bin, test, bench or example of the same package.
   > `scripts/unsafe-budget.sh` is rule 1's gate.

2. **Everything else keeps `#![forbid(unsafe_code)]`** — `tf_tree_math`,
   `tf_tree_cli`. The facade staying provably safe is what lets a reader trust the
   C ABI's `unsafe` is confined to argument validation.

   > **Amended by [`0017`](./0017-owned-handles-and-the-lifetime-rule.md):**
   > `tf_tree` moved to `#![deny(unsafe_code)]` with exactly one `#[allow]`
   > (`OwnedWriter`). No new *kind* was admitted; it is one named exception with a
   > record behind it, which is the shape rule 1 asks for.

3. **Every `unsafe` block carries a `// SAFETY:` comment naming the invariant it
   relies on, and every crate with `unsafe` carries a module-level `// SAFETY:`
   block explaining the boundary.**

4. **A crate with `unsafe` must declare `#![deny(unsafe_op_in_unsafe_fn)]`**, so an
   `unsafe fn` does not silently confer permission on its whole body. A C ABI is
   mostly `unsafe fn`s, and without it rule 3 is unenforceable where it matters
   most.

### Where the C ABI lives

`crates/tf_tree_c`, `crate-type = ["staticlib", "cdylib", "rlib"]`, depending on
`tf_tree` — not `tf_tree_core` — so the C ABI cannot reach an invariant the Rust
API protects. Its `unsafe` is confined to **turning caller pointers into Rust
references, once, at the entry point**; past that the body is safe:

```rust
#[no_mangle]
pub unsafe extern "C" fn tft_plan_at(
    plan: *const tft_plan, stamp: i64, layout: tft_layout, out: *mut c_void,
) -> tft_status {
    // SAFETY: `plan` is checked non-null and its magic word validated before
    // any dereference; `tft_plan` is only ever handed out by `tft_plan_create`,
    // which allocates it with the matching magic (§3.2).
    guard(|| { /* safe from here down */ })
}
```

`guard` is the `catch_unwind` wrapper §3.4 requires; wrapping *is* the boundary.

### `cbindgen` is a tool, not a dependency — NORMATIVE

`cbindgen` is **MPL-2.0** and `deny.toml`'s allowlist has no MPL, so it cannot be
a `[build-dependencies]` entry. **It runs as an `xtask` step that regenerates the
headers, and the generated headers are committed**: no licence question, a header
change is a reviewable diff (§3.1 requires `tf_tree.h` "frozen and reviewed by
hand"), and a C or C++ consumer needs no Rust toolchain to read the interface. CI
runs `cargo xtask headers --check`, which regenerates into a temp dir and diffs.

## Rationale

**Not the C ABI in `tf_tree` behind a feature:** it would delete the facade's
`forbid`; a C ABI is an unbounded number of exceptions, unlike `OwnedWriter`'s one.
**`tf_tree` rather than `tf_tree_core`:** going to `core` would bypass the facade's
fork-generation and detach checks. **Not keeping the enumeration:** a criterion
survives; a list does not. **Not hand-vendoring the headers:** a wrong *signature*
would escape §3.5's byte-pattern tests.

## Consequences

- `CLAUDE.md`'s "Hard rules" bullet and `docs/PROJECT.md` §6's design-smell
  "Writing `unsafe` outside `tf_tree_arena`, `buffer.rs`, or `arena_view.rs`" change.
- `#![deny(unsafe_op_in_unsafe_fn)]` goes on `tf_tree_arena`, `tf_tree_core`,
  `tf_tree_ipc` and `tf_tree_py` too; `just lint` gains `cargo xtask headers --check`.
- The generated headers are committed and carry a "generated, do not edit" banner.

## Implementation plan

1. This record, listed `ready` in `docs/decisions/README.md`.
2. `CLAUDE.md` + `docs/PROJECT.md` §6 restated; `#![deny(unsafe_op_in_unsafe_fn)]`
   on the four existing crates.
3. `crates/tf_tree_c` skeleton: `tft_abi_version_{major,minor}`, the handle header
   with its magic word, `tft_error` + `tft_last_error`, `guard` — a C test forcing a
   Rust panic asserts survival with `TFT_ERR_INTERNAL` (§6.1).
4. `cargo xtask headers` + `--check`, headers committed.
5. The rest of §3.

## Open questions

None. Not decided here: whether `tf_tree_c` is published to crates.io separately
(a Phase 5 §10 packaging question).
