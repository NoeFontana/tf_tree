# 0007: The unsafe budget, restated — and where the C ABI lives

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** this record, plus the `CLAUDE.md` and `docs/PROJECT.md` edits it authorises

## Context

`docs/PHASE4.md` §3 requires a C ABI. Every `extern "C"` entry point takes raw
pointers from a caller the compiler cannot see, so the crate that implements it
is necessarily full of `unsafe`.

`CLAUDE.md` says, under **Hard rules — do not relitigate**:

> **Unsafe budget:** `#![forbid(unsafe_code)]` on `tf_tree_math`, `tf_tree`,
> `tf_tree_cli`. `unsafe` is permitted only in `tf_tree_arena` and in
> `tf_tree_core::{buffer, arena_view}`, each with a module `// SAFETY:` block and
> a per-block `// SAFETY:` comment naming the invariant relied on.

Read literally, **a C ABI crate cannot exist**, and neither can two crates that
already shipped.

### The budget has already been overtaken twice, without amendment

This is the part worth pausing on, because it changes what the right fix is:

| Crate | `unsafe` it carries | Phase | Amended? |
|---|---|---|---|
| `tf_tree_ipc::fork` | `pthread_atfork`, declared locally because libc omits it for `linux_like` (`fork.rs:71`, `:112`) | 2 | **No** |
| `tf_tree_py` | `#![allow(unsafe_code)]` at `lib.rs:45`; numpy slice access and one lifetime transmute (`tree.rs:99-116`) | 3 | **No** |
| `tf_tree_bench::bin::fork_child` | one `libc::fork()` | 2 | Called out in `0005`, but the budget text was never changed |

So the enumeration in `CLAUDE.md` has been wrong since Phase 2 and wrong again
since Phase 3. A reader following it today would conclude that two shipped
crates violate a hard rule.

**An enumeration of crate names is the wrong shape for this rule.** It was right
for Phase 1, when the workspace was five crates and every one of them was pure
Rust. It goes stale every time the project grows a boundary with the outside
world — and growing those boundaries is precisely what Phases 2–4 are.

## Decision

**Replace the enumeration with a rule about what makes `unsafe` acceptable, plus
a short list of the boundaries that currently have it.** The discipline is
unchanged and is the part that was always load-bearing; only the bookkeeping
changes.

### The rule — NORMATIVE

1. **`unsafe` is permitted only at a boundary the compiler cannot see across.**
   Today there are exactly four kinds: the arena's raw memory
   (`tf_tree_arena`, `tf_tree_core::{buffer, arena_view}`), the OS
   (`tf_tree_ipc`), a foreign runtime that owns its own objects
   (`tf_tree_py`), and a foreign *caller* (`tf_tree_c`, new). Anything that is
   not one of those is not eligible, and a fifth kind needs a decision record.

2. **Everything else keeps `#![forbid(unsafe_code)]`** — `tf_tree_math`,
   `tf_tree`, `tf_tree_cli`. **`tf_tree` in particular does not move.** The
   facade staying provably safe is what lets a reader trust that the C ABI's
   `unsafe` is confined to argument validation rather than smeared through the
   engine.

   > **Amended by [`0017`](./0017-owned-handles-and-the-lifetime-rule.md), which
   > moved `tf_tree` to `#![deny(unsafe_code)]` with exactly one `#[allow]`.**
   > The clause above was written as a prediction and it was wrong about one
   > case: `EdgeWriter<'a>` cannot be stored, three consumers needed to store it,
   > and two hand-rolled the lifetime extension in crates the Rust test suite,
   > Miri and TSan cannot instrument — one of them as a
   > `transmute::<EdgeWriter, Publisher>` that leaked a claim lease and bypassed
   > the fork guard for the life of every Python publisher. The block is now
   > written once, in `OwnedWriter`, beside the `ClaimLease` and fork-guard code
   > whose invariants it depends on.
   >
   > **Rule 1 is what made that decidable, and it is unchanged.** This is not a
   > fifth boundary and no new *kind* of `unsafe` was admitted; it is one named
   > exception with a record behind it, which is exactly the shape rule 1 asks
   > for. The reason the paragraph above gave is still the reason `tf_tree` gets
   > `deny` and one greppable `#[allow]` rather than a blanket
   > `#![allow(unsafe_code)]`.

3. **Every `unsafe` block carries a `// SAFETY:` comment naming the invariant it
   relies on, and every crate with `unsafe` carries a module-level `// SAFETY:`
   block explaining the boundary.** Unchanged, and this is the rule that matters.

