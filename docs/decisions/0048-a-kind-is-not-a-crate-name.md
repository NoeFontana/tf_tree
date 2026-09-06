# 0048: a kind is not a crate name

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** (filled in as work lands)

## Context

[`0007`](./0007-the-unsafe-budget-and-the-c-abi.md) (`ready`) rule 1 is NORMATIVE:

> **`unsafe` is permitted only at a boundary the compiler cannot see across.**
> Today there are exactly four kinds: the arena's raw memory (`tf_tree_arena`,
> `tf_tree_core::{buffer, arena_view}`), the OS (`tf_tree_ipc`), a foreign
> runtime that owns its own objects (`tf_tree_py`), and a foreign *caller*
> (`tf_tree_c`, new). Anything that is not one of those is not eligible, and a
> **fifth kind needs a decision record**.

That sentence is the reason this record exists, and the answer it was opened to
give — *is there a fifth boundary?* — turned out to be the wrong question.

**Rule 1 had never been amended before this record.** `0007` carried one `>`
amendment block and it sat under rule 2, signed by
[`0017`](./0017-owned-handles-and-the-lifetime-rule.md).

**And rule 1 had no gate of any kind.** Not a script, not a `just` recipe, not a
CI step, not a lint: before `scripts/unsafe-budget.sh`, `grep -rn -i unsafe
scripts/` returned nothing, every `unsafe` in `justfile` and in
`.github/workflows/` was inside a `#` comment, and the root
`[workspace.lints.rust]` names `missing_docs`, `unreachable_pub`,
`unused_must_use` and `unexpected_cfgs` — **not `unsafe_code`**, which is
allow-by-default. Enforcement was review, and review had not caught it.

### The census, and how it was taken

**By the compiler, not by a grep**, and the difference is large enough that the
instrument is the finding. **This table carried four answers, dated 2026-09-05,
each beside the command that produced it, and three of them measurably drifted
within a day of being written** — which is why the answers are gone and the
commands are not. Run them:

| instrument | what it answers |
|---|---|
| `grep -rn --include='*.rs' 'unsafe' crates \| wc -l` | lines mentioning the word at all |
| `grep -rn --include='*.rs' -e unsafe_code -e unsafe_op_in_unsafe_fn crates \| wc -l` | how many of those name the rule's own **enforcement mechanism**, which the line above counts as violations of it |
| `grep -rn --include='*.rs' -E '\bunsafe\b' crates \| wc -l` | the better grep, and still not the census |
| `just unsafe-budget` | the census: `unsafe` **sites**, from `--force-warn unsafe_code` |

The word-boundary form is the better grep and still overshoots the compiler by a
wide margin, because this repository's prose *about* `unsafe` is unusually dense
and every `// SAFETY:` paragraph that uses the word counts. It also silently
drops what the plain form over-counts: `\bunsafe\b` does **not** match
`unsafe_code`, because the character after `unsafe` there is `_` and there is no
word boundary. So the two greps are wrong in opposite directions, and neither
number is the census. The four rows are also not in one unit — three count
*lines* and the last counts *sites* — which is a second reason a reader should
run them rather than subtract two figures printed side by side.

`RUSTFLAGS="--force-warn unsafe_code"` is the instrument. It **overrides
`#![forbid(unsafe_code)]`** rather than being suppressed by it — measured, by
seeding an `unsafe` block into `crates/tf_tree_cli/src/lib.rs` (which forbids)
and watching the census report it.

**Over `--all-targets`, and over a matrix read out of the justfile.** `unsafe`
behind a feature no command builds is invisible to any census, and this
repository has several: `footprint` builds by default, `abi_attached` needs
`abi-probe`, `fork_child` needs `shm` and its fourth mode `bridge`. The matrix is
therefore the justfile's own `cargo clippy … --all-targets` lines, so a new
feature-named pass enters the census the day it is added
rather than when somebody remembers — **provided it is written on one physical
line**, because the extractor reads one line at a time. That proviso was
unstated and unheld until 2026-09-06, and the file being read already contained
the shape (`py-compile`'s `cargo clippy \` / `--all-targets`, excluded for
another reason). `scripts/unsafe-budget.sh`'s `continuation_blind_spots` now
reports such a pass by name and refuses; the `MIN_SELECTORS` floor could not,
because a floor absorbs a drop rather than reporting one.

