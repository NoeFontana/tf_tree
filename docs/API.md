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

`Tree::lookup`'s per-thread cache is keyed `(target, source, generation)` and
`PHASE3.md` §7.2 already requires it be genuinely `thread_local!` rather than a
shared map behind a lock — a collapsed convenience that becomes a contention
point has failed condition 1 in a second way.

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
extension is written in the workspace.

`EdgeWriter<'a>` **stays**. A scoped claim whose scope the borrow checker
enforces is better when it fits, and most publishers are scoped.

### 2.2 `Tree` is not `Clone`, and `Arc<Tree>` is the idiom — NORMATIVE (doc)

`Tree` owns its arena backing and holds a registered slot in the arena's
participant table — 64 slots by default (`DEFAULT_MAX_PARTICIPANTS`), and a
number an arena is built with rather than an unbounded pool. A derived `Clone`
would either burn a second slot or lie about sharing one.
`Arc<Tree>` is what `tests/tsan.rs`, `tf_tree_c`'s `TreeShare` and PyO3's
`Py<PyTree>` all already do.

This needs no code change and one paragraph of crate-level documentation. It is
the first question every embedder asks, and today the answer exists only as
three independent precedents in the source.

### 2.3 Cross-crate inlining is part of the zero-cost claim

A depth-3 lookup costs ~290 ns interpolating
([`0013`](./decisions/0013-the-benchmark-gate-never-interpolated.md)), and
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
What is **not** done is the write side *applying* it. `tf_tree_bridge`'s
`TopologyConfig::default_domain` is still a bare `u8` and its `parse_domain`
still maps only `"system"` and `"sensor"`, so `PHASE4.md` §5.5's "the bridge tags
every edge it declares as `SimDomain`" is still a statement about a number an
operator writes: a deployment that wants tag 2 today writes `2`. Rewiring
`parse_domain` is a separate change and is the one that makes §5.5 true as
written.

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

### 2.6 Stability tiering — deferred, and the deferral is recorded

C has `tf_tree.h` and `tf_tree_unstable.h`; Rust has one tier, so everything
`pub` reads as a stability promise — including `ArenaView`, which is re-exported
from the facade for the CLI. The mirror (`tf_tree::unstable::*` behind a feature
whose documentation *is* the waiver) is **deferred while the crate is private**.

**What is not deferred**, because it costs a comment: a `# Stability` heading on
`ArenaView` and the other CLI-facing exports stating they move behind `unstable`
before any published tag. Then the move executes a documented plan instead of
breaking someone.

---

## 3. Python — mirror, plus conveniences that pay for themselves

The Python surface mirrors §1's three tiers exactly (`open`/`build` → `plan` →
`at`). Divergences from Rust are deliberate and few: `mode="ro"` and
no-creation-by-default (R6), and scalar/array dispatch on `at` (the NumPy idiom,
which is what makes the vectorized path the *obvious* path).

### 3.1 Still refused — NORMATIVE

Float stamps; `asyncio`; any view into the arena; `pickle` of `Tree`/`Plan`/
`Publisher`; keyword arguments on `at`, `at_into`, `latest`, `push` (measured:
`METH_FASTCALL` is 29 ns cheaper — ~10% of a depth-3 budget at §3.4's corrected
290 ns, and the "20%" `PHASE3.md` §4.2 states against the superseded 150 ns);
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

### 3.3 Parity deltas to close

| Gap | Where | Disposition |
|---|---|---|
| `at_with_derivatives` absent from Python | Rust and C have it since Phase 4 (`tft_plan_at_with_derivatives`, unstable tier); `PHASE4.md` §0 scoped Python out | **Phase 5**, as `Layout::QuatTwist` — see below |
| `Publisher` holds an extended borrow by hand | `tf_tree_py/src/tree.rs` | [`0017`](./decisions/0017-owned-handles-and-the-lifetime-rule.md) |

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

