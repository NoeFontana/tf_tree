# 0008: Keep the name `tf_tree`

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** the §10 open-source-readiness PR

## Context

`docs/PHASE5.md` §10 opens with "**Name check before anything else.** Confirm
`tf_tree` is available on crates.io and PyPI, and decide deliberately whether the
proximity to ROS's `tf` / `tf2` package names helps discovery or invites
confusion. Renaming after 1.0 is not an option; renaming now is an afternoon."

### Availability, measured 2026-07-27

Checked against the **sparse index** (`index.crates.io`), since the crates.io web
API returns 403 to this environment for every crate; `serde` answering 200 is the
control that makes a 404 mean "absent".

| Name | Registry | Result |
|---|---|---|
| `tf_tree`, `tf-tree` (both spellings collide), `tf_tree_math`, `tf_tree_arena`, `tf_tree_core`, `tf_tree_ipc`, `tf_tree_c` | crates.io index | 404 — free |
| `tf_tree` / `tf-tree` | PyPI | 404 — **unregistered, which did not mean available** (see the correction) |

```sh
curl -s -o /dev/null -w '%{http_code}\n' https://index.crates.io/se/rd/serde      # 200
curl -s -o /dev/null -w '%{http_code}\n' https://index.crates.io/tf/_t/tf_tree    # 404
curl -s -o /dev/null -w '%{http_code}\n' https://pypi.org/pypi/numpy/json         # 200
curl -s -o /dev/null -w '%{http_code}\n' https://pypi.org/pypi/tf-tree/json       # 404
```

Availability is a snapshot, not a reservation; only publishing secures a name.

## Correction (2026-08-16): the PyPI probe was right and the inference was wrong

**`tf_tree` could not be registered on PyPI.** The upload was refused as too
similar to an existing project, and the distribution is now **`transform_tree`**.
The *import* name is unchanged (`import tf_tree`), an ordinary split
(`pillow`/`PIL`); crates.io is unaffected. The probe answers *"is this name
registered?"*, not *"will PyPI accept it?"*: PyPI's upload-time similarity check
strips separators, so `tf_tree` → `tf-tree` → `tftree`, which exists.

```sh
curl -s -o /dev/null -w '%{http_code}\n' https://pypi.org/pypi/tf-tree/json     # 404 — unregistered
curl -s -o /dev/null -w '%{http_code}\n' https://pypi.org/pypi/tftree/json      # 200 — the neighbour
```

## Decision

**Keep `tf_tree` for the Rust crates (and the import name), and publish the whole
`tf_tree_*` family together so the prefix is not split across owners.** The Python
distribution is `transform_tree` (see the correction).

## Rationale

The proximity to `tf` / `tf2` helps rather than misleads: the audience searches for
`tf`; `tf_tree` is not in the `tf2`, `tf2_ros` family and cannot be mistaken for it;
`README.md` states the relationship in its first two paragraphs; and it is already
the name everywhere. **`tf3` is rejected** (reads as an official successor). A name
with no `tf` loses discovery, and a prefixed name reads as a vendored fork.

## Consequences

- The name is settled; a rename is breaking for the crates, the Python distribution,
  the CLI, the runtime directory and every document.
- `README.md` keeps stating the relationship to `tf2`, and the `tf_tree_*` prefix is
  published as one family in one session.
- The availability table expires: re-run the probes immediately before publishing.

## Implementation plan

1. Record the decision; state the `tf2` relationship in `README.md`'s opening.
2. Re-run the availability probes immediately before the first `cargo publish`.
3. Publish the `tf_tree_*` crates in dependency order in one session (`cargo publish
   --dry-run` per crate, then the index returning 200).

## Open questions

None.
