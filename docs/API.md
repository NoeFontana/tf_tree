# tf_tree — the API contract

> **Companions:** [`PROJECT.md`](./PROJECT.md) (decision log D1–D22),
> [`PHASE1.md`](./PHASE1.md)–[`PHASE5.md`](./PHASE5.md) (per-phase specs),
> [`PHASE7.md`](./PHASE7.md) (the `tf2`-shaped shim, gated by D21).

The rules that generate every binding. Sections marked **NORMATIVE** are
requirements. **Status:** Ready; this document schedules nothing.

## 1. The six rules

A question none of these answers is a decision record, not an API choice.

### R1 — Three tiers, always: attach → compile → evaluate

`open`/`build` once, `plan` once per `(target, source)`, `at` in the loop. Every
binding exposes all three tiers (D3), including the shim.

A convenience that collapses tiers (`Tree::lookup`, `tf_tree.lookup`) is allowed
on two conditions, both **NORMATIVE**:

1. It goes through the plan cache; it does **not** re-resolve topology per call.
2. It is visibly the collapsed one: named differently, documented as paying a
   cache probe, and never the example in the README's hot loop.

`Tree::lookup`'s per-thread cache is keyed `(arena, target, source, generation)`
(`PHASE3.md` §7.2) and holds the **result** of compiling its key, a refusal
included. Errors raised *after* compilation (`NoData`, `Extrapolation`,
`SlotRecycled`, `TimeDomainMismatch`) are **not** cached.

### R2 — The hot tier never allocates, never locks, never converts

An operation that allocates, locks, resolves a name or converts a representation
belongs on `Tree` (tier 1) or the compile step (tier 2), not on `Plan`.
**NORMATIVE:** every batch entry point has an `_into` form writing into caller
memory.

### R3 — Time is integer nanoseconds carrying a domain

No float, no seconds keyword, no convenience overload, on any surface
(`PHASE3.md` §3); see §5.

### R4 — Memory layout is stated, never inferred

`Layout` / `tft_layout` / `layout=` are explicit with no default that could be
silently wrong (`PHASE4.md` §3.5). In C++ the layout is chosen **by type**
(`layout_of<T>`).

### R5 — Errors are identifiers; prose is a separate layer

Error types stay `Copy`, `String`-free and `no_std` (D11); D11 does not reach a
binding's own exception
([`0058`](./decisions/0058-the-fields-a-python-exception-only-printed.md) §7).
Across FFI the *code* is the contract. Three layers
([`0040`](./decisions/0040-the-error-that-cannot-be-returned.md);
[`0059`](./decisions/0059-the-arena-errors-that-cannot-describe-themselves.md)):

| Layer | Knows | Says |
|---|---|---|
| the type and its discriminant | nothing | the contract — what a caller matches on, in Rust and across FFI |
| `Display` on the error itself | what the error carries | `edge 3: stamp 5 ns is outside its window [1, 4] ns` |
| `Described`, from `Tree::describe` | the arena | `odom -> base_link: ...`, plus a sample of the frames that do exist |

**NORMATIVE for every surface, including the shim:** error *types* are a
compatibility promise; message *text* is not, and no surface may document text a
caller could match on.

### R6 — Read-only by default for anything that did not ask to write

Python defaults to `mode="ro"`; `tft_tree_open` attaches read-only; a frozen
`.tft` is read-only permanently. Every mutating entry point consults
`is_writable()` first and returns a typed error (D18).

## 2. Rust — the embedding surface

Its distinct requirement is **embedding**: users hold tf_tree types inside their
own types for the life of their process.

### 2.1 The lifetime rule — NORMATIVE
> **No type a user stores in their own struct may carry a lifetime.**

| Type | Storable? | |
|---|---|---|
| `Plan` | ✅ | `Copy + Send + Sync + 'static` |
| `Tree` | ✅ | via `Arc<Tree>` — §2.2 |
| `Guard<'a>` | ✅ | per-batch, stack-only, never stored |
| `EdgeWriter<'a>` | ❌ | fixed by [`0017`](./decisions/0017-owned-handles-and-the-lifetime-rule.md) |

`Tree::claim_owned` → `OwnedWriter` is the **only** lifetime extension; a second
is a new decision record.