**Flagged, not yet fixed.** `PHASE3.md` §6.1 sets
`NS_PER_STEP_ESTIMATE = 55`, derived from the Phase 1 lookup benchmark.
[`0013`](./decisions/0013-the-benchmark-gate-never-interpolated.md) shows that
benchmark queried on-grid stamps, so `I::eval` never ran; the honest depth-3
cost is ~290 ns, i.e. ~97 ns/step.

At 55 ns/step the release crossover is n ≈ 6; at 97 ns/step it is n ≈ 4.

**Nothing is broken, and that is the interesting part.** §6.1's own claim was
that "the exact constant does not need tuning; what matters is that neither
branch is ever badly wrong", and a 1.8× error in the constant moves the
crossover by two elements. The design absorbed a wrong input, which is the
strongest evidence it is right.

**NORMATIVE:** when `0013` re-baselines, `NS_PER_STEP_ESTIMATE` is re-derived
from the new number in the same commit, and `PHASE3.md` §6.1 gains a line saying
which measurement it came from. A constant with no cited source is how this
happened.

---

## 4. C and C++

Both are specified in `PHASE4.md` §3–§4 and implemented. Restated here only
where they carry a rule the other surfaces must also obey.

**Two tiers of header, and the split is the stability promise.** `tf_tree.h` is
semver'd; `tf_tree_unstable.h` is opt-in by macro and promises nothing. This is
the model §2.6 defers for Rust.

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
failure rather than a first-message one. Both types now exist (§2.5); what the
bridge still carries is a bare `u8` tag, so §5.5 is true of the *number* and not
yet of the name. **The read side is not yet specified,
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
| 1 | `Tree::claim_owned` → `OwnedWriter`; delete the PyO3 and C ABI lifetime extensions | Rust, Python, C | [`0017`](./decisions/0017-owned-handles-and-the-lifetime-rule.md) | its own plan |
| 2 | `Arc<Tree>` documented as the embedding idiom | Rust (docs only) | §2.2 | `0017` step 8 |
| 3 | `#[inline]` on the fold; LTO guidance; a cross-crate bench row gated at 5% | Rust | §2.3 | Phase 5 bench artifact (`PHASE5.md` §9.2) |
| 4 | `# Stability` headings on CLI-facing exports; `unstable` tier deferred | Rust (docs only) | §2.6 | any time; blocks a published tag |
| 5 | Per-edge nominal rate reachable from a plan (`Plan::span` already ships) | Rust core | [`0018`](./decisions/0018-blocking-waits-belong-in-the-shim.md) | its own plan |
| 6 | No blocking primitive in the arena; the escalation path recorded | all | [`0018`](./decisions/0018-blocking-waits-belong-in-the-shim.md) | recorded, not built |
| 7 | `Layout::QuatTwist`; derivatives reach Python and C | core, Python, C | §3.3 | `PHASE5.md` §4 |
| 8 | `tree.frames()`, `tree.edges()`, `plan.edges()` | Python | §3.2 | `PHASE5.md` §4.2 |
| 9 | `from_parts` / `from_timespec` / `from_ros` | Rust, Python, C | §5.1 | `PHASE5.md` §4 |
| 10 | `NS_PER_STEP_ESTIMATE` re-derived when `0013` re-baselines | Python | §3.4 | `0013`'s re-baseline commit |
| 11 | Clock-step `doctor` check (`TFT019`) + runbook row | CLI | §5.3 | `PHASE5.md` §6 |
| 12 | The shim's query domain from `rcl_clock_type_t` | shim | [`PHASE7.md`](./PHASE7.md) §4 J9 | Phase 7, gated by D21 |

Items 2, 4, 8, 9 and 11 are additive and independent. Items 1, 5 and 7 touch
core and are the ones to sequence. Item 12 is gated by D21 and must not be
started before `PHASE7.md` §0.0's four gates are met.

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
