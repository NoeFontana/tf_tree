# 0059: the arena errors that cannot describe themselves

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** (none yet)

## Context

[`0040`](./0040-the-error-that-cannot-be-returned.md) gave `Display` and
`core::error::Error` to the five enums in `crates/tf_tree_core/src/error.rs`, so
that an error can leave a function through `?`. Its scope was those five by
name (its *Context*, first paragraph), and it is `implemented`, so it cannot be
widened in place. Four error types in published crates were outside it and still
have neither trait:

| Type | Defined in | Compiled when | `#[non_exhaustive]` | Variants | Returned by, or wrapped in |
|---|---|---|---|---|---|
| `ShmError` | `tf_tree_arena`, `src/check.rs` | `shm` on Linux | no | 16 | `MappedArena::create`/`attach`, `Tree::attach_shared`/`attach_shared_at`; wrapped by `BuildError::Shm`, `OpenError::Map`, `FrozenError::Arena` |
| `FrozenError` | `tf_tree_arena`, `src/frozen.rs` | `shm` on Linux | no | 9 | `FrozenArena::open`, `read_manifest`, `write_frozen`; wrapped by `FrozenFileError::Frozen` |
| `LayoutError` | `tf_tree_arena`, `src/layout.rs` | always | yes | 3 | `ArenaLayout::new`; wrapped by `BuildError::Layout` |
| `ParticipantError` | `tf_tree_core`, `src/participant.rs` | always | yes | 3 | `ParticipantTable::register`/`register_at`; wrapped by `BuildError::Participant` |

A workspace sweep for `impl .*Display for` and `impl .*Error for` over every
`pub enum *Error` in `tf_tree_math`, `tf_tree_arena`, `tf_tree_core`,
`tf_tree_ipc` and `tf_tree` finds these four and three more with neither trait.
The three are out of scope, and *Decision* 7 says why.

**#339, in flight, fixed everything around these four and stopped at them on
purpose.** On its tip (`b2fa6c2`) the facade prints `BuildError::Frame`,
`BuildError::Topology`, `ReparentError::Topology` and `AwaitError::Frame` with
their payload's `Display`. It also re-exports `LayoutError` and
`ParticipantError` at the root, and amends `tf_tree_core`'s crate docs to put
`ParticipantError` on the promise. Its `CHANGELOG.md` entry names what is left:

> **Not in this change:** `BuildError::Layout`/`Shm`/`Participant`,
> `OpenError::Map` and `FrozenFileError::Frozen` still print `Debug`, because
> their payloads (`LayoutError`, `ShmError`, `ParticipantError`, `FrozenError`)
> have no `Display`, and adding one to a published type is a trait commitment
> that wants a record extending `0040`.

This is that record.

### What a user reads today, measured

These were measured on #339's tip, because that is the base this record's
implementation lands on. A scratch binary depends on `tf_tree` (`shm`) from a
checkout of `b2fa6c2`. It **constructs** each wrapper around a chosen payload and
prints its `Display` and its `source()`. So the table shows how each wrapper
renders; it does not claim each combination occurs. The two marked *constructed*
cannot reach a user (see below the table).

| Wrapper | `Display` | `source()` |
|---|---|---|
| `BuildError::Layout` | `arena layout error: ArenaTooLarge { total_size: 5000000000 }` | `None` |
| `BuildError::Shm` | `shared memory error: Truncate(Os { code: 28, kind: StorageFull, message: "No space left on device" })` | `None` |
| `BuildError::Participant` (*constructed*) | `participant table full: SlotOutOfRange { slot: 70, capacity: 64 }` | `None` |
| `OpenError::Map` | `LayoutMismatch { found: 1024475540, expected: 1024475541 }` | `None` |
| `OpenError::Map` (*constructed*) | `Create(Os { code: 24, kind: Uncategorized, message: "Too many open files" })` | `None` |
| `FrozenFileError::Frozen` | `Io(Os { code: 13, kind: PermissionDenied, message: "Permission denied" })` | `None` |
| `FrozenFileError::Frozen` | `Arena(SizeMismatch { actual: 4096, expected: 8192 })` | `None` |

- **`BuildError::Participant` only ever holds `TableFull`.** Its two producers
  (`TreeBuilder::build` and `build_shared`, `tf_tree/src/tree.rs`) go through
  `register_participant`, which calls `ParticipantTable::register`, whose only
  error is `TableFull`. So its prefix *"participant table full"* is true of every
  value that reaches it.