### 2.2 `Tree` is not `Clone`, and `Arc<Tree>` is the idiom — NORMATIVE (doc)

`Tree` holds a registered participant slot (`DEFAULT_MAX_PARTICIPANTS`, 64); a
derived `Clone` would burn a second slot or lie about sharing one. `tf_tree_c`
wraps an `Arc<Tree>` in an `Arc<TreeShare>`: the outer is the *handle* refcount
shared with every `tft_plan`, the inner the *arena* refcount
`Tree::claim_owned` takes and a publisher holds after every handle is gone.

### 2.3 Cross-crate inlining is part of the zero-cost claim

**NORMATIVE:**

1. `#[inline]` on `Plan::at`, `fold_at`, the `Guard` sampling entry points
   (`sample`, `sample_hinted`, `sample_from`) and the `Iso3` operators; not on
   `fold_at_with_derivatives`, `fold_latest`, `fold_latest_common`,
   `fold_at_cursors`.
2. Crate docs state `lto = "thin"`, `codegen-units = 1` for embedders; **an
   embedder's profile is not ours**
   ([`0060`](./decisions/0060-the-batch-fold-that-reads-before-it-interpolates.md)
   Decision A).
3. A benchmark row measures the **facade path from a separate crate** against the
   in-crate path, gated at 5% — the gate `PHASE4.md` §7 applies to the C ABI.

Item 3 is `crates/tf_tree_bench/src/embed.rs`, `just embed-cost` and
`PHASE5.md` §9.2's row `embedding_cross_crate`.

