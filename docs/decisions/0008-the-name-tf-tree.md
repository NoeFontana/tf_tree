# 0008: Keep the name `tf_tree`

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** the §10 open-source-readiness PR

## Context

`docs/PHASE5.md` §10 opens with "**Name check before anything else.** Confirm
`tf_tree` is available on crates.io and PyPI, and decide deliberately whether the
proximity to ROS's `tf` / `tf2` package names helps discovery or invites
confusion. Renaming after 1.0 is not an option; renaming now is an afternoon."

Nothing is published yet, so the afternoon is still available. It stops being
available the moment the first crate goes to crates.io.

### Availability, measured 2026-07-27

Checked against the **sparse index** (`index.crates.io`), not the `crates.io` web
API — the web API returns HTTP 403 to this environment for *every* crate,
including `serde`, so a 403 there is evidence of nothing. The sparse index
answered `serde` with 200, which is the control that makes the 404s below mean
"absent" rather than "unreachable".

| Name | Registry | Result |
|---|---|---|
| `serde` (control) | crates.io index | **200** — the probe works |
| `tf_tree` | crates.io index | 404 — free |
| `tf-tree` | crates.io index | 404 — free (crates.io treats `-` and `_` as colliding, so both spellings had to be checked) |
| `tf_tree_math`, `tf_tree_arena`, `tf_tree_core`, `tf_tree_ipc`, `tf_tree_c` | crates.io index | 404 — free |
| `numpy` (control) | PyPI | **200** — the probe works |
| `tf_tree` / `tf-tree` | PyPI | 404 — free (PEP 503 normalises both to `tf-tree`) |

Reproduce:

```sh
curl -s -o /dev/null -w '%{http_code}\n' https://index.crates.io/se/rd/serde      # 200
curl -s -o /dev/null -w '%{http_code}\n' https://index.crates.io/tf/_t/tf_tree    # 404
curl -s -o /dev/null -w '%{http_code}\n' https://pypi.org/pypi/numpy/json         # 200
curl -s -o /dev/null -w '%{http_code}\n' https://pypi.org/pypi/tf-tree/json       # 404
```

Availability is a snapshot, not a reservation. Neither registry holds a name for
anyone, so the only thing that secures these is publishing.

## Decision

**Keep `tf_tree`, for the Rust crates and for the Python distribution, and
publish the whole `tf_tree_*` family together so the prefix is not split across
owners.**

Not decided here, and deliberately left open: the Python *import* name. The
distribution is `tf_tree`; whether `import tf_tree` shadows anything in a ROS
workspace is a Phase 3 packaging question with its own evidence.

## Rationale

The proximity to `tf` / `tf2` is the point, and it is the kind of proximity that
helps rather than misleads.

- **Discovery.** The audience is people who already know what a transform tree
  is and are unhappy with `tf2`'s behaviour under load. `tf` is the word they
  search. A name with no lexical overlap — `frames`, `posegraph`, `rigid` —
  would have to buy that recognition back with marketing this project does not
  have.
- **It does not claim to be tf2, and cannot be mistaken for it.** ROS package
  names are `tf2`, `tf2_ros`, `tf2_geometry_msgs`, `tf2_py` — a `tf2_` prefix and
  a `2`. `tf_tree` is neither a member of that family nor a plausible successor
  name for one. The dangerous name would have been `tf3`, which reads as an
  official next version; that name is rejected here explicitly.
- **The confusion that does exist is honest confusion.** Someone who finds
  `tf_tree` while looking for `tf2` has found a thing that does the same job
  differently, which is what they wanted. `README.md` states the relationship in
  the first two paragraphs rather than leaving it implied.
- **It is already the name.** It is in the workspace, every crate name, every
  document, `FORMAT_VERSION`'s owner, the CLI binary and its `tft` alias, and
  seven decision records. Renaming is an afternoon of edits and a permanent tax
  on every historical link.

### Alternatives considered

- **`tf3`** — rejected. It reads as an official ROS successor, which would be a
  misrepresentation, and it also invites a trademark conversation with Open
  Robotics that the project has no reason to start.
- **A name with no `tf`** (`framestore`, `posetree`, `rigidtree`) — rejected. It
  buys distance from a confusion that is not actually harmful, and pays for it
  with the discovery the project most needs.
- **A scoped/prefixed name** (`nf-tf-tree`) — rejected. crates.io has no scopes;
  a hand-rolled prefix reads as a vendored fork rather than a project.

## Consequences

- The name is settled. A later rename is a breaking change to the crate names,
  the Python distribution, the CLI binary, the shared-memory runtime directory
  and every document, and this record is the answer to "should we rename".
- `README.md` and the crate descriptions must keep stating the relationship to
  `tf2` explicitly, since the name alone will keep attracting people who assume
  a ROS lineage.
- The `tf_tree_*` prefix should be published as one family, in one go. Publishing
  `tf_tree` while leaving `tf_tree_arena` free lets a third party own part of the
  namespace.
- The availability table above expires. Re-run the probes immediately before the
  first publish; do not treat this record as a reservation.

## Implementation plan

1. Record the decision — this document. Verified by its own presence and the
   reproduction commands above.
2. State the `tf2` relationship in `README.md`'s opening. Verified by reading it.
3. Re-run the availability probes immediately before the first `cargo publish`.
   Verified by the commands in *Context* returning 404 for every `tf_tree*` name
   and 200 for the controls.
4. Publish the `tf_tree_*` crates in dependency order in one session. Verified by
   `cargo publish --dry-run` per crate, then the index returning 200 for each.

## Open questions

None. (The Python import name is out of scope for this record, not unresolved
within it — see *Decision*.)
