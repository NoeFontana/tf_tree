# 0017: Owned handles, and the rule that no stored type carries a lifetime

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** steps 1–5 landed — `OwnedWriter`, `Tree::claim_owned`, the
crate attribute move, and the drop / lease / fork / `compile_fail` tests. Steps
6–8 (the `tf_tree_py` and `tf_tree_c` migrations, and the crate-level docs) are
outstanding, and until they land the two `extend_to_static` helpers this record
exists to delete are still in the tree.

## Context

`Tree::claim` returns `EdgeWriter<'_>`, which borrows the tree. That is the
right type for a scoped claim and the borrow checker enforcing claim scope is
worth keeping. It is the wrong type — and the only wrong type in the public
surface — for the case where a claim outlives the scope that created it: a
perception node, a driver, or a binding that owns a publisher for the life of a
process.

**Three consumers have needed the owned shape, and two built it by hand.**

1. `tf_tree_c` built it correctly, as `Arc<TreeShare>` held beside the plan and
   the tree handle, with the refcount as the lifetime argument. Its
   `publisher::extend_to_static` is used from two places — `tft_tree_claim` and
   the bridge's per-edge writer map.
2. `tf_tree_py` built it twice. The second version is correct — a `Py<PyTree>`
   strong reference alongside an `EdgeWriter<'static>`, behind an
   `extend_to_static` helper whose signature pins both types so only the
   lifetime can differ. The first version was
   `transmute::<EdgeWriter, Publisher>`, which compiled for as long as the two
   types happened to be the same size and **was not a lifetime extension at
   all** — it reinterpreted one type as another. Because `publisher` is
   `EdgeWriter`'s first field the bytes lined up and the remaining fields were
   dropped on the floor:
   - the **claim lease** was never released. `ClaimLease::drop` is what unlocks
     the edge's OFD byte, so every Python publisher leaked one for the life of
     the process — and a leaked lease is indistinguishable from a live writer,
     so no reaper would ever collect the edge either.
   - the **fork guard** was bypassed.

   Both facts are recorded in that crate's own source comments, which is the
   only place they have ever been written down.
3. Any downstream Rust embedder is the third, and has neither a PyO3 refcount
   nor a C handle to hang the lifetime on. Their options today are a
   self-referential struct, `ouroboros`, or the same transmute — written by
   someone with less context than the person who wrote it here and got it
   wrong. [`PHASE7.md`](../PHASE7.md) §4 J11 is a fourth consumer already
   visible on the horizon: `setTransform`'s claim is stored on a `Buffer`, not
   scoped.

**What forces the decision now.** The crate is private, so the API is free to
change; and [`API.md`](../API.md) §2.1 states the embedding rule that this is
the sole violation of. Fixing it after a published tag means either a breaking
change or shipping the workaround as the documented answer.

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
pub struct OwnedWriter { /* Arc<Tree>, EdgeWriter<'static> — both private */ }

impl OwnedWriter {
    pub fn push<D: Domain>(&self, stamp: Stamp<D>, iso: &Iso3) -> Result<(), PushError>;
    pub fn release(self);
}
```

**`OwnedWriter` is the only place in the workspace where a lifetime is
extended.** The `unsafe` block lives there, with the `Arc` field named in its
`// SAFETY:` comment as the thing that makes the extension sound. `tf_tree_py`
deletes its `extend_to_static`; `tf_tree_c` deletes
`publisher::extend_to_static` and keeps its `Arc<TreeShare>` for the tree and
plan handles, routing both the publisher and the bridge's writer map through
`OwnedWriter`.

**`EdgeWriter<'a>` and `Tree::claim` are unchanged and are not deprecated.**

**`Tree` does not become `Clone`.** `Arc<Tree>` is the embedding idiom and gets
one paragraph of crate-level documentation saying so (`API.md` §2.2). A derived
`Clone` would either register a second participant slot — out of the 64 an arena
gets by default — or share one and lie about it; and `Arc` is already what
`crates/tf_tree/tests/tsan.rs`, `tf_tree_c`'s `TreeShare` and
`Py<PyTree>` each arrived at independently.

## Rationale

**Why the facade and not each binding.** Three consumers, two hand-rolled
implementations, one shipped defect with two distinct failure modes. The
workaround being reinvented is the definition of a missing API. Putting it in
`tf_tree` means the `unsafe` is written once, sits next to the `ClaimLease` and
fork-guard code whose invariants it depends on, and is covered by the Rust test
suite, Miri and TSan — none of which can see a `transmute` inside a PyO3 crate's
`#[pyclass]` glue.

**Why `self: &Arc<Self>` and not `Arc<Tree>` by value.** By value forces the
caller to clone at the call site even when they hold the `Arc` already, and it
reads as though the tree is consumed. `&Arc<Self>` makes the refcount bump an
implementation detail and makes the requirement — that the tree is already
shared — visible in the signature.