**Amendment (2026-09-06) — item 3's row has no independent variable.**
`Plan::at_tagged` (`0038`) sits between `Plan::at` and `fold_at` without
`#[inline]`, so both columns compile to one out-of-line symbol and the quotient is
1.0 by construction. The recipe checks structurally that the two symbols' sizes
differ, **refuses when a symbol is absent**, and refuses unless
`EMBED_COST_KNOWN_COLLAPSED=1` is set (CI sets it; restoring the variable deletes
both the `env:` block and the recipe's escape branch). The row's committed
baseline status is `unavailable`
([`0013`](./decisions/0013-the-benchmark-gate-never-interpolated.md) step 6).

### 2.4 Two things that are not going to exist
**No `trait TransformSource`:** `TreeBuilder::build()` **is** the test double.
**No blocking wait in the core** ([`0018`](./decisions/0018-blocking-waits-belong-in-the-shim.md)).

### 2.5 Domains are an open trait

`Domain` stays implementable by users (`struct PtpDomain;` with a free `TAG`).
**The built-in set is four; tags `0`–`3` are reserved for it:** `SystemDomain`
(0), `SensorDomain` (1), `SimDomain` (2), `SteadyDomain` (3). User domains pick a
tag from `4` upwards. All live in `tf_tree_core::plan`, re-exported through the
facade.

**A tag is a permanent choice:** it is written into `EdgeRecord::domain` and every
recording on disk; a test pins the four values.

`tf_tree_bridge::config::parse_domain` maps all four names onto their `TAG`.
Nothing derives the bridge's tag from `use_sim_time` (`PHASE4.md` §5.5).

### 2.6 Stability tiering — built; the split *is* the promise — NORMATIVE

Rust mirrors C's `tf_tree.h` / `tf_tree_unstable.h` as `tf_tree::unstable`
behind a default-off `unstable` Cargo feature whose documentation *is* the waiver.

**What goes there is decided by *does its shape follow the arena layout*, not
"is it low-level"**: `PHASE5.md` §1 changes that layout on purpose. Moved:
`ArenaView`, `EdgeKind`, `EdgeMeta`, `Tree::arena_view`. `Plan`, `Guard` and
`Stamp` are stable.

**Gating the door must not remove the capability — NORMATIVE.** The facade
carries **stable `Tree::frames` and `Tree::edges`** (names only) (§7 check 1).

**The consumer list is checked.** Every crate that turns the feature on may
break at a patch release. `just stable-tier-check` fails if §6 row 4 or
`crates/tf_tree/Cargo.toml` disagrees with the `[dependencies]` entries by name;
it greps row 4 for each of `tf_tree_bench`, `tf_tree_c`, `tf_tree_cli` and
`tf_tree_py`. It also names the `cargo add tf_tree` configuration (plus `shm` and
no-default-features) that `--workspace` unifies away, and runs in CI as its own
job.

### 2.7 A kernel the engine already runs is public on its own terms — NORMATIVE

**The rule:** a kernel this crate already evaluates on the hot path is `pub` when
reaching it through its wrapper would make the caller **manufacture a value of a
type their problem does not contain** (a `slerp` caller would have to invent a
*translation*). **A policy's kernel is stable-tier public API.** It passes §2.6's
test: `(Quat, Quat, f64) -> Quat` follows no arena layout.

**§7 check 8 (losses):** no benchmark row
([`0023`](./decisions/0023-the-gate-that-could-not-gate.md)); at `s = 0` and
`s = 1` the direct call is **worse** than the round trip, because
`LerpSlerp::eval`'s endpoint shortcuts hide two things the kernel does not,
discharged by the differential test
`the_iso3_round_trip_it_replaces_agrees_as_a_rotation`. **A math primitive answers
check 8 with a differential; a binding with a benchmark.** `Quat` is
`[w, x, y, z]`, stated in the `# Storage order` heading.

**The `tf_tree` facade re-exports `slerp`**; `tf_tree/tests/math_reexports.rs`
pins that `tf_tree::slerp` **is** `tf_tree_math::slerp`. `screw_pow` deliberately
does **not** follow it: a bare `screw_pow` at the facade root would be a second
spelling (`PROJECT.md` §6).

### 2.8 The two path bounds are public, and each prices a different slot — NORMATIVE (doc)

`tf_tree::MAX_DEPTH` and `tf_tree::MAX_PATH_EDGES` are `pub const` on the stable
tier, so each is a semver promise about a *value*
([`0034`](./decisions/0034-the-depth-bound-priced-two-slots-the-same.md)).

| constant | bounds | a slot costs | value |
|---|---|---|---|
| `MAX_DEPTH` | the **compiled** plan — `Plan`'s `[Step; MAX_DEPTH]`, counted after folding | **64 B** per `Step` since [`0042`](./decisions/0042-the-cacheline-the-arena-never-asked-for.md) (`Plan` 2064 B) | **32** |
| `MAX_PATH_EDGES` | the **raw walk** — edges visited on both sides of the common ancestor | **4 B**, in `compile`'s stack frame, off the hot path (D3) | **64** |

**A binding may not invent a third bound and may not hide these two.** Both
overruns are `LookupError::TreeTooDeep` (the C ABI's `tft_status` table is
frozen); the `depth` field separates them. **Rust's** message names
`TreeBuilder::static_edge` as the remedy, **Python's** must not (`tf_tree.build`
declares every edge dynamic), and **C's** header names no macro (`TFT_MAX_DEPTH`
is defined nowhere).

## 3. Python — mirror, plus conveniences that pay for themselves

Python mirrors §1's three tiers. Divergences from Rust: `mode="ro"` (R6),
scalar/array dispatch on `at`, and `interp=` defaulting to `"sclerp"` (D5).

### 3.1 Still refused — NORMATIVE

Float stamps; `asyncio`; any view into the arena; `pickle` of `Tree`/`Plan`/
`Publisher`; keyword arguments on `at`, `at_into`, `latest`, `push`; any branching
logic that could live in Rust; `scipy.spatial.transform.Rotation` interop.

### 3.2 Accepted conveniences

All run at tier 1 or tier 2 frequency, so R2 is not in tension:

- scalar/array dispatch on `at`; context managers; `__repr__`; `.pyi` and
  `py.typed`; `from_sec` / `from_datetime` / `now` / **`from_ros`** (§5.1)
- **introspection: `tree.frames()`, `tree.edges()`, `plan.edges()`** — names
  only; `tree.edges()` is the names half of `PHASE5.md` §4.2's `ds.edges()`.
- **build identity: `__version__`, `arena_format_version()`,
  `arena_layout_hash()`**; see `crates/tf_tree_py/src/lib.rs`'s crate docs.
- **arena headroom: `frame_headroom=` on `build` / `open`**, mirroring
  `TreeBuilder::frame_headroom`, default `0`.

### 3.3 Parity deltas to close

