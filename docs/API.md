# tf_tree — the API contract

> **Companions:** [`PROJECT.md`](./PROJECT.md) (decision log D1–D22),
> [`PHASE1.md`](./PHASE1.md)–[`PHASE5.md`](./PHASE5.md) (per-phase specs),
> [`PHASE7.md`](./PHASE7.md) (the `tf2`-shaped shim, gated by D21).

**What this document is.** The rules that generate every binding, and the
normative surface of each one. The phase specs say *what gets built when*; this
says *what shape it has to have* and **what may not be added.** Sections marked
**NORMATIVE** are requirements.

**Status.** Ready. §1–§5 describe surfaces that exist and the deltas §6 lists;
§7 is the check any new surface has to pass. This document is not a phase: it
schedules nothing, and every row in §6 lands inside a phase or a decision record.

---

## 1. The six rules

Every design question is answered by one of these. A question none of them
answers is a decision record, not an API choice.

### R1 — Three tiers, always: attach → compile → evaluate

`open`/`build` once, `plan` once per `(target, source)`, `at` in the loop. Every
binding exposes all three tiers (D3). Every surface, including the shim, must
offer a way down the ladder.

A convenience that collapses tiers (`Tree::lookup`, `tf_tree.lookup`) is allowed
on two conditions, both **NORMATIVE**:

1. It goes through the plan cache. It does **not** re-resolve topology per call.
2. It is visibly the collapsed one: named differently, documented as paying a
   cache probe, and never the example in the README's hot loop.

`Tree::lookup`'s per-thread cache is keyed `(arena, target, source, generation)`
and is genuinely `thread_local!` (`PHASE3.md` §7.2), not a shared map behind a
lock. The `arena` component is required: `FrameId`s are handed out in interning
order and a built tree's generation is its edge count, so without it a second
tree is served the first tree's plan. Two handles onto one *shared segment*
deliberately share an id.

A slot holds the **result** of compiling its key, and a refusal is a result
(#259): a pair that cannot be planned is compiled at most once per key. A
refusal is filed only when the topology did not move under the compile that
produced it, and the generation component retires refusals and plans together.
Errors raised *after* compilation — `NoData`, `Extrapolation`, `SlotRecycled`,
`TimeDomainMismatch`, anything describing the sample history or the query stamp —
are **not** cached.

### R2 — The hot tier never allocates, never locks, never converts

This is the test for **which type a method goes on**: an operation that
allocates, locks, resolves a name or converts a representation belongs on `Tree`
(tier 1) or the compile step (tier 2), not on `Plan`.

Corollary, **NORMATIVE**: every batch entry point has an `_into` form writing
into caller memory. The allocation is a flat ~270 ns, half the call at n = 64,
and n = 64 is the control loop.

### R3 — Time is integer nanoseconds carrying a domain

No float, no seconds keyword, no convenience overload, on any surface
(`PHASE3.md` §3). §5 covers what that means at each boundary.

### R4 — Memory layout is stated, never inferred

`Layout` / `tft_layout` / `layout=` are explicit with no default that could be
silently wrong: row-major versus column-major differ by a transpose (a rotation's
inverse), `wxyz` versus `xyzw` is a different unit quaternion, and both give a
valid-looking transform pointing the wrong way (`PHASE4.md` §3.5). In C++ the
layout is chosen **by type** (`layout_of<T>`); any future typed binding does the
same.

### R5 — Errors are identifiers; prose is a separate layer

Error types stay `Copy`, `String`-free and `no_std` (D11). Name resolution
against the arena is `Described`, a `Display` wrapper, not a field. That is D11's
rule for a Rust error; it does not reach a binding's own exception, so a Python
attribute holding a resolved name puts no field on a Rust type
([`0058`](./decisions/0058-the-fields-a-python-exception-only-printed.md) §7).
Across FFI the *code* is the contract and the message a diagnostic.

Three layers ([`0040`](./decisions/0040-the-error-that-cannot-be-returned.md);
[`0059`](./decisions/0059-the-arena-errors-that-cannot-describe-themselves.md) for
`ShmError`, `FrozenError`, `LayoutError`, `ParticipantError`):

| Layer | Knows | Says |
|---|---|---|
| the type and its discriminant | nothing | the contract — what a caller matches on, in Rust and across FFI |
| `Display` on the error itself | what the error carries | `edge 3: stamp 5 ns is outside its window [1, 4] ns` |
| `Described`, from `Tree::describe` | the arena | `odom -> base_link: ...`, plus a sample of the frames that do exist |

The middle layer lets an error leave a function (`core::error::Error` requires
`Display`). It resolves no names, and `Described` delegates to it rather than
restating.

**NORMATIVE for every surface, including the shim:** exception and error *types*
are a compatibility promise; message *text* is not, and no surface may document
text a caller could match on.

### R6 — Read-only by default for anything that did not ask to write

Python defaults to `mode="ro"`; `tft_tree_open` attaches read-only; a frozen
`.tft` is read-only permanently. A `PROT_READ` mapping delivers `SIGSEGV` on a
`compare_exchange`, so every mutating entry point consults `is_writable()` first
and returns a typed error (D18). A binding may make writing easier; it may not
make it the default.

---

## 2. Rust — the embedding surface

The Rust API is the one every other surface projects. Its distinct requirement is
**embedding**: a user's node or library holds tf_tree types inside their own
types for the life of their process.

### 2.1 The lifetime rule — NORMATIVE

> **No type a user stores in their own struct may carry a lifetime.**

| Type | Storable? | |
|---|---|---|
| `Plan` | ✅ | `Copy + Send + Sync + 'static` |
| `Tree` | ✅ | via `Arc<Tree>` — §2.2 |
| `Guard<'a>` | ✅ | per-batch, stack-only, never stored |
| `EdgeWriter<'a>` | ❌ | fixed by [`0017`](./decisions/0017-owned-handles-and-the-lifetime-rule.md) |

[`0017`](./decisions/0017-owned-handles-and-the-lifetime-rule.md) (fully
implemented) adds `Tree::claim_owned` → `OwnedWriter`, the workspace's **only**
lifetime extension; a second one is a new decision record. `EdgeWriter<'a>`
stays: a scoped claim the borrow checker enforces is better when it fits.

### 2.2 `Tree` is not `Clone`, and `Arc<Tree>` is the idiom — NORMATIVE (doc)

`Tree` owns its arena backing and holds a registered participant slot (64 by
default, `DEFAULT_MAX_PARTICIPANTS`); a derived `Clone` would burn a second slot
or lie about sharing one. `tf_tree_c` refcounts an `Arc<TreeShare>` wrapping an
`Arc<Tree>` — two refcounts, not redundant: the outer is the *handle* refcount a
`tft_tree` shares with every `tft_plan` compiled from it (free in any order), the
inner the *arena* refcount `Tree::claim_owned` (`self: &Arc<Tree>`) takes and a
publisher holds after every handle is gone. PyO3 holds both shapes (`Py<PyTree>`,
`PyTree::inner`).

### 2.3 Cross-crate inlining is part of the zero-cost claim

A depth-3 interpolating lookup costs ~193 ns
([`0013`](./decisions/0013-the-benchmark-gate-never-interpolated.md)) and
`Plan::at` sits across a crate boundary from every consumer.

**NORMATIVE:**

1. `#[inline]` on `Plan::at`, the fold step `fold_at`, the `Guard` sampling entry
   points (`sample`, `sample_hinted`, `sample_from`) and the `Iso3` operators.
   Deliberately not marked: `fold_at_with_derivatives`, `fold_latest`,
   `fold_latest_common` (large, off the measured hot path) and `fold_at_cursors`
   (the batch fold; measured a pessimization, 328 vs 285 ns/elem at the embedder
   profile).
2. Crate docs state `lto = "thin"`, `codegen-units = 1` for embedders. This
   workspace's `[profile.release]` sets both; **an embedder's profile is not
   ours**. A placement invisible under LTO can cost 6–11% without it
   ([`0060`](./decisions/0060-the-batch-fold-that-reads-before-it-interpolates.md) Decision A), and
   `#[inline(always)]` on `SampleRing::sample_from` is faster only where the
   profile is already ours, so it is declined.
3. A benchmark row measures the **facade path from a separate crate** against the
   in-crate path, gated at 5% — the gate `PHASE4.md` §7 applies to the C ABI.

Landed: item 2 is a section of `tf_tree`'s crate docs; item 3 is
`crates/tf_tree_bench/src/embed.rs`, the `embed_cost` binary, `just embed-cost`
and `PHASE5.md` §9.2's row `embedding_cross_crate` (in-crate half:
`tf_tree_core::bench_probe`, behind the default-off `bench-probe` feature; read
at `[profile.embedder]`; `boundary_ratio`, `out_of_crate_ns` and `in_crate_ns`
are all gated at 5%).