**No count from this section is repeated in `CLAUDE.md`.** Run
`just unsafe-budget`, which prints its own file and site totals; `0005`'s
Consequences bullet, which said `fork_child.rs` carries *"one
`unsafe { libc::fork() }`"* and is a `Status: implemented` record, is the
standing demonstration of what a number in prose does next.

### What the census found

Not a fifth *boundary*. **Three of `0007`'s four kinds occurring in crates its
parentheticals do not name**, one genuinely new kind, and one class the rule
never contemplated:

| where | kind | what it is |
|---|---|---|
| `tf_tree_bench/src/bin/footprint.rs` | 2, the OS | `mallinfo2`, through a locally declared `extern "C"` block because libc does not expose it |
| `tf_tree_bench/src/bin/fork_child.rs` | 2, the OS | `libc::fork` ×2, `_exit` ×2, `waitpid` |
| `tf_tree_tf2_sys/src/lib.rs` | 3, a foreign runtime **or library** | a textbook `-sys` shim into `tf2::BufferCore`, which owns its frame table exactly as CPython owns its `PyObject`s |
| `tf_tree_c/tests/*`, `tf_tree_c/examples/*`, `tf_tree_bench/src/bin/abi_attached.rs`, and `fork_child.rs`'s `bridge` mode | **new** | Rust calling **our own** `unsafe extern "C"` entry points, to exercise or measure them |
| `tf_tree_bench/tests/{zero_alloc,relocation}.rs`, `tf_tree_bridge/tests/steady_state_alloc.rs` | **new** | `unsafe impl GlobalAlloc` / `unsafe impl Arena`, in a test target that never ships |

**The finding is not that a fifth boundary appeared. It is that rule 1 wrote a
crate name in brackets beside each kind, and every downstream reader copied the
bracket.** `CLAUDE.md`, `docs/PROJECT.md` §6, `docs/PHASE1.md` §0,
`crates/tf_tree_tf2_sys/src/lib.rs`'s own header and
`crates/tf_tree_bench/src/bin/fork_child.rs`'s all restated the parenthetical
rather than the criterion. `0007`'s *Rationale* had already diagnosed exactly
this failure one level up:

> Why not keep the enumeration and add one name? Because it would be the third
> amendment nobody made… A rule that states the criterion survives; a list does
> not.

It then wrote a list in brackets beside the criterion.

### The scope question the sentence never answered

**Rule 1 is written per crate; rustc enforces `unsafe_code` per crate root.**
`#![forbid(unsafe_code)]` on `crates/tf_tree_bench/src/lib.rs` governs that root
and **no bin, test, bench or example of the same package** — which is why
`footprint.rs`'s `mallinfo2` compiled under a plain `just build` for the whole
life of the project, and why several of that package's bins carry `unsafe` today
under a crate whose library forbids it — `scripts/unsafe-budget.txt` lists them.

That misreading was load-bearing in prose, in several places at once: two bench
binaries justify a process-per-reader architecture with it, `shm_torture.rs`'s
header and `docs/PHASE2.md` §0.0's §11.4 row defer an invariant on it, and an
example declines `sched_setaffinity` on it. Every one of those design choices is
**right**; none of them rests on the reason it gave. The one place in the tree
that had the scope correct is `0005`'s Consequences bullet — *"so this is a
separate bin target"* — whose count is the part that went stale.

## Decision

Five clauses, and rule 1's *criterion* is untouched by all of them.

### D1 — the kinds are properties; the crate names become a non-normative index

Rule 1 keeps its sentence and loses its parentheticals. The list of *where each
kind currently lives* moves to `scripts/unsafe-budget.txt`, explicitly a snapshot
that goes stale without invalidating the rule — because a check recomputes it.

### D2 — kind 3 widens to "a foreign runtime **or library** that owns its own objects"

`tf_tree_tf2_sys` joins the index under it. `tf2::BufferCore` owning its frame
table is the same relationship `tf_tree_py` was named for, and the two
`unsafe impl Send/Sync` in that crate restore what the C++ side documents and no
more. **It was never a fifth kind.**

### D3 — a fifth kind: our own C ABI, called from Rust to exercise or measure it

Eligible in **any non-`lib` target of a `publish = false` package** whose purpose
is exercising, forking or measuring `tf_tree_c`'s `extern "C"` surface.