| Gap | Where | Disposition |
|---|---|---|
| `at_with_derivatives` absent from Python | Rust and C have it (`tft_plan_at_with_derivatives`, unstable tier); `PHASE4.md` §0 scoped Python out | **Phase 5**, as `Layout::QuatTwist` — see below |
| `Publisher` holds an extended borrow by hand | `tf_tree_py/src/tree.rs` | [`0017`](./decisions/0017-owned-handles-and-the-lifetime-rule.md) |
| ~~`at_extrapolating` takes no `layout=` and has no `_into` form~~ | `tf_tree_py` | **Closed.** `quat_twist` is refused, as in C |
| ~~Python cannot declare a static edge, a per-edge capacity, a rate, or a domain~~ | `tf_tree_py` | **Closed by [`0041`](./decisions/0041-python-declares-a-topology-the-way-everything-else-does.md)**: `build`'s `edges` and `open`'s `create` accept the topology config text |

**`at_with_derivatives` ships as a layout, not a method:** `Layout::QuatTwist` is
an `(N, 13)` write of `[qw qx qy qz tx ty tz | ωx ωy ωz vx vy vz]` (a **minor** C
ABI bump, `PHASE4.md` §3.6); `LerpSlerp` returns `DerivativesUnavailable`.

### 3.4 The GIL threshold constant is calibrated against a benchmark that never ran

`NS_PER_STEP_ESTIMATE` is **64 ns/step**
([`0013`](./decisions/0013-the-benchmark-gate-never-interpolated.md)), checked by
a `const` assertion in `tf_tree_py::tree`; [`PHASE3.md`](./PHASE3.md) §6.1's
amendment is the **single account** of the measurement. **NORMATIVE:** when `0013`
re-baselines, it is re-derived in the same commit and §6.1 records the
measurement.

## 4. C and C++

Specified in `PHASE4.md` §3–§4. `tf_tree.h` is semver'd; `tf_tree_unstable.h` is
opt-in by macro and promises nothing (§2.6). **Every struct passed by pointer
begins with `uint32_t struct_size`**. **The C++ wrapper contains no logic**:
anything that can be wrong there must be a `static_assert`, not a runtime branch.

### 4.1 The three capabilities the bindings could not reach — NORMATIVE (doc)

Each was callable from Rust only.

**The time domain** ([`0038`](./decisions/0038-the-domain-a-binding-cannot-name.md)):
the tag is data on the **plan handle**, not the call.

**Extrapolation** ([`0039`](./decisions/0039-extrapolation-you-cannot-fail-to-notice.md)):
in C a required out-parameter (null `info` is `TFT_ERR_NULL_ARG`); in Python the
batch distance is an `(N,)` array, not a scalar.

**Recovery from owner death**
([`0044`](./decisions/0044-recovery-the-languages-a-robot-is-written-in-cannot-reach.md)).
A `&mut self` is a boundary refusal in disguise: **a new mutating method on `Tree`
should take `&self`**. A capability is only as reachable as the *state* it needs:
`tft_tree_inherit_ownership` would answer `TFT_READ_ONLY` (D18) on a read-only
`tft_tree_open`, so `tft_tree_open_named` is part of the surface. All four functions are **unstable**;
`tft_tree_plan_in_domain` is stable.

## 5. Time at the boundary

### 5.1 The unit was never the imposition — the conversion was

Every ROS, PTP and POSIX clock agrees with int64 ns, so accepting it *skips* a
conversion a float API would force.

**NORMATIVE — every surface ships exact, total converters, and none takes a
float:**

```rust
Stamp::<D>::from_parts(sec: i64, nanos: u32)          -> Option<Stamp<D>>
Stamp::<D>::from_timespec(tv_sec: i64, tv_nsec: i64)  -> Option<Stamp<D>>
```
```python
tf_tree.from_ros(msg.header.stamp)   # exact; never via to_sec()
```

`from_sec` stays, documented as lossy above ~10⁷ s.

**`Option`, not a normalising or wrapping result:** a `nanos` outside
`[0, 1e9)` or a sum outside `i64` has no correct answer, and `None` does not
distinguish them (D11). The sum is range-checked, not the product.
`from_timespec` adds one refusal over `from_parts`: a negative `tv_nsec`. Python
`from_ros` and the C entry point inherit the shape.

