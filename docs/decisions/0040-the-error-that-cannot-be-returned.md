# 0040: the error that cannot be returned

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** #278

## Decision

1. Every `tf_tree_core` error enum (`LookupError`, `PushError`, `ClaimError`, `FrameError`, `TopologyError`) implements `Display`, printing identifiers only: no allocation, no `String` field.
2. Every error enum implements `core::error::Error` (MSRV 1.87 >= 1.81), so `?` works into `anyhow::Error` and `Box<dyn Error>`.
3. `Described` stays the naming layer; its fallback arm becomes `other => write!(f, "{other}")`.
4. Message text is uncontracted (`docs/API.md` R5). No `thiserror` (D14).

## Consequences

- The five impls are a commitment; a doctest on `LookupError` demonstrates `anyhow::Result`.

## Implementation plan

Step 1: `Display` for all five enums in `crates/tf_tree_core/src/error.rs`, no catch-all; a test formats one value per variant.

Step 2: `core::error::Error` for all five; tests box each into one `Box<dyn Error>`.

Step 3: `Described`'s fallback delegates.
