# 0017: Owned handles, and the rule that no stored type carries a lifetime

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** steps 1–7 landed; **step 8 alone is outstanding.** Steps 1–5 landed first — `OwnedWriter`,
`Tree::claim_owned`, the crate attribute move, and the drop / lease / fork /
`compile_fail` tests.
Steps 2 and 3's tests are `shm`-gated (a claim lease is an OFD byte), so
`just shm-check` runs `crates/tf_tree/tests/owned_writer.rs`, not `nextest
--workspace`. Steps 6–7 deleted both `extend_to_static` helpers: `OwnedWriter` is
the only lifetime extension in the workspace. `tf_tree_c::TreeShare` holds an
`Arc<Tree>` because `claim_owned` takes `self: &Arc<Tree>`. Step 8 is
documentation only and this record stays **ready** until it lands.

## Context

`Tree::claim` returns `EdgeWriter<'_>`, which borrows the tree — right for a
scoped claim, wrong for a claim that outlives its scope (a node, a driver, a
binding that owns a publisher for the life of a process). `tf_tree_c` built the
owned shape as `Arc<TreeShare>`; `tf_tree_py` first built it as a `transmute`
that reinterpreted `EdgeWriter` as `Publisher`, which leaked the **claim lease**
(no reaper would ever collect the edge) and bypassed the **fork guard**. A
downstream embedder has only a self-referential struct, `ouroboros`, or the same
transmute; `PHASE7.md` §4 J11 is a further consumer. [`API.md`](../API.md) §2.1
states the embedding rule this is the sole violation of.

## Decision

**State the rule, in `API.md` §2.1 and in the crate docs:**

> No type a user stores in their own struct may carry a lifetime.

**Add the owned handle to the `tf_tree` facade:**

```rust
impl Tree {
    /// Claim `child`'s edge, keeping the tree alive for as long as the writer
    /// lives.
    ///
    /// The scoped [`Tree::claim`] is preferable where the claim's scope is
    /// lexical — the borrow checker then enforces the claim's lifetime for
    /// free. Use this where the writer is stored: a node that publishes for
    /// the life of the process, or a binding whose handle type cannot carry a
    /// lifetime.
    pub fn claim_owned(
        self: &Arc<Self>,
        child: FrameId,
        parent: FrameId,
    ) -> Result<OwnedWriter, ClaimApiError>;
}

/// An [`EdgeWriter`] that owns its tree.
///
/// `Send + !Sync`, exactly as `EdgeWriter` is — single-writer-per-edge stays a
/// type-level property (D7), not a convention this type relaxes.
pub struct OwnedWriter { /* Arc<Tree>, Box<EdgeWriter<'static>> — both private */ }

impl OwnedWriter {
    // An `i64` stamp, matching `EdgeWriter::push` and `Publisher::push`.
    pub fn push(&self, stamp: i64, iso: &Iso3) -> Result<(), PushError>;
    // Forwarded by hand, not via `Deref`, which would also expose
    // `Publisher::push` — the copy without the fork check.
    pub fn edge(&self) -> EdgeId;
    pub fn release(self);
}
```

**`OwnedWriter` is the only place in the workspace where a lifetime is
extended.** The `unsafe` block lives there, with the `Arc` field named in its
`// SAFETY:` comment. Both bindings claim through `Tree::claim_owned`.

**`EdgeWriter<'a>` and `Tree::claim` are unchanged and not deprecated.**

**`Tree` does not become `Clone`.** `Arc<Tree>` is the embedding idiom
(`API.md` §2.2). A derived `Clone` would either register a second participant
slot or share one and lie about it.

## Rationale

The facade holds the `unsafe` once, next to the `ClaimLease` and fork-guard code it
depends on, under Rust tests, Miri and TSan. `self: &Arc<Self>` avoids a clone at
the call site. `ouroboros` would move the `unsafe` into a dependency that knows
nothing of claim leases or fork generations (D14). A `'static` `EdgeWriter` would
lose compile-time claim-scope enforcement.

## Consequences

- `tf_tree`'s `forbid(unsafe_code)` became `deny` with one documented exception;
  [`0007`](./0007-the-unsafe-budget-and-the-c-abi.md)'s criterion makes that
  decidable. `CLAUDE.md` and `PROJECT.md` change with it.
- The docs must say the scoped claim is the default and the owned one is for storage.
- `OwnedWriter` must reproduce every guard `EdgeWriter::drop` has: `Publisher::abandon`
  on a forked child, the `ClaimLease` release, the fork-generation compare.
- **The `EdgeWriter` must stay behind a `Box`.** Miri reports *"deallocating while
  item … is strongly protected"* on the inline version: a by-value pass strongly
  protects the reference fields, and `drop(writer)` / `release(self)` free the arena
  they point into. `tf_tree_py` and `tf_tree_c` must not "simplify" the box away.

## Implementation plan

1. `OwnedWriter` and `Tree::claim_owned` in `crates/tf_tree/src/tree.rs`; crate
   attribute `#![deny(unsafe_code, unsafe_op_in_unsafe_fn)]` with a module
   `// SAFETY:` block; a doc test storing an `OwnedWriter` in a lifetime-free struct.
2. Drop test (drop the caller's `Arc<Tree>`, push, re-claim from a fresh tree).
   **Mutant:** drop the `Arc` field ⇒ use-after-free under `just miri`.
3. Lease test (edge's OFD byte free after drop). **Mutant:** skip the `ClaimLease`.
4. Fork test (child push gets `ChildDetached`, parent's claim survives). **Mutant:**
   omit the fork-generation compare.
5. `compile_fail` doc tests: `OwnedWriter` is `Send`, not `Sync`.
6. **Done.** `tf_tree_py`: `PyPublisher` holds `Mutex<Option<OwnedWriter>>`.
7. **Done.** `tf_tree_c`: `tft_publisher_*` and `bridge.rs`'s writer map go through
   `OwnedWriter`; `a_publisher_outlives_the_tree_handle_it_came_from`
   (`crates/tf_tree_c/tests/publish.rs`). `just ros-test` covers the bridge half.
8. Crate-level docs on `tf_tree`: the lifetime rule and the scoped-vs-owned guidance
   — `cargo doc`, `#![deny(missing_docs)]`. The `Arc<Tree>` paragraph already landed.

## Open questions

None.