### 5.2 The epoch is the hard part, and it is what `Domain` is for

Mixing epochs yields a well-formed, catastrophically wrong transform. **The
offset between two domains is not recoverable from the stamps themselves**
(D19; `PROJECT.md` §4), so a wrong domain is unfixable after the fact, which is
why the domain is a **type** (D9).

`PHASE4.md` §5.5 makes a domain mismatch in the ingest bridge a **startup**
failure (`TopologyConfig::check_domain`); the read side is
[`PHASE7.md`](./PHASE7.md) §4 J9.

### 5.3 `CLOCK_REALTIME` is not monotone, and the failure reads like our bug

NTP steps move `CLOCK_REALTIME` backwards, so a step surfaces as a burst of
`NonMonotonicStamp` rejections (`PHASE1.md` §2 invariant 6).

1. A `doctor` check naming the cause: a run of rejected pushes on an edge whose
   **domain tag is a wall clock** (only `SystemDomain`, §2.5). It ships as
   `TFT019` (`PHASE5.md` §6), with a `RUNBOOK.md` row under `NonMonotonicStamp`.
2. A documentation line recommending a steady or PTP domain (`SteadyDomain`,
   tag 3) for anything published at rate.

The bridge's `/clock`-reset problem is
[`0012`](./decisions/0012-the-authoritative-clock-jump-signal-and-the-degradation-ladder.md).

## 6. Delta summary

**Lands in** is what makes each row schedulable; nothing here is authorized by
this document alone.