- **`OpenError::Map` never carries `Create`.** It is built only by
  `From<ShmError> for OpenError`, reached from `attach_joined_at` and
  `MappedArena::attach` (`tf_tree/src/open.rs`), and neither calls
  `memfd_create`. The only `ShmError::Create` producer is `MappedArena::create`,
  reached through `build_shared`, so a refused memfd arrives as
  `OpenError::Build(BuildError::Shm(ShmError::Create(..)))`, which is the second
  row.

Three things in that picture are worse than a missing sentence:

- **A joiner is told the participant table is full when it is not.**
  `register_at`, the only source of `ParticipantError::SlotTaken` and
  `SlotOutOfRange`, is called from `attach_shared_inner` as
  `register_participant_at(&view, s).map_err(|_| ShmError::ParticipantTableFull)?`
  (`tf_tree/src/tree.rs`), and `Open`'s joiner reaches it through
  `attach_joined_at`. `ShmError::ParticipantTableFull`'s rustdoc says *"Every
  participant slot is taken, so this process cannot join"*. `open.rs`'s slot
  assigner already records the consequence: granting a non-`FREE` slot *"hands
  the joiner `ShmError::ParticipantTableFull` … about the very slot this loop
  just decided was free"*. A table that really is full is refused before the
  attach, as `HelloStatus::NoParticipantSlots` or `IpcError::NoParticipantSlots`.
  So on the `Open` path this variant almost never means full. Today the `Debug`
  dump prints only the discriminant; a `Display` written from the rustdoc would
  turn that into a false sentence.
- **The layout hash prints in decimal.** Every document in this repository
  spells it in hex (`layout_hash` `0x3D10_4195`, `PHASE5.md` §0.0). An operator
  comparing `1024475541` against the docs has to convert it by hand.
- **The errno's text comes from a `rustix` feature, not from this crate.** See
  *Rationale*, *Why `errno N`*.

**`?` fails one layer out, the same way it failed before `0040`.** This scratch
function does not compile against `b2fa6c2`:

```rust
fn attach(fd: OwnedFd) -> Result<Tree, Box<dyn std::error::Error>> {
    Ok(Tree::attach_shared(fd, AttachMode::ReadOnly)?)
}
```

```text
error[E0277]: `?` couldn't convert the error: `ShmError: std::error::Error` is not satisfied
  = note: required for `Box<dyn std::error::Error>` to implement `From<ShmError>`
```

`Tree::attach_shared` is a facade entry point that returns `ShmError` bare, with
no wrapper to derive an `Error` impl through. `MappedArena::attach`,
`FrozenArena::open` and `ArenaLayout::new` do the same in the published arena
crate.

### Who works around it, and what each workaround says

