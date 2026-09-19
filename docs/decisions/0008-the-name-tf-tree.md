# 0008: Keep the name `tf_tree`

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** the §10 open-source-readiness PR

## Context

`docs/PHASE5.md` §10 requires a name check: confirm `tf_tree` is available on
crates.io and PyPI, and decide whether proximity to ROS's `tf` / `tf2` helps
discovery or invites confusion.

`tf_tree` and every `tf_tree_*` crate were free on crates.io. **`tf_tree` could not
be registered on PyPI**: its similarity check strips separators and `tf_tree`
collides with `tftree`. The distribution is **`transform_tree`**; the import name
is unchanged.

## Decision

**Keep `tf_tree` for the Rust crates (and the import name), and publish the whole
`tf_tree_*` family together so the prefix is not split across owners.** The Python
distribution is `transform_tree`.

## Rationale

The audience searches for `tf`; `README.md` states the relationship. `tf3` reads as
an official successor.

## Consequences

- A rename is breaking for the crates, the Python distribution, the CLI, the
  runtime directory and every document.
- Availability is a snapshot: re-run the probes immediately before publishing.

## Implementation plan

1. Record the decision; state the `tf2` relationship in `README.md`'s opening.
2. Re-run the availability probes immediately before the first `cargo publish`.
3. Publish the `tf_tree_*` crates in dependency order in one session.

## Open questions

None.
