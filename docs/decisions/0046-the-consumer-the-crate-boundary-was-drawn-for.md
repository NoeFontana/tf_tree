# 0046: the consumer the crate boundary was drawn for

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** (filled in as work lands)

## Context

`docs/PHASE5.md` §0.0's §3 row states why `tf_tree_ingest` is a workspace member
rather than a module of the CLI, in as many words:

> it is not in `tf_tree_cli` **because §4's offline Python API needs the same
> logic and cannot depend on a binary crate**

**That consumer was never built.** The crate boundary has been drawn, and paid
for, for a caller that does not exist. `tf_tree_py` has no dependency on
`tf_tree_ingest` at all:

```
$ grep -c tf_tree_ingest crates/tf_tree_py/Cargo.toml
0
```

What Python has is `open_file` — the way *in* to an index somebody else built —
and `Tree.freeze` — the way *out* of a tree assembled by hand. Both halves of
the §3 path that turns a recording into either one are Rust-only. So the
audience §4 exists for, and the README opens with, can open a `.tft` and cannot
produce one; the two commands that produce one are `tf_tree ingest --bag` and
`tf_tree freeze --from-bag`, and until [`#298`](../PHASE5.md) both of those
needed a clone and a Rust toolchain.

This is the fourth instance of the pattern
[`0044`](./0044-recovery-the-languages-a-robot-is-written-in-cannot-reach.md)
names — after `0038` (the time domain), `0039` (extrapolation) and `0044` itself
(recovery) — **a capability that is implemented, tested, documented, and
callable by nobody who needs it**. It differs from the other three in one way
worth stating: those were capabilities that grew a binding gap after the fact,
whereas this one has a *status row asserting the binding as the reason the code
is shaped the way it is*.

## Decision

**One new function, and no second way to spell it.**

```python
tf_tree.ingest_bag(
    path, /, *,
    static_topics=None, tf_topics=None, tf_prefix=None,
    max_memory_mb=None, max_record_bytes=None,
) -> Tree
```

It returns **an ordinary `Tree`** — the same type `open_file` returns — so
`plan`, `at`, `span`, `frames`, `edges` and `freeze` all work on it unchanged.
That is §4.1's *"no parallel offline API"* holding structurally rather than by
promise, which is the same argument `open_file` already won.

**The tree carries where it came from.** A new read-only property:

```python
Tree.source -> dict | None
```

