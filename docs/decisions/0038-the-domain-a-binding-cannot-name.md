# 0038: the domain a binding cannot name

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** #278

## Decision

**A binding carries the query domain as a runtime tag, checked once at plan time, and the tag lives on the binding's plan handle rather than on each call.**

### 1. `tf_tree_core` grows a tag-taking sibling for each query shape

`Plan::check_domain::<D>()` becomes `Plan::check_domain_tag(u8)`; typed entry points delegate with `D::TAG`. Five additive methods take `domain: u8`: `at_tagged`, `at_with_derivatives_tagged`, `at_many_into_tagged`, `at_many_into_f32_tagged`, `at_adaptive_tagged`. `Tree::lookup_tagged` mirrors `Tree::lookup`.

### 2. The C ABI adds one function and changes no existing signature

`tft_tree_plan_in_domain(tree, target, source, uint8_t domain, out)`. `tft_plan` gains a `domain: u8` field validated at creation against `Plan::domain()`. `tft_plan_create` is the same call with `domain = 0`; `tft_plan_at*` route through the tagged core methods.

### 3. Python takes a keyword with a default

`Tree.plan(target, source, domain=0)`; `tf_tree.SYSTEM_DOMAIN`/`SENSOR_DOMAIN`/`SIM_DOMAIN`/`STEADY_DOMAIN` are plain ints; `TFT_ERR_TIME_DOMAIN`'s prose gains the remedy.

### 4. The check moves, it does not disappear

`check_domain_tag` runs on the same condition as before (`has_dynamic()`) and returns the same `LookupError::TimeDomainMismatch { expected, got }`.

## Rationale

The default stays `0`, explicit and loudly wrong for a sim arena; defaulting to the plan's own domain would delete the check D9 protects.

## Consequences

- The default stays `0`; defaulting to the plan's own domain would delete the check D9 protects.

## Implementation plan

1. `check_domain::<D>()` to `check_domain_tag(u8)`.
2. The five `*_tagged` methods plus `Tree::lookup_tagged`; a test that a `SensorDomain` plan answers `at_tagged(.., 1)` and refuses `at_tagged(.., 0)`.
3. `tft_plan.domain` and `tft_tree_plan_in_domain` in `crates/tf_tree_c/src/lib.rs` and both headers; a C test with a tag-1 arena.
4. Python `domain=0`, the constants, and `tests/python/test_domains.py`.
5. `docs/API.md` §2.5/§3.3, `docs/PHASE4.md` §5.5, the `ros/tf_tree_ros` warning.

## Open questions

None.