**Price of item 1.** Caller code size grows ~15× (106 B → 1565 B) at every
scalar embedder call site. `Plan::at` is generic, so its MIR already crossed the
boundary; the attribute changes LLVM's `inlinehint` on the non-generic links, so
marking fewer leaves a real call in the middle. `lto = "thin"` does not subsume
the hint (~4.5% on this workspace's own profile).

**Amendment (2026-09-06) — item 3's row has had no independent variable since
2026-08-29.** `Plan::at_tagged` (`0038`) was interposed between `Plan::at` and
`fold_at` carrying no `#[inline]`, so both columns compile to the same call stub
around one out-of-line symbol in `tf_tree_core` (both bodies the same 58 bytes),
the quotient is 1.0 by construction and `Verdict::Over` is unreachable. Before
the interposition the row read 1.250–1.254× at `lto = false`,
`codegen-units = 16` (0.994–0.996× under thin LTO); marking `at_tagged` restores
1.243× but grows every embedder call site ~10×, and the one instruction
measurement taken of it is negative (see `Plan::at_tagged`'s doc comment). The
recipe therefore:

- checks structurally that the two symbols' sizes differ, and **refuses when a
  symbol is absent** rather than passing;
- prints the diagnosis on every run, and refuses unless
  `EMBED_COST_KNOWN_COLLAPSED=1` is set (CI sets it; restoring the variable
  deletes both the `env:` block and the recipe's escape branch).

The row's committed baseline status is `unavailable`. Restoring the variable is a
trade between item 1's placement list and item 3's ability to fail, and needs
`just embed-cost` plus `just bench-ab` on a host whose `fair_for_timing` is true
([`0013`](./decisions/0013-the-benchmark-gate-never-interpolated.md) step 6).

### 2.4 Two things that are not going to exist

**No `trait TransformSource`.** `TreeBuilder::build()` **is** the test double: a
real engine over a heap arena. A trait buys mockability and costs the
devirtualized hot path plus an untested second implementation.

**No blocking wait in the core** ([`0018`](./decisions/0018-blocking-waits-belong-in-the-shim.md)).
Core has `Plan::span` and the per-edge nominal rate; the waiting lives in the
caller.

### 2.5 Domains are an open trait

`Domain` stays implementable by users — a PTP-disciplined driver declares
`struct PtpDomain;` and picks a free `TAG`. A closed set collapses everything to
`SystemDomain` and `TimeDomainMismatch` never fires for the people who need it.
See §5.2.

**The built-in set is four; tags `0`–`3` are reserved for it:** `SystemDomain`
(0), `SensorDomain` (1), `SimDomain` (2), `SteadyDomain` (3). User domains pick a
tag from `4` upwards. All live in `tf_tree_core::plan`, re-exported through the
facade; each is a unit struct and a `TAG`. Two built-ins were too close to a
closed set: a sim deployment and a steady-clock driver both landed on tag 0 and
`TimeDomainMismatch` never fired for them.

**A tag is a permanent choice.** It is written into `EdgeRecord::domain` (an
existing `u8`; neither `FORMAT_VERSION` nor `layout_hash` moved) and read by every
consumer and recording on disk, so re-numbering silently re-interprets all of
them. A test pins the four values.

`tf_tree_bridge::config::parse_domain` maps all four names onto their `TAG`, so a
topology file says `domain = "sim"`; `TopologyConfig::default_domain` stays a `u8`
because user tags from 4 have no name. Still open: nothing derives the bridge's
tag from `use_sim_time` (`PHASE4.md` §5.5's amendment). `PHASE5.md` §6's `TFT019`
keys on the tag and fires only on tag 0, which is correct: a `SteadyDomain` edge
cannot have stepped. Teaching it that tag 3 is provably steady is a refinement
not yet made.

The set is uniformly `*Domain`; `SimTime`/`SystemTime` in older `PHASE4.md` /
`PHASE7.md` text refer to ROS's `use_sim_time` concept, not to a type.

### 2.6 Stability tiering — built; the split *is* the promise — NORMATIVE

C has `tf_tree.h` and `tf_tree_unstable.h`; Rust mirrors it as `tf_tree::unstable`
behind a default-off `unstable` Cargo feature whose documentation *is* the waiver.

**The rule for what goes there is *does its shape follow the arena layout*, not
"is it low-level".** `PHASE5.md` §1 changes that layout on purpose, so anything
shaped by it is scheduled to move. `Plan`, `Guard` and `Stamp` are stable because
their shape is the engine's contract rather than the arena's. Moved: `ArenaView`,
`EdgeKind`, `EdgeMeta` (unusable from the stable tier, being an input to a
`tf_tree_core::compile` the facade never re-exported). `Tree::arena_view` is gated
with them because it is the door.

**Gating the door must not remove the capability — NORMATIVE.** §7 check 1 asks
whether all three tiers are reachable, and "what is in this tree" is a tier-1
question; Python answers it with `tree.frames()`, `tree.edges()`, `plan.edges()`.
So the facade carries **stable `Tree::frames` and `Tree::edges`** (names only;
§4.2's statistics half of `PHASE5.md` waits for §3's counting pass). `unstable`
gates only the arena-shaped spelling.

**The consumer list is checked.** Every crate that turns the feature on may break
at a patch release. `just stable-tier-check` reads the list from the
`[dependencies]` entries and fails if §6 row 4 or `crates/tf_tree/Cargo.toml`
disagrees by name (`[dev-dependencies]` counted separately).

**A tier nothing compiles is not a tier.** `cargo build --workspace` unifies
`unstable` in from the crates that ask for it, so the default configuration a
`cargo add tf_tree` consumer gets is invisible to it (`cargo tree -p tf_tree -f
'{p} FEATURES=[{f}]' --depth 0` prints `FEATURES=[counters,default]`). Every
`-p tf_tree` selection resolves to that set. `just stable-tier-check` remains the
gate, run by CI as its own job: it names the configuration on purpose, adds the
`shm` and no-default-features variants, and renders the tier's own rustdoc (which
`just doc`, at `--all-features`, cannot: a link into `tf_tree::unstable` resolves
there and not for a published consumer). A recipe that reaches a configuration on
its way somewhere else is not a gate for it. The check compares only the consumer
*names* — it greps row 4 for each of `tf_tree_bench`, `tf_tree_c`, `tf_tree_cli`
and `tf_tree_py` — and asserts nothing about the prose around them.

### 2.7 A kernel the engine already runs is public on its own terms — NORMATIVE

`dualquat::screw_pow` (`ScLerp`'s kernel) has been `pub` since the first commit
while `slerp` (`LerpSlerp`'s) was private, so a caller wanting rotation-only
interpolation built two `Iso3` with throwaway zero translations.

**The rule:** a kernel this crate already evaluates on the hot path is `pub` when
reaching it through its wrapper would make the caller **manufacture a value of a
type their problem does not contain**. A `slerp` caller holds two quaternions and
must invent a *translation*; a `screw_pow` caller already holds a relative
transform. `screw_pow` establishes only the narrower claim this section rests on:
**a policy's kernel is stable-tier public API, not a private detail of the
policy.**

It is safe to promise by §2.6's test: `(Quat, Quat, f64) -> Quat` follows no
arena layout and `PHASE5.md` §1 does not schedule it to move. `tf_tree_math` has
no `unstable` feature, so this is a statement, not a mechanism. It is a section
and not a record because §2.6 decides the tier and R2 is satisfied (the function
**is** the hot path); a section may authorise a §6 row, and the row still has to
land somewhere.

**§7, walked:**

1. **Tiers.** Takes two poses the caller holds, never a `(target, source)`;
   a rung *below* tier 3, not a fourth lookup.
2. **Hot tier.** No allocation, lock, name resolution or conversion; `#[inline]`;
   the body is untouched, so `Plan::at` is unchanged. No batch form, so no
   `_into`.
3. **Time.** No stamp; `s` is a dimensionless fraction.
4. **Layout.** `Quat` is `[w, x, y, z]`, scalar-first, stated in the function's
   `# Storage order` heading with the Eigen/`nalgebra` transposition named.
5. **Errors.** Cannot fail. Both inputs must be unit; a `NaN` `s` propagates
   through every branch except the numerically-identical early return, which
   answers `qa` for any `s`.
6. **Writability.** Pure function of `Copy` values.
7. **Lifetimes.** By value in and out.
8. **Losses.** No benchmark row: the static instruction count of the `Iso3`
   round trip already moves ~10% between two `opt-level = 3` builds, a gate
   whose denominator moves more than its signal
   ([`0023`](./decisions/0023-the-gate-that-could-not-gate.md)). The loss is
   behavioural: at `s = 0` and `s = 1` the direct call is **worse** than the
   round trip, because `LerpSlerp::eval`'s endpoint shortcuts hide two things
   the kernel does not (`-qb` at `s = 1` under the sign fix; renormalized
   endpoints inside the `1e-6`-rad fallback band). Discharged by the
   differential test `the_iso3_round_trip_it_replaces_agrees_as_a_rotation`.
   **A math primitive answers check 8 with a differential; a binding with a
   benchmark. The check binds either way.**

**The `tf_tree` facade re-exports `slerp`** (`pub use`, so checks 2–8 are
unchanged): otherwise a consumer holds two direct dependencies to pin in
lockstep on a `0.0.x` line. `tf_tree/tests/math_reexports.rs` pins that
`tf_tree::slerp` **is** `tf_tree_math::slerp`. The cost is this project's: the
name joins the facade's semver promise. `screw_pow` deliberately does **not**
follow it — a bare `screw_pow` at the facade root would be a second spelling of
`tf_tree_math::dualquat::screw_pow` (`PROJECT.md` §6).

### 2.8 The two path bounds are public, and each prices a different slot — NORMATIVE (doc)

`tf_tree::MAX_DEPTH` and `tf_tree::MAX_PATH_EDGES` are `pub const` on the stable
tier, so each is a semver promise about a *value*
([`0034`](./decisions/0034-the-depth-bound-priced-two-slots-the-same.md)).

| constant | bounds | a slot costs | value |
|---|---|---|---|
| `MAX_DEPTH` | the **compiled** plan — `Plan`'s `[Step; MAX_DEPTH]`, counted after folding | **64 B** per `Step` since [`0042`](./decisions/0042-the-cacheline-the-arena-never-asked-for.md) (`Plan` 2064 B) | **32** |
| `MAX_PATH_EDGES` | the **raw walk** — edges visited on both sides of the common ancestor | **4 B**, in `compile`'s stack frame, off the hot path (D3) | **64** |

**A binding may not invent a third bound and may not hide these two.** Checking a
raw edge count against `MAX_DEPTH` (as `crates/tf_tree_bench/src/workload.rs` once
did) refuses a long rigid chain that compiles to three steps.

**R5 and the one error variant.** Both overruns are `LookupError::TreeTooDeep`,
because the C ABI's `tft_status` table is frozen; the `depth` field separates
them and its two ranges are disjoint by construction. The prose layer differs per
binding on purpose: **Rust's** message names `TreeBuilder::static_edge` as the
remedy, **Python's** must not (`tf_tree.build` declares every edge dynamic), and
**C's** header names no macro (`TFT_MAX_DEPTH` is defined nowhere).

---

## 3. Python — mirror, plus conveniences that pay for themselves

The Python surface mirrors §1's three tiers exactly (`open`/`build` → `plan` →
`at`). Divergences from Rust are deliberate and few: `mode="ro"` and
no-creation-by-default (R6), and scalar/array dispatch on `at`. The default
`interp=` is `"sclerp"`, matching `TreeBuilder`: D5 forbids making `LerpSlerp`
(left- but not right-invariant) the default without a measurement, and
`interp="lerpslerp"` stays for bit-compatible differential testing against `tf2`.

### 3.1 Still refused — NORMATIVE

Float stamps; `asyncio`; any view into the arena; `pickle` of `Tree`/`Plan`/
`Publisher`; keyword arguments on `at`, `at_into`, `latest`, `push`
(`METH_FASTCALL` is 29 ns cheaper, ~15% of a depth-3 lookup); any logic with a
branch in it that could live in Rust; **`scipy.spatial.transform.Rotation`
interop** (it pulls scipy into a NumPy-only wheel, and `layout="quat"` already
produces what `Rotation.from_quat` wants modulo coefficient order — a
documentation line, not a dependency).

### 3.2 Accepted conveniences

All run at tier 1 or tier 2 frequency, so R2 is not in tension:

- scalar/array dispatch on `at`; context managers; `__repr__`; the hand-written
  `.pyi` and `py.typed`
- `from_sec` / `from_datetime` / `now` / **`from_ros`** (§5.1)
- **introspection: `tree.frames()`, `tree.edges()`, `plan.edges()`** — names
  only; `plan.depth()` and `tree.span()` already ship, and `tree.edges()` is the
  names half of `PHASE5.md` §4.2's `ds.edges()`.
- **build identity: `__version__`, `arena_format_version()`,
  `arena_layout_hash()`.** Three values because they fail independently: the
  right version can still refuse an arena written by a different *geometry*. The
  last two are the words every participant compares on attach (`PHASE5.md` §1),
  re-exported from the facade; see `crates/tf_tree_py/src/lib.rs`'s crate docs
  (*Build identity*).
- **arena headroom: `frame_headroom=` on `build` / `open`.** Spare frame-name
  slots, `TreeBuilder::frame_headroom` under the same name, default `0`. A sizing
  knob beside `capacity=`, not a layout in R4's sense. Without it a
  Python-created arena admits no runtime-interned frame name from any
  participant.

### 3.3 Parity deltas to close

| Gap | Where | Disposition |
|---|---|---|
| `at_with_derivatives` absent from Python | Rust and C have it since Phase 4 (`tft_plan_at_with_derivatives`, unstable tier); `PHASE4.md` §0 scoped Python out | **Phase 5**, as `Layout::QuatTwist` — see below |
| `Publisher` holds an extended borrow by hand | `tf_tree_py/src/tree.rs` | [`0017`](./decisions/0017-owned-handles-and-the-lifetime-rule.md) |
| ~~`at_extrapolating` takes no `layout=` and has no `_into` form~~ | `tf_tree_py` | **Closed.** `at_extrapolating(.., layout=)` and `at_extrapolating_into(stamps, policy, poses, by_ns, layout=)` ship. `quat_twist` is refused, as in C: there is no extrapolating `at_with_derivatives` |
| ~~Python cannot declare a static edge, a per-edge capacity, a rate, or a domain~~ | `tf_tree_py` | **Closed by [`0041`](./decisions/0041-python-declares-a-topology-the-way-everything-else-does.md)**: `build`'s `edges` and `open`'s `create` accept the text of the topology config `ros/tf_tree_ros` and the CLI already read — one schema, three consumers |

**`at_with_derivatives` ships as a layout, not a method.** `Layout::QuatTwist` is
a contiguous `(N, 13)` write of `[qw qx qy qz tx ty tz | ωx ωy ωz vx vy vz]`: one
core enum variant beside `Mat4`/`Quat`/`Affine32`, which the layout dispatch
carries to Python *and* C, where a separate method would need its own GIL
threshold, validation and tests for the same bytes.

*C ABI impact:* a new `tft_layout` enumerator is a **minor** bump under
`PHASE4.md` §3.6; no `struct_size` change. `TFT_TWIST_BYTES` keeps its meaning.

*Refusal path:* `LerpSlerp` has no exact twist, so `Layout::QuatTwist` returns
`DerivativesUnavailable` there (`PHASE4.md` §2) rather than a finite difference —
a layout that silently changes meaning per interpolator is R4's failure on the
time axis.

### 3.4 The GIL threshold constant is calibrated against a benchmark that never ran

`NS_PER_STEP_ESTIMATE` is **64 ns/step**
([`0013`](./decisions/0013-the-benchmark-gate-never-interpolated.md)'s
re-baseline; the earlier 55 came from an on-grid benchmark where `I::eval` never
ran). [`PHASE3.md`](./PHASE3.md) §6.1's amendment is the **single account** of the
measurement and of the one element it moves, checked by a `const` assertion in
`tf_tree_py::tree`.

**NORMATIVE:** when `0013` re-baselines, `NS_PER_STEP_ESTIMATE` is re-derived in
the same commit and `PHASE3.md` §6.1 gains a line saying which measurement it
came from. §6.1 is the only place the derivation is written.

---

## 4. C and C++

Specified in `PHASE4.md` §3–§4 and implemented; restated only where a rule binds
other surfaces.

**Two tiers of header, and the split is the stability promise.** `tf_tree.h` is
semver'd; `tf_tree_unstable.h` is opt-in by macro and promises nothing. §2.6 is
the Rust mirror.

**Every struct passed by pointer begins with `uint32_t struct_size`**, so fields
append without a major bump and the callee rejects sizes it does not know.

**The C++ wrapper contains no logic.** It is inline in the user's translation
unit, invisible to the Rust test suite, Miri and ASan. Anything that can be wrong
there must be a `static_assert`, not a runtime branch; any future header-only
surface inherits this.

**Layout by type** (`layout_of<T>`, `raw_writable<T>`) is R4's strongest form.

### 4.1 The three capabilities the bindings could not reach — NORMATIVE (doc)

Each was implemented in the engine and callable from Rust only. Their shapes
generalise, which makes "is this reachable from C and Python?" a question §7 has
to ask.

**The time domain** ([`0038`](./decisions/0038-the-domain-a-binding-cannot-name.md)).
A binding cannot name the type `at::<D>` needs and must carry the tag as data.
The tag lives on the **plan handle**, not on the call: the ABI is frozen so one
new creation entry point beats three new calls; a domain belongs to a route, not
an instant; and plan time is where frame *names* are in hand for the diagnostic.

**Extrapolation** ([`0039`](./decisions/0039-extrapolation-you-cannot-fail-to-notice.md)).
`Plan::at_extrapolating` has no pose-only accessor. **C cannot enforce that, so
the analogue is a required out-parameter**: a null `info` is `TFT_ERR_NULL_ARG`
and there is no second spelling without it. C is *sharper* in two places: `edge`
is `TFT_INVALID_ID` when `by_ns == 0`, and the twist-carrying layout is refused
rather than served under `Error`. In Python the batch distance is an `(N,)` array,
not a scalar: collapsing it is a `max` that marks fresh elements stale or a `min`
that marks stale ones fresh.

**Recovery from owner death**
([`0044`](./decisions/0044-recovery-the-languages-a-robot-is-written-in-cannot-reach.md)).
Two shapes generalise. First, a `&mut self` is a boundary refusal in disguise:
both bindings hold the tree in an `Arc`, and `Arc::get_mut` fails once any plan or
publisher holds a clone, so `Tree::attachment` moved behind a `Mutex` and the
method takes `&self`. **A new mutating method on `Tree` should assume the same;
R6's read-only default keeps that rare.** Second, a capability is only as
reachable as the *state* it needs: `tft_tree_inherit_ownership` would have
answered `TFT_READ_ONLY` every time (D18) because `tft_tree_open` was read-only,
so `tft_tree_open_named` is part of the surface. **Check the precondition, not
only the call.** All four functions are in the **unstable** tier because §3.5's
protocol is young; `tft_tree_plan_in_domain` is stable because it is a query
shape, not a protocol.

---

## 5. Time at the boundary

R3 settles the *unit*. The hard parts are getting a stamp in without friction
and getting the **epoch** right.

### 5.1 The unit was never the imposition — the conversion was

`rclcpp::Time`, `rclpy.time.Time`, `builtin_interfaces/Time`, PTP and
`clock_gettime` all agree with int64 ns (±292 years), so accepting it *skips* a
conversion a float API would force; a driver that hands out floats has already
destroyed the precision upstream. What users resent is writing
`stamp.sec * 10**9 + stamp.nanosec` in every node.

**NORMATIVE — every surface ships exact, total converters, and none takes a
float:**

```rust
Stamp::<D>::from_parts(sec: i64, nanos: u32)          -> Option<Stamp<D>>
Stamp::<D>::from_timespec(tv_sec: i64, tv_nsec: i64)  -> Option<Stamp<D>>
```
```python
tf_tree.from_ros(msg.header.stamp)   # exact; never via to_sec()
```

`from_sec` stays, documented as lossy above ~10⁷ s and kept out of the examples.

**`Option`, not a normalising or wrapping result.** A `nanos` outside `[0, 1e9)`
or a sum outside `i64` has no correct answer, and normalising or wrapping would
turn a malformed message into a plausible stamp. `None` does not distinguish them
because no consumer branches on it (D11). The sum is what is range-checked, not
the product: a staged `checked_mul` then `checked_add` refuses a band of
representable stamps at the negative end.

**`from_timespec` takes two fields, not a struct**: the core's dependency budget
has no `libc::timespec`, and a private `#[repr(C)]` copy would be a type to
convert *into*. It adds one refusal over `from_parts`: a negative `tv_nsec`
(POSIX permits it only in a relative interval). Python `from_ros` and the C entry
point inherit the shape: exact, total, no float, a refusal rather than a
normalisation.

### 5.2 The epoch is the hard part, and it is what `Domain` is for

Nanoseconds since *what* — Unix epoch, boot, TAI, a sensor's free-running
counter? Mixing them yields a well-formed, catastrophically wrong transform.

**The constant offset between two domains is not recoverable from the stamps
themselves**: any `(offset, delay)` and `(offset + δ, delay − δ)` produce the same
observed stamp. Recovery needs a two-way exchange or an out-of-band declaration
(Phase 8's clock-domain alignment, which reports an uncertainty; D19;
`PROJECT.md` §4). A wrong domain is therefore unfixable after the fact, which is
why the domain is a **type** (D9).

`PHASE4.md` §5.5 makes the ingest bridge tag edges `SimDomain` or `SystemDomain`
and makes a domain mismatch a **startup** failure. The mismatch half is
implemented (`TopologyConfig::check_domain`) and a config file names its domain
(§2.5); the `use_sim_time` *derivation* remains open. The read side is specified
by [`PHASE7.md`](./PHASE7.md) §4 J9: a `Buffer` derives its query domain from its
clock's `rcl_clock_type_t`, so mixing `/clock` sim time with a driver's steady
time gets `TimeDomainMismatch`, which `tf2` cannot detect.

### 5.3 `CLOCK_REALTIME` is not monotone, and the failure reads like our bug

NTP steps and leap seconds move `CLOCK_REALTIME` backwards. `PHASE1.md` §2
invariant 6 requires per-edge non-decreasing stamps, so a step surfaces as a burst
of `NonMonotonicStamp` rejections that reads as a tf_tree defect.

**Implementation items:**

1. A `doctor` check naming the cause: a run of rejected pushes on an edge whose
   **domain tag is a wall clock**, reported as a clock step and not a publisher
   fault. It ships as `TFT019` (`PHASE5.md` §6), a refinement of `TFT018`'s
   attribution, with a `RUNBOOK.md` row under `NonMonotonicStamp`. Per §2.5 the
   only built-in wall clock is `SystemDomain`; the check keys on the tag.
2. A documentation line recommending a steady or PTP domain (`SteadyDomain`,
   tag 3) for anything published at rate.

The online bridge's `/clock`-reset versus `transform_tolerance` problem is settled
by [`0012`](./decisions/0012-the-authoritative-clock-jump-signal-and-the-degradation-ladder.md).
This check is the single-process, no-ROS case, where a good diagnostic is the only
honest response.

---

## 6. Delta summary

Everything this document adds to what is already specified. **Lands in** is what
makes each row schedulable; nothing here is authorized by this document alone.

| # | Change | Surface | Where | Lands in |
|---|---|---|---|---|
| 1 | `Tree::claim_owned` → `OwnedWriter`; delete the PyO3 and C ABI lifetime extensions | Rust, Python, C | [`0017`](./decisions/0017-owned-handles-and-the-lifetime-rule.md) | **landed** — `0017` steps 6–7; both `extend_to_static` helpers are deleted |
| 2 | `Arc<Tree>` documented as the embedding idiom | Rust (docs only) | §2.2 | **landed** — `tf_tree` crate docs |
| 3 | `#[inline]` on the fold; LTO guidance; a cross-crate bench row gated at 5% | Rust | §2.3 | **all three landed.** The row has had no independent variable since 2026-08-29 and reports nothing (quotient 1.0 by construction); §2.3's 2026-09-06 amendment is the account |
| 4 | `# Stability` headings on CLI-facing exports; then the `unstable` tier itself | Rust | §2.6 | **landed** — `tf_tree::unstable` behind a default-off `unstable` feature. `ArenaView`, `EdgeKind`, `EdgeMeta` and `Tree::arena_view` moved into it; `tf_tree_cli`, `tf_tree_c`, `tf_tree_bench` and `tf_tree_py` turn it on, checked by `just stable-tier-check`; three `compile_fail,E0432` doctests pin that the root no longer answers. Stable `Tree::frames` and `Tree::edges` landed with it (§7 check 1). CI runs `just stable-tier-check` as its own job |
| 5 | Per-edge nominal rate reachable from a plan | Rust core | [`0018`](./decisions/0018-blocking-waits-belong-in-the-shim.md) | **landed** — `Plan::slowest_nominal_rate_mhz`, `Guard`-scoped and generation-checked like `span`; `0` means undeclared and is skipped |
| 6 | No blocking primitive in the arena; the escalation path recorded | all | [`0018`](./decisions/0018-blocking-waits-belong-in-the-shim.md) | recorded, not built |
| 7 | `Layout::QuatTwist`; derivatives reach Python and C | core, Python, C | §3.3 | **landed** — `PHASE5.md` §4.4 item 1: `plan.at(..., layout=...)` and `at_into` serve all four layouts; refusals are typed (`DerivativesUnavailableError` for a `LerpSlerp` edge, `NoSegmentError` for a stamp with no segment). Python's `interp=` default is `"sclerp"` (§3) |
| 8 | `tree.frames()`, `tree.edges()`, `plan.edges()` | Python, Rust | §3.2, §2.6 | **landed** — `tf_tree_py`; authorised by `PHASE5.md` §4.4 item 2 (names half). Rust followed in row 4 as stable `Tree::frames` / `Tree::edges`; `plan.edges()` has **no** Rust twin, because nothing on the stable tier turns an `EdgeId` into a name pair |
| 9 | `from_parts` / `from_timespec` / `from_ros` | Rust, Python, C | §5.1 | **landed** — Rust (`Stamp::from_parts`, `from_timespec`), Python (`from_parts`, `from_ros`; duck-typed on `.sec`/`.nanosec`) and C (`tft_stamp_from_parts`, `tft_stamp_from_timespec`, `TFT_ERR_BAD_STAMP`, ABI minor 3 → 4). One refusal table is asserted on both sides of the boundary |
| 10 | `NS_PER_STEP_ESTIMATE` re-derived when `0013` re-baselines | Python | §3.4 | **landed** — 55 → **64 ns/step** (`PHASE3.md` §6.1's amendment is the account). `0013` itself is still `draft`; this row is the constant, not the gate |
| 11 | Clock-step `doctor` check (`TFT019`) + runbook row | CLI | §5.3 | **landed** — `tf_tree_cli`; fires only on tag 0 and only on a run of at least 8 consecutive rejected arrivals (an implementation choice), skips naming the tag otherwise, and does not demote `TFT018`. `doctor --from-bag <recording.mcap>` runs `TFT019`/`TFT018`; `--from-file <index.tft>` skips them (keyed on `checks::PushStream`, since a ring holds only accepted pushes) |
| 12 | The shim's query domain from `rcl_clock_type_t` | shim | [`PHASE7.md`](./PHASE7.md) §4 J9 | Phase 7, gated by D21 |
| 13 | `tft_bridge_options::arena_name` + `TFT_ERR_ARENA_UNAVAILABLE` | C | [`0015`](./decisions/0015-the-bridge-fills-a-shared-arena.md) | **landed** — `arena_name` appended under §3.6's `struct_size` rule and `TFT_ERR_ARENA_UNAVAILABLE` added to the frozen header, ABI minor 4 → 5. NULL is the private heap arena; non-NULL is `tf_tree::Open` with `require_create(true)`, and an unavailable shared arena is a startup refusal with **no heap fallback**. Outstanding on `0015` and not C API surface: the `atfork` test and §9.2's N = 1…16 curve |
| 14 | `__version__`, `arena_format_version()`, `arena_layout_hash()` on the Python module | Python | §3.2 | **landed** — `tf_tree_py`; import frequency, no arena or lock touched, so R2 is not in tension. See its crate docs, *Build identity* |
| 15 | `frame_headroom=` on `tf_tree.build` and `tf_tree.open` | Python | §3.2 | **landed** — `tf_tree_py`; mirrors `TreeBuilder::frame_headroom`, default `0`. Closes a defect: a Python arena sized `max_frames = unique_frames + 1` gives any Rust, C or ROS-bridge peer `CapacityExceeded` from `Tree::frame()`. No `edge_headroom` (`PHASE5.md` §5.8's amendment: nothing declares an edge at runtime). Gated by `tests/python/test_errors.py::test_frame_headroom_reaches_the_arena_and_stays_out_of_the_frame_list` (frozen `.tft` sizes 2262912 / 2264320 / 2274048 B at headroom 0 / 8 / 64) |
| 16 | `tf_tree_math::slerp` is `pub` | Rust | §2.7 (§2.6's test) | **landed** — the kernel `LerpSlerp` already evaluates, exported at `tf_tree_math`'s root and by the **`tf_tree` facade**; `tf_tree/tests/math_reexports.rs` pins that the paths are one item. **Visibility only**: the body is unchanged, so nothing on the hot path moves. No instruction count is portable and none is quoted; the wrapper is never optimized out. Two costs are documented rather than removed: the endpoint-shortcut asymmetry, pinned by `the_iso3_round_trip_it_replaces_agrees_as_a_rotation`, and **`s` outside `[0, 1]` is documented as unsupported rather than refused**. §7's walk is in §2.7 |
| 17 | `MAX_DEPTH` and `MAX_PATH_EDGES` documented as public surface, and the one-variant/two-bounds rule for `TreeTooDeep` | Rust, Python, C | §2.8, [`0034`](./decisions/0034-the-depth-bound-priced-two-slots-the-same.md) | **landed** — `0034` in full. `MAX_DEPTH` 16 → 32 and a new `MAX_PATH_EDGES` = 64, a semver-relevant change to a value and a meaning, chosen against a survey of 91 robot descriptions (binding quantity: graph *diameter*, max 30, p95 24). Every `lookup`/`at`/`at_many` row is flat within ±2% at 32; `Tree::plan` on a cache miss moves 166.2 → 308.6 ns (+83.5%). `TFT_MAX_DEPTH` is still not defined |
| 18 | `tf_tree.ingest_bag` and `Tree.source`; `Tree.freeze` writes the recording's `source_digest` | Python | [`0046`](./decisions/0046-the-consumer-the-crate-boundary-was-drawn-for.md), [`PHASE5.md`](./PHASE5.md) §3–§4 | **landed** — `tf_tree_py`. `ingest_bag` returns **the ordinary `Tree`** (no parallel offline API, `PHASE5.md` §4.1); R6 is not in tension. `Tree.source` is dropped by `publisher()`, because `source_digest` answers "was this index built from that file" and a false *yes* defeats it. **No `freeze_bag`**: it would be a second spelling of `ingest_bag(p).freeze(out)` (`PROJECT.md` §6). `max_record_bytes` is a keyword per [`0010`](./decisions/0010-naming-the-record-size-refusal.md). Gated by `tests/python/test_ingest.py` on both interpreters |
| 19 | The payload of every public error variant is nameable from `tf_tree`: `TopologyError`, `ParticipantError`, `LayoutError`, and under `shm` `IpcError` with the eight types its variants carry | Rust | §2.6, §7 | **landed** — re-exports only (`PROJECT.md` §6). §7 check 5 applies: the types were already reachable through a stable variant. `ParticipantError` and `LayoutError` are `#[non_exhaustive]`; `IpcError` is deliberately not, because a caller dispatches on it. `rustix::io::Errno` is left out, as `ShmError` leaves it. Pinned by `tf_tree/tests/error_payloads.rs` and `tests/rendezvous.rs` |
| 20 | Python exceptions carry the fields a handler branches on, and five classes are added: `TimeDomainMismatchError`, `EdgeAlreadyClaimedError`, `NonMonotonicStampError`, `ArenaHeldButUnreachableError`, `ArenaAbsentError` | Python | R5, [`PHASE3.md`](./PHASE3.md) §4.4 | **landed** — [`0058`](./decisions/0058-the-fields-a-python-exception-only-printed.md) steps 1–8. Every new class is a direct `TfTreeError` subclass. `ClaimRevokedError` waits for a Python-reachable trigger; `PHASE3.md` §11.1 records the four arms no test reaches |

**Eighteen of twenty rows have landed in full: 1, 2, 3, 4, 5, 7, 8, 9, 10, 11,
13, 14, 15, 16, 17, 18, 19 and 20.** Row 6 is recorded-not-built on purpose
([`0018`](./decisions/0018-blocking-waits-belong-in-the-shim.md)); row 12 is gated
by D21 and must not start before `PHASE7.md` §0.0's four gates are met. Rows 3 and
10 carry caveats: row 3's benchmark row measures nothing (§2.3), and `0013`, which
row 10's constant came from, is still `draft`. Re-take the count from the table
whenever it changes.

**Row 7's Python half:** `layout=` is keyword-only on `at`/`at_into` per
`PHASE3.md` §4.2, whose measurement of what that keyword costs a caller who does
not pass one **does not exist** (the A/B was swamped by run-to-run spread) and is
owed. Its other NORMATIVE ask is done: `at`, `at_into` and `push` carry
`METH_FASTCALL | METH_KEYWORDS` and `latest` carries `METH_NOARGS`, read from
`PyMethodDef::ml_flags` by a test on both interpreters.

---

## 7. The check a new surface has to pass

Applied to the shim in `PHASE7.md` §7, and to anything after it.

1. **Tiers.** Are all three reachable? Is the collapsed convenience visibly the
   collapsed one, and does it go through the plan cache? Is there a documented
   way *down* to tier 2?
2. **Hot tier.** Does the evaluate path allocate, lock, resolve a name or
   convert? Is there an `_into` form?
3. **Time.** Integer nanoseconds end to end? Any path where a float round trip
   can occur, including inside a message type the surface accepts? Is the domain
   derived from something the caller already holds?
4. **Layout.** Explicit, with no silently-wrong default? Chosen by type where the
   language allows?
5. **Errors.** Typed, `Copy`, prose separate? Does any documentation invite a
   caller to match on message text?
6. **Writability.** What does a caller who did not ask to write get? Is it
   enforced by something stronger than our own care?
7. **Lifetimes** (Rust, and anything embedding Rust). Does the surface hand out
   a type carrying a lifetime that a user will want to store?
8. **Losses.** Does the benchmark table have a row where this surface is
   *worse* than the alternative it replaces? If not, it is not finished.

## 8. The real-time envelope — NORMATIVE

The pitch is *"fast enough to sit inside a control loop"*. A control loop is a
deadline, so this section states the worst case and what cannot happen on the
query path, each claim attached to whatever re-derives it
(`docs/benchmarks/EVIDENCE.md`).

### 8.1 What the query path does not do

For `Plan::at`, `Plan::at_many_into`, `Plan::at_with_derivatives` and
`Plan::at_extrapolating`, evaluated under a `Guard` the caller already holds:

| Does not | Why, and what checks it |
|---|---|
| **Allocate** | The plan is a fixed `[Step; MAX_DEPTH]` by value and every batch form has an `_into` (R2). Checked by `crates/tf_tree_bench/tests/zero_alloc.rs`, which counts allocations through a wrapping global allocator across `Plan::at` loops (24-frame tree; 1537-frame tree across ring wraparound), `at_many`, `at_many_into` in `Mat4`, `Quat` and `QuatTwist`, `at_many_into_f32` in `Affine32`, `at_with_derivatives`, `at_extrapolating` under all three `ExtrapPolicy` values, and a stale plan's `TopologyChanged` refusal. **Not** reached: `SampleRing::read_slot`'s seqlock retry, which needs a concurrent writer that file does not run |
| **Take a lock** | Reads are seqlock reads; a reader never blocks a writer and a writer never waits for a reader |
| **Read a clock** | `tf_tree_core` is `no_std`; the query's stamp is the caller's, always (R3) |
| **Resolve a name** | Frames are interned to integer ids at compile time (R1, D3) |
| **Make a syscall** | The arena is already mapped; evaluation touches that mapping and nothing else |
| **Branch on the transport** | The same code runs against a heap arena, a `MAP_SHARED` memfd and a frozen `.tft`; `docs/PHASE5.md` §2.1 makes that NORMATIVE and the relocation gate tests it |

### 8.2 The worst case is bounded, and here is the bound

**A reader that meets a slot mid-write retries `SEQ_RETRY_LIMIT` (64) times and
then returns `LookupError::SlotContended`.** It neither spins indefinitely nor
blocks, so a writer preempted inside its two-store publish window cannot hold a
higher-priority `SCHED_FIFO` reader past a fixed bound. What a control loop does
about a contended slot is the caller's call. `docs/decisions/0018` is the same
principle for waits: no blocking wait, futex or notification primitive lives in
the arena.

**Not on this path, and must not be put there:** `Tree::lookup` (tier 1, R1);
`Tree::reparent`, which takes the topology lock with a bounded spin (A2, `0029`);
`Publisher::push`, which reads the wall clock on a countdown (`0036`'s
receipt-time sampler, ~1 ns amortised, `just push-sampler-cost`). None is a query.

### 8.3 Page faults are the residual, and they are the embedder's to remove

The arena is a `memfd`. An untouched page costs a minor fault on first touch,
which inside a control cycle is a deadline miss. Two things address it and a third
does not exist:

- **Per-edge population at take-up** (`docs/PHASE2.md` §7.1, `0024`) faults the
  pages an edge uses when the edge is claimed, not when first read.
- **`mlockall(MCL_CURRENT | MCL_FUTURE | MCL_ONFAULT)` in the embedding process**
  pins the mapping, and it is the *application's* call: a library that locks
  memory decides an `RLIMIT_MEMLOCK` budget it cannot see. **`MCL_ONFAULT` is
  load-bearing:** plain `MCL_CURRENT|MCL_FUTURE` prefaults an untouched 64 MiB
  `memfd` mapping the instant it is issued (`Rss` 0 → 65 536 kB), which is
  per-arena population at address-space scope, removed by
  [`0024`](./decisions/0024-population-is-per-edge-at-take-up.md) at a measured
  5.2×. `TFT016` reports the limit against the arena size — **one term of two**:
  `mlockall` charges the whole address space, so a quiet check is not a
  clearance.
- **There is no `LockPolicy` and no `mlock` call in this codebase**
  ([`0049`](./decisions/0049-the-flag-that-prefaults-the-arena.md)), because of
  *who* may spend the budget. `MLOCK_ONFAULT` does not prefault, but it is what
  keeps the PTEs §7.1 establishes (`MADV_PAGEOUT` is refused while locked); whether
  a swapless host reclaims those pages is **undetermined**
  (`crates/tf_tree_bench/examples/mlock_probe.rs`).

### 8.4 What this section does not claim

- **No number here is a latency guarantee.** `docs/PHASE1.md` §11.3's latency
  criteria need dedicated core-pinned hardware and are recorded UNAVAILABLE on
  every host measured; the figures in `docs/benchmarks/` are medians on a
  shared-tenancy VM.
- **"No syscall" and "no lock" are read from the code, not enforced by a test.**
  Only the allocation claim has an executor (`zero_alloc.rs`).
- **The tail has a reading, not a gate.** `just control-loop` runs
  `crates/tf_tree/examples/control_loop.rs` — two queries under one guard at 1 kHz
  against a 200 Hz estimate, under a concurrent writer — and reports p50 / p99 /
  p99.9 / max. The host is unpinned with no real-time scheduler, so read it for
  shape and §11.3 for the number.
- **`PHASE4.md` §1's operational exit criterion is still open**: no node has run
  this on real hardware for two weeks.