4. **A crate with `unsafe` must declare `#![deny(unsafe_op_in_unsafe_fn)]`**, so
   an `unsafe fn` does not silently confer permission on its whole body. This is
   new, and it is the one place the discipline gets *stricter*: a C ABI is mostly
   `unsafe fn`s, and without it the `// SAFETY:` requirement above would be
   unenforceable inside exactly the crate that needs it most.

### Where the C ABI lives

A new `crates/tf_tree_c`, `crate-type = ["staticlib", "cdylib", "rlib"]`,
depending on `tf_tree` — not on `tf_tree_core`. It goes through the same safe
facade every other consumer does, so the C ABI cannot reach an invariant the
Rust API protects.

Its `unsafe` is confined to one job: **turning caller pointers into Rust
references, once, at the entry point.** Past that check the body is safe code.
Concretely, the pattern every entry point follows:

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

`guard` is the `catch_unwind` wrapper §3.4 requires. Wrapping *is* the boundary,
so the two requirements collapse into one helper rather than being remembered
separately at ~30 call sites.

### `cbindgen` is a tool, not a dependency — NORMATIVE

`cbindgen` is **MPL-2.0**. `deny.toml`'s allowlist has ten entries and no MPL, so
adding it as a `[build-dependencies]` entry fails `cargo deny check` and
therefore the CI lint job.

That constraint pushes toward the better design anyway: **`cbindgen` runs as an
`xtask` step that regenerates the headers, and the generated headers are
committed.** Consequences, all wanted:

- The license question disappears — a developer tool is not a dependency of the
  shipped artifact.
- **A header change shows up as a reviewable diff.** §3.1 requires `tf_tree.h`
  to be "frozen and reviewed by hand — not merely `cbindgen` output", and a
  header regenerated at build time cannot be reviewed at all.
- A C or C++ consumer needs no Rust toolchain to *read* the interface.

CI runs `cargo xtask headers --check`, which regenerates into a temp dir and
diffs. A drifted header is a failed build, not a surprise at release.

## Rationale

**Why not put the C ABI in `tf_tree` behind a feature?** It would delete
`#![forbid(unsafe_code)]` from the facade — the crate most readers audit first,
and the one whose safety claim carries the most weight with the industrial
integrators D18 is aimed at. A separate crate costs one `Cargo.toml`.

**Why depend on `tf_tree` rather than `tf_tree_core`?** Going to `core` directly
would let the C ABI construct a `Guard` or a `Plan` without the facade's
fork-generation and detach checks, which are exactly the checks a C caller is
least equipped to reproduce.

**Why not keep the enumeration and add one name?** Because it would be the third
amendment nobody made, and the fourth boundary is coming in Phase 5 (the frozen
arena's `mmap`). A rule that states the *criterion* survives; a list does not.

**Alternative considered: vendor `cbindgen`'s output by hand and drop the tool.**
Rejected — ~30 functions plus five enums and two structs is enough surface for
hand-transcription errors, and §3.5's byte-pattern tests would catch a wrong
layout but not a wrong *signature*.

## Consequences

- `CLAUDE.md`'s "Hard rules" bullet and `docs/PROJECT.md` §6's design-smell
  "Writing `unsafe` outside `tf_tree_arena`, `buffer.rs`, or `arena_view.rs`"
  both change. They are the two places a reader meets this rule.
- Two shipped crates stop being in violation of a documented rule.
- `#![deny(unsafe_op_in_unsafe_fn)]` must be added to `tf_tree_arena`,
  `tf_tree_core`, `tf_tree_ipc` and `tf_tree_py` as well, not only to the new
  crate — otherwise the strictest rule applies only to the newest code, which is
  backwards.
- `just lint` gains `cargo xtask headers --check`.
- The generated headers are build artifacts *in the repository*, which is a
  category this project has not had before. They need a "generated, do not edit"
  banner naming the xtask that produces them.

## Implementation plan

1. This record — verified by `docs/decisions/README.md` listing it `ready`.
2. `CLAUDE.md` + `docs/PROJECT.md` §6 restated; `#![deny(unsafe_op_in_unsafe_fn)]`
   added to the four existing crates — verified by `just lint` clean and by
   `grep -rn 'unsafe_op_in_unsafe_fn' crates/` naming all of them.
3. `crates/tf_tree_c` skeleton: `tft_abi_version_{major,minor}`, the handle
   header with its magic word, `tft_error` + `tft_last_error`, and the `guard`
   helper — verified by a C test that forces a Rust panic and asserts the
   process survives with `TFT_ERR_INTERNAL` (§6.1).
4. `cargo xtask headers` + `--check`, headers committed — verified by CI failing
   on a deliberate drift.
5. The rest of §3, per that section.

## Open questions

None. One thing deliberately *not* decided here: whether `tf_tree_c` is
published to crates.io separately or only as a release artifact. That is a
Phase 5 §10 packaging question and does not block any of the above.
