# 0041: Python declares a topology the way everything else does

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** (filled in as work lands)

## Context

Python can build a tree two ways, and both take the same impoverished shape:

```python
tf_tree.build(edges=[("map", "odom"), ("odom", "base_link")], capacity=1024, interp="sclerp")
tf_tree.open(create=[("map", "odom")], capacity=1024, ...)
```

A list of `(parent, child)` pairs and **one global capacity**. Which means a
Python-built tree has:

- **no static edges.** Every edge is dynamic, so a sensor mount — a constant —
  becomes a ring buffer somebody has to publish into forever. That is the
  latched-topic behaviour `docs/PROJECT.md` §2 lists as one of the ten `tf2`
  problems this engine exists to solve, and `TreeBuilder::static_edge` folds it
  into a constant at plan time. A Python-built tree cannot reach that win.
- **no per-edge capacity.** One number sizes a 1 kHz IMU edge and a 10 Hz map
  edge together, so it either starves the fast one or wastes memory on the slow
  one at every slot.
- **no declared rate.** `EdgeRecord::nominal_rate_mhz` is the *only* evidence
  `tf_tree doctor`'s `TFT007` has that an observed rate is wrong rather than
  merely what it is. An edge sized by capacity declares no rate, so `TFT007`
  skips it — a Python-built arena is undiagnosable on that check by construction.
- **no per-edge domain or interp**, which `0038` just spent a decision making
  reachable everywhere else.

**Meanwhile the project already has a complete declaration format, and two
consumers of it.** `crates/tf_tree_bridge/src/config.rs` defines a topology
config — static edges with poses, per-edge `rate_hz` + `history_secs` or
`capacity`, per-edge `interp` and `domain`, plus `frames` and `frame_headroom` —
with `TopologyConfig::parse` and `TopologyConfig::builder() -> TreeBuilder` both
public. `ros/tf_tree_ros` reads one to start a bridge; `tf_tree_cli` reads one and
can *write* one from a recording (`tf_tree topology --discover`). Python is the
only surface that cannot.

## Decision

**Python accepts the same topology config, wherever it currently accepts a list
of edge pairs.** No new declaration vocabulary, and no new named function:
`build`'s `edges` and `open`'s `create` each accept **either** the existing list
of `(parent, child)` pairs **or** a `str` holding topology-config text.

```python
cfg = pathlib.Path("robot_topology.toml").read_text()

tree = tf_tree.build(cfg)                       # heap tree, full topology
tree = tf_tree.open(create=cfg, mode="rw")      # shared arena, full topology
```

Three consequences of that shape, each deliberate:

1. **`capacity=` and `interp=` are refused alongside a config**, with an error
   saying the config carries them. Accepting both would make it ambiguous which
   won, and the config's own `[topology] interp` and per-edge sizing already say
   it. The list form keeps them, unchanged.
2. **Text, not a path.** Python opens its own files better than this binding
   would, and taking text keeps encoding, `pathlib`, and packaged-resource
   loading on the Python side where they belong. It also makes the surface
   trivially testable without a filesystem.
3. **A `str` is never a valid edge list**, so the dispatch is unambiguous and
   needs no second keyword. This is one parameter with two accepted types — the
   `open(file_or_path)` idiom — not two spellings of one path.

`tf_tree_py` gains a path dependency on `tf_tree_bridge`. That crate's only
dependency is `tf_tree`; its config parser is hand-written, with no `serde` and
no `toml` crate, and it carries no `target_os` and no `shm`, so nothing
third-party and nothing Linux-only enters the wheel.

## Rationale

**Why not a Python `Builder` mirroring `TreeBuilder`** — which is what this
record was expected to propose, and what `docs/API.md` §3's "mirror, plus
conveniences" would suggest.

Because it would be the **third** spelling of one declaration surface, and
`docs/PROJECT.md` §6 names exactly that as a design smell: *"adding a second
spelling of an existing path … instead of documenting the one that exists"*. A
Python builder would need its own validation, its own error prose, and its own
tests, and it would drift from the schema the CLI *emits* — so
`tf_tree topology --discover > t.toml` would produce a file Python could not
consume, which is the most useful thing in the whole loop.

There is a deeper reason. **A robot's topology is deployment configuration, not
program text.** It is the same on every process on the robot, it is what an
integrator edits without recompiling, and it is what the bridge is already
started from. Encoding it in Python literals inside a dataloader means the
dataloader and the robot can disagree, silently, about the tree they are talking
about.

**Why not move `config.rs` somewhere more neutral.** It would be tidier —
`tf_tree_bridge` is now a dependency of things that do no bridging. But moving a
module across a crate boundary is its own decision with its own version-skew and
naming questions, and the dependency it saves is a path dependency on a
`publish = false` crate with one dependency of its own. Recorded as the cost it
is; if a third non-bridge consumer appears, revisit it then rather than now.

**Why `build` keeps the list form.** `build([("a","b")], capacity=64)` is the
right amount of ceremony for a test that needs two frames and no rates, and it is
what every existing example and test uses. Removing it would be a breaking
change that buys nothing.

## Consequences

- A Python-built tree can carry static edges, per-edge sizing, declared rates,
  and per-edge domains — so `TFT007` can diagnose one, and a mount costs the
  query path nothing.
- One schema now serves the ROS bridge, the CLI and Python. A change to it is a
  change to all three, which is the point and also the risk: the config format
  becomes load-bearing for a published wheel, so it stops being the bridge's
  private business. `docs/API.md` §3 records that.
- `ConfigError` borrows from the config text (deliberately, so it can name the
  offending frame with no allocation). The binding must therefore render it to a
  Python exception **while the text is still alive**, which is a constraint on
  the implementation rather than on the surface.
- The wheel gains one path dependency and no third-party code.

## Implementation plan

1. `tf_tree_py` depends on `tf_tree_bridge`; `build` accepts `str` for `edges`
   and refuses `capacity=`/`interp=` beside it. Verified by a test that builds a
   tree with a static edge from config text and asserts the plan folds it —
   `plan.len()` is smaller than the frame count, which a dynamic edge could not
   produce.
2. `open(create=...)` accepts the same. **Correction to this plan, made while
   implementing:** it said to read a declared rate back out, and Python has no
   accessor for `EdgeRecord::nominal_rate_mhz` — `Plan.edges()` is names only, by
   design — so that is not observable from this surface and asserting it would be
   asserting something the test cannot see. Verified instead by the property that
   *is* observable and that no value of `capacity=` on the list form can produce:
   the static mount is absent from the edges the created arena's plan samples, so
   the config reached `layout_if_creating`. The rate becomes observable in
   `doctor`'s `TFT007`, which is a Rust-side check.
3. A malformed config raises with the offending frame named, not a generic
   parse error. Verified against a config declaring both `capacity` and
   `rate_hz` on one edge — the case `config.rs`'s own doc uses as its example.
4. `docs/API.md` §3.3's open row is closed, and §3 records that the config schema
   is now shared with a published surface.
5. The README's Python example is left alone: it opens a frozen `.tft`, which is
   the wedge, and declares no topology at all.

## Open questions

None. One was resolved while writing: whether `create=` should take a separate
keyword rather than overloading its type. It should not — a second keyword would
make `create` and `create_config` mutually exclusive parameters that both mean
"declare the topology", which is the same duplication this record is avoiding one
level down.
