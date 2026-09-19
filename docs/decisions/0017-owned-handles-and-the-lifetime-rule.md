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

## Decision

**The rule, in `API.md` §2.1 and the crate docs:** no type a user stores in their
own struct may carry a lifetime. **The owned handle, on the `tf_tree` facade:**

```rust
impl Tree {
    /// Claim `child`'s edge, keeping the tree alive as long as the writer lives.
    pub fn claim_owned(self: &Arc<Self>, child: FrameId, parent: FrameId)
        -> Result<OwnedWriter, ClaimApiError>;
}

/// An [`EdgeWriter`] that owns its tree. `Send + !Sync` (D7).
pub struct OwnedWriter { /* Arc<Tree>, Box<EdgeWriter<'static>> — both private */ }

impl OwnedWriter {
    // Forwarded by hand: `Deref` would expose `Publisher::push`, lacking the fork check.
    pub fn push(&self, stamp: i64, iso: &Iso3) -> Result<(), PushError>;
    pub fn edge(&self) -> EdgeId;
    pub fn release(self);
}
```

**`OwnedWriter` is the only lifetime extension in the workspace**; its `unsafe`
block names the `Arc` field in its `// SAFETY:` comment. Both bindings claim through
`Tree::claim_owned`.

**`EdgeWriter<'a>` and `Tree::claim` are unchanged** (prefer the scoped claim where
lexical). **`Tree` does not become `Clone`:** `Arc<Tree>` is the idiom (`API.md` §2.2).

## Consequences

- `tf_tree`'s `forbid(unsafe_code)` became `deny` with one exception
  ([`0007`](./0007-the-unsafe-budget-and-the-c-abi.md)).
- `OwnedWriter` reproduces every `EdgeWriter::drop` guard: `Publisher::abandon`,
  the `ClaimLease` release, the fork-generation compare.
- **The `EdgeWriter` must stay behind a `Box`**: Miri rejects the inline version
  (*"deallocating while item … is strongly protected"*).

## Implementation plan

1. `OwnedWriter`, `Tree::claim_owned`, `#![deny(unsafe_code, unsafe_op_in_unsafe_fn)]`
   with a module `// SAFETY:` block.
2. Drop test. **Mutant:** drop the `Arc` field ⇒ use-after-free under `just miri`.
3. Lease test. **Mutant:** skip the `ClaimLease`.
4. Fork test (child push gets `ChildDetached`). **Mutant:** omit the generation compare.
5. `compile_fail` doc tests: `OwnedWriter` is `Send`, not `Sync`.
6. **Done.** `tf_tree_py`: `PyPublisher` holds an `OwnedWriter`.
7. **Done.** `tf_tree_c` goes through `OwnedWriter`:
   `a_publisher_outlives_the_tree_handle_it_came_from` (`tf_tree_c/tests/publish.rs`).
8. Crate-level docs: the lifetime rule and scoped-vs-owned guidance.
