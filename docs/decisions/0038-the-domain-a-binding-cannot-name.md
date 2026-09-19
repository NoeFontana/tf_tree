# 0038: the domain a binding cannot name

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** #278

## Context

[`Domain`](../../crates/tf_tree_core/src/plan.rs) is an open trait; a query's domain is a compile-time fact in Rust (D9). Every C and Python query site constructed `Stamp::<SystemDomain>`, so on any arena whose edges are not tag `0`, every C, C++ and Python query failed `TimeDomainMismatch` permanently, while `ros/tf_tree_ros`'s bridge tells operators to configure a non-zero `time_domain` (`docs/PHASE4.md` §5.5).

## Decision

**A binding carries the query domain as a runtime tag, checked once where it is cheap to check, and the tag lives on the binding's plan handle rather than on each call.**

### 1. `tf_tree_core` grows a tag-taking sibling for each query shape

`Plan::check_domain::<D>()` becomes `Plan::check_domain_tag(u8)`, and each typed entry point delegates with `D::TAG`. The typed form stays the Rust surface and a domain mistake there stays a compile error. Five additive methods, one per query shape: `at_tagged`, `at_with_derivatives_tagged`, `at_many_into_tagged`, `at_many_into_f32_tagged`, `at_adaptive_tagged`, each taking `domain: u8`.

`at_adaptive_tagged` keeps a type parameter `D` that is *storage* (the element type of the caller's `AdaptiveScratch` and the returned stamp slice; the fold never reads `D::TAG`); a binding passes `SystemDomain` with the real tag as data. `Tree::lookup_tagged` mirrors `Tree::lookup` in the facade.

### 2. The C ABI adds one function and changes no existing signature

```c
tft_status tft_tree_plan_in_domain(const tft_tree *tree, const char *target,
                                   const char *source, uint8_t domain,
                                   tft_plan **out);
```

`tft_plan` gains a `domain: u8` field, validated at creation against `Plan::domain()` so a mismatch is reported once, at plan time, with the frame names in hand. `tft_plan_create` is `tft_tree_plan_in_domain` with `domain = 0`. `tft_plan_at`, `tft_plan_at_many` and `tft_plan_at_with_derivatives` route through the tagged core methods with the handle's tag.

### 3. Python takes a keyword with a default

`Tree.plan(target, source, domain=0)`; `TFT_ERR_TIME_DOMAIN`'s prose gains the remedy. `tf_tree.SYSTEM_DOMAIN`/`SENSOR_DOMAIN`/`SIM_DOMAIN`/`STEADY_DOMAIN` are exported as plain ints.

### 4. The check moves, it does not disappear

`check_domain_tag` runs on the same condition as before (`has_dynamic()`) and returns the same `LookupError::TimeDomainMismatch { expected, got }`. No path skips the comparison, and none is added.

## Rationale

- No dispatch over the four built-in domains: the trait is open, and a match would sit on the hot path.
- No domain-erased `Stamp`: `Stamp<D>` is `size_of == 8`.
- The tag lives on the handle: the ABI is frozen, a domain is a property of a route rather than an instant, and plan time still has the frame names.
- The default stays `0`, explicit and loudly wrong for a sim arena; defaulting to the plan's own domain would delete the check D9 protects.

## Consequences

- One C function, one Python keyword, four constants and five tagged core methods; `docs/PHASE4.md` §5.5's domain agreement is satisfiable from C.
- Two spellings of each query shape exist in core, deliberately: they differ in where the tag comes from. The tagged form is the binding surface, the typed form the Rust one.

## Implementation plan

1. `check_domain::<D>()` to `check_domain_tag(u8)`; typed entry points delegate (`crates/tf_tree_core/src/tests.rs` unchanged).
2. The five `*_tagged` methods plus `Tree::lookup_tagged`; test that a `SensorDomain` plan answers `at_tagged(.., 1)` and refuses `at_tagged(.., 0)` with `TimeDomainMismatch { expected: 1, got: 0 }`.
3. `tft_plan.domain` and `tft_tree_plan_in_domain` in `crates/tf_tree_c/src/lib.rs` and both headers; a C test with a tag-1 arena.
4. Python `Tree.plan(..., domain=0)`, the constants, and a pytest reproducing step 3 (`tests/python/test_domains.py`).
5. `docs/API.md` §2.5/§3.3, `docs/PHASE4.md` §5.5 and the `ros/tf_tree_ros` warning record the surface.

## Open questions

None. Tags 4+ are reserved for users; `tft_plan_create` is not deprecated.
