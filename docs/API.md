# tf_tree — the API contract

> **Companions:** [`PROJECT.md`](./PROJECT.md) (decision log D1–D22),
> [`PHASE1.md`](./PHASE1.md)–[`PHASE5.md`](./PHASE5.md) (per-phase specs),
> [`PHASE7.md`](./PHASE7.md) (the `tf2`-shaped shim, gated by D21).

**What this document is.** The rules that generate every binding, and the
normative surface of each one. The phase specs say *what gets built when*; this
says *what shape it has to have* and, more usefully, **what may not be added.**

**Why it exists.** Four surfaces now exist — Rust, Python, C, C++ — designed
across five documents at four different times. The rules below were obeyed in
most places and violated in a few, and nothing recorded them in one place, so a
fifth surface (the shim) had no contract to be checked against. Sections marked
**NORMATIVE** are requirements.

**Status.** Ready. §1–§5 describe surfaces that exist and the deltas §6 lists;
§7 is the check any new surface has to pass.

**This document is not a phase.** It constrains work the phase specs schedule;
it schedules nothing itself. Every row in §6 lands inside a phase or a decision
record, and the row says which.

---

## 1. The six rules

Every design question below is answered by one of these. If a question is not
answered by one of them, it is a decision record, not an API choice.

### R1 — Three tiers, always: attach → compile → evaluate

`open`/`build` once, `plan` once per `(target, source)`, `at` in the loop. Every
binding exposes all three tiers. This is D3 expressed as an API rule.

A convenience that collapses tiers is allowed — `Tree::lookup`,
`tf_tree.lookup` — on two conditions, both **NORMATIVE**:

1. It goes through the plan cache. It does **not** re-resolve topology per call.
2. It is visibly the collapsed one: named differently, documented as paying a
   cache probe, and never the example in the README's hot loop.

`Tree::lookup`'s per-thread cache is keyed `(arena, target, source, generation)`
and `PHASE3.md` §7.2 already requires it be genuinely `thread_local!` rather
than a shared map behind a lock — a collapsed convenience that becomes a
contention point has failed condition 1 in a second way.

