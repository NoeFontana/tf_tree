# 0038: the domain a binding cannot name

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** #278

## Context

[`Domain`](../../crates/tf_tree_core/src/plan.rs) is an **open trait**. `SystemDomain`,
`SensorDomain`, `SimDomain` and `SteadyDomain` hold tags `0`–`3`, and its own doc
comment invites a user to declare a fifth: *"a driver with a PTP-disciplined clock
declares `struct PtpDomain;` rather than pretending to be one of these"*
(`docs/API.md` §2.5). The tag is a `const` on the trait, so a query's domain is a
*compile-time* fact in Rust — which is D9 working exactly as intended.

**Neither binding can express that, and both currently pretend the answer is
always `0`.** Every query site in `tf_tree_c` and `tf_tree_py` constructs
`Stamp::<SystemDomain>` — nine sites in `crates/tf_tree_c/src/{lib.rs,unstable.rs}`
and eight in `crates/tf_tree_py/src/tree.rs`. `Plan::check_domain` compares
`D::TAG` against the plan's stored domain, so on any arena whose edges are not
tag `0`, **every C, C++ and Python query fails `TimeDomainMismatch`, permanently
and by construction.** There is no argument the caller can pass to say otherwise;
the C header does not mention a domain in any signature.

That is not a hypothetical arena. `ros/tf_tree_ros/src/bridge_node.cpp:164` warns
the operator to configure one:

> `use_sim_time` is true but `time_domain` is 0, the same tag a real-time bridge
> uses. A consumer querying this arena cannot be told the difference; give the
> simulated tree its own domain (`docs/PHASE4.md` §5.5).

So the project instructs an operator to do the one thing that makes the arena
unreadable from the two languages a robot node is written in. Following our own
advice breaks the deployment. `docs/PHASE4.md` §5.5's domain agreement and the
`TFT_ERR_TIME_DOMAIN` status code both exist; what is missing is any way for a
foreign caller to *satisfy* them.

## Decision

**A binding carries the query domain as a runtime tag, checked once where it is
cheap to check, and the tag lives on the binding's plan handle rather than on
each call.**

### 1. `tf_tree_core` grows a tag-taking sibling for each query shape

`Plan::check_domain::<D>()` becomes `Plan::check_domain_tag(u8)`, and each typed
entry point becomes a one-line delegation that passes `D::TAG`. The typed form
stays the Rust surface and stays the default — a domain mistake there is a
*compile* error, which is D9's whole value and is not being weakened. The tagged
form is additive:

```rust
impl Plan {
    pub fn at_tagged(&self, g: &Guard, nanos: i64, domain: u8) -> Result<Iso3, LookupError>;
    pub fn at_with_derivatives_tagged(&self, g: &Guard, nanos: i64, domain: u8)
        -> Result<Sampled, LookupError>;
    pub fn at_many_into_tagged(&self, g: &Guard, stamps: &[i64], domain: u8,
        layout: Layout, out: &mut [f64]) -> Result<(), LookupError>;
    pub fn at_many_into_f32_tagged(&self, g: &Guard, stamps: &[i64], domain: u8,
        layout: Layout, out: &mut [f32]) -> Result<(), LookupError>;
    pub fn at_adaptive_tagged<'s, D: Domain>(&self, g: &Guard, span: (Stamp<D>, Stamp<D>),
        domain: u8, tol: ErrBound, scratch: &'s mut AdaptiveScratch<D>)
        -> Result<(&'s [Stamp<D>], &'s [Iso3]), LookupError>;
}
```

Five, because there are five query shapes. Each is three lines. The batch forms
already take `&[i64]`, so their type parameter was *only* ever the domain check.

**`at_adaptive_tagged` keeps a type parameter, and it means something different
there.** Its `D` fixes the element type of the caller's `AdaptiveScratch` and of
the returned stamp slice; the fold never reads `D::TAG`. So `D` is *storage* and
`domain` is the *query*, and a binding passes the one type it can name
(`SystemDomain`) with the real tag as data. The alternative — erasing the phantom
so the tagged form could return `&[i64]` — needs either a second public marker
domain, a `#[repr(transparent)]` slice cast (unsafe, in a file the unsafe budget
does not cover), or dropping `Stamp<D>` from `at_adaptive`'s return, which
deletes a D9 guarantee from the *typed* path to serve the untyped one. Carrying
one documented phantom is the smallest of the four.

`Tree::lookup_tagged` mirrors `Tree::lookup` in the facade for the same reason.

### 2. The C ABI adds one function and changes no existing signature

The ABI is frozen at 1.0 (`docs/PHASE4.md` §7), so nothing already declared may
move. It does not have to:

```c
tft_status tft_tree_plan_in_domain(const tft_tree *tree, const char *target,
                                   const char *source, uint8_t domain,
                                   tft_plan **out);
```

`tft_plan` gains a `domain: u8` field, set at creation and validated *there*
against `Plan::domain()` — so a mismatch is reported once, at plan time, with the
frame names still in hand, instead of on every lookup in the hot loop.
`tft_plan_create` keeps its meaning exactly: it is `tft_tree_plan_in_domain` with
`domain = 0`. Every existing evaluate entry point (`tft_plan_at`,
`tft_plan_at_many`, `tft_plan_at_with_derivatives`) then routes through the
tagged core method with the handle's tag. **No caller recompiles, and no
signature changes.**