**The scope of this clause is the thing to get right, and the obvious phrasing
gets it wrong.** "A `publish = false` package whose stated purpose is measuring
or forking `tf_tree_c`" silently excludes `tf_tree_c` itself — and `tf_tree_c`'s
own `tests/` and `examples/` are where most of this kind's population lives, not
`tf_tree_bench`. Enumerated by the census: `tf_tree_c/tests/{abi, bridge,
bridge_shared, live, publish, recovery}.rs`, `tf_tree_c/examples/{abi_cost,
bridge_cost}.rs`, `tf_tree_bench/src/bin/abi_attached.rs` and the C-ABI half of
`fork_child.rs`. A clause scoped to the harness crate would have
authorised the smaller half and left the larger half unauthorised while reading
as complete.

**Why it is not kind 4.** Kind 4 is *a foreign caller*. This is a **domestic**
caller paying the same cost, and it exists because `tf_tree_c` chose an
`unsafe extern "C"` surface (`0007` §3.2's own pattern): any Rust caller of those
entry points must write `unsafe`, whoever it is. `abi_attached`'s entire result
is that the two callers cost the same — *"the cost is the boundary itself and
NOT the language"* — so a rule that treats them as different kinds would be
denying the finding the binary exists to produce.

### D4 — the budget's subject is the CRATE ROOT, not the package

Every crate root that contains `unsafe` declares its posture explicitly rather
than inheriting rustc's default `allow`, and carries a module-level `// SAFETY:`
block naming the kind it takes. `#![forbid(unsafe_code)]` on a `src/lib.rs` is a
statement about one root.

### D5 — a sixth kind: a trait the language requires be implemented unsafely, in a target that never ships

`unsafe impl GlobalAlloc` for an allocation counter and `unsafe impl Arena` for a
relocation harness are not boundaries the compiler cannot see across; they are
traits whose contract the language declines to check. They are admitted as their
own kind rather than folded into D3, because folding them in would make D3's
sentence say something it does not mean, and **silence about them is what
produced this record**.

### D6 — the register, and a check that recomputes it

`scripts/unsafe-budget.txt` is the index. `scripts/unsafe-budget.sh` takes the
census and compares the **file set** in both directions; `just lint` depends on
it, so it runs on every pull request and every tag.

## Rationale

### Why not "a measurement harness is a fifth kind"

That is the reading the work item was opened with, and it authorises a **place**
rather than a **property** — the shape `0007` spent its whole *Rationale*
arguing against, one level down. It would also immediately license
`sched_setaffinity` in `contended_scaling.rs`, which is the exact call two
binaries currently architect around: both spawn a process per reader and place it
with `taskset` rather than pinning a thread. If a record deletes those two files'
reason for existing in their current shape, it has gone wrong.

### Why `publish = false` is not the criterion, and what it *is* good for

**The proof is inside `0007` itself.** `tf_tree_c` is `publish = false` and is
the crate `0007` was written *for*; `tf_tree_py` is `publish = false` and is kind
3. That is two of the four named boundaries, and **both ship anyway**:
`tf_tree_py` is the PyPI wheel `transform_tree`, and `tf_tree_c` is the C library
product, off crates.io for the artifact shape rather than for maturity — its own
manifest says so. If unpublished-ness earned an exemption, half the existing list
would not have needed a record.

What it *does* change is the blast radius, and that is worth writing down rather
than assuming. Unsound `unsafe` in a crate that is not on crates.io cannot reach
a dependant's vendored `.crate` tarball. But the radius is not small here, for
two reasons.
`fork_child` is the harness that *proves* the fork-poisoning guards (`0005` step
9), and it forks a process sharing the parent's OFD locks and its
`MADV_DONTFORK` hole — unsound `unsafe` there makes a green fork test
meaningless about a **published** crate's behaviour. And `tf_tree_tf2_sys` is
workspace-excluded, so it is a crate carrying `unsafe` that
`cargo clippy --workspace --all-targets` never compiles: being outside the sweep
is an argument for *more* discipline, not less.

So `publish = false` is a **scoping device** in D3 and D5 — it makes the eligible
set machine-readable — and never a licence.

### Why the `mem::zeroed()` conveniences are deleted rather than authorised

They are not a boundary at all. `..unsafe { core::mem::zeroed() }` filling a
`#[repr(C)]` out-parameter is a convenience with a safe replacement, and in this
tree the replacement **already existed in two places, privately**:
`tft_error::blank()` and `blank_outcome()`.

`blank_outcome()` is the one that matters, and it is why *reuse* rather than
*write a twin* is a decision and not a detail. It fills
`tft_bridge_outcome`'s five `*const c_char` fields with `EMPTY.as_ptr()` — the
static empty string — and its own doc says so. A public `blank()` that used
`ptr::null()` instead would have put **two contradictory blanks in one crate**
and handed a C consumer a null where the crate's convention is a valid empty
string. `0048` decides that question explicitly: **the strings are `EMPTY`**, and
the existing constructor is the one exposed.

**One consequence is a rule about tests, and it cost an assertion.**
`tft_extrapolated::blank()` is field-identical to what
`tft_plan_at_extrapolating` writes on the in-window path — `struct_size`,
`by_ns: 0`, `edge: TFT_INVALID_ID` — so a test that seeds `blank()` into the
out-parameter cannot tell *the callee wrote the sentinel* from *the callee wrote
nothing*, where the old `unsafe { core::mem::zeroed() }` seed forced the write by
being wrong. `crates/tf_tree_c/tests/live.rs` seeds `by_ns: i64::MIN` instead and
says why at the constant; measured by deleting the callee's
`core::ptr::write(info, e)` on the `by_ns == 0` arm, which the `blank()` seed
passes and the seeded one fails. **The safe replacement of an `unsafe`
convenience can be the expected answer, and where it is, an out-parameter test
has to seed something else.**

`#[derive(Default)]` is not the answer for any of the four types: `tft_error`'s
`message` is `[c_char; 256]` and arrays longer than 32 have no `Default`, and
`tft_bridge_outcome` holds five raw pointers, which have none either. The two
that *could* derive one are spelled by hand anyway, so all four blanks read the
same way at a call site.

### Why the check pins a FILE set and says so

`--force-warn unsafe_code` emits `file:line: warning: usage of an unsafe block`.
**It carries no kind.** So the `kind` column in the register is bookkeeping a
human maintains, and the criterion in D1 stays a review rule. A check that
claimed to enforce the kinds would be claiming something its instrument cannot
see.

What it does hold is worth having on its own: a **new file under the covered
roots** cannot start carrying `unsafe` without somebody writing down which kind
it is, and a register row cannot outlive the file it names.

**The covered roots are `crates/` and `xtask/`, and the qualifier is not
decorative.** The census filters diagnostic lines by path prefix, and the first
version of that filter named `crates/` alone — so `xtask`, a workspace member the
`--workspace` selector compiles, with no `#![forbid(unsafe_code)]` and no
`unsafe_code` in `[workspace.lints.rust]`, was outside the check entirely. It was
found by seeding an `unsafe` block into `xtask/src/bin/` and watching the gate
exit 0. The filter names both roots now; the script's *What it does NOT prove*
section carries the residual, which is that a path prefix is a hand-maintained
list and a third root would repeat the defect.

### Why the empty-subject question decided the check's shape

A violation inside the covered path set *adds* a row to this census. So one way
one hides is by the census collapsing — a wrong feature set, a renamed recipe, a
filter typo, a `cargo check` that failed. `census − register` is then empty and a
naive comparison is **green**. Three things follow, and all three are in the
script: floors on the selector count and the site count that run **before** any
comparison; a comparison in **both** directions, so a deleted register row is
red too; and a **failing `cargo check` fails the script**, because a selector
that does not compile contributes zero rows and would otherwise be reported as
`STALE` register rows — a tidy-up job, in the place where a build error is.

`bash scripts/unsafe-budget.sh --self-test` drives the comparison over synthetic
inputs, **including an empty census**, and asserts each verdict.

## Consequences

- **`0007` rule 1 gains its first amendment.** The kinds survive; the
  parentheticals are retired into `scripts/unsafe-budget.txt`.
- **Every prose site that restated the parenthetical or the wrong scope is
  corrected in place, keeping what it used to say**: `CLAUDE.md`'s
  hard-rules bullet and its project-shape table, `docs/PROJECT.md` §6,
  `docs/PHASE1.md` §0's amendment note, `crates/tf_tree_tf2_sys/src/lib.rs`,
  `crates/tf_tree_bench/src/bin/fork_child.rs`, `shm_torture.rs`,
  `contended_scaling.rs`, `load_child.rs`,
  `crates/tf_tree_bench/examples/contended_search.rs`,
  `crates/tf_tree_bridge/Cargo.toml`, `justfile`'s `tf2-check` comment, and
  `docs/PHASE2.md` §0.0's §11.4 row.
- **Two superlatives are deleted rather than corrected.**
  `crates/tf_tree_bridge/Cargo.toml` and `justfile` both called
  `tf_tree_tf2_sys` *"the one crate carrying `unsafe` with no lint coverage"* —
  a count written beside the thing that produces it, and false in any case
  (`tf_tree_py` is also workspace-excluded and also carries `unsafe`; `just
  py-compile` is what covers it). The instrument replaces the claim.
- **`0005` is amended for its COUNT only.** Its Status is `implemented`; its
  scope clause is correct and is the counter-example the rest of the tree needed.
- **`just lint` gains a dependency that compiles.** Measured 9 s warm and 55 s
  cold, with about 1 GiB of extra `target/` because `RUSTFLAGS` is part of
  cargo's fingerprint. It is last in the chain.
- **Two crates outside the cargo workspace are outside the check's reach**, and
  their register rows say so: `tf_tree_py` (covered by `just py-compile`) and
  `tf_tree_tf2_sys` (covered by the container-only `just tf2-check`). Their
  counts in this record were taken by running the same census against their own
  manifests — **`tf_tree_tf2_sys`'s inside the container, so its sites are
  compiler-verified rather than read**, which is the residual this record was
  expected to have to leave open and did not.
- **A `blank()` on three `#[repr(C)]` types is now public API of a
  `publish = false` crate.** Adding a field to any of them is a compile error in
  `blank()` rather than a silently-zeroed field, which is the property the
  `unsafe` blocks were trading away.

## Implementation plan

Landed with this record:

1. **`scripts/unsafe-budget.sh` and `scripts/unsafe-budget.txt`**, and
   `just unsafe-budget` in `lint`'s dependency chain. — verified by its own
   `--self-test`, and red-tested four ways against the real tree: an `unsafe`
   block seeded into an unregistered crate root (reported `UNAUTHORISED`, exit
   1); the same seed with `#![forbid(unsafe_code)]` left in place (still
   reported, which is what proves `--force-warn` overrides `forbid`); a register
   row for a file with no `unsafe` (`STALE`, exit 1); and an authorised row
   deleted (`UNAUTHORISED`, exit 1). Restored and green after each.
2. **The `core::mem::zeroed()` sites are gone**, replaced by
   `tft_error::blank()`, `tft_extrapolated::blank()`,
   `tft_bridge_outcome::blank()` and `tft_bridge_stats::blank()`. Two
   hand-rolled twins in the tests — `zeroed_stats()` and `blank_error()` — and
   one in `fork_child.rs` are deleted with them. — verified by the census
   dropping by exactly the number of sites removed, by
   `cargo nextest run -p tf_tree_c --features bridge,shm,test-hooks` (all green),
   and by `cargo xtask headers --check` (one doc-comment line moved with the
   renamed helper and is regenerated).
3. **D4's postures on the three `tf_tree_bench` bins that carry `unsafe`**, each
   with `#![allow(unsafe_code)]`, `#![deny(unsafe_op_in_unsafe_fn)]` and a
   module `// SAFETY:` block naming its kind — and on `tf_tree_tf2_sys`, in its
   **manifest `[lints.rust]`** rather than in `src/lib.rs`, because that crate is
   workspace-excluded, cannot inherit `[workspace.lints]`, and already spells its
   lints there. — verified in the container: `unsafe_op_in_unsafe_fn = "deny"`
   was red-tested by adding an `unsafe fn` with a bare deref and watching
   `E0133`, then restored.

**Not landed, and named rather than implied:**

4. **D4 is not applied to `tf_tree_c`'s own `tests/` and `examples/` roots**, nor
   to `tf_tree_bench`/`tf_tree_bridge`'s test roots. That is the larger half of
   D3's and D5's population, it is a sweep of its own, and the check covers those
   files by *file set* today. Until it is done, D4 holds for the roots step 3
   names and for nothing else.
5. **`docs/decisions/README.md`**'s index row. The `CHANGELOG.md` entry *is*
   landed; the index row is added centrally rather than on this branch, because
   four branches editing one index is a conflict with no useful three-way merge.

## Open questions

1. **Should the register's `kind` column be mechanised?** It cannot be derived
   from `--force-warn unsafe_code`, which emits no kind. A per-site `// SAFETY:`
   tag would make it derivable and would also be a second spelling of the kind
   beside the prose that already names it. Left open deliberately: the check's
   header states what it does not prove, which is the honest position until
   somebody wants the stronger one.
2. **Does D3 want an upper bound?** As written it authorises a kind wherever a
   `publish = false` non-`lib` target exercises the C ABI, and the population is
   already the largest of any kind in the tree. Nothing here proposes a ceiling,
   and a ceiling on a number nobody has a reason for would be a budget rather
   than a rule.