**Why not `ouroboros` or a self-referential crate.** It moves the same `unsafe`
into a dependency, and the dependency does not know about claim leases or fork
generations. The soundness argument here is not "the borrow is valid"; it is
"the arena the claim points into outlives this writer", which is a statement
about `Arc<Tree>` specifically. It would also be a new dependency on the facade,
which D14's budget reasoning applies to even though the facade is not the core.

**Why not make `EdgeWriter` `'static` outright.** Then the scoped case loses its
compile-time claim-scope enforcement, which is the better default and the one
most publishers should use.

**Why not leave it.** The status quo is a documented-nowhere hazard whose one
observed instance leaked a kernel lock and disabled reaping. The next person to
meet it will have less context than the person who already got it wrong.

## Consequences

- One `unsafe` block is added to `tf_tree` (which is `#![forbid(unsafe_code)]`
  today). **This is a real cost and it is the point of the trade**: the crate's
  forbid attribute becomes a `deny` with one documented exception, in exchange
  for deleting an undocumented `transmute` from a crate the Rust test suite
  cannot instrument. [`0007`](./0007-the-unsafe-budget-and-the-c-abi.md)
  restated the unsafe budget as a criterion rather than a crate list precisely
  so this kind of trade is decidable; this is the first exercise of it.
  **`CLAUDE.md` and `PROJECT.md` both name `tf_tree` in the forbid list, so both
  change in the same commit** — a rule that says `forbid` while the code says
  `deny` is worse than either.
- If a reviewer prefers, the block may live in `tf_tree_core` beside `Publisher`
  instead — the argument is unchanged and that crate is already `deny` rather
  than `forbid`. Taking that option keeps `tf_tree`'s `forbid` intact and is the
  cheaper documentation change; it costs the property that the facade is where
  an embedder looks.
- `tf_tree_py` loses its only non-NumPy `unsafe`.
- Two ways to claim an edge now exist. The documentation must be explicit that
  the scoped one is the default and the owned one is for storage, or the owned
  one becomes the copy-pasted default.
- `OwnedWriter` must reproduce every guard `EdgeWriter::drop` has:
  `Publisher::abandon` on a forked child, the `ClaimLease` release, the fork
  generation compare. A missing guard here is the exact defect this record
  exists to remove.
- It commits us to `API.md` §2.1 as a rule that new API is checked against, not
  a description of the current state.

## Implementation plan

1. `OwnedWriter` and `Tree::claim_owned` in `crates/tf_tree/src/tree.rs`, with
   the single `unsafe` and its `// SAFETY:` block naming the `Arc` — verified by
   `cargo check -p tf_tree --all-targets` and by a doc test that stores an
   `OwnedWriter` in a struct with no lifetime parameter. The crate attribute
   moves from `#![forbid(unsafe_code)]` to
   `#![deny(unsafe_code, unsafe_op_in_unsafe_fn)]` with a module `// SAFETY:`
   block, and `CLAUDE.md` §*Hard rules* plus `PROJECT.md` are updated in the
   same commit.
2. A drop test: build a tree, `claim_owned`, drop the `Arc<Tree>` the caller
   held, push, drop the writer, then re-claim the same edge from a fresh tree
   over the same arena — verified by the re-claim succeeding. **Mutant:** drop
   the `Arc` field from `OwnedWriter` ⇒ use-after-free under `just miri`.
3. A lease test: `claim_owned`, drop the writer, assert the edge's OFD byte is
   free. **Mutant:** skip the `ClaimLease` field ⇒ the edge is permanently
   unclaimable, which is the shipped Python defect reproduced as a test.
4. A fork test: `claim_owned`, `fork`, push from the child — verified by the
   child getting `ChildDetached` and the parent's claim surviving. **Mutant:**
   omit the fork-generation compare ⇒ the child releases the parent's lease.
5. `compile_fail` doc tests: `OwnedWriter` is `Send`, is **not** `Sync`.
6. `tf_tree_py`: `PyPublisher` holds `Mutex<OwnedWriter>`; delete
   `extend_to_static` and its module — verified by `just py-test` and by
   `rg 'transmute' crates/tf_tree_py` returning nothing.
7. `tf_tree_c`: route `tft_publisher_*` **and `bridge.rs`'s writer map** through
   `OwnedWriter`; delete `publisher::extend_to_static` — verified by
   `just c-header-check`, the ASan build in `just cpp-check`, and `just ros-test`
   for the bridge half.
8. Crate-level docs: the lifetime rule and the scoped-vs-owned guidance —
   verified by `cargo doc` and by `#![deny(missing_docs)]`. **`Arc<Tree>` as the
   embedding idiom was in this step and has already landed** on `tf_tree`'s
   crate docs ahead of the rest (`docs/API.md` §6 row 2 records it); do not
   write it a second time.

## Open questions

None.