### 3. Python takes a keyword with a default

`Tree.plan(target, source, domain=0)`, and `TFT_ERR_TIME_DOMAIN`'s prose gains
the remedy. `tf_tree.SYSTEM_DOMAIN`/`SENSOR_DOMAIN`/`SIM_DOMAIN`/`STEADY_DOMAIN`
are exported as plain ints so a user writes a name rather than a magic number,
and a user-declared tag is just the integer they chose.

### 4. The check moves, it does not disappear

`check_domain_tag` runs on the same condition as today (`has_dynamic()`) and
returns the same `LookupError::TimeDomainMismatch { expected, got }`. What
changes is only *where the tag comes from*. There is no path that skips the
comparison, and none is added: an "I already checked" fast path would be the
footgun this record exists to remove, not a smaller version of it.

## Rationale

**Why not dispatch over the four built-in domains in each binding?** Because the
trait is open. A `match tag { 0 => at::<SystemDomain>, … }` cannot serve the PTP
driver `Domain`'s own documentation invites, so it would ship a surface that is
correct for exactly the users who did not need it. It also puts a five-arm match
on the hot path in two languages.

**Why not a domain-erased `Stamp`?** `Stamp<D>` is documented and tested at
`size_of == 8` with the domain carried by a `PhantomData`. A variant that stores
its tag is 16 bytes, and it would be the type every binding uses — so the
zero-cost claim would hold only for the callers who are already fine.

**Why the handle and not the call?** Three reasons, in order of weight. The ABI
is frozen, and putting the tag on the call means new spellings of three functions
rather than one. A domain is a property of a route through the tree, not of an
instant — it cannot legitimately vary between two queries on one plan. And
checking at plan time is where the frame *names* are still available, so the
diagnostic can say which route disagreed rather than only which two integers did.

**Why not make the binding default to the plan's own domain instead of `0`?**
That is the tempting one-line fix and it is wrong: it would make every existing
caller silently correct and every *mistaken* caller silently correct too,
deleting the check for the population D9 exists to protect. The default stays
`0` — explicit, wrong for a sim arena, and loudly so.

## Consequences

- The binding surface grows by one C function, one Python keyword, four exported
  integer constants, and five tagged core methods that are delegations. That is
  API growth in a project trying to shrink, and it is accepted because the
  alternative is a shipped configuration that cannot be read.
- `docs/PHASE4.md` §5.5's domain agreement becomes satisfiable from C for the
  first time; the `ros/tf_tree_ros` warning stops being advice that breaks the
  system that follows it.
- `Domain::TAG` stays a `const` and the Rust surface stays compile-time checked.
  This record does not weaken D9; it gives the dynamic half of the world the same
  check by a different route.
- Two spellings of each query shape now exist in core. `docs/PROJECT.md` §6 warns
  against a second spelling of an existing path, and this is deliberately one:
  the rule's target is a *convenience alias* that re-resolves work the first
  spelling already did, and these differ in where a value comes from, not in what
  they do. The tagged form is documented as the binding surface and the typed
  form as the Rust one, so the choice is not left to taste.

## Implementation plan

1. `check_domain::<D>()` → `check_domain_tag(u8)`; typed entry points delegate.
   Verified by the existing domain tests continuing to pass unchanged
   (`crates/tf_tree_core/src/tests.rs`) — this step is behaviour-preserving.
2. The five `*_tagged` methods on `Plan`, plus `Tree::lookup_tagged`. Verified by
   a test that a `SensorDomain` plan answers `at_tagged(.., 1)` and refuses
   `at_tagged(.., 0)` with `TimeDomainMismatch { expected: 1, got: 0 }`.
3. `tft_plan` gains `domain`; `tft_tree_plan_in_domain` added to
   `crates/tf_tree_c/src/lib.rs` and both headers; the three evaluate entry
   points route through the tagged methods. Verified by a C test that opens a
   tag-1 arena, plans in domain 1, and reads a transform — which fails today.
4. Python `Tree.plan(..., domain=0)` and the four exported constants. Verified by
   a pytest that reproduces step 3 through the Python API.
5. `docs/API.md` §2.5/§3.3 and `docs/PHASE4.md` §5.5 record the binding surface;
   the `ros/tf_tree_ros` warning gains the remedy it currently lacks. Verified by
   `just artifact-versions` (every `just <recipe>` reference resolves) and by
   re-reading the warning against the header.

## Open questions

None. Three were resolved while writing:

- *Runtime tag or compile-time const?* **Both** — the const for Rust, the tag for
  bindings. They are not alternatives; the const cannot cross an ABI.
- *Are tags 4+ reserved for users?* Yes, and `Domain`'s doc comment already says
  so. Nothing in this record allocates a new built-in tag.
- *Should `tft_plan_create` be deprecated in favour of the new spelling?* No. It
  is the correct call for a tag-0 arena, which is most of them, and the ABI is
  frozen.