| # | Change | Surface | Where | Lands in |
|---|---|---|---|---|
| 1 | `Tree::claim_owned` → `OwnedWriter`; delete the PyO3 and C ABI lifetime extensions | Rust, Python, C | [`0017`](./decisions/0017-owned-handles-and-the-lifetime-rule.md) | **landed** — `0017` steps 6–7 |
| 2 | `Arc<Tree>` documented as the embedding idiom | Rust (docs only) | §2.2 | **landed** — `tf_tree` crate docs |
| 3 | `#[inline]` on the fold; LTO guidance; a cross-crate bench row gated at 5% | Rust | §2.3 | **all three landed.** The row reports nothing; §2.3's 2026-09-06 amendment is the account |
| 4 | `# Stability` headings on CLI-facing exports; then the `unstable` tier itself | Rust | §2.6 | **landed** — `tf_tree::unstable` behind a default-off `unstable` feature; `tf_tree_cli`, `tf_tree_c`, `tf_tree_bench` and `tf_tree_py` turn it on, checked by `just stable-tier-check`; `compile_fail,E0432` doctests pin that the root no longer answers. Stable `Tree::frames` and `Tree::edges` landed with it |
| 5 | Per-edge nominal rate reachable from a plan | Rust core | [`0018`](./decisions/0018-blocking-waits-belong-in-the-shim.md) | **landed** — `Plan::slowest_nominal_rate_mhz`, `Guard`-scoped and generation-checked like `span`; `0` means undeclared |
| 6 | No blocking primitive in the arena; the escalation path recorded | all | [`0018`](./decisions/0018-blocking-waits-belong-in-the-shim.md) | recorded, not built |
| 7 | `Layout::QuatTwist`; derivatives reach Python and C | core, Python, C | §3.3 | **landed** — `PHASE5.md` §4.4 item 1; refusals are typed (`DerivativesUnavailableError`, `NoSegmentError`). Python's `interp=` default is `"sclerp"` |
| 8 | `tree.frames()`, `tree.edges()`, `plan.edges()` | Python, Rust | §3.2, §2.6 | **landed** — `tf_tree_py`; `PHASE5.md` §4.4 item 2. Rust followed in row 4; `plan.edges()` has **no** Rust twin |
| 9 | `from_parts` / `from_timespec` / `from_ros` | Rust, Python, C | §5.1 | **landed** — Rust, Python (duck-typed on `.sec`/`.nanosec`) and C (`tft_stamp_from_parts`, `tft_stamp_from_timespec`, `TFT_ERR_BAD_STAMP`, ABI minor 3 → 4). One refusal table is asserted on both sides |
| 10 | `NS_PER_STEP_ESTIMATE` re-derived when `0013` re-baselines | Python | §3.4 | **landed** — 55 → **64 ns/step** (`PHASE3.md` §6.1's amendment). `0013` itself is still `draft` |
| 11 | Clock-step `doctor` check (`TFT019`) + runbook row | CLI | §5.3 | **landed** — `tf_tree_cli`; fires only on tag 0 and on a run of at least 8 consecutive rejected arrivals, and does not demote `TFT018`. `doctor --from-bag` runs `TFT019`/`TFT018`; `--from-file` skips them |
| 12 | The shim's query domain from `rcl_clock_type_t` | shim | [`PHASE7.md`](./PHASE7.md) §4 J9 | Phase 7, gated by D21 |
| 13 | `tft_bridge_options::arena_name` + `TFT_ERR_ARENA_UNAVAILABLE` | C | [`0015`](./decisions/0015-the-bridge-fills-a-shared-arena.md) | **landed** — ABI minor 4 → 5. NULL is the private heap arena; non-NULL is `tf_tree::Open` with `require_create(true)`, and an unavailable shared arena is a startup refusal with **no heap fallback** |
| 14 | `__version__`, `arena_format_version()`, `arena_layout_hash()` on the Python module | Python | §3.2 | **landed** — `tf_tree_py` |
| 15 | `frame_headroom=` on `tf_tree.build` and `tf_tree.open` | Python | §3.2 | **landed** — `tf_tree_py`; default `0`. No `edge_headroom` (`PHASE5.md` §5.8). Gated by `tests/python/test_errors.py::test_frame_headroom_reaches_the_arena_and_stays_out_of_the_frame_list` |
| 16 | `tf_tree_math::slerp` is `pub` | Rust | §2.7 (§2.6's test) | **landed** — exported at `tf_tree_math`'s root and by the `tf_tree` facade; `tf_tree/tests/math_reexports.rs` pins that the paths are one item. `s` outside `[0, 1]` is documented as unsupported rather than refused |
| 17 | `MAX_DEPTH` and `MAX_PATH_EDGES` documented as public surface, and the one-variant/two-bounds rule for `TreeTooDeep` | Rust, Python, C | §2.8, [`0034`](./decisions/0034-the-depth-bound-priced-two-slots-the-same.md) | **landed** — `0034` in full: `MAX_DEPTH` 16 → 32 and `MAX_PATH_EDGES` = 64. `TFT_MAX_DEPTH` is still not defined |
| 18 | `tf_tree.ingest_bag` and `Tree.source`; `Tree.freeze` writes the recording's `source_digest` | Python | [`0046`](./decisions/0046-the-consumer-the-crate-boundary-was-drawn-for.md), [`PHASE5.md`](./PHASE5.md) §3–§4 | **landed** — `tf_tree_py`. `ingest_bag` returns **the ordinary `Tree`**; `Tree.source` is dropped by `publisher()`. **No `freeze_bag`**: a second spelling of `ingest_bag(p).freeze(out)` (`PROJECT.md` §6). `max_record_bytes` is a keyword per [`0010`](./decisions/0010-naming-the-record-size-refusal.md). Gated by `tests/python/test_ingest.py` |
| 19 | The payload of every public error variant is nameable from `tf_tree`: `TopologyError`, `ParticipantError`, `LayoutError`, and under `shm` `IpcError` with the eight types its variants carry | Rust | §2.6, §7 | **landed** — re-exports only. `ParticipantError` and `LayoutError` are `#[non_exhaustive]`; `IpcError` is deliberately not. Pinned by `tf_tree/tests/error_payloads.rs` and `tests/rendezvous.rs` |
| 20 | Python exceptions carry the fields a handler branches on, and five classes are added: `TimeDomainMismatchError`, `EdgeAlreadyClaimedError`, `NonMonotonicStampError`, `ArenaHeldButUnreachableError`, `ArenaAbsentError` | Python | R5, [`PHASE3.md`](./PHASE3.md) §4.4 | **landed** — [`0058`](./decisions/0058-the-fields-a-python-exception-only-printed.md) steps 1–8; each new class is a direct `TfTreeError` subclass. `ClaimRevokedError` waits for a Python-reachable trigger |

Row 12 must not start before `PHASE7.md` §0.0's four gates are met. Row 7:
`layout=` is keyword-only per `PHASE3.md` §4.2; `METH_FASTCALL | METH_KEYWORDS` on
`at`, `at_into` and `push` is asserted by a test on both interpreters.

## 7. The check a new surface has to pass

Applied to the shim in `PHASE7.md` §7, and to anything after it.

1. **Tiers.** Are all three reachable? Is the collapsed convenience visibly the
   collapsed one, through the plan cache, with a documented way *down* to tier 2?
2. **Hot tier.** Does the evaluate path allocate, lock, resolve a name or
   convert? Is there an `_into` form?
3. **Time.** Integer nanoseconds end to end, with no float round trip? Is the
   domain derived from something the caller already holds?
4. **Layout.** Explicit, with no silently-wrong default; by type where possible?
5. **Errors.** Typed, `Copy`, prose separate; nothing invites matching on text?
6. **Writability.** What does a caller who did not ask to write get, and is it
   enforced by something stronger than our own care?
7. **Lifetimes.** Does the surface hand out a storable type carrying a lifetime?
8. **Losses.** Does the benchmark table have a row where this surface is
   *worse* than the alternative it replaces? If not, it is not finished.

## 8. The real-time envelope — NORMATIVE

### 8.1 What the query path does not do

For `Plan::at`, `Plan::at_many_into`, `Plan::at_with_derivatives` and
`Plan::at_extrapolating`, evaluated under a `Guard` the caller already holds:

| Does not | Why, and what checks it |
|---|---|
| **Allocate** | The plan is a fixed `[Step; MAX_DEPTH]` by value and every batch form has an `_into` (R2). Checked by `crates/tf_tree_bench/tests/zero_alloc.rs` through a wrapping global allocator. **Not** reached: `SampleRing::read_slot`'s seqlock retry, which needs a concurrent writer |
| **Take a lock** | Reads are seqlock reads; neither side waits for the other |
| **Read a clock** | `tf_tree_core` is `no_std`; the stamp is the caller's (R3) |
| **Resolve a name** | Frames are interned to integer ids at compile time (R1, D3) |
| **Make a syscall** | Evaluation touches the already-mapped arena and nothing else |
| **Branch on the transport** | The same code runs against a heap arena, a memfd and a frozen `.tft` (`docs/PHASE5.md` §2.1; the relocation gate tests it) |

### 8.2 The worst case is bounded, and here is the bound

**A reader that meets a slot mid-write retries `SEQ_RETRY_LIMIT` (64) times and
then returns `LookupError::SlotContended`**; it neither spins indefinitely nor
blocks.

**Not on this path, and must not be put there:** `Tree::lookup`;
`Tree::reparent` (bounded topology-lock spin, `0029`); `Publisher::push` (wall
clock on a countdown, `0036`).

### 8.3 Page faults are the residual, and they are the embedder's to remove

An untouched arena page costs a minor fault on first touch, a deadline miss inside
a control cycle.

- **Per-edge population at take-up** (`docs/PHASE2.md` §7.1, `0024`) faults an
  edge's pages when it is claimed.
- **`mlockall(MCL_CURRENT | MCL_FUTURE | MCL_ONFAULT)` in the embedding process**
  is the *application's* call: a library cannot see the `RLIMIT_MEMLOCK`
  budget. **`MCL_ONFAULT` is
  load-bearing:** without it the call prefaults the whole `memfd` mapping
  ([`0024`](./decisions/0024-population-is-per-edge-at-take-up.md)). `TFT016`
  reports the limit against the arena size — **one term of two**, so a quiet
  check is not a clearance.
- **There is no `LockPolicy` and no `mlock` call in this codebase**
  ([`0049`](./decisions/0049-the-flag-that-prefaults-the-arena.md)), because of
  *who* may spend the budget (`crates/tf_tree_bench/examples/mlock_probe.rs`).

### 8.4 What this section does not claim

- **No number here is a latency guarantee**; `docs/PHASE1.md` §11.3's criteria
  are recorded UNAVAILABLE.
- **"No syscall" and "no lock" are read from the code, not enforced by a test.**
  Only the allocation claim has an executor (`zero_alloc.rs`).
- **The tail has a reading, not a gate:** `just control-loop`
  (`crates/tf_tree/examples/control_loop.rs`).
- **`PHASE4.md` §1's operational exit criterion is still open.**
