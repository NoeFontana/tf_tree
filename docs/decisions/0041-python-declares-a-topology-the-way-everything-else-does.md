# 0041: Python declares a topology the way everything else does

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** #279

## Decision

**Python accepts the bridge's topology config wherever it accepts a list of edge pairs.** `tf_tree.build`'s `edges` and `tf_tree.open`'s `create` each accept either the existing list of `(parent, child)` pairs or a `str` of topology-config text (`tf_tree_bridge::config::TopologyConfig`), giving static edges, per-edge capacity, rate, interp and domain. No new vocabulary, no Python `Builder` (a third spelling of one declaration surface, `docs/PROJECT.md` §6).

1. `capacity=` and `interp=` are refused alongside a config; the list form keeps them.
2. Text, not a path: file loading stays on the Python side.
3. A `str` is never a valid edge list, so dispatch needs no second keyword.

`tf_tree_py` gains a path dependency on `tf_tree_bridge` (hand-written parser, no `serde`/`toml`, no `target_os`, no `shm`).

## Consequences

- One schema serves the ROS bridge, the CLI and Python; a change to it is a change to all three (`docs/API.md` §3).
- `ConfigError` borrows the config text, so the binding renders it to a Python exception while the text is alive.
- A malformed config raises with the offending frame named.

## Implementation plan

Step 1: `build` accepts `str`; test asserts a static edge folds (`plan.len()` below the frame count).

Step 2: `open(create=...)` accepts the same. Verified by the static mount being absent from the created arena's plan edges (`nominal_rate_mhz` is not observable from Python; it is `TFT007`'s input).