`None` for every tree not produced by `ingest_bag`. Otherwise a `dict` with
`path`, `digest` (BLAKE3 of the recording's bytes, hex), `transforms`,
`edges_without_samples`, `recording_start_ns` and `recording_end_ns`.

**`Tree.freeze` writes that digest into the container's `source_digest`.** It
currently passes all-zero unconditionally. Zero is correct for a tree assembled
in Python — there is no recording to name — and wrong for one that came from a
bag.

**`Tree.source` is dropped the moment the tree can be written to**, i.e. inside
`Tree.publisher()`. See *Rationale*.

**The dependency is unconditional** — see *Rationale* for why the Cargo feature
this record was drafted with is not here.

### `recording_*`, not `span_*`

The two stamps are named for the *recording*, because they are not the interval
the tree can answer. A ring retains what fits, so the queryable window is at
most this and usually narrower. The first end-to-end run of this API took the
upper stamp from the report, queried it, and got `ExtrapolationError` from an
edge whose retained history had stopped 10 ms earlier. `Tree.span(...)` is the
queryable interval and is the one to plan against.

## Rationale

### Why not a `freeze_bag`, which is what the first prototype had

`tf_tree_ingest::tft::freeze_bag` exists and does more than `Tree::freeze_to`,
so binding it directly is the obvious move. It was the shape of the prototype
that produced this record's measurements, and its module doc claimed the
composition `ingest_bag(...).freeze(out)` "**loses the recording's identity**"
and that a direct binding was therefore "a capability the composition cannot
express, not a second way to spell one it can."

**Reading the function refutes that.** In full, it is:

```rust
let digest = digest_file(source)?;
let ingested = crate::run(source, opts, frames)?;
let header = ingested.tree.freeze_to(out, Some(&source…), digest, created)?;
```

It streams the **digest**, not the tree; the whole arena is in memory after
`run` either way. `Tree::freeze_to` already takes `source_digest` as a
parameter. So the composition can express it in Rust, and the gap is entirely in
`tf_tree_py`'s `Tree.freeze`, which hardcodes the zero.

Which makes a top-level `freeze_bag` exactly what `CLAUDE.md` forbids: **a
second spelling** of `ingest_bag(p).freeze(out)`, differing only in whether the
provenance field gets filled in — and differing *silently*, so the user who
writes the obvious composition gets an unattributable index and no diagnostic.
Widening `Tree.freeze` instead leaves one path, and makes it correct by default.

### Why the provenance is on the tree and not a `freeze` argument

`Tree.freeze(out, source=..., source_digest=...)` would also work and needs no
new state. It loses on defaults: provenance is the case that matters least at
the moment of writing and most six months later, so the spelling that requires
the caller to remember it is the spelling that produces unattributed indexes.
The digest describes where the *tree* came from, which is a property of the
tree, not of a particular write of it.

### Why it is dropped in `publisher()`

`Tree.publisher()` is `tf_tree_py`'s only mutation entry point. A caller may
ingest a bag, add a computed calibration edge, and freeze — and the resulting
`.tft` would then carry a digest asserting it is that recording, while
containing samples the recording does not.

**A wrong digest is worse than an absent one.** §2.3's entire purpose for the
field is answering *"was this index built from that file"* without re-ingesting;
a false *yes* defeats the investigation the field exists for, whereas a zero is
the documented "there was no recording" and sends the reader to look elsewhere.
Dropping it is one field write, and the result — zeros — is *true*.

### Why `max_record_bytes` is exposed and five other `IngestOptions` fields are not

`IngestOptions` has ten fields. `tf_prefix`, the two topic lists and
`max_memory_mb` are what a user with a real recording reaches for. The
remainder — `on_clock_reset`, `on_bad_chunk`, and the chunk-bomb ceilings — are
either a single supported value or a guard whose default exists to stop a
hostile file. Exposing all ten would make the signature the struct's shape
rather than the task's.

`max_record_bytes` is the exception among the guards, because
[`0010`](./0010-a-ceiling-on-one-record.md) added it *specifically* so that "the
person who meets it can raise it without forking the crate". Reachable only from
Rust, that argument does not hold for the audience §4 is for. It is exposed for
the reason it was built.

### Why the wheel does not split

Measured: the wheel goes from **548 129 to 726 396 bytes**, a +178 267-byte
(+32.5 %) delta — and only once a binding actually calls into the crate, since
the first measurement of this added the dependency without a caller, the linker
stripped it, and the delta read zero. Against that, `numpy` — which the wheel
already requires — installs **29.3 MiB**, some 42× the entire wheel. Splitting
the distribution to save 174 KiB would double a seven-row `wheels.yml` matrix
and give every user a second decision to get wrong.

### Why there is no `ingest` Cargo feature, though this record was drafted with one

The draft made the dependency optional behind a default-on `ingest` feature, on
the stated grounds that `just py-cross-check` should not compile it for three
non-Linux targets. **That was checked and is false.** The recipe passes
`--features pure-hash,pyo3/extension-module,pyo3/abi3-py39` *without*
`--no-default-features`, so it already compiles the ingest half for all three —
and that is **coverage worth having**, since it is the only thing in the
workspace proving `mcap`, `ruzstd` and `lz4_flex` cross-compile to macOS and
Windows. Turning the feature off there would have removed a check, not added one.

What was left was a feature that no gate ever exercised in its *off* position.
That is precisely the unchecked configuration `just shm-check` and
`just ingest-check` exist to prevent — a `#[cfg]` arm nobody compiles is not a
checked arm — and the honest options were to give it a gate row or to delete it.
Nothing ships with it off, and no consumer can even ask for it: `tf_tree_py` is
`publish = false` and excluded from the workspace, so the wheel is the only
artifact and the wheel always wants ingestion. It is deleted.

The `features = ["shm"]` that came with it is gone for the same kind of reason:
it was there for `tft::freeze_bag`, which this design does not call.

## Consequences

- **`tf_tree_py` gains a dependency on a `publish = false` crate.** This is
  already true of `tf_tree_cli` and is not a new class of edge. `tf_tree_py` is
  itself excluded from the workspace and unpublished to crates.io, so nothing
  about the publishable five changes.
- **`Tree.source` is state the Rust `Tree` does not carry.** Rust passes the
  digest to `freeze_to` per call; the binding stores it. The C ABI has no
  equivalent and gains none here. If a third binding wants it, that asymmetry is
  the thing to revisit — not by copying the field, but by asking whether
  `tf_tree::Tree` should carry provenance itself.
- **A new invariant to maintain:** every future mutation entry point on `PyTree`
  must drop `source`. Today `publisher()` is the only one. The test that pins
  this asserts on the *property*, not on the call, so a second entry point added
  without the drop fails it.
- **§3's `freeze_from_arrays` is unaffected and still owed.** It is the way in
  for a user whose poses were never in a bag; this is the way in for one whose
  poses are in a recording. Neither substitutes for the other.
- **`just py-cross-check` now cross-compiles `mcap` and the codecs** to
  `{x86_64,aarch64}-apple-darwin` and `x86_64-pc-windows-msvc`. That is new
  coverage rather than new cost — measured, the three targets still check in
  seconds — but it does mean a future ingest dependency that is Linux-only would
  fail there, which is the right place to find out.

## Implementation plan

1. **`tf_tree_py` takes the dependency**, unconditionally and with no `shm`.
   `digest_file` moves from `tf_tree_ingest::tft` — which is
   `#[cfg(all(feature = "shm", target_os = "linux"))]` — to the crate root, so
   the binding can report a digest on every platform the wheel builds for; its
   `blake3` dependency stops being optional, which adds nothing to any build
   because `tf_tree_core` already requires it (D14). — verified by
   `cargo check -p tf_tree_ingest --no-default-features` and by
   `just py-cross-check`.
2. **`ingest_bag`**, with the five keyword arguments, GIL released around the
   whole ingest, and errno-bearing `IngestError`s mapped to `OSError` as
   `offline::frozen_err` already does. — verified by a new
   `tests/python/test_ingest.py` against a fixture recording.
3. **`Tree.source`**, populated by `ingest_bag` and `None` everywhere else. —
   verified by a test asserting `open_file(...).source is None` and that an
   ingested tree's digest equals `hashlib`-independent BLAKE3 of the file, taken
   from the CLI's own output rather than recomputed in the test.
4. **`Tree.freeze` writes the digest**, and `Tree.publisher()` drops it. —
   verified by two tests: freeze-then-`doctor --explain-version` (or
   `open_frozen`) shows the digest; and ingest → `publisher()` → freeze shows
   zeros. **Both must fail at step 3's commit**, which is what makes them
   gates rather than descriptions.
5. **`_core.pyi` stub, `python/tf_tree/__init__.py` re-export, `docs/API.md` §6
   delta row, `PHASE5.md` §0.0 §3 and §4 rows, README, CHANGELOG.** — verified
   by `just py-lint` (which carries `tf_tree_py`'s rustdoc), `just py-test` and
   `just py-test-freethreaded` on both interpreters, and
   `just artifact-versions`.

## Open questions

None. The four this record was opened with are answered above: whether a
`freeze_bag` binding survives (**no** — reading the function showed it to be a
second spelling); where provenance lives (**on the tree**, dropped on mutation);
which `IngestOptions` fields are keywords (**five**, with `max_record_bytes`
included for `0010`'s own stated reason); and whether the wheel splits (**no** —
+174 KiB against numpy's 29.3 MiB).

**Two of this record's own arguments did not survive being implemented**, and
they are corrected above rather than quietly dropped: the claim that
`tft::freeze_bag` expresses something the composition cannot (refuted by reading
it), and the claim that the `ingest` feature earns its place by letting
`py-cross-check` drop the dependency (refuted by running the recipe). Both were
load-bearing when written. `docs/decisions/README.md` notes that a `ready`
record's own next step has falsified its claims before; this is another.
