# 0041: Python declares a topology the way everything else does

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** #279

## Decision

**Python accepts the bridge's topology config wherever it accepts a list of edge pairs.** `tf_tree.build`'s `edges` and `tf_tree.open`'s `create` accept a list of `(parent, child)` pairs or a `str` of `tf_tree_bridge::config::TopologyConfig` text. No Python `Builder` (`docs/PROJECT.md` §6). `capacity=` and `interp=` are refused alongside a config; text, not a path.

## Consequences

- One schema serves the ROS bridge, the CLI and Python (`docs/API.md` §3).

## Implementation plan

Step 1: `build` accepts `str`; a test asserts a static edge folds.

Step 2: `open(create=...)` accepts the same; the static mount is absent from the created arena's plan edges.
