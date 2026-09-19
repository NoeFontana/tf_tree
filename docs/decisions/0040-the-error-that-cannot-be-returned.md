# 0040: the error that cannot be returned

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** #278

## Decision

**1. Every `tf_tree_core` error enum (`LookupError`, `PushError`, `ClaimError`, `FrameError`, `TopologyError`) implements `Display`, and prints identifiers.** It writes into the caller's formatter: no allocation, no `String` field, no change to layout or `Copy`. It names what the error carries (`EdgeId`, `FrameId`, domain tag, stamp) and resolves nothing against an arena.

**2. Every error enum implements `core::error::Error`** (not `std`, so `tf_tree_core` stays `no_std`; in `core` since 1.81, MSRV is 1.87), so `?` works into `anyhow::Error` and `Box<dyn Error>`.

**3. `Described` stays the naming layer.** `Tree::describe(err)` resolves frame and edge names and keeps every arm that names something; its fallback arm becomes `other => write!(f, "{other}")` instead of `{other:?}`.

**4. Message text is uncontracted.** `docs/API.md` R5: error types are the compatibility promise, text is not; the rustdoc says so. Raw `Display` is ids, `Described` is names.

No `thiserror` (dependency budget, D14). This does not weaken R5: errors stay `Copy`, `String`-free and `no_std`, and name resolution stays in `Described`, not a field.

## Consequences

- Removing a trait impl is breaking, so the five impls are a commitment. `fn setup() -> anyhow::Result<Plan>` is demonstrated by a compiling doctest on `LookupError`; `Tree::await_frames`' example stays `text` because `Open`/`OpenError` are `shm`-gated and `just test` runs doctests on default features.
- A drop of the MSRV below 1.81 would break `tf_tree_core`.

## Implementation plan

Step 1: `Display` for all five enums in `crates/tf_tree_core/src/error.rs`, every variant covered with no catch-all, so a new variant is a compile error; a test formats one value per variant and asserts the carried identifier appears.

Step 2: `core::error::Error` for all five; tests box each and `?`-chain a `LookupError` and an `OpenError` into one `Box<dyn Error>`.

Step 3: `Described`'s fallback delegates; a `BufferTooSmall` through `Tree::describe` no longer renders as `Debug`.