- **Python** labels the dump as raw in three arms, and says why in comments
  that become false once this record is implemented. The arms are
  `crates/tf_tree_py/src/errors.rs:502` (`BuildError::Shm`), `:526`
  (`OpenError::Map`) and `crates/tf_tree_py/src/offline.rs:644`
  (`FrozenError::Arena`), all at `b2fa6c2`. The comments are `errors.rs:415-422`
  and `offline.rs:633` (*"`ShmError` has no `Display`, sixteen variants …"*),
  `errors.rs:389-393` (`build_err`'s *"five Debug dumps … two of the five print
  their payload's `Display` now"*), `errors.rs:514-518` (`open_err`'s *"every
  one of those five Debug dumps"*) and `errors.rs:523` (*"`#[error("{0:?}")]`,
  and the same argument as `BuildError::Shm`"*). Measured through the
  extension installed in `.venv`: flipping one bit of the arena's `layout_hash`
  inside a frozen `.tft` (the arena header's offset 12, at file offset
  2 097 152) makes `open_file` raise

  ```text
  TfTreeError: …/bad_hash.tft: contains an arena image whose header did not
  validate; the file is corrupt, or was written by a build with a different arena
  layout. The engine's reason, raw: LayoutMismatch { found: 1024475540, expected: 1024475541 }
  ```

  Flipping the arena's magic byte instead ends the same message in `raw: BadMagic`.
- **C.** `tft_tree_open_named` writes `could not open the arena: {e}` into the
  message buffer (`crates/tf_tree_c/src/unstable.rs:475`), and the bridge writes
  `shared arena could not be created: {detail} …` (`bridge.rs:1089`). The
  bridge's comment at `bridge.rs:1045-1049` says *"a refused memfd … arrives with
  its own text"*. It does not: that text is the `BuildError::Shm` row above,
  reached through `OpenError::Build`. Every one of these failures carries the
  single status `TFT_ERR_ARENA_UNAVAILABLE`, so for a C caller the message is
  the only thing that says which check failed.
- **The CLI** prints `{e}` at `crates/tf_tree_cli/src/lib.rs:1101` (`open_frozen`)
  and `:1871` (`freeze_to`), and through `anyhow` context at `attach.rs:91`. So
  an operator gets the table's rows verbatim.
- **`docs/RUNBOOK.md`** heads its shared-memory entries with these very `Debug`
  spellings (`` `LayoutMismatch { found, expected }` ``, `` `HeaderInconsistent` ``,
  `` `Unsealed` ``, `` `ParticipantTableFull` / `NoParticipantSlots` ``). The
  discriminant in today's dump is what finds the entry.

### Constraints this record has to check, not assume

- **`no_std`.** `tf_tree_arena` and `tf_tree_core` are `#![no_std]`. The arena
  names `std` only under `cfg(test)`. The core names it under
  `cfg(any(test, feature = "crash-points"))` (`lib.rs:128`), which is
  default-off. So the trait is `core::error::Error`.
  **Verified at the MSRV:** `core::error::Error` carries
  `#[stable(feature = "error_in_core", since = "1.81.0")]` in the stable and
  nightly `rust-src`. A `#![no_std]` library that implements it compiles with
  `rustc +1.87` and `+1.83`. The negative control, the same file spelling
  `std::error::Error`, fails with `E0433`. `0040` already relies on this at 1.87,
  and `just msrv` builds it.
- **No allocation.** `Display` writes into the caller's `Formatter`. Nothing may
  call `alloc::format!` or hold a `String`. `tf_tree_arena` links `alloc`, and
  the impls must not use it anyway. The reason is `0040`'s: an error type's
  rendering should not be what makes it unusable on a path that does not
  allocate.
- **`rustix`'s `std` feature is on in every build of `tf_tree_arena --features
  shm`, whatever its manifest comment says.** The arena's `Cargo.toml` says
  `default-features = false` *"keeps it `no_std`"*. The dependency is
  `workspace = true`, and the workspace entry lists `features = ["mm", "fs",
  "std"]`. `cargo tree -p tf_tree_arena --features shm -e features -i rustix`
  shows `rustix feature "std"` enabled by `tf_tree_arena` itself. The crate's
  own code is still `no_std`, so this does not change which trait is nameable.
  It does decide what `Errno`'s `Display` prints (*Rationale*). Whether the
  manifest comment or the inheritance is the defect is not this record's
  question.
- **`rustix::io::Errno` has a bounded constructor.** Nine variants carry one
  (seven in `ShmError`, two in `FrozenError`). `Errno` is a `u16` holding the
  negated code, and `Errno::from_raw_os_error` asserts the code is in `1..=4095`
  (`rustix-1.1.4/src/backend/linux_raw/io/errno.rs`): 4095 constructs, and 4096
  and `i32::MAX` panic.
- **`PHASE5.md` §2.4 is NORMATIVE about one of these texts.** *"`layout_hash`
  mismatch is a hard error naming both values and stating that the file must be
  re-frozen."* §2.4's read path validates the layout hash twice: in the
  `FrozenHeader` (`FrozenError::LayoutMismatch`) and in the mapped arena header
  (`validate_arena_header`, which yields `FrozenError::Arena(ShmError::LayoutMismatch)`).
  Today the Rust facade prints `{0:?}`, which names both values and never says
  re-freeze; the CLI and Python each add their own sentence. Once decision 4
  prints `FrozenFileError::Frozen` bare, the payload's `Display` is the whole
  Rust message. Separately, `PHASE2.md` §3.7 says a `LayoutMismatch` on attach
  *"must say exactly that"* (a binary built against a different struct layout),
  which is not what the same `ShmError` variant means inside a `.tft` whose
  container hash already matched.

## Decision

**1. `ShmError`, `FrozenError`, `LayoutError` and `ParticipantError` implement
`core::fmt::Display` and `core::error::Error`, by hand, in the file that defines
each.**

Every `match` is exhaustive with no catch-all, as in `0040` step 1. Inside the
defining crate `#[non_exhaustive]` grants no wildcard, so a variant added later
fails to compile there. The impls use no `thiserror` and add no dependency.

**2. What the text may contain.** These are rules, not sentences, because
`docs/API.md` R5 makes sentences uncontracted:

- **(a) What the variant carries, and nothing resolved.** Sizes, versions,
  slots and capacities print in decimal. Layout hashes print as `0x{:08X}`,
  which is how `IpcError`'s `Display` (`layout_hash 0x{owner_layout_hash:08X}`)
  and `tf_tree doctor --explain-version` print them, so an operator sees one
  spelling from every Rust layer. It is also what `0032`'s case-sensitive census
  regex matches. No name is resolved, because these types have no arena to
  resolve against, just as `0040` decision 1 found for the core enums.
- **(b) An errno prints as `errno N`, from `Errno::raw_os_error()`**, and never
  through `Errno`'s `Display` or `Debug`. This matches `IpcError`'s errno arms
  and `FrozenFileError::Path` (`could not open the .tft path (errno N)`).
- **(c) ASCII only.**
- **(d) One clause that states the condition.** A remedy may follow only if it
  names no binding's API. *"Use `tf_tree::Open`"* and *"attach
  `AttachMode::ReadOnly`"* are not allowed on
  `ShmError::ReadWriteNeedsRendezvous`. That text reaches a C and a Python reader
  verbatim, and `docs/API.md` §2.8 already establishes, for `TreeTooDeep`, that
  the remedy sentence is binding-specific. Such remedies stay in the rustdoc and
  in each binding's own prose. **One remedy is required, not allowed**:
  `FrozenError::LayoutMismatch` names both hashes and states that the file must be
  re-frozen, because `PHASE5.md` §2.4 is NORMATIVE that it does. The same holds
  for `FrozenError::Arena`, which appends the re-freeze statement after its
  payload's text. Its one producer is `validate_arena_header` in
  `FrozenArena::open`, so every value it carries is an arena-header failure
  inside a `.tft` whose container header validated: a damaged or mis-written
  file, a `.tft` is a cache, and
  §2.4's read path covers the arena header's hash too. `ShmError::LayoutMismatch`
  itself stays neutral about the cause, because the `memfd` attach path, where
  `PHASE2.md` §3.7 gives it a different meaning, shares it.
- **(e) At most 120 bytes**, rendered with every carried integer at its type's
  maximum, except an `Errno`, which is built at 4095 (*Context*) and renders as
  `errno 4095`. For `FrozenError::Arena` the bound applies to the **whole** nested
  rendering, for every `ShmError` payload. The derivation is in *Rationale*.
- **(f) No `Display` claims the participant table is full unless the variant
  can only mean that.** `ParticipantError::TableFull` may. `ShmError::ParticipantTableFull`
  may not: on the `Open` path it is `attach_shared_inner`'s erasure of
  `SlotTaken` and `SlotOutOfRange` (*Context*), so its text says the process could
  not register in the arena's participant table, and its rustdoc is corrected to
  match. Carrying the real cause would need a new variant on an exhaustive enum,
  which is not this record's change (*Consequences*).
- **(g) The text ends with the innermost variant's name as a search key**, in
  parentheses: `(LayoutMismatch)`, `(BadMagic)`. `FrozenError::Arena` adds none
  of its own, because its payload's name is the more specific key. This is the
  choice `push_msg` already made for `NonMonotonicStamp` in the Python binding
  (*"the variant name is back, and it is a search key rather than a dump"*), for
  the same reason: `docs/RUNBOOK.md` heads its entries with the name, and a C
  caller has one status code for all of these. The name is R5's contract layer,
  so repeating it promises nothing the type does not already promise.

**3. No `source()`.** The payload of `FrozenError::Arena` is written inline
through `ShmError`'s `Display`. `source()` stays the default `None` on all four
types. This matches the facade today: every wrapper above measured `None`, and
`OpenError::Rendezvous` and `OpenError::Build` already print `{0}` with no
`#[source]`.

**4. The five facade wrappers print `{0}`, and no prefix restates or
contradicts its payload.**

- `BuildError::Layout` and `BuildError::Shm` keep a prefix that names the stage.
- `BuildError::Participant` loses *"participant table full"*. It is true of every
  value that reaches the wrapper, and it would say again what
  `ParticipantError::TableFull`'s own `Display` now says.
- `OpenError::Map` and `FrozenFileError::Frozen` print the payload bare, as
  `OpenError::Rendezvous` and `OpenError::Build` do.

The wording of each prefix is left to the implementing PR, because text is
uncontracted.

**5. The rustdoc on each impl says what `0040`'s say.** The text is a
diagnostic that may change in any release, and the discriminant is the contract.
No doc example shows a literal message.

**6. What this commits to under R5, stated precisely.** It is three separate
things with three different answers:

- **That the impls exist: committed.** Removing either one breaks compilation
  for any caller that `?`s the type into `Box<dyn Error>` or `anyhow::Error`, or
  formats it with `{}`. This is the category `0040`'s *Consequences* names:
  *"Removing a trait impl is breaking, so this is a commitment"*. On the `0.0.x`
  line cargo already treats every release as incompatible, so the commitment is
  one of intent. It is still the one `0040` made, and it is made the same way.
- **The text: not committed, with one exception a spec already made.** R5 is
  NORMATIVE: *"exception and error* types *are a compatibility promise. Message*
  text *is not, and no surface may document text that a downstream caller could
  be tempted to match on."* These impls fill the middle row of R5's layer table
  (*"`Display` on the error itself — knows what the error carries"*). They add no
  layer and no promise to it. Tests in this repository may pin structure:
  non-empty, no braces, ASCII, the carried numbers present, the variant name
  present, the length. They may not pin a sentence, which is the line `0040`'s
  test already draws. The exception is decision 2(d)'s re-freeze statement,
  whose presence `PHASE5.md` §2.4 requires, so a test holds that the rendering
  mentions re-freezing and no more.
- **`source()` returning `None`: not committed.** Changing it later would change
  what a chain-walker such as `anyhow`'s `{:#}` prints, and nothing a type
  checker sees. That puts it with the text, and the rustdoc says so.

Two things this does **not** change. It makes no promise about variant sets:
`LayoutError` and `ParticipantError` stay `#[non_exhaustive]`, and `ShmError`
and `FrozenError` stay exhaustive, a commitment they already carry and this
record neither makes nor revisits. And it moves no type between tiers:
`ShmError` and `FrozenError` are already at the facade root, and #339 places
`LayoutError` and `ParticipantError` there under `docs/API.md` §2.6.

**7. Scope: these four, and not the three the sweep also found.**

- `TopoLockError` (`tf_tree_core::topology`) reaches no user. Its own doc says
  the facade converts it at the boundary into `ReparentError`, which is what a
  caller sees.
- `WireError` and `ProcParseError` (`tf_tree_ipc`) are carried inside
  `IpcError`, and since #339 both are also **nameable at `tf_tree`'s root**
  (under `shm`, `docs/API.md` §6 row 19). So reachability is not what separates
  them from the four. What does is where a user meets them. No facade wrapper
  prints either with `Debug`: #339 made `IpcError`'s `Display` describe both in
  words, and no facade entry point returns either bare, so the only way to hold
  one unwrapped is to call `HelloResponse::from_bytes` or `parse_start_time` on
  `tf_tree_ipc` directly. The four in scope are printed with `Debug` by five
  facade wrappers today, and one of them is returned bare by
  `Tree::attach_shared`. #339 chose an inline match inside `IpcError`'s
  `Display` over a trait impl, on the ground that *"a trait impl on a published
  type is a commitment, and this sentence is not one"*. That route works where
  one wrapper is the only printer. It does not work for the four, because a bare
  `ShmError` has no wrapper to match inside (*Rationale*, *Why at the
  definition*), and a trait impl is then the commitment this record makes on
  purpose.

A later record may adopt the stronger rule, *every public error type in a
published crate*. The case for it would be a caller who propagates
`HelloResponse::from_bytes` or `parse_start_time` directly, and no such caller
exists in the workspace today.

**8. The follow-ons land with the impls, in the same PR**, because each is a
sentence the impls make false:

- **Python.** The three `raw: {inner:?}` arms become `{inner}`. The comments
  that describe the dumps go or are reworded: `errors.rs:415-422` and
  `offline.rs:633` (no `Display` on `ShmError`), `errors.rs:389-393` (the
  *"five Debug dumps"* count), `errors.rs:514-518` (`open_err`'s doc) and
  `errors.rs:523` (`OpenError::Map`'s `{0:?}`). Python's per-variant prose
  stays: `FrozenError`'s arms in `offline.rs`, and `BuildError::Layout` and
  `::Participant` in `errors.rs`. It carries Python-specific remedies (lower
  `capacity=`), which R5 allows because the prose layer differs per binding.
  `BuildError::Participant`'s *"every slot is occupied"* is true of every value
  that reaches it (*Context*), so it stays.
- **C.** No status code changes, and no header or ABI changes. The messages
  improve through the existing `{e}`. The comment at `bridge.rs:1045-1049`
  becomes true, and is reworded so that it no longer implies it always was.
- **CLI.** No change. Its three `{e}` sites improve on their own.
- **`docs/RUNBOOK.md`.** No change: decision 2(g) keeps the variant name that
  its headings are searched by.
- **Docs.** `CHANGELOG.md` gets an entry, which `just artifact-versions`
  requires for a `src/` change. R5's layer paragraph cites `0059` beside
  `0040`. **No `docs/API.md` §6 row**, and #339's row 19 is not the precedent it
  looks like. §6 lists what the document adds to what is already specified. Row
  19 added root re-exports, which no rule specified. These impls fill R5's
  middle layer, which `0040` already specified for error types, so they add
  nothing §6 would record, which is also why `0040` added none.

## Rationale

**Why at the definition, not a facade wrapper.** A `tf_tree`-side helper, such
as a `Shown(ShmError)` newtype or a private formatting function called from the
`thiserror` attributes, would keep the arena crates free of any new trait impl.
It fails three ways:

- `Tree::attach_shared` returns `ShmError` bare, so its `?` stays broken (the
  `E0277` above).
- The published arena API (`MappedArena::attach`, `FrozenArena::open`,
  `write_frozen`, `ArenaLayout::new`) stays unpropagatable.
- Python and C would have to call the helper rather than `{}`, which is a second
  spelling of one rendering (`PROJECT.md` §6).

The Python comments already reject writing a second copy of `check.rs`'s
reasons *"in a place that cannot see them change"*. A facade helper is exactly
that place.

**Why not `thiserror`.** `tf_tree_core`'s budget is D14's (`libm` + `bytemuck` +
`blake3`), and `tf_tree_arena`'s manifest describes a two-dependency budget.
That settles it. **One of `0040`'s two reasons was already false when `0040` was
written, and this record does not repeat it.** `0040` also said `thiserror` *"is
a `std`-oriented derive whose generated `Error` impl is `std::error::Error`"*.
That was true of `thiserror` 1. The workspace has pinned `thiserror = "2"` since
its initial commit (`4e068a0`), and #278, which implemented `0040`, locked
`2.0.19`. That crate is `#![no_std]`, has `default = ["std"]`, and its
`src/private.rs:11` is an unconditional `pub use core::error::Error`, which is
the path the derive expands to. So only the budget argument ever applied.
`0040` is `implemented` and frozen, so the correction is made in its index row
in `docs/decisions/README.md`, in the same change that adds this record.

**Why `errno N`, and not `Errno`'s own `Display`.** Delegating would print more:
`No space left on device (os error 28)` rather than `errno 28`. It still loses,
for three reasons:

1. **The text would depend on a feature flag nobody chose for it.** `rustix`'s
   `Errno` prints `std::io::Error`'s rendering when its `std` feature is on and
   `os error N` when it is off (`rustix-1.1.4/src/io/errno.rs:28-52`). That
   feature reaches the arena through workspace inheritance, against the
   manifest's own comment (*Context*).
2. **With `std` on, it is `strerror_r`'s text.** glibc translates that under
   `LC_MESSAGES` in a host process that has called `setlocale`. The result can
   be non-ASCII, which breaks decision 2(c) for exactly the C caller that rule
   protects. *Not measured*: this host has no non-English locale (`locale -a`
   lists `C`, `C.utf8`, `en_US.utf8` and `POSIX`).
3. **`errno N` is the house spelling.** `IpcError`'s errno arms and
   `FrozenFileError::Path` both use it.

**The cost is real and is written down here.** An operator reading the CLI loses
the *"Permission denied"* that today's `Debug` dump happens to include. A layer
with `std` can restore it. Python already does for `FrozenError::Io`, through
`std::io::Error::from_raw_os_error` (`offline.rs`).

**Why ASCII.** `tft_error::set_message` replaces every non-ASCII *byte* with `?`
(`crates/tf_tree_c/src/error.rs:253-265`), so an em dash reaches C as `???`. The
hazard is live next door. On #339's tip, `IpcError`'s `Display` arms for
`NetworkFilesystem` and `ArenaHeldButUnreachable` contain em dashes and `§`, and
name `CreatePolicy::Always`, and they reach C through the same `{e}`. That is
recorded here as an observation. It is a sibling defect in a type this record
does not cover.

**Why 120 bytes.** `TFT_MESSAGE_LEN` is 256, including the NUL. The longest
fixed text a C path puts *before* one of these payloads is the bridge's
`shared arena could not be created: ` (35 bytes) followed by
`BuildError::Shm`'s current `shared memory error: ` (21). That leaves 199 bytes.
The arena name that follows is designed to be the part the buffer loses
(`generic_failure_message`'s doc). 120 leaves 79 bytes for a reworded prefix.
The number is derived, not measured against a failure, and a reviewer may move
it. What matters is that a test holds *some* bound, so a remedy paragraph cannot
grow into the arena layer. `FrozenError::Arena`'s nested rendering, with its
re-freeze statement, is the longest text the bound has to hold, which is why
decision 2(e) applies it there in full.

**Why no `source()`.** Only `Display` reaches the C buffer and the Python
message. A payload printed inline *and* returned from `source()` appears twice
under `anyhow`'s `{:#}`. And the facade has made this choice at every wrapper
already.

**Why the variant name stays in the text.** The alternative is a test that
forbids it, so that no `Debug` leaks. But the name is not what makes a dump a
dump: braces, field lists and decimal hashes are, and the tests forbid those.
Removing the name would cost every runbook lookup and every C caller's only
discriminator, to prevent a leak the other assertions already catch.

## Consequences

- `?` works from `Tree::attach_shared`, `MappedArena::attach`,
  `FrozenArena::open`, `write_frozen` and `ArenaLayout::new` into
  `Box<dyn Error>` and `anyhow::Error`.
- Four public types gain two trait impls each: 31 variants, each written once.
  A variant added later is a compile error in its `Display` and in the test's
  exhaustiveness guard (step 2).
- Every user-visible rendering in *What a user reads today* becomes one clause
  with no braces, ending in its variant name. The CLI shows errno numbers where
  the dump showed strerror text (*Rationale*).
- The Rust facade satisfies `PHASE5.md` §2.4's re-freeze statement on its own
  for the first time, rather than through the CLI's and Python's sentences.
- The Python binding's three "raw" arms and its false comments go. The C
  bridge's comment becomes true.
- **The dependency on #339 is hard.** The facade attributes, the root
  re-exports and `tests/error_payloads.rs`'s `prints_its_payload` are all #339's.
  If that PR changes in review, step 0 re-reads it.
- **Not decided here, and named so that nobody reads it as decided:**
  `IpcError`'s non-ASCII text on the C path; `WireError`, `ProcParseError` and
  `TopoLockError`; the arena manifest's `rustix`/`std` comment;
  `attach_shared_inner`'s erasure of `SlotTaken`/`SlotOutOfRange` into
  `ParticipantTableFull`, which decision 2(f) only stops from lying and which a
  variant carrying the cause would fix; and `tf_tree_bench`'s
  `map_err(|e| anyhow!("…{e:?}"))` sites (`backing.rs:807`, `:922`;
  `frozen_workers.rs:348`, `:366`; `bin/frozen_open.rs:385`). Those format a
  facade wrapper that already implements `std::error::Error`; after this record
  they could print `{e}` instead of a `Debug` dump, in a `publish = false`
  crate.

## Implementation plan

Steps 1 to 4 land as **one PR**. Steps 3 and 4 fix sentences that step 1 makes
false (the facade attributes' `{0:?}`, Python's "raw" arms and comments, the
bridge's comment), so landing them apart would ship code and comments that
contradict each other for as long as the gap lasted.

0. **Rebase onto `main` after #339 merges**, and re-read its five attributes,
   its re-exports and `tests/error_payloads.rs`. Verified by `git grep -n
   '{0:?}' crates/tf_tree/src` naming exactly the five sites listed in *Context*.
1. **The four impls, following decisions 1–3 and 5, and
   `ShmError::ParticipantTableFull`'s rustdoc per decision 2(f).** Verified by
   `just lint`, `just shm-check` (`cargo clippy -p tf_tree_arena --features shm
   --all-targets` compiles `check.rs` and `frozen.rs`), `just doc` and
   `just msrv`.
2. **A rendering test beside each type**, in the style of `tf_tree_core`'s
   `every_error_variant_renders_as_prose_naming_what_it_carries`. Each test does
   the following:
   - Construct one value of every variant, with every carried integer at its
     type's maximum and every `Errno` at 4095. The list is guarded by an
     exhaustive `match`, so a new variant does not compile until the test covers
     it.
   - For each value, assert:
     - it is non-empty;
     - it differs from `{:?}`;
     - it contains no `{` or `}`;
     - it `is_ascii()`;
     - it is at most 120 bytes;
     - it contains the innermost variant's name (the identifier before the first
       `(` or ` {` of `format!("{e:?}")`, computed rather than hard-coded, and
       taken from the inner value for `FrozenError::Arena`);
     - it contains every carried integer in decision 2(a)'s spelling, an `Errno`
       as `errno {raw_os_error()}`.
   - For `FrozenError`, **also wrap every value from `ShmError`'s list in
     `FrozenError::Arena`**, and assert each nested rendering holds every
     assertion above and contains the inner value's `Display`, computed. That is
     what fails an arm written as `{inner:?}` when the inner value is a unit
     variant with no braces to catch it.
   - For `FrozenError::LayoutMismatch` and every `FrozenError::Arena` value,
     assert the rendering mentions re-freezing (decision 2(d)).
   - For `ShmError::ParticipantTableFull`, assert that the rendering with its
     trailing variant name removed does not contain `full`, case-insensitively
     (decision 2(f)); the name itself ends in `Full`.
   - For `ShmError`, also `?` a value into `Box<dyn core::error::Error>`, as the
     compile-time pin `0040` used.

   **Mutants, each applied alone. The PR quotes the failure each one produced,
   and the test is not merged on a predicted failure:**
   - (M1) `ShmError::SizeMismatch`'s arm → `write!(f, "{self:?}")`.
   - (M2) `ParticipantError::TableFull`'s arm → `Ok(())`.
   - (M3) `LayoutError::ArenaTooLarge`'s arm drops `{total_size}`.
   - (M4) delete `impl core::error::Error for ShmError`. This should be `E0277`
     at the `?`.
   - (M5) `FrozenError::Arena`'s arm → `write!(f, "arena header did not
     validate: {inner:?}")`, which the unit-variant payloads must catch.
   - (M6) drop the re-freeze clause from `FrozenError::LayoutMismatch`'s arm.

   Verified by `cargo nextest run -p tf_tree_arena --features shm` (a line of
   `just shm-check`) and by `just test` for `ParticipantError` and
   `LayoutError`.
3. **The facade.**
   - Change the five attributes to `{0}` per decision 4.
   - Extend #339's `wrapped_payloads_print_their_display` with
     `prints_its_payload` over `BuildError::Layout` and `BuildError::Participant`.
     Under `cfg(all(feature = "shm", target_os = "linux"))`, cover
     `BuildError::Shm`, `OpenError::Map` and `FrozenFileError::Frozen`, plus a
     `?` from `Tree::attach_shared` into `Box<dyn Error>`.
   - **Make a recipe run it with `shm`.** On #339's tip `just shm-check` only
     *clippies* that target under `shm`: `grep -n error_payloads justfile`
     finds nothing, and the recipe's `tf_tree --features shm` nextest lines
     name `--lib`, `--test frozen`, `--test rendezvous` and `--test
     owned_writer`. So a runtime assertion under that `cfg` would be compiled
     and never executed. Add `cargo nextest run -p tf_tree --features shm
     --test error_payloads` to `just shm-check` in the same commit as the
     `shm`-only case, which is the spirit of `CLAUDE.md`'s `just shm-check` row
     (*"a new `shm`-only target belongs on that list in the commit that adds
     it"*), though this target is not new.
   - **Mutant (M7):** `OpenError::Map` back to `{0:?}`.

   Verified by `cargo nextest list -p tf_tree --features shm --test
   error_payloads` naming the new cases, and by M7 failing under
   `just shm-check`.
4. **The follow-ons in decision 8.**
   - Python: the three arms, the comments decision 8 lists, and a
     `tests/python/test_frozen.py` case that flips one `layout_hash` bit of the
     arena header inside a frozen `.tft`, which is this record's own probe. It
     asserts the `TfTreeError` message contains no `{` and no `raw`. **The
     magic-byte flip is not a row**: `BadMagic` has no braces, and under decision
     2(g) its `Debug` spelling is a substring of its `Display`, so no structural
     assertion on the Python message can tell `{inner}` from `{inner:?}` there.
     The Rust nested test (step 2) is what holds that case.
   - **Mutant (M8):** restore `{inner:?}` in `offline.rs`, which the hash row's
     brace assertion must catch.
   - C: the comment at `bridge.rs:1045-1049`.
   - `CHANGELOG.md`, and R5's citation.

   Verified by `just py-lint`, `just py-test`, `just c-header-check` (no
   header change is expected, and the check is what shows none happened), and
   `just artifact-versions`.

## Open questions

None. Four judgement calls were made while writing, and they are the ones a
reviewer is likeliest to reopen, so each names its alternative:

- **The `errno N` spelling** over `Errno`'s strerror text. The alternative is
  more informative today and loses on feature-dependence, locale and house
  style (*Rationale*).
- **`source()` staying `None`.** The alternative is a real error chain, and it
  loses because the C buffer and Python message cannot walk one and `anyhow`
  would print the payload twice.
- **The variant name kept as a trailing search key** (decision 2(g)). The
  alternative forbids it in the text and rewrites `docs/RUNBOOK.md`'s headings
  to the new sentences, and loses because those sentences are uncontracted and
  would have to be re-synchronised with the runbook every time one changed.
- **Scope at four types rather than every public error type** (decision 7). The
  alternative adds `WireError`, `ProcParseError` and `TopoLockError`, none of
  which a facade wrapper prints with `Debug` or a facade entry point returns bare.

The 120-byte bound is a derived number rather than a question. Moving it changes
one constant in four tests.