The `arena` component is not decoration and this sentence used to omit it
(issue #196). One thread's cache is shared by every `Tree` it touches, and
the other three components agree across trees as a matter of course —
`FrameId`s are handed out in interning order and a built tree's generation is
its edge count — so without it a second tree is served the first tree's
compiled plan. Two handles onto one *shared segment* deliberately share an
id, because they share a topology; everything else gets its own.

What a slot holds is the **result** of compiling that key, and a refusal is a
result (#259). A pair that cannot be planned is compiled **at most once per
key** and answered from the cache thereafter, rather than recompiled per call
for the life of the process — which is what it used to be, and what `0034` made
expensive by walking a doomed path to its full length before refusing it. *At
most*, because the store is conditional: a refusal is filed only when the
topology did not move under the compile that produced it, so a pair queried
against a mutating topology still recompiles. That is the conservative
direction, and it is what keeps the guarantee below unconditional.

This is a property of condition 1, not an exception to it: the generation
component is what makes a refusal as safe to reuse as a plan, and a topology
mutation retires both together. Errors raised *after* compilation — `NoData`,
`Extrapolation`, `SlotRecycled`, `TimeDomainMismatch`, and anything else that
describes the sample history or the query stamp rather than the topology — are
**not** cached and must not be; the key says nothing about them.

The tiers are also the migration ladder. A user arrives at tier 3 by way of
tiers 1 and 2, and every surface — including the shim — must offer a way down.

### R2 — The hot tier never allocates, never locks, never converts

This is the test for **which type a method goes on**. If an operation allocates,
takes a lock, resolves a name, or converts a representation, it belongs on
`Tree` (tier 1) or on the compile step (tier 2), not on `Plan`.

Corollary, **NORMATIVE**: every batch entry point has an `_into` form writing
into caller memory. The allocation is a flat ~270 ns, which is noise at n = 65536
and half the call at n = 64 — and n = 64 is the control loop.

### R3 — Time is integer nanoseconds carrying a domain

No float, no seconds keyword, no convenience overload, on any surface. Settled
by `PHASE3.md` §3 with the measurement. §5 of this document covers what that
means at each boundary, and why the *unit* was never the hard part.

### R4 — Memory layout is stated, never inferred

`Layout` / `tft_layout` / `layout=` are explicit and have no default that could
be silently wrong. Row-major versus column-major differ by a transpose, which for
a rotation is its inverse; `wxyz` versus `xyzw` is a different, still-unit
quaternion. Both produce a valid-looking transform pointing the wrong way.
`PHASE4.md` §3.5 is where the trap is written up; this rule is that write-up
generalized to every surface.

In C++ the layout is chosen **by type** (`layout_of<T>`), so the mistake is not
expressible. Any future typed binding does the same.

### R5 — Errors are identifiers; prose is a separate layer

Error types stay `Copy`, `String`-free and `no_std` (`PROJECT.md` §5 D11).
Name resolution against the arena is `Described`, a `Display` wrapper, not a
field. Across an FFI boundary the *code* is the contract and the message is a
diagnostic.

**Three layers, and only the middle one is new** ([`0040`](./decisions/0040-the-error-that-cannot-be-returned.md)):

| Layer | Knows | Says |
|---|---|---|
| the type and its discriminant | nothing | the contract — this is what a caller matches on, in Rust and across FFI |
| `Display` on the error itself | what the error carries | `edge 3: stamp 5 ns is outside its window [1, 4] ns` |
| `Described`, from `Tree::describe` | the arena | `odom -> base_link: ...`, plus a sample of the frames that do exist |

The middle layer is why an error can *leave a function*: `core::error::Error`
requires `Display`, and without it nothing `?`-chains into `anyhow::Error` or
`Box<dyn Error>`. It resolves no names — it has no arena — so it does not
encroach on `Described`, and where the two would overlap `Described` delegates
rather than restating.

**NORMATIVE for every surface, including the shim:** exception and error
*types* are a compatibility promise. Message *text* is not, and no surface may
document text that a downstream caller could be tempted to match on.

### R6 — Read-only by default for anything that did not ask to write

Python defaults to `mode="ro"`; the C ABI's `tft_tree_open` attaches read-only;
a frozen `.tft` is read-only permanently. A `PROT_READ` mapping does not fault
politely on a `compare_exchange` — it delivers `SIGSEGV` — so every mutating
entry point consults `is_writable()` first and returns a typed error. That is
the difference between read-only being a safety boundary and being a loaded gun
(D18).

A binding may make writing *easier* than this. It may not make it the default.

---

## 2. Rust — the embedding surface

The Rust API is the one every other surface is a projection of. Its distinct
requirement is **embedding**: a user's node, driver or library holds tf_tree
types inside their own types, for the life of their process.

### 2.1 The lifetime rule — NORMATIVE

> **No type a user stores in their own struct may carry a lifetime.**

Measured against it:

| Type | Storable? | |
|---|---|---|
| `Plan` | ✅ | `Copy + Send + Sync + 'static` |
| `Tree` | ✅ | via `Arc<Tree>` — see §2.2 |
| `Guard<'a>` | ✅ | borrow is correct: per-batch, stack-only, never stored |
| `EdgeWriter<'a>` | ❌ | **fails** — fixed by [`0017`](./decisions/0017-owned-handles-and-the-lifetime-rule.md) |

`EdgeWriter<'a>` is the one violation and it is not theoretical: three consumers
needed the owned shape and two built it by hand
(`tf_tree_c::publisher::extend_to_static`, `tf_tree_py`'s copy of the same), one
of them originally as a `transmute::<EdgeWriter, Publisher>` that leaked a claim
lease and bypassed the fork guard for the life of every Python publisher.
[`0017`](./decisions/0017-owned-handles-and-the-lifetime-rule.md) adds
`Tree::claim_owned` → `OwnedWriter` and makes it the single place lifetime
extension is written in the workspace. **That record is now fully implemented**:
`OwnedWriter` landed first and its steps 6–7 deleted both hand-rolled helpers,
so `OwnedWriter` is the workspace's only lifetime extension — present tense, and
a second one anywhere is a new decision record rather than a patch.

`EdgeWriter<'a>` **stays**. A scoped claim whose scope the borrow checker
enforces is better when it fits, and most publishers are scoped.

### 2.2 `Tree` is not `Clone`, and `Arc<Tree>` is the idiom — NORMATIVE (doc)

`Tree` owns its arena backing and holds a registered slot in the arena's
participant table — 64 slots by default (`DEFAULT_MAX_PARTICIPANTS`), and a
number an arena is built with rather than an unbounded pool. A derived `Clone`
would either burn a second slot or lie about sharing one.
`Arc<Tree>` is what `tests/tsan.rs` does directly; `tf_tree_c` refcounts an
`Arc<TreeShare>`, where `TreeShare` is a one-field wrapper holding an
`Arc<Tree>` — **two refcounts, and they are not redundant**: the outer one is
the *handle* refcount a `tft_tree` shares with every `tft_plan` compiled from
it, so a C caller may free them in any order, and the inner one is the *arena*
refcount `Tree::claim_owned` takes `self: &Arc<Tree>` for and a publisher holds
after every handle is gone. PyO3 holds both shapes too: `Py<PyTree>` is the
handle refcount spelled in CPython's allocator, and `PyTree::inner` is the same
`Arc<Tree>` for the same `claim_owned` reason.

> **Correction.** This paragraph previously said the shared thing was the
> wrapper and not the `Tree`, and that "an embedder grepping `tf_tree_c` for
> `Arc<Tree>` will not find it". That was true when `TreeShare` held a bare
> `Tree`; `0017` step 7 made the field an `Arc<Tree>` so the crate could stop
> hand-rolling `extend_to_static`, and the grep now finds it in both bindings.

This needs no code change and one paragraph of crate-level documentation. It is
the first question every embedder asks, and today the answer exists only as
three independent precedents in the source.

### 2.3 Cross-crate inlining is part of the zero-cost claim

A depth-3 lookup costs ~193 ns interpolating
([`0013`](./decisions/0013-the-benchmark-gate-never-interpolated.md)'s
re-baseline; the ~290 ns this line quoted was that record's draft reading), and
`Plan::at` sits across a crate boundary from every consumer. Rust does not
inline across crates without `#[inline]` or LTO, so without one of them the fold
is a call per step.

**NORMATIVE:**

1. `#[inline]` on `Plan::at`, the fold step, `Guard::sample` and the `Iso3`
   operators.
2. Crate docs state `lto = "thin"`, `codegen-units = 1` for embedders. This
   workspace's own `[profile.release]` already sets both, which is exactly why
   nothing here has ever measured the un-LTO'd path — **an embedder's profile is
   not ours**, and the default `cargo build --release` in their repository is.
3. A benchmark row measures the **facade path from a separate crate** against the
   in-crate path, gated at 5% — the same gate `PHASE4.md` §7 applies to the C
   ABI. Today the C ABI is measured against native and the native *embedding*
   path is not measured at all.

> **Amendment — item 1 is done and measured, items 2 and 3 are not, and the
> paragraph above states the wrong mechanism.**
>
> `#[inline]` is on `Plan::at`, on the scalar fold `fold_at`, and on the three
> `Guard` sampling entry points (`sample`, `sample_hinted`, `sample_from`) —
> **five placements, not the six this amendment first claimed.** The `Iso3`
> operators **already carried it** — every method on `Iso3`, `Vec3` and `Quat`,
> and `impl Mul` — so `tf_tree_math` was not touched. Deliberately *not* marked:
> `fold_at_with_derivatives`, `fold_latest`, `fold_latest_common`. Each is large,
> none is on the measured hot path, and `#[inline]` on a large body grows the
> caller at every site.
>
> **`fold_at_cursors` was the sixth, and it is now the counter-example that
> earns the rest.** It is the *batch* fold: reachable from `at_many`,
> `at_many_into`, `at_many_into_f32` and `fold_batch`, and not from `Plan::at`,
> so the probe below never executed it and it was marked by symmetry with
> `fold_at`. Extending the probe with an `#[inline(never)]` caller doing
> `at_many_into(.., Layout::Mat4, ..)` over 1024 monotone stamps at depth 3, best
> of five, and toggling that one attribute:
>
> | downstream profile | with `#[inline]` | without |
> | --- | --- | --- |
> | `lto = false`, `codegen-units = 16` | 328 ns/elem | **285 ns/elem** |
> | `lto = "thin"`, `codegen-units = 1` | 285 ns/elem | **278 ns/elem** |
>
> A pessimization in both, so it was removed. `objdump` says why: at the default
> profile the body is ~1.9 kB and LLVM declines to inline it at either
> `fold_batch` call site *with or without* the hint — both builds leave a real
> call — so the attribute only duplicated a 1.9 kB function into the embedder's
> object. The scalar tables below are unaffected: `Plan::at` cannot reach it, and
> the probe's `caller_scalar` is byte-identical across the two builds.
>
> **Measured**, because a change on the hot path justified by a mechanism nobody
> checked is worth less than silence. Method: an external crate depending on
> `tf_tree` by path, one `#[inline(never)]` function doing a depth-3
> interpolating lookup, 20 M iterations, best of five, x86-64.
>
> | downstream profile | before | after |
> | --- | --- | --- |
> | `lto = false`, `codegen-units = 16` (cargo's `--release` default) | 313 ns | 256 ns |
> | `lto = "thin"`, `codegen-units = 1` (this workspace's own) | 217 ns | 207 ns |
>
> **The sentence above this amendment — "Rust does not inline across crates
> without `#[inline]` or LTO, so without one of them the fold is a call per
> step" — is not what happens.** `Plan::at` is *generic*, so its MIR crossed the
> boundary regardless and the downstream caller was already inlining it.
> `objdump` of that caller, default profile:
>
> | attribute placed on | caller `.text` | calls left in the caller |
> | --- | --- | --- |
> | nothing (the state before) | 106 B | 1 → `Plan::fold_at` |
> | `Plan::at` **alone** | 106 B — *byte-identical* | 1 → `Plan::fold_at` |
> | `fold_at` alone | 62 B | 1 → `Plan::at` |
> | `fold_at` + `Plan::at` | 1332 B | 1 → `Guard::sample_hinted` |
> | all five on this path, as shipped | 1565 B | 3 → `sampler`, 2× `SampleRing::sample_from` |
>
> So there was exactly **one** cross-crate call, not one per step, and what the
> attribute changes is LLVM's `inlinehint` on the *non-generic* links. `fold_at`
> removes that call; `Plan::at`'s hint then stops the cost model halting at a
> now-larger `at`; `Guard::sample*` removes the last one. "Marking fewer leaves
> a call in the middle" survives as a claim — rows three and four are it — but
> it is now a measurement rather than an assertion.
>
> **The price is caller code size: 106 B → 1565 B, ~15×, at every embedder call
> site.** That is the trade this section should have named and did not — and it
> is the *scalar* caller's price. The batch caller's was named nowhere until
> `fold_at_cursors` was measured above, which is the reason it went the other way.
>
> **`lto = "thin"` does not subsume the hint**, which corrects the other thing
> this amendment first claimed. This workspace's own profile still moves ~4.5%,
> so the benchmark gate is *not* indifferent to the change. `just bench-ab` is
> workspace-wide and was still not run.
>
> Item 2 (the `lto`/`codegen-units` guidance in the crate docs) and item 3 (the
> gated cross-crate benchmark row) are untouched. The table above is item 3 done
> once by hand; item 3 is doing it continuously.

> **Amendment — items 2 and 3 have landed, and item 3's first act was to report
> that its own gate is not met.**
>
> Item 2 is a section of `tf_tree`'s crate docs: set `lto = "thin"`,
> `codegen-units = 1`, with the measured size of what it buys and an explicit
> note that how that splits between the two settings was *not* measured.
>
> Item 3 is `crates/tf_tree_bench/src/embed.rs`, the `embed_cost` binary,
> `just embed-cost`, and `PHASE5.md` §9.2's row `embedding_cross_crate` in the
> benchmark artifact. **It is what this item asks for and not a substitute for
> it: one build, one profile, two identical `#[inline(never)]` bodies, one
> compiled in `tf_tree_bench` and one in `tf_tree_core`.** The in-crate half is
> `tf_tree_core::bench_probe`, behind a default-off `bench-probe` feature — the
> same pattern as `tf_tree_c`'s `test-hooks`, which exists so `abi_cost.rs` can
> measure the ABI boundary this row's 5% is copied from.
>
> `[profile.embedder]` (cargo's `--release` defaults, spelled out field by field
> so they cannot drift with this workspace's) is the profile the row is read at,
> because §9.2 requires it. A **second, exploratory** measurement — the same
> out-of-crate body under the two profiles — is printed beside it and is
> deliberately not in `results.json`; `PHASE5.md` §9.2's amendment is the table
> of which is which.
>
> **The measured ratio is 1.250–1.254×, so §9.2's 5% criterion is NOT met**, and
> that is reported rather than engineered around. Three consecutive pinned runs,
> paired rounds:
>
> | downstream profile | out-of-crate | in-crate | ratio |
> |---|---|---|---|
> | `lto = false`, `codegen-units = 16` | 240.0–240.1 ns | 191.3–191.8 ns | **1.250–1.254** |
> | `lto = "thin"`, `codegen-units = 1` | 193.0–195.0 ns | 194.2–196.2 ns | 0.994–0.996 |
>
> The second row is the control, and it is what item 2's guidance is worth
> stated as a measurement rather than as advice: same host, same two bodies, one
> profile setting different, boundary gone.
>
> **The earlier revision of this amendment said "no `#[inline]` placement closes
> that; the embedder's profile does". The first half is refuted by this row's own
> toggle and has been removed.** Removing `#[inline]` from `Plan::fold_at` and
> re-running takes the ratio from 1.253 to **1.001** — a passing gate — by making
> the in-crate column 6.7% slower and the `[profile.release]` control 6.9%
> slower, while the out-of-crate embedder column gets 15% *faster* (240 → 204
> ns). So a placement demonstrably moves it; what it does not do is improve
> everything at once. Whether that trade is worth taking is not this row's call —
> item 1 is normative and `just bench-ab` is workspace-wide and was not run for
> it — and it is exactly the kind of question a standing row exists to surface.
>
> That toggle is also why **`boundary_ratio`, `out_of_crate_ns` and `in_crate_ns`
> are all gated at 5%, not just the quotient §9.2 names**: a ratio-only gate
> reads the run above as a 20% improvement. `in_crate_ns` is the metric that
> fires on it.
>
> Two method notes, both found by measuring. **A probe in `tf_tree` is not
> in-crate** (241.5 vs 243.6 ns — the facade re-exports the engine rather than
> containing it), which is why the in-crate half had to go into `tf_tree_core`.
> And **the in-crate body must not be generic**: the first version took
> `Stamp<D>`, was therefore monomorphized in the *calling* crate, and the row
> reported 1.000× while both columns were out-of-crate. Making it concrete moved
> the same measurement to 1.250×.
>
> The verdict is `unresolved` rather than a pass or a fail whenever the observed
> round-to-round band straddles the threshold. Pairing the two columns inside a
> round is what makes 5% resolvable here at all. Across four runs the paired
> ratio moved by 0.3% *between* runs (1.250, 1.253, 1.254, 1.254) with a
> *within*-run band of 1.0% to 4.4%; every one of them still resolves the
> criterion, because a band of 1.216–1.270 does not reach 1.05. The unpaired
> two-process profile ratio moved 1.188 → 1.235 over the same runs, which is
> four times the whole allowance and is why that one is not gated.
>
> Host, and it matters: a 4-physical-core AMD EPYC-Milan VM that fails
> `Fitness::probe`, so the row is `unavailable` in this repository's own
> committed baseline and the numbers above are evidence for a design decision
> rather than claims.

### 2.4 Two things that are not going to exist

Recorded here so they do not arrive later by adjacency.

**No `trait TransformSource`.** Embedders will ask for it to mock in tests. The
answer is that `TreeBuilder::build()` **is** the test double: a real engine over
a heap arena, with the real interpolation, the real seqlock and the real errors.
A trait buys mockability and costs the devirtualized hot path plus a second
implementation nobody tests.

**No blocking wait in the core.** Settled by
[`0018`](./decisions/0018-blocking-waits-belong-in-the-shim.md). Core already has
`Plan::span`; it gains the per-edge nominal rate; the waiting itself lives in the
caller.

### 2.5 Domains are an open trait

`Domain` stays implementable by users — a driver with a PTP-disciplined clock
declares `struct PtpDomain;` and picks a free `TAG`. If the built-in set is
closed, everything collapses to `SystemDomain` and `TimeDomainMismatch` never
fires for the people who need it most. See §5.2.

**The built-in set is four, and tags `0`–`3` are reserved for it:**
`SystemDomain` (0), `SensorDomain` (1), `SimDomain` (2), `SteadyDomain` (3). A
user-declared domain picks a free tag from `4` upwards. All four live in
`tf_tree_core::plan` and are re-exported through the `tf_tree` facade; each is a
unit struct and a `TAG` and nothing else.

**Why four, when it was two — the argument is retained because it is still the
reason the last two exist.** Two built-ins is close enough to a closed set that
a sim deployment and a steady-clock driver both end up on tag 0, since both are
"not a sensor"; `TimeDomainMismatch` then never fires for the two populations
most exposed to the bug it exists to catch. That is this section's own opening
warning arriving as a concrete cost. The trait being open is what made the fix
cheap — two unit structs beside the existing two — but it was never a substitute
for making it, and this is the argument to reach for the next time a domain is
proposed.

**A tag is a permanent choice.** It is written into `EdgeRecord::domain` at
declaration time and read by every consumer, every diagnostic and every recording
already on disk, so re-numbering one silently re-interprets all of them — §5.2's
"unfixable after the fact" applied to the numbering rather than to the choice.
The `Domain` trait's own documentation says so, and a test pins the four values
rather than leaving them to convention.

**`EdgeRecord::domain` is an existing `u8` field**, so declaring the two later
types moved neither `FORMAT_VERSION` nor `layout_hash`.

**What is settled, and what is not.** The naming and numbering are settled:
`sim` is 2, `steady` is 3, permanently — the prerequisite
[`PHASE7.md`](./PHASE7.md) §4 J9 asks for before its read side can be specified.
**The write side now names it:** `tf_tree_bridge::config::parse_domain` maps all
four names onto their `Domain::TAG`, so a topology file says `domain = "sim"`.
`TopologyConfig::default_domain` stays a `u8` on purpose — this section's own
point is that the trait is open, and a user-declared domain from tag 4 upwards
has no name for a parser to accept — so the numeric form is kept beside the
names rather than replaced by them.

One clause of `PHASE4.md` §5.5 is **still** open: nothing derives the bridge's
own tag from `use_sim_time`. §5.5's amendment records what is left and what
today's misconfigurations actually do; this document does not restate it.

§5.3's `doctor` check does not depend on the two new types and still does not.
`PHASE5.md` §6's `TFT019` keys on the tag and fires only on tag 0, which is now
*correct rather than merely conservative*: a `SteadyDomain` edge cannot have
stepped, so a run of `NonMonotonicStamp` rejections there is a real publisher
defect, and reporting it as a clock step would be the fabricated all-clear that
amendment refuses. Teaching `TFT019` that tag 3 is provably steady is a
refinement it can now make and has not yet made.

> **Naming note, because two documents will send a reader looking for the wrong
> identifier.** `PHASE4.md` §5.5 and `PHASE7.md` §4 J9 named these
> `SimTime`/`SystemTime` before either existed. `SystemTime` has never existed
> under that name, so that pairing was never this code's convention; the set is
> uniformly `*Domain`, and both documents now say so. Where those documents
> discuss ROS's `use_sim_time` — sim time the *concept*, not our type — the prose
> is unchanged.

### 2.6 Stability tiering — built; the split *is* the promise — NORMATIVE

C has `tf_tree.h` and `tf_tree_unstable.h`; Rust has one visibility tier, so
everything `pub` reads as a stability promise whether it was meant as one or
not. **The deferral this section used to record has been taken up**: the mirror
exists, as `tf_tree::unstable` behind a default-off `unstable` Cargo feature
whose documentation *is* the waiver. A macro is what C had to spell it with
because a header is text; a feature is what Rust has, because it is the only
mechanism a **caller** has to write down.

**The rule for what goes there is not "is it low-level".** It is *does its shape
follow the arena layout*, and the reason is that `PHASE5.md` §1 changes that
layout on purpose — `FORMAT_VERSION` to 3, plus regions Phase 6 fills. Anything
shaped by it is scheduled to move by a document that already exists. `Plan`,
`Guard` and `Stamp` are as low-level as anything in the crate and are **stable**,
because their shape is the engine's contract rather than the arena's. Three items
moved: `ArenaView`, `EdgeKind`, and `EdgeMeta` — the last because the audit found
it was already *unusable* from the stable tier, being an input to a
`tf_tree_core::compile` the facade never re-exported. `Tree::arena_view` is gated
with them, because it is the door: a caller reaches every accessor on the
returned value by inference without ever naming the type, so leaving the method
stable would have made the split a spelling convention.

**Gating the door must not remove the capability, and that is the other half of
this section — NORMATIVE.** §7 check 1 asks of every surface whether all three
*call* tiers are reachable, and "what is in this tree" is a tier-1 question.
Python has answered it since §3.2 with `tree.frames()`, `tree.edges()` and
`plan.edges()`; if the Rust answer were `arena_view`, an embedder
would have had to sign the waiver to ask a question about their own data, which
is backwards. So the facade carries **stable `Tree::frames` and `Tree::edges`**,
mirroring the Python surface: names only, since §4.2's statistics half is held
back on every surface until `PHASE5.md` §3's counting pass. What `unstable` gates
is the arena-shaped *spelling* of an answer the stable tier already gives.

**The waiver is not free and the consumer list is checked, not asserted.** Every
crate that turns the feature on is one whose build may break at a patch release.
`just stable-tier-check` reads the list back out of the `[dependencies]` entries
and fails if this document's §6 row 4 or `crates/tf_tree/Cargo.toml` disagrees
with it — `[dev-dependencies]` counted separately, because a test-only waiver is
a different fact from a shipped one.

**A tier nothing compiles is not a tier.** The default configuration — the one a
`cargo add tf_tree` consumer gets — is invisible to `cargo build --workspace`,
because the resolver unifies the feature in from the crates that ask for it.
That half is permanent, and it is measurable rather than folklore:

```
$ cargo tree -p tf_tree -f '{p} FEATURES=[{f}]' --depth 0
tf_tree v0.0.1 (…/crates/tf_tree) FEATURES=[counters,default]

$ cargo tree --workspace -e features -f '{p} FEATURES=[{f}]' | grep '^tf_tree v'
tf_tree v0.0.1 (…/crates/tf_tree) FEATURES=[counters,default,unstable]
```

**What changed at 0.0.1 is the other half — how many recipes reach it.** The
facade used to carry a `tf_tree = { path = ".", features = ["unstable"] }`
dev-dependency, which turned the feature on for every one of its own test
targets; `just stable-tier-check`'s `--lib` lines were then genuinely the only
place the default tier compiled. That line does not survive `cargo package` and
was deleted, so *every* `-p tf_tree` selection now resolves to
`counters,default` — including the test targets, and including two recipes that
were not written with this tier in mind: `just miri`'s
`-p tf_tree --lib --test owned_writer` and `just test-doc-error-codes`'
`--doc -p tf_tree -p tf_tree_core`. Both were run through the first command
above and print `FEATURES=[counters,default]`. `cargo nextest list -p tf_tree
--lib --tests` counts 70 tests against 77 with `--features unstable`, which is
the same fact from the target side.

`just stable-tier-check` remains the *gate*, and CI runs it as its own job: it is
the recipe that names the configuration on purpose, puts the `shm` and
no-default-features variants beside it, and renders the tier's own rustdoc — that
last line is still unique, because `just doc` renders the facade at
`--all-features`, where a link into `tf_tree::unstable` resolves and a published
consumer's does not. What is no longer unique is the *compiling*: the `shm`
variant falls out of `just shm-check`'s
`cargo clippy -p tf_tree --features shm --all-targets` the same way
(`FEATURES=[counters,default,shm]`), and the no-features configuration out of
`just ingest-check`, whose facade is a dependency at `FEATURES=[]` because
`[workspace.dependencies]` declares `tf_tree = { default-features = false }` —
a third configuration, not the one `cargo add tf_tree` produces. A recipe that
reaches a configuration on its way somewhere else is not a gate for it; it is
how a break gets noticed by the wrong error message.

**One warning about how much of that the check itself defends.** The consumer
list above is compared to the `[dependencies]` entries, name by name, in both
this document's §6 row 4 and the manifest comment — but only the *names*: the
recipe greps row 4 for each of `tf_tree_bench`, `tf_tree_c`, `tf_tree_cli` and
`tf_tree_py` and asserts nothing about the prose around them. Which recipes
compile which feature set is exactly the kind of sentence that stays green while
going stale, and it is the sentence that did.

### 2.7 A kernel the engine already runs is public on its own terms — NORMATIVE

`tf_tree_math` ships two interpolation policies and, until row 16, one of their
two kernels: `dualquat::screw_pow` — `ScLerp`'s — has been `pub` since the
crate's first commit, while `slerp` — `LerpSlerp`'s — was `fn slerp`. A
downstream crate that wanted rotation-only interpolation therefore built two
`Iso3` with throwaway zero translations and called `LerpSlerp::eval`. The
numerics were right and the entry point was missing.

**The rule, stated once so the next kernel does not need its own argument:** a
kernel this crate already evaluates on the hot path is `pub` when reaching it
through its wrapper would make the caller **manufacture a value of a type their
problem does not contain**.

**`screw_pow` is the precedent for the tier and is not an instance of that
rule**, and the difference is worth being exact about because an earlier
revision of this section stated the rule as *"the only way to reach it from
outside is to construct a degenerate input to its wrapper"* and cited
`screw_pow` under it. A review pass refuted the first attempt at drawing that
line, and the refutation is kept because it is the trap. It is **not** true that
`eval(&Iso3::IDENTITY, &rel, s)` "invents nothing": `Iso3::IDENTITY` is
manufactured, and it is *more* degenerate than the `Vec3::ZERO` a `slerp` caller
manufactures, since it fixes a rotation as well as a translation. Measured, that
call is bit-identical to `screw_pow(&rel, s)` in both `q` and `t` at
`s` = 0.25/0.5/0.75, so the identity it manufactures is read only by `inv_mul` and
the trailing compose — which is the older wording's own consequence clause. So
`screw_pow` *was* an instance of it, and "no valid precedent" would be too
strong. Nor does arity separate them: `screw_pow` takes one `&Iso3` where
`ScLerp::eval` takes two, so on a count of arguments its input is the narrower
one.

What separates them is the **type**, which is why the rule is stated that way
above. A `slerp` caller holds two quaternions and must invent a *translation* —
a value of a kind their problem does not contain at all. A `screw_pow` caller
already holds a relative transform, and supplies a second, legitimate member of
a type they are already working in. What `screw_pow` does establish is the
narrower claim this section actually rests on — **that a policy's kernel is
stable-tier public API in this crate rather than a private detail of the
policy** — which it has demonstrated since the first commit without an argument
ever being written down. `slerp` is the first item the rule above decides, and
the asymmetry row 16 closes is between the two kernels' *visibility*, not
between two instances of one condition.

What makes the tier safe to promise is §2.6's test — *does its shape follow the
arena layout* — and `(Quat, Quat, f64) -> Quat` follows nothing: no arena in
it, no `FORMAT_VERSION` under it, and `PHASE5.md` §1 does not schedule it to
move. It is stable-tier for the reason `Plan` and `Stamp` are. `tf_tree_math`
has no `unstable` feature to gate it with in any case, which is the same
position `tf_tree_core` is in and answers the same way: a statement instead of
a mechanism.

**Why this is a section and not a decision record.** `CLAUDE.md` sends *a change
the specs do not cover* to a `draft` record. This one is covered: §2.6 decides
the tier, R2 is satisfied rather than consulted (the function **is** the hot
path), and `PROJECT.md` §6's no-second-spelling rule is answered by the kernel
being a different function from the policy — different inputs, one of them
absent — rather than another way to spell it. Rows 14 and 15 are the closest
precedent: both are new public API on a published artifact (the PyPI wheel),
both cite §3.2, and `grep` over `docs/decisions/` finds no record authorising
either — `0026` and `0027` only *invoke* `arena_format_version()` in shell
transcripts, and `0019`'s `frame_headroom` is the Rust `TreeBuilder` knob, not
the Python keyword. They are not an isolated pair either. **Of the fifteen rows
before this one, ten name a section of this document as their authority and
five name a record or a phase spec** — so the §6 preamble's "nothing here is
authorized by this document alone" cannot mean that a section never authorises,
or two thirds of its own table would be unauthorised. It is about *scheduling*:
a row still has to land somewhere, and this one lands in the PR that writes it.
A record here would contain no question.

**§7, walked**, here rather than in a phase spec because §7 is here. The
surface is one free function, so checks 3, 4, 6 and 7 fall out of the signature
and 1 and 2 out of where it sits; 5 and 8 needed an argument.

1. **Tiers.** Not a fourth way to look a transform up: it takes two poses the
   caller already holds, never a `(target, source)`, and resolves no name. The
   ladder is unaffected — `Plan::at` reaches it through `LerpSlerp::eval`
   exactly as before, and this is a rung *below* tier 3, not beside it.
2. **Hot tier.** It is the hot tier. No allocation, no lock, no name
   resolution, no conversion; `#[inline]`; the body is untouched by row 16, so
   `Plan::at` compiles to what it compiled to before. The `_into` corollary
   binds batch entry points and there is no batch here — one 32-byte value out.
3. **Time.** No stamp anywhere. `s` is a dimensionless fraction and the doc now
   says who divides in integer nanoseconds to obtain it; there is no float
   seconds path to leak in, because there is no time in the signature.
4. **Layout.** `Quat` is `[w, x, y, z]`, scalar-first, stated in the function's
   own `# Storage order` heading with the Eigen/`nalgebra` transposition named
   as the hazard it is. Chosen-by-type is not available with one type.
5. **Errors.** It cannot fail and returns no error. What it *can* do is answer
   nonsense: the preconditions section states that both inputs must be unit and
   that `NaN` propagates through every branch except the numerically-identical
   early return, which returns `qa` for any `s` — `NaN` and `±inf` included.
6. **Writability.** A pure function of `Copy` values. Nothing to write.
7. **Lifetimes.** Takes and returns by value; no lifetime to store, which is
   the check most Rust surfaces fail and this one cannot.
8. **Losses — and this is the one that needed a decision.** Read literally,
   check 8 asks for a *benchmark* row where the new surface is worse, and there
   is none to add: both call shapes end in the same out-of-line `slerp`, and
   what the `Iso3` round trip puts in front of it is tens of instructions of
   wrapper — 28 through the consumer's own `nalgebra` adapter, 45 bare, and 31
   and 48 in other release profiles. **That spread is itself the argument.** A
   criterion row measuring a quantity whose *static* instruction count already
   moves 10% between two `opt-level = 3` builds would be a gate whose
   denominator moves more than its signal — [`0023`](./decisions/0023-the-gate-that-could-not-gate.md)
   is what that costs, 43% on an unrelated edit, and it is not a lesson worth
   re-learning for a wrapper. Read for what it is *for* — name what the reader loses — it
   binds, and the loss is behavioural rather than temporal: at `s = 0` and
   `s = 1` the direct call is **worse** than the round trip it replaces, because
   `LerpSlerp::eval`'s endpoint shortcuts hide two things the kernel does not
   (`-qb` at `s = 1` under the sign fix; renormalized endpoints inside the
   `1e-6`-rad fallback band). The table discharging check 8 is therefore a
   differential test, `the_iso3_round_trip_it_replaces_agrees_as_a_rotation`,
   not a benchmark row. **A math primitive answers check 8 with a differential;
   a binding answers it with a benchmark. The check binds either way.**

**The `tf_tree` facade re-exports it, and that is a second surface with its own
§7 answer — a short one.** It was missing from row 16 as first landed, and the
omission mattered: the facade already re-exports `LerpSlerp`, so a consumer who
took the policy from `tf_tree` and the kernel from `tf_tree_math` held **two
direct dependencies to keep pinned in lockstep** on a line where every release
breaks every other — a worse position than the `Iso3` round trip this section
told them to abandon, and the exact opposite of what row 16 was for. Checks 2
through 8 are answered above and unchanged, because `pub use` names the item
rather than wrapping it: `tf_tree::slerp` **is** `tf_tree_math::slerp`, pinned
by `tf_tree/tests/math_reexports.rs`, which compiles only if the two paths
resolve to one function item. Check 1 is unaffected for the same reason as
above — a kernel is a rung below tier 3, not a fourth lookup. The loss under
check 8 is not the caller's, it is this project's: the name joins the facade's
stable tier and is a semver promise there, where before it was one crate over.
`ScLerp`'s kernel deliberately does **not** follow it: `screw_pow` is reached
through `tf_tree_math::dualquat`, and a bare `screw_pow` at the facade root
would be a second spelling of that path (`PROJECT.md` §6) rather than the same
one. `slerp` has no module-path spelling to compete with — `tf_tree_math`
already re-exports it at its own root.

### 2.8 The two path bounds are public, and each prices a different slot — NORMATIVE (doc)

`tf_tree::MAX_DEPTH` and `tf_tree::MAX_PATH_EDGES` are `pub const` on the stable
tier, so on a published tag each is a semver promise about a *value* as well as a
name. Neither appeared in this document before
[`0034`](./decisions/0034-the-depth-bound-priced-two-slots-the-same.md), and that
omission is what let one of them be documented as bounding the compiled plan
while it was enforced on the raw walk.

| constant | bounds | a slot costs | value |
|---|---|---|---|
| `MAX_DEPTH` | the **compiled** plan — `Plan`'s `[Step; MAX_DEPTH]`, counted after folding | **128 B**, in every `Plan`, in a 16-slot thread-local cache, and behind every Python `Plan` | **32** |
| `MAX_PATH_EDGES` | the **raw walk** — edges visited on both sides of the common ancestor | **4 B**, in `compile`'s stack frame, on a call D3 already places off the hot path | **64** |

**A binding may not invent a third bound and may not hide these two.** Both are
re-exported by the facade and both are what a `TreeTooDeep` message must be
written against; `crates/tf_tree_bench/src/workload.rs` checking a raw edge count
against `MAX_DEPTH` — refusing a long rigid chain that compiles to three steps —
is the defect this row exists to prevent recurring.

**R5 and the one error variant.** Both overruns are `LookupError::TreeTooDeep`,
one variant, because the C ABI's `tft_status` table is frozen and R5 makes the
*code* the contract across FFI. The `depth` field is what separates them and its
two ranges are disjoint by construction — see the variant's own documentation.
The prose layer differs per binding on purpose, which is R5 working rather than
drifting: **Rust's** message names `TreeBuilder::static_edge` as the remedy,
**Python's** must not, because `tf_tree.build` declares every edge dynamic and
the remedy is unreachable there, and **C's** header names no macro at all,
because `TFT_MAX_DEPTH` was referenced for two phases and defined nowhere.

---

## 3. Python — mirror, plus conveniences that pay for themselves

The Python surface mirrors §1's three tiers exactly (`open`/`build` → `plan` →
`at`). Divergences from Rust are deliberate and few: `mode="ro"` and
no-creation-by-default (R6), and scalar/array dispatch on `at` (the NumPy idiom,
which is what makes the vectorized path the *obvious* path).

**A third divergence existed and has been closed, and the list above is the
reason it counted as a defect.** `tf_tree.build` and `tf_tree.open(create=...)`
defaulted to `interp="lerpslerp"` where `TreeBuilder` defaults to `ScLerp`, so a
Python caller silently got an interpolator that is left-invariant but **not**
right-invariant while the Rust caller's default satisfies both.
`PROJECT.md` §5 D5 says in terms: *do not* make `LerpSlerp` the default without
a measurement justifying it, and no such measurement exists in this repository.
The Python default is now `"sclerp"`; `interp="lerpslerp"` stays and is what D5
keeps `LerpSlerp` for — bit-compatible differential testing against `tf2`. It is
recorded here as closed rather than deleted, because a binding that quietly
picks a different interpolator is the kind of divergence this list exists to
make expensive to add.

### 3.1 Still refused — NORMATIVE

Float stamps; `asyncio`; any view into the arena; `pickle` of `Tree`/`Plan`/
`Publisher`; keyword arguments on `at`, `at_into`, `latest`, `push` (measured:
`METH_FASTCALL` is 29 ns cheaper — ~15% of a depth-3 lookup at §3.4's
re-baselined 193 ns, and the "20%" `PHASE3.md` §4.2 states against the
superseded 150 ns budget);
any logic with a branch in it that could live in Rust.

Also refused, and this one gets asked: **`scipy.spatial.transform.Rotation`
interop.** It pulls scipy into a wheel that needs only NumPy, and
`layout="quat"` already produces what `Rotation.from_quat` wants modulo
coefficient order — a documentation line, not a dependency.

### 3.2 Accepted conveniences

All of these run at tier 1 or tier 2 frequency, so R2 is not in tension:

- scalar/array dispatch on `at`; context managers; `__repr__`; the hand-written
  `.pyi` and `py.typed`
- `from_sec` / `from_datetime` / `now` / **`from_ros`** (§5.1)
- **introspection: `tree.frames()`, `tree.edges()`, `plan.edges()`.** Notebook
  users currently shell out to the CLI to see what is in the arena. A list of
  names costs nothing at import-time frequency, and its absence is the single
  most-reported friction a Phase 5 offline user will hit. (`plan.depth()` and
  `tree.span()` already ship; `tree.edges()` is the offline `ds.edges()` of
  `PHASE5.md` §4.2, which that section's amendment holds back until §3's
  counting pass exists — the *names* half of it does not wait on that.)
- **build identity: `__version__`, `arena_format_version()`,
  `arena_layout_hash()`.** A benchmark number or a bug report that cannot be
  attributed to a build is worth very little, and `tf_tree.__version__` raised
  `AttributeError` until these landed, in the first release. Three values
  because they fail independently: the right version can still refuse to
  attach, because the arena it was pointed at was written by a different
  *geometry*. The last two are the two words every participant already compares
  on attach (`PHASE5.md` §1), re-exported from the facade under the facade's own
  names — this crate does not gain a `tf_tree_arena` dependency to answer a
  diagnostic. Import frequency is below even tier 1 and neither call reaches an
  arena, so R2 is not in tension by the letter either; row 14 has the rest,
  including why the two are functions and not module constants.

- **arena headroom: `frame_headroom=` on `build` / `open`.** Spare frame-name
  slots, `TreeBuilder::frame_headroom` under the same name, default `0`. A
  sizing knob beside `capacity=`, not a layout in R4's sense — R4 is about the
  *pose* layout, where a wrong guess is a valid-looking transform pointing the
  wrong way. Without it a Python-created arena admits no runtime-interned frame
  name from any participant, including a Rust or C peer and the ROS bridge.

### 3.3 Parity deltas to close

| Gap | Where | Disposition |
|---|---|---|
| `at_with_derivatives` absent from Python | Rust and C have it since Phase 4 (`tft_plan_at_with_derivatives`, unstable tier); `PHASE4.md` §0 scoped Python out | **Phase 5**, as `Layout::QuatTwist` — see below |
| `Publisher` holds an extended borrow by hand | `tf_tree_py/src/tree.rs` | [`0017`](./decisions/0017-owned-handles-and-the-lifetime-rule.md) |
| `at_extrapolating` takes no `layout=` and has no `_into` form | `tf_tree_py`; C can extrapolate into `affine32` and Python cannot | **Open.** R2 makes `_into` NORMATIVE for *batch* entry points, and this is one, so the asymmetry is owed rather than declined. It was scoped out of [`0039`](./decisions/0039-extrapolation-you-cannot-fail-to-notice.md)'s binding step rather than argued away |
| Python cannot declare a static edge, a per-edge capacity, a rate, or a domain | `tf_tree.build(edges, capacity, interp)` makes every edge dynamic under one capacity | **Open.** A sensor mount published as a dynamic edge is the latched-topic behaviour `PROJECT.md` §2 lists as a problem `tf_tree` solves, so a Python-built tree cannot reach one of the engine's headline wins. Needs a builder mirroring `TreeBuilder`, which is new public API and therefore a decision record |

**`at_with_derivatives` ships as a layout, not a method.** `Layout::QuatTwist`
is a contiguous `(N, 13)` write of `[qw qx qy qz tx ty tz | ωx ωy ωz vx vy vz]`.
One core enum variant beside `Mat4`/`Quat`/`Affine32`, and the existing layout
dispatch carries it to Python *and* C for free. A separate `at_d` method would
need its own GIL threshold, its own buffer validation and its own tests, for the
same bytes.

Twist-at-a-stamp is most wanted by exactly the ML and perception people writing
Python, and it is the surface where "a different primitive, not a faster tf2" is
most legible. Phase 5 §4 already opens the Python module for the offline API, so
the opening does not have to be manufactured.

*C ABI impact:* adding a `tft_layout` enumerator is a **minor** bump under
`PHASE4.md` §3.6 (major must match exactly; runtime minor may exceed
compiled-against minor). No major bump, no `struct_size` change. The existing
`TFT_TWIST_BYTES` in the unstable header keeps its meaning; the layout is the
*batch* path to the same numbers.

*Refusal path:* `LerpSlerp` has no exact twist, so `at_with_derivatives` already
returns `DerivativesUnavailable` there (`PHASE4.md` §2). `Layout::QuatTwist`
returns the same typed error rather than emitting a plausible-looking finite
difference — a layout that silently changes what it means per interpolator would
be R4's failure in the time axis instead of the memory one.

### 3.4 The GIL threshold constant is calibrated against a benchmark that never ran

**Fixed, in `0013`'s re-baseline commit, as this section requires below.**
`PHASE3.md` §6.1 set `NS_PER_STEP_ESTIMATE = 55`, derived from the Phase 1
lookup benchmark. [`0013`](./decisions/0013-the-benchmark-gate-never-interpolated.md)
shows that benchmark queried on-grid stamps, so `I::eval` never ran.

Re-measured off-grid, the constant is now **64 ns/step**.
[`PHASE3.md`](./PHASE3.md) §6.1's amendment is the **single account** of it — the
measurement, its band, its protocol, and the one element it moves — and this
section is deliberately a pointer to that account and not a second copy of it.
The ~290 ns / ~97 ns/step figure this section used to quote was `0013`'s draft
reading, taken with `cargo bench --quick`; `0013`'s *Re-baseline* supersedes it.

**Nothing was broken, and that is the interesting part.** §6.1's own claim was
that "the exact constant does not need tuning; what matters is that neither
branch is ever badly wrong". The correction moves the depth-3 crossover by
exactly one element; §6.1 prices that and a `const` assertion in
`tf_tree_py::tree` keeps it checked. The design absorbed a wrong input, which is
the strongest evidence it is right — now a checked statement rather than an
assumed one.

**NORMATIVE:** when `0013` re-baselines, `NS_PER_STEP_ESTIMATE` is re-derived
from the new number in the same commit, and `PHASE3.md` §6.1 gains a line saying
which measurement it came from. A constant with no cited source is how this
happened. This applies again to any later re-measurement `0013` ratifies.
**§6.1 is where that line lives, and it is the only place the derivation is
written down**: this section is the instruction, not a second account, and a
third one is how the first two drifted.

---

## 4. C and C++

Both are specified in `PHASE4.md` §3–§4 and implemented. Restated here only
where they carry a rule the other surfaces must also obey.

**Two tiers of header, and the split is the stability promise.** `tf_tree.h` is
semver'd; `tf_tree_unstable.h` is opt-in by macro and promises nothing. §2.6 is
the Rust mirror of it and is now built: a `tf_tree::unstable` module behind a
default-off Cargo feature. The two surfaces spell the same promise with the only
mechanism each language gives a *caller* to write down — a `#define` there, a
feature there.

**Every struct passed by pointer begins with `uint32_t struct_size`**, so fields
append without a major bump and the callee rejects sizes it does not know. An
older caller's prefix is read as the prefix it is.

**The C++ wrapper contains no logic — a rule, not an aspiration.** It is inline
in the user's translation unit, invisible to the Rust test suite, to Miri and to
ASan-instrumented Rust. Anything that can be wrong there must be a
`static_assert`, not a runtime branch. Any future header-only surface inherits
this verbatim.

**Layout by type** (`layout_of<T>`, `raw_writable<T>`) is R4's strongest form
and is the model for any future typed binding.

### 4.1 The two capabilities the bindings could not reach — NORMATIVE (doc)

Both were implemented in the engine, tested, and callable from Rust only. They
are recorded here because the *shape* each took generalises to any future
capability that has to cross this boundary.

**The time domain** ([`0038`](./decisions/0038-the-domain-a-binding-cannot-name.md)).
`Domain` is an open trait whose tag is a `const`, so a binding cannot name the
type `at::<D>` needs and must carry the tag as data. Every query site in both
bindings hardcoded `SystemDomain`, which made any arena not on tag 0 unreadable
from C, C++ and Python — while `ros/tf_tree_ros` warns an operator to configure
one under `use_sim_time`. The tag lives on the **plan handle**, not on the call:
the ABI is frozen so a new creation entry point costs one declaration where a new
call would cost three; a domain is a property of a route rather than of an
instant; and validating at plan time is where the frame *names* are still in hand,
so the diagnostic says which route disagreed rather than which two integers did.

**Extrapolation** ([`0039`](./decisions/0039-extrapolation-you-cannot-fail-to-notice.md)).
`Plan::at_extrapolating` returns a value with no pose-only accessor, so a caller
cannot read the pose without the distance it was extrapolated by. **C cannot
enforce that, so the analogue is a required out-parameter**: a null `info` is
`TFT_ERR_NULL_ARG`, nothing is written, and there is deliberately no second
spelling of the call without it. Two places where C is *sharper* than the Rust it
mirrors, both because C has no way to say "meaningless": `edge` is
`TFT_INVALID_ID` when `by_ns == 0`, since a plausible edge id attached to an
answer nothing invented would be logged as fact; and the twist-carrying layout is
refused rather than served under `Error`, which would put two extrapolation
policies in one 13-`f64` row.

**In Python the batch distance is an `(N,)` array and not a scalar**, and that is
the same property a third time. A batch straddling the newest sample holds
interpolated and extrapolated elements together, so collapsing it is either a
`max` that marks fresh elements stale or a `min` that marks stale ones fresh —
and the second is exactly the failure `0039` exists to prevent.

---

## 5. Time at the boundary

R3 settles the *unit*. This section is about the two things that are actually
hard: getting a stamp in without friction, and getting the **epoch** right.

### 5.1 The unit was never the imposition — the conversion was

The ecosystem already agrees with int64 ns. `rclcpp::Time` stores int64
nanoseconds internally and `nanoseconds()` is exact and free; `rclpy.time.Time`
is integer nanoseconds; `builtin_interfaces/Time` is `{int32 sec, uint32 nanosec}`
and converts exactly; PTP is nanosecond-based; `clock_gettime` yields a
`timespec`. Range at int64 ns is ±292 years.

So accepting int64 ns is not an imposition on a ROS 2 or modern-sensor caller —
it *skips* a conversion that a float API would force. Drivers that hand out
floats (Realsense: double milliseconds) have already destroyed the precision
upstream; accepting the float would not recover it, only move the blame.

What users resent is writing `stamp.sec * 10**9 + stamp.nanosec` in every node.
**NORMATIVE — every surface ships exact, total converters, and none of them
takes a float:**

```rust
Stamp::<D>::from_parts(sec: i64, nanos: u32)   // the ROS 2 / timespec shape
Stamp::<D>::from_timespec(ts)
```
```python
tf_tree.from_ros(msg.header.stamp)   # exact; never via to_sec()
```

`from_sec` stays, stays documented as lossy above ~10⁷ s, and stays out of the
examples. It already carries that warning in `tf_tree_py`; what it does not yet
have is an exact sibling to be pointed at, which is what makes the warning
actionable rather than merely true.

> **Amendment — the Rust half ships, and it settles two shapes the other
> surfaces have to mirror.**
>
> ```rust
> Stamp::<D>::from_parts(sec: i64, nanos: u32)          -> Option<Stamp<D>>
> Stamp::<D>::from_timespec(tv_sec: i64, tv_nsec: i64)  -> Option<Stamp<D>>
> ```
>
> **`Option`, because "total" and "exact" are in tension with `Stamp` being a
> bare `i64`.** Two inputs have no correct answer — a `nanos` outside
> `[0, 1e9)`, and a sum outside `i64` — and both of the alternatives are the
> silent wrongness this whole section exists to remove: normalising an
> out-of-range nanosecond field converts a malformed message into a plausible
> stamp, and wrapping the sum hands back a stamp on the other side of the epoch
> that compares, interpolates and prints perfectly. `None` does not distinguish
> them, because a caller rejects the message either way and a `Copy`,
> `String`-free error carrying the distinction would be a new type for a fact no
> consumer branches on (D11).
>
> Note the sum is what is range-checked, not the product: a staged
> `checked_mul` then `checked_add` refuses a one-second band of *representable*
> stamps at the negative end.
>
> **`from_timespec` takes the two fields, not a struct.** `tf_tree_core`'s
> dependency budget is `libm` + `bytemuck` + `blake3`, so there is no
> `libc::timespec` to accept and none may be added; declaring our own `#[repr(C)]`
> copy would be worse, because it is a type the caller then has to convert
> *into* — the conversion the method exists to remove. `time_t` and `long` are
> both `i64` on a 64-bit target, so the call site is
> `Stamp::from_timespec(ts.tv_sec, ts.tv_nsec)` with no cast. It adds exactly one
> refusal over `from_parts`: a negative `tv_nsec`, which POSIX permits only in a
> *relative* interval, so it means an interval is being converted as an instant.
>
> The Python `from_ros` and the C entry point are not built by this and inherit
> the shape: exact, total, no float, and a refusal rather than a normalisation.

### 5.2 The epoch is the hard part, and it is what `Domain` is for

Nanoseconds since *what* — Unix epoch, boot, TAI, a sensor's free-running
counter? Those differ by hours or by uptime, and mixing them yields a transform
that is catastrophically wrong while being perfectly well-formed.

**The constant offset between two domains is not recoverable from the stamps
themselves.** A one-way timestamp carries no information about the offset it was
produced under: any pair `(offset, delay)` and `(offset + δ, delay − δ)` produce
the same observed stamp, so no amount of received data separates them. Recovering
it needs a two-way exchange or an out-of-band declaration — which is exactly what
Phase 8's clock-domain alignment is, and why it reports an uncertainty rather
than a number (D19; `PROJECT.md` §4). Getting the domain wrong is therefore
unfixable after the fact, and that asymmetry is why the domain is a **type** (D9)
and not a convention.

`PHASE4.md` §5.5 already makes the ingest bridge tag edges `SimDomain` or
`SystemDomain` from `use_sim_time`, and makes a domain mismatch a **startup**
failure rather than a first-message one. The mismatch half is implemented
(`TopologyConfig::check_domain`, called before the arena is built) and a config
file now names its domain (§2.5); the `use_sim_time` *derivation* is the clause
that remains, and §5.5's amendment is where it is tracked. **The read side is not yet specified,
and [`PHASE7.md`](./PHASE7.md) §4 J9 specifies it**: a `Buffer` derives its query
domain from the `rcl_clock_type_t` of the clock it was constructed with, so a
node mixing `/clock`-driven sim time with a driver's steady time gets
`TimeDomainMismatch` instead of a tree wrong by however long the bag has been
playing. `tf2` cannot detect this at all.

This is the same shape of argument as multi-publisher detection: a real bug
class in real stacks that the incumbent silently averages over.

### 5.3 `CLOCK_REALTIME` is not monotone, and the failure reads like our bug

NTP steps and leap seconds move `CLOCK_REALTIME` backwards. `PHASE1.md` §2
invariant 6 requires per-edge non-decreasing stamps, so a clock step surfaces as
a burst of `NonMonotonicStamp` rejections — correct behaviour that reads as a
tf_tree defect to whoever meets it at 3 a.m.

**Implementation items:**

1. A `doctor` check that names the cause: a run of rejected pushes on an edge
   whose **domain tag is a wall clock**, reported as a clock step and not as a
   publisher fault. It ships as `TFT019` (`PHASE5.md` §6) — a refinement of
   `TFT018`'s attribution, not a second detector — and gets a row in
   `RUNBOOK.md` under `NonMonotonicStamp`. Per §2.5, the only built-in wall
   clock today is `SystemDomain`; the check keys on the tag and states what it
   cannot yet tell apart rather than inferring it.
2. A documentation line recommending a steady or PTP domain for anything
   published at rate — which was also the argument for §2.5's then-missing
   built-ins, because there was no steady domain to recommend by name. There is
   now: `SteadyDomain`, tag 3.

The online bridge's much harder version of this problem — distinguishing a
`/clock` reset from a publisher's `transform_tolerance` — is settled by
[`0012`](./decisions/0012-the-authoritative-clock-jump-signal-and-the-degradation-ladder.md)
and is not relitigated here. §5.3's check is the single-process, no-ROS case,
where there is no authoritative signal and no second publisher, so the only
honest response is a good diagnostic.

---

## 6. Delta summary

Everything this document adds to what is already specified, in one table. The
**Lands in** column is what makes each row schedulable; nothing here is
authorized by this document alone.

| # | Change | Surface | Where | Lands in |
|---|---|---|---|---|
| 1 | `Tree::claim_owned` → `OwnedWriter`; delete the PyO3 and C ABI lifetime extensions | Rust, Python, C | [`0017`](./decisions/0017-owned-handles-and-the-lifetime-rule.md) | **landed** — `OwnedWriter` plus `0017` steps 6–7; `PyPublisher`, `tft_publisher` and the bridge's writer map all hold one, both `extend_to_static` helpers are deleted, and §2.1's rule is now a description rather than a direction |
| 2 | `Arc<Tree>` documented as the embedding idiom | Rust (docs only) | §2.2 | **landed** — `tf_tree` crate docs; `0017` step 8 keeps only the lifetime rule and the scoped-vs-owned guidance |
| 3 | `#[inline]` on the fold; LTO guidance; a cross-crate bench row gated at 5% | Rust | §2.3 | **all three landed.** `#[inline]`: five placements, measured, a sixth measured as a *pessimization* and left off. LTO guidance: `tf_tree` crate docs. Row: `embedding_cross_crate` in `PHASE5.md` §9.2's artifact (`just embed-cost`) — one build, one profile, `tf_tree_bench` against `tf_tree_core::bench_probe` — gated at 5% on `boundary_ratio`, `out_of_crate_ns` and `in_crate_ns`, and **reporting 1.250–1.254×, i.e. over §9.2's criterion**, with the `lto = "thin"` control at 0.994–0.996× beside it. The two-profile comparison is kept as an exploratory measurement, never gated |
| 4 | `# Stability` headings on CLI-facing exports; then the `unstable` tier itself | Rust | §2.6 | **landed** — `tf_tree::unstable` behind a default-off `unstable` feature whose docs are the waiver. Three items moved off the crate root: `ArenaView`, `EdgeKind`, and `EdgeMeta` (which the audit found was *unusable* from the stable tier — the facade never re-exported the `compile` it is an input to). `Tree::arena_view` is gated with them, because a caller reaches every accessor on the returned value by inference without naming the type. `tf_tree_cli`, `tf_tree_c`, `tf_tree_bench` and `tf_tree_py` turn it on — four, checked against the `[dependencies]` entries by `just stable-tier-check` rather than counted by hand; three `compile_fail,E0432` doctests pin that the root no longer answers. **Gating the door did not remove the capability**: stable `Tree::frames` and `Tree::edges` land with it, mirroring row 8's Python surface, so an embedder never signs the waiver to ask what is in their own tree (§7 check 1). the facade's own default feature set with the feature off is what **no `--workspace` command compiles**, because the resolver unifies the feature in from those four — measured with `cargo tree -f '{p} FEATURES=[{f}]'`, §2.6. `just stable-tier-check` is the gate for it and CI runs it as its own job; since 0.0.1 deleted the facade's self-dev-dependency it is no longer the *only* recipe that reaches the configuration, because every `-p tf_tree` selection now does — `just miri` and `just test-doc-error-codes` included |
| 5 | Per-edge nominal rate reachable from a plan (`Plan::span` already ships) | Rust core | [`0018`](./decisions/0018-blocking-waits-belong-in-the-shim.md) | **landed** — `Plan::slowest_nominal_rate_mhz`, `Guard`-scoped and generation-checked like `span`; `0` means undeclared and is skipped, not treated as slow |
| 6 | No blocking primitive in the arena; the escalation path recorded | all | [`0018`](./decisions/0018-blocking-waits-belong-in-the-shim.md) | recorded, not built |
| 7 | `Layout::QuatTwist`; derivatives reach Python and C | core, Python, C | §3.3 | **landed** — `PHASE5.md` §4.4 item 1 in full: `plan.at(..., layout=...)` and `at_into` serve all four layouts, and both refusals the twist layout adds are typed — `DerivativesUnavailableError` for a `LerpSlerp` edge, `NoSegmentError` for a stamp with no segment. Python's `interp=` default moved to `"sclerp"` (§3), so a Python-built tree answers a twist without one |
| 8 | `tree.frames()`, `tree.edges()`, `plan.edges()` | Python, Rust | §3.2, §2.6 | **landed** — `tf_tree_py`; authorised by `PHASE5.md` §4.4 item 2, which is the *names* half. §4.2's `ds.edges()` statistics stay held back until §3's counting pass, and this row is not them. **Rust followed in row 4's commit**, as stable `Tree::frames` / `Tree::edges`, for §7 check 1's reason; `plan.edges()` has **no** Rust twin, and the gap is real rather than deferred by symmetry: `Plan::steps` gives a Rust caller the `Step::Dyn` edge ids, but nothing on the stable tier turns an `EdgeId` into a name pair — `Tree::edges` answers positionally, exactly as Python's does |
| 9 | `from_parts` / `from_timespec` / `from_ros` | Rust, Python, C | §5.1 | **landed** — Rust (`Stamp::from_parts`, `from_timespec`), Python (`from_parts`, `from_ros`; duck-typed on `.sec`/`.nanosec`, no `rclpy` in the wheel) and C (`tft_stamp_from_parts`, `tft_stamp_from_timespec`, `TFT_ERR_BAD_STAMP`, ABI minor 3 → 4). One refusal table is asserted on both sides of the boundary |
| 10 | `NS_PER_STEP_ESTIMATE` re-derived when `0013` re-baselines | Python | §3.4 | **landed** — 55 → **64 ns/step**, in `0013`'s re-baseline commit as §3.4 requires. `PHASE3.md` §6.1's amendment is the single account of the measurement and of the one element it moves, pinned by a `const` assertion in `tf_tree_py::tree`; nothing here or in §3.4 restates it. `0013`'s two threshold questions stay open — this row is the constant, not the gate |
| 11 | Clock-step `doctor` check (`TFT019`) + runbook row | CLI | §5.3 | **landed, and now reachable on real data** — `tf_tree_cli`; fires only on tag 0 and only on a run of at least 8 consecutive rejected arrivals (a threshold this implementation chose, not one §5.3 states), skips naming the tag otherwise, and does not demote `TFT018`. `doctor` gained two recording sources — `--from-bag <recording.mcap>` and `--from-file <index.tft>` — and `TFT019`/`TFT018` **run on the first and skip on the second**: the skip is re-keyed from liveness onto `checks::PushStream`, because a ring holds only the pushes `SampleRing::push` accepted, so an arena of any kind (live, frozen, or bag-built and §3.1-sorted) would have passed both checks unconditionally. `PHASE5.md` §6's last `TFT019` amendment records that its own predicted fix was the wrong one and why |
| 12 | The shim's query domain from `rcl_clock_type_t` | shim | [`PHASE7.md`](./PHASE7.md) §4 J9 | Phase 7, gated by D21 |
| 13 | `tft_bridge_options::arena_name` + `TFT_ERR_ARENA_UNAVAILABLE` | C | [`0015`](./decisions/0015-the-bridge-fills-a-shared-arena.md) | **landed** — `arena_name` appended under §3.6's `struct_size` prefix rule and `TFT_ERR_ARENA_UNAVAILABLE` added to the frozen header, ABI minor 4 → 5. A NULL `arena_name` is the private heap arena every pre-`0.5` caller already had; a non-NULL one is `tf_tree::Open` with `require_create(true)`, and a shared arena that cannot be had is a startup refusal with **no heap fallback**. An earlier revision of this cell said "the ABI half landed" and that `0015` steps 3–7 were outstanding; all eight of that record's steps have since landed — #139, #141, #142, #143 and step 7's `PHASE4.md` §5.8 half; that cell's own C surface was complete at #141. What is still outstanding there is the `atfork` test its *Invariants to maintain* clause demands and §9.2's N = 1…16 curve for the new benchmark arm — **neither of which is C API surface**, so neither holds this row open |
| 14 | `__version__`, `arena_format_version()`, `arena_layout_hash()` on the Python module | Python | §3.2 | **landed** — `tf_tree_py`. `__version__` is `env!("CARGO_PKG_VERSION")` and never a literal: a hand-copied string is wrong exactly once, on the release where somebody bumps the manifest and not the line, and it is wrong *silently* — a report carrying it is mis-attributed rather than un-attributed, which is worse than having no version at all. The other two re-export the facade's own `arena_format_version` / `arena_layout_hash` under the facade's own names, so no second spelling of an existing path exists (`PROJECT.md` §6) and `tf_tree_arena` does not become a dependency of the binding to answer a diagnostic. All three run at import frequency, and neither of the two calls touches an arena or takes a lock — they bottom out in `tf_tree_arena::FORMAT_VERSION` and in `layout_hash`, which is a `const fn` — so R2 is not in tension. **The two are functions rather than module constants, and that is forced rather than chosen:** `tests/python/test_stubs.py` is what keeps the hand-written `.pyi` from rotting, and it collects the stub's `ClassDef`s and `FunctionDef`s only (`test_stubs.py:36`) while skipping underscore-prefixed names on both sides (`:50`). A module-level `FORMAT_VERSION: int` is an `AnnAssign`, invisible to that comparison — it would be the one name in the surface whose existence nothing checks. `has_shared_memory` is the precedent: a compile-time-constant fact about the build, exposed as a nullary function. `__version__` is exempt by that same underscore skip and keeps the dunder because it is what a bug-report template asks for; it is not the canonical answer — `importlib.metadata.version("transform_tree")` is, it reads `pyproject.toml` where this one reads the crate manifest, and `tests/python/test_version.py` is the only thing that stops the two files drifting |
| 15 | `frame_headroom=` on `tf_tree.build` and `tf_tree.open` | Python | §3.2 | **landed** — `tf_tree_py`. Spare frame-name slots, mirroring `TreeBuilder::frame_headroom`, defaulting to `0` so no existing caller's arena changes size. It is not a `layout=` in R4's sense — R4 governs the *pose* layout, where row-major against column-major and `wxyz` against `xyzw` both produce a valid-looking transform pointing the wrong way; this is an arena-sizing knob beside `capacity=`, which has carried a default since Phase 3. The defect it closes is not ergonomic: a Python-created arena sized `max_frames = unique_frames + 1` can never accept a runtime-interned name from **any** participant, so a Rust, C or ROS-bridge peer calling `Tree::frame()` on it gets `CapacityExceeded` with no way for the creator to have allowed it. No `edge_headroom` beside it — `PHASE5.md` §5.8's amendment records that nothing declares an edge at runtime. Gated by `tests/python/test_errors.py::test_frame_headroom_reaches_the_arena_and_stays_out_of_the_frame_list`, which compares frozen `.tft` sizes (2262912 / 2264320 / 2274048 B at headroom 0 / 8 / 64) with `frames()` identical at each — mutation-verified: dropping `.frame_headroom(...)` makes all three sizes equal and fails the test |
| 16 | `tf_tree_math::slerp` is `pub` | Rust | §2.7 (§2.6's test) | **landed** — the shortest-arc quaternion kernel `LerpSlerp` already evaluates, exported under its own name, re-exported at `tf_tree_math`'s root, and — since the review of the commit that landed this row — re-exported by the **`tf_tree` facade** as well. That last part was missing on arrival and was not cosmetic: the facade already re-exports `LerpSlerp`, so an engine consumer reaching the kernel had to add `tf_tree_math` as a second direct dependency and pin the two in lockstep, which on a `0.0.x` line is worse than the `Iso3` round trip this row exists to delete. `tf_tree/tests/math_reexports.rs` pins that the two paths are one item rather than two functions. The asymmetry closed here is between the two kernels' **visibility** — `dualquat::screw_pow` has been public since the crate's first commit — and *not* between two instances of §2.7's condition, which `screw_pow` does not meet and never needed to; §2.7 says why in its own words. **Visibility only**: the body is unchanged, so nothing on the hot path moves. What it deletes downstream is the `Iso3` prologue — 256 bytes of stack, two isometries written out field by field, a zero translation lerped into another — which LLVM folds away in no configuration measured: 28 x86-64 instructions through the consumer's `nalgebra` adapter (41 against 69) and 45 bare (7 against 52) at `opt-level = 3`, moving to 31 and 48 across four release profiles. **No instruction count here is portable and none is quoted as one.** Two earlier revisions did quote one — `15` against `51`, then "the same 36 either way" — and neither reproduces; the second was also arithmetically false against the counts it cited (`59 − 7` is 52). What reproduces is the sign: the wrapper is never optimized out. Two costs, both documented rather than removed: `LerpSlerp::eval`'s `s == 0` / `s == 1` shortcuts hide an endpoint asymmetry the kernel exposes (`-qb` at `s = 1` under the sign fix, renormalized endpoints inside the `1e-6`-rad fallback band), pinned by `the_iso3_round_trip_it_replaces_agrees_as_a_rotation`; and **`s` outside `[0, 1]` is documented as unsupported rather than refused** — which branch runs is a property of the *pair*, so an extrapolation's accuracy is set by the publish rate (closed form holds `7.2e-15` out to `s = ±20`, the series leaves `1e-15` between `\|s\| ≈ 2.3` and `≈ 5` depending on the angle), and `tf_tree_core` never does it, answering an out-of-window stamp with `ExtrapPolicy` instead. §7's walk, item 8 included, is in §2.7 |
| 17 | `MAX_DEPTH` and `MAX_PATH_EDGES` documented as public surface, and the one-variant/two-bounds rule for `TreeTooDeep` | Rust, Python, C | §2.8, [`0034`](./decisions/0034-the-depth-bound-priced-two-slots-the-same.md) | **landed** — `0034` in full. Neither constant appeared in this document before, which is how one of them came to be *documented* as bounding the compiled plan while being *enforced* on the raw walk. `MAX_DEPTH` 16 → 32 and a new `MAX_PATH_EDGES` = 64; both are `pub const` on five published crates, so this is a semver-relevant change to a value **and** to a meaning. The values are chosen against a survey of 91 real robot descriptions rather than against the retired "real trees are 4–8": the binding quantity is the graph *diameter* (max 30, p95 24), not root-to-leaf depth, and a survey that measured depth would have concluded 24 was plenty. `0034`'s own rationale (A) — that raising `MAX_DEPTH` makes "everyone pay, on the hot path" — is **reversed by measurement, not by preference**: every `lookup`/`at`/`at_many` row is flat within ±2% at 32 and the plan-cache-hit path within +0.5%. What does move is `Tree::plan` on a cache miss, and the same change reduces it — three interleaved arms, n = 30, `taskset -c 2`: 166.2 ns at HEAD, **361.9 ns at `MAX_DEPTH = 32` alone (+114%)**, **308.6 ns with the array deletion (+83.5%)**, the deletion buying back −14.1% unanimously (0/30). The one thing R5 gains is a *rule*: two bounds, one status code, `depth`'s two ranges disjoint by construction, and the remedy sentence binding-specific — Rust names `static_edge`, Python must not (it cannot declare one), C names no macro (`TFT_MAX_DEPTH` was referenced for two phases and defined nowhere, and is **still** not defined, because `0034` split the quantity it was vaguely about into two) |

**Fifteen of seventeen rows have landed in full: 1, 2, 3, 4, 5, 7, 8, 9, 10, 11,
13, 14, 15, 16 and 17.** The two that have not are 6 and 12, and neither is merely
unscheduled: row 6 is recorded-not-built on purpose and row 12 is gated by D21.
Two of the fifteen that landed — 3 and 10 — carry a caveat worth keeping, so they are in the
list below as well. (This count is re-taken from the table above rather than
carried forward, every time it changes. It has been wrong twice: an early
revision said "ten … 1, 2, 5, 7, 8, 9 and 11 in full, 3 in part", which named
eight rows and called them ten; the revision after that kept row 13 out of the
count after its record's remaining steps had landed.)

- **Row 3 has landed in full, and its benchmark row reports a failing gate.**
  `embedding_cross_crate` measures **1.250–1.254×** against §9.2's 5% criterion.
  That is the row working — it was built to report what it finds — but "landed"
  here means the row exists, not that the number passes. What would close it is
  `API.md` §2.3 item 2's LTO guidance in the *embedder's* profile, and the
  `lto = "thin"` control at 0.994–0.996× is that stated as a measurement.
- **Row 6** is recorded, not built, and is meant to stay that way
  ([`0018`](./decisions/0018-blocking-waits-belong-in-the-shim.md)).
- **Row 10 has landed, but `0013` has not.** The constant moved (55 → 64
  ns/step) because §3.4 is NORMATIVE that it moves with the measurement. The
  record it came from is still `draft`: its threshold questions are a policy
  call, and a fourth question — which *call shape* a latency budget is written
  against — was opened by the measurement itself.
- **Row 12** is gated by D21 and must not be started before `PHASE7.md` §0.0's
  four gates are met.

**Row 7's Python half brought two things this section did not anticipate**,
recorded here because the next reader will meet them. The `layout=` parameter is
keyword-only on `at`/`at_into`, per `PHASE3.md` §4.2 — but §4.2 also asks for a
measurement of what that keyword costs the caller who does not pass one, and
**that measurement does not exist**: the A/B was attempted and this host's
run-to-run spread on a single binary swamped any plausible effect. It is owed.
(§4.2's *other* NORMATIVE ask on that line — verify PyO3 really emits
`METH_FASTCALL` rather than assuming it — is now done: `at`, `at_into` and
`push` carry `METH_FASTCALL | METH_KEYWORDS` and `latest` carries `METH_NOARGS`,
read out of `PyMethodDef::ml_flags` by a test on both interpreters.)
And a Python-built tree could not answer a twist at all, because
`tf_tree.build` hard-coded `LerpSlerp` — so `build` and `open(create=...)` gained
an `interp=` keyword, spelled as `PHASE3.md` §4.1's own sketch spells it. **The
default then moved to `"sclerp"`**, closing a divergence from Rust that D5
forbids without a measurement nobody ever took; §3 records it. That does change
the numbers a caller who passed no `interp=` was getting, which is why it is a
one-line break taken before a published tag rather than a divergence kept after
one.

---

## 7. The check a new surface has to pass

Applied to the shim in `PHASE7.md` §7, and to anything after it.

1. **Tiers.** Are all three reachable? Is the collapsed convenience visibly the
   collapsed one, and does it go through the plan cache? Is there a documented
   way *down* to tier 2 from whatever the surface's idiomatic call is?
2. **Hot tier.** Does the evaluate path allocate, lock, resolve a name or
   convert? Is there an `_into` form?
3. **Time.** Integer nanoseconds end to end? Is there any path where a float
   round trip can occur, including inside a message type the surface accepts?
   Is the domain derived from something the caller already holds, or does the
   caller have to remember it?
4. **Layout.** Explicit, with no silently-wrong default? Chosen by type where
   the language allows?
5. **Errors.** Typed, `Copy`, prose separate? Does any documentation invite a
   caller to match on message text?
6. **Writability.** What does a caller who did not ask to write get? Is it
   enforced by something stronger than our own care?
7. **Lifetimes** (Rust, and anything embedding Rust). Does the surface hand out
   a type carrying a lifetime that a user will want to store?
8. **Losses.** Does the benchmark table have a row where this surface is
   *worse* than the alternative it replaces? If not, it is not finished.

## 8. The real-time envelope — NORMATIVE

The project's one-line pitch is *"fast enough to sit inside a control loop"*, and
until this section existed **nothing stated what that means**. A mean latency does
not answer it. A control loop is a deadline, so what it needs is the worst case
and the list of things that cannot happen on the query path — and it needs each
claim attached to whatever re-derives it, per `docs/benchmarks/EVIDENCE.md`'s
rule that a maintained claim owes an executor.

### 8.1 What the query path does not do

For `Plan::at`, `Plan::at_many_into`, `Plan::at_with_derivatives` and
`Plan::at_extrapolating`, evaluated under a `Guard` the caller already holds:

| Does not | Why, and what checks it |
|---|---|
| **Allocate** | The plan is a fixed `[Step; MAX_DEPTH]` by value and every batch form has an `_into` writing into caller memory (R2). Checked: `crates/tf_tree_bench/tests/zero_alloc.rs` counts allocations through a wrapping global allocator across a lookup loop, and again over a 1537-frame tree across ring wraparound |
| **Take a lock** | Reads are seqlock reads. A reader never blocks a writer and a writer never waits for a reader; there is no mutex on the path at any depth |
| **Read a clock** | `tf_tree_core` is `no_std` and has no clock to read. The query's stamp is the caller's, always (R3) |
| **Resolve a name** | Frames are interned to integer ids at compile time (R1, D3). No hashing, no string comparison, no arena name-store access |
| **Make a syscall** | Nothing above needs one. The arena is already mapped; evaluation touches that mapping and nothing else |
| **Branch on the transport** | The same code runs against a heap arena, a `MAP_SHARED` memfd and a frozen `.tft`; `docs/PHASE5.md` §2.1 makes that NORMATIVE and the relocation gate tests it |

### 8.2 The worst case is bounded, and here is the bound

**A reader that meets a slot mid-write retries `SEQ_RETRY_LIMIT` (64) times and
then returns `LookupError::SlotContended`.** It does not spin indefinitely and it
does not block. That is the property that makes the read path usable from a
`SCHED_FIFO` thread: a writer preempted inside its two-store publish window
cannot hold a higher-priority reader past a fixed bound, so there is no unbounded
priority inversion to reason about. The reader is handed an error naming the
edge, and deciding what a control loop does about a contended slot is the
caller's — which is the point of returning rather than waiting.

`docs/decisions/0018` is the same principle stated for waits: no blocking wait,
futex or notification primitive lives in the arena.

**What is *not* on this path, and must not be put there:** `Tree::lookup`
resolves names and consults the plan cache (tier 1 — R1 says so, and it is never
the example in a hot loop); `Tree::reparent` takes the topology lock with a
bounded spin (A2, `0029`); `Publisher::push` reads the wall clock on a countdown
(`0036`'s receipt-time sampler, every `sample_every` pushes — priced at ~1 ns
amortised by `just push-sampler-cost`). None of the three is a query.

### 8.3 Page faults are the residual, and they are the embedder's to remove

The arena is a `memfd`. An untouched page costs a minor fault on first touch,
which inside a control cycle is a deadline miss rather than a slowdown. Two
things address it and a third does not exist:

- **Per-edge population at take-up** (`docs/PHASE2.md` §7.1, `0024`) faults the
  pages an edge uses when the edge is claimed, not when it is first read.
- **`mlockall(MCL_CURRENT | MCL_FUTURE)` in the embedding process** is what pins
  the mapping, and it is the *application's* call, not this library's — a library
  that locks memory on its caller's behalf is deciding an `RLIMIT_MEMLOCK` budget
  it cannot see. `TFT016` reports the limit against the arena size so the failure
  is found before the control loop meets it.
- **There is no `LockPolicy` and no `mlock` call in this codebase.** `MLOCK_ONFAULT`
  would not prefault, so it adds nothing over §7.1; and on a swapless host — which
  is what a real-time robot runs — the pages are not reclaimable anyway.

### 8.4 What this section does not claim

Stated because the rest of it reads like a guarantee and only part of it is one:

- **No number here is a latency guarantee.** `docs/PHASE1.md` §11.3's latency
  criteria need dedicated core-pinned hardware and are recorded UNAVAILABLE on
  every host this project has measured on. The published figures in
  `docs/benchmarks/` are medians on a shared-tenancy VM.
- **"No syscall" and "no lock" are read from the code, not enforced by a test.**
  Only the allocation claim has an executor (`crates/tf_tree_bench/tests/zero_alloc.rs`).
  A test that asserted the other two would be worth having and does not exist.
- **The tail now has a reading, and a reading is not a gate.** `just control-loop`
  runs `crates/tf_tree/examples/control_loop.rs` — two queries under one guard at
  1 kHz against a 200 Hz estimate, under a concurrent writer — and reports p50 /
  p99 / p99.9 / max. It exists because this section previously stated an envelope
  that nothing executed at all. It is *not* §11.3's criterion: the host is
  unpinned, there is no real-time scheduler, and two clock reads bracket a
  sub-microsecond operation, so every one of those inflates the result. Read it
  for shape, and read §11.3 for the number.
- **`PHASE4.md` §1's operational exit criterion is still open**: no node has run
  this on real hardware for two weeks. Every claim above is a claim about the
  code, and none of them is that claim.
