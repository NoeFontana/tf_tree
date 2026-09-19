# 0059: the arena errors that cannot describe themselves

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** #345 (step 1, all four parts). Step 0, re-reading #339's
merged code, was done in the change that moved this record to `ready` (#343).

## Decision

**1. `ShmError`, `FrozenError`, `LayoutError` and `ParticipantError` implement
`core::fmt::Display` and `core::error::Error`, by hand, in the file that defines
each,** every `match` exhaustive with no catch-all (`0040` step 1). No
`thiserror`, no new dependency.

**2. What the text may contain.** Rules, not sentences (`docs/API.md` R5):

- **(a) What the variant carries, and nothing resolved.** Numbers in decimal;
  layout hashes as `0x{:08X}`, as `IpcError` prints them.
- **(b) An errno prints as `errno N`**, from `Errno::raw_os_error()`, never
  through `Errno`'s `Display` or `Debug` (feature- and locale-dependent).
- **(c) ASCII only** (`tft_error::set_message` turns non-ASCII bytes into `?`).
- **(d) One clause that states the condition.** A remedy may follow only if it
  names no binding's API. **Required:** `FrozenError::LayoutMismatch` names both
  hashes and states the file must be re-frozen (`PHASE5.md` §2.4 NORMATIVE);
  `FrozenError::Arena` states re-freezing **before** its payload's text.
  `ShmError::LayoutMismatch` stays neutral about the cause (the `memfd` path,
  `PHASE2.md` §3.7, shares it).
- **(e) At most 120 bytes**, with every carried integer at its type's maximum
  and every `Errno` at 4095; for `FrozenError::Arena`, the whole nested
  rendering for every `ShmError` payload.
- **(f) No `Display` claims the participant table is full unless the variant can
  only mean that.** `ParticipantError::TableFull` may;
  `ShmError::ParticipantTableFull` may not (`attach_shared_inner` erases
  `SlotTaken` and `SlotOutOfRange` into it).
- **(g) The text ends with the innermost variant's name as a search key**, in
  parentheses: `(LayoutMismatch)`. `FrozenError::Arena` adds none of its own and
  ends with its payload's name.

**3. No `source()`;** `FrozenError::Arena`'s payload is written inline.

**4. The five facade wrappers print `{0}`, and no prefix restates or contradicts
its payload.** `BuildError::Layout` and `::Shm` keep a stage-naming prefix;
`BuildError::Participant` loses *"participant table full"*; `OpenError::Map` and
`FrozenFileError::Frozen` print the payload bare.

**5. Each impl's rustdoc says the text may change in any release.**

**6. Under R5 the impls' existence is committed; the text and `source()`
returning `None` are not**, except 2(d)'s re-freeze statement. Tests pin
structure, never a sentence. Variant sets and `#[non_exhaustive]` are unchanged.

**7. Scope: these four.** `TopoLockError`, `WireError` and `ProcParseError`
reach users only through `ReparentError` and `IpcError`, which describe them.

**8. Follow-ons land in the same PR.** Python's three `raw: {inner:?}` arms
become `{inner}`; the `bridge.rs` comment is reworded; `CHANGELOG.md` gets an
entry and R5's layer paragraph cites `0059` beside `0040`; no C ABI, CLI,
`RUNBOOK.md` or `docs/API.md` §6 change.

## Rationale

At the definition, not a facade helper: a helper leaves `Tree::attach_shared`
and the published arena API unpropagatable and gives Python and C a second
spelling of one rendering. Not `thiserror`: D14's dependency budget. **120
bytes:** `TFT_MESSAGE_LEN` is 256 including the NUL and the longest fixed C-path
prefix is 56 bytes, so 120 leaves room for a reworded prefix; derived, not
measured, and a test must hold some bound. The variant name stays because it is
every runbook lookup and every C caller's only discriminator.

## Consequences

- `?` works from `Tree::attach_shared`, `MappedArena::attach`,
  `FrozenArena::open`, `write_frozen` and `ArenaLayout::new` into
  `Box<dyn Error>` and `anyhow::Error`.
- A new variant fails to compile in its `Display` and in the test's
  exhaustiveness guard (step 1(b)). Lands before `0058`'s implementation.

## Implementation plan

0. **Re-read #339's merged code.** Done (#343).
1. **The implementing PR**, four parts.
   - **(a) The four impls** per decisions 1–3 and 5.
   - **(b) A rendering test beside each type**, in the style of `tf_tree_core`'s
     `every_error_variant_renders_as_prose_naming_what_it_carries`: every variant
     (integers at type maximum, `Errno` at 4095) behind an exhaustive `match`;
     each rendering is non-empty, differs from `{:?}`, has no `{`/`}`, is ASCII,
     at most 120 bytes, **ends with** `(Name)`, and contains every carried
     integer. `FrozenError` also wraps every `ShmError` value in `::Arena`;
     re-freeze mentions and the 2(f) `full` check are asserted; `ShmError` is
     `?`-ed into `Box<dyn core::error::Error>`.
   - **(c) The facade.** The five attributes become `{0}`;
     `crates/tf_tree/tests/error_payloads.rs` gains `prints_its_payload` cases
     (`shm` ones under `cfg(all(feature = "shm", target_os = "linux"))`); `just
     shm-check` runs `cargo nextest run -p tf_tree --features shm --test
     error_payloads`.
   - **(d) The follow-ons in decision 8**, plus a `tests/python/test_frozen.py`
     case flipping one `layout_hash` bit of the arena header in a frozen `.tft`
     and asserting the message has no `{` and no `raw`.
2. **Status to `implemented`** when step 1 has landed.
