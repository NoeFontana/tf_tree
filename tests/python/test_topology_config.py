"""Declaring a topology from Python (`docs/decisions/0041`).

Before this, `build(edges=[("a", "b"), ...], capacity=N)` was the whole surface:
every edge dynamic, one global capacity. That cannot express a **static** edge —
so a sensor mount became a ring somebody had to publish into forever, which is
the latched-topic behaviour `docs/PROJECT.md` §2 lists among the `tf2` problems
this engine exists to solve — nor a per-edge size, nor a declared rate (the only
evidence `TFT007` has that an observed rate is *wrong* rather than merely what it
is), nor a per-edge domain.

The config below is the same schema `ros/tf_tree_ros` starts a bridge from and
`tf_tree topology --discover` writes, so a file produced by the CLI is consumable
here. Every test asserts something the list form **structurally cannot produce**,
which is what keeps them from passing for the wrong reason.
"""

import tempfile

import numpy as np
import pytest
import tf_tree

#: A robot shaped like a robot: a slow map edge, a fast odometry edge, and a
#: sensor bolted on — the mount static, because it is.
CONFIG = """
[topology]
interp = "sclerp"
frame_headroom = 4

[[edge]]
parent = "map"
child = "odom"
kind = "dynamic"
rate_hz = 10.0
history_secs = 30.0

[[edge]]
parent = "odom"
child = "base_link"
kind = "dynamic"
rate_hz = 200.0
history_secs = 2.0

[[edge]]
parent = "base_link"
child = "lidar"
kind = "static"
pose = [1.0, 0.0, 0.0, 0.0, 0.12, 0.0, 0.31]
"""


def test_a_static_edge_folds_out_of_the_plan():
    """The win the list form cannot reach.

    `TreeBuilder::static_edge` folds a constant into the plan at compile time.
    A three-edge route `map -> lidar` therefore compiles to **two** dynamic
    steps plus the folded constant, not three — and a tree that had declared the
    mount dynamic would have to sample it on every lookup, forever, from data
    somebody has to keep publishing.
    """
    tree = tf_tree.build(CONFIG)
    sampled = tree.plan("lidar", "map").edges()
    assert ("base_link", "lidar") not in sampled, (
        f"the static mount is still being sampled every lookup: {sampled}"
    )
    assert len(sampled) == 2, f"only the two dynamic edges should be sampled: {sampled}"

    # And the list form, given the same three edges, cannot do it — there is no
    # way to say "static" at all, so the mount is a ring somebody must publish
    # into forever.
    dynamic = tf_tree.build(
        [("map", "odom"), ("odom", "base_link"), ("base_link", "lidar")],
        capacity=64,
    )
    assert ("base_link", "lidar") in dynamic.plan("lidar", "map").edges()


def test_capacity_and_interp_are_refused_beside_a_config():
    """Two ways to say the same thing, with no rule for which wins, is the
    ambiguity `0041` declines to ship."""
    for kwargs in ({"capacity": 64}, {"interp": "lerpslerp"}):
        with pytest.raises(ValueError) as e:
            tf_tree.build(CONFIG, **kwargs)
        assert "config carries" in str(e.value)


def test_a_malformed_config_names_the_offending_edge():
    """`ConfigError` borrows from the config text so it can name the frame
    without allocating, which means the binding has to render it while the text
    is still alive. This is that, observed from Python."""
    bad = CONFIG.replace("history_secs = 2.0", "history_secs = 2.0\ncapacity = 512")
    with pytest.raises(ValueError) as e:
        tf_tree.build(bad)
    msg = str(e.value)
    assert "base_link" in msg, f"the error must name the edge it is about: {msg}"


def test_the_list_form_still_works_unchanged():
    """`build([...], capacity=, interp=)` is the right ceremony for a test that
    needs two frames and no rates, and every existing example uses it."""
    tree = tf_tree.build([("map", "base")], capacity=32, interp="lerpslerp")
    tf_tree.push(tree, "base", "map", 0, [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
    tf_tree.push(tree, "base", "map", 1_000_000, [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0])
    plan = tree.plan("base", "map")
    assert np.isfinite(plan.at(500_000)).all()


def test_a_config_tree_answers_a_composed_lookup():
    """The control that stops the folding assertion above passing on a tree that
    cannot answer anything."""
    tree = tf_tree.build(CONFIG)
    tf_tree.push(tree, "odom", "map", 0, [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
    tf_tree.push(tree, "odom", "map", 1_000_000, [1.0, 0.0, 0.0, 0.0, 0.1, 0.0, 0.0])
    tf_tree.push(tree, "base_link", "odom", 0, [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
    tf_tree.push(
        tree, "base_link", "odom", 1_000_000, [1.0, 0.0, 0.0, 0.0, 0.2, 0.0, 0.0]
    )
    pose = tree.plan("lidar", "map").at(500_000)
    assert np.isfinite(pose).all()
    # The static mount's translation is in the answer, so the folded constant is
    # genuinely on the path rather than dropped.
    assert not np.allclose(pose, np.eye(4))


@pytest.fixture
def runtime_dir(monkeypatch):
    """A private rendezvous directory, as `test_shared.py` uses."""
    with tempfile.TemporaryDirectory() as d:
        monkeypatch.setenv("TF_TREE_RUNTIME_DIR", d)
        yield d


def test_open_creates_a_shared_arena_from_the_same_config(runtime_dir):
    """`0041` step 2: `create=` takes the config too.

    **What this asserts and what it cannot.** The record's plan said to read a
    declared rate back out, since a rate is the thing the list form cannot
    express at all. Python has no accessor for `EdgeRecord::nominal_rate_mhz` —
    `Plan.edges()` is names only, deliberately — so that is not observable from
    here, and asserting it would mean asserting something this test cannot see.

    What it does assert is that the config reached `layout_if_creating`: the
    static mount is absent from the edges the plan samples, which no value of
    `capacity=` on the list form could produce. `tf_tree doctor`'s `TFT007` is
    where the rate becomes observable, and it is a Rust-side check.
    """
    tree = tf_tree.open(create=CONFIG, mode="rw", name="cfg_arena")
    sampled = tree.plan("lidar", "map").edges()
    assert ("base_link", "lidar") not in sampled, (
        f"the config did not reach the created arena's layout: {sampled}"
    )
    assert len(sampled) == 2


def test_open_refuses_capacity_beside_a_config(runtime_dir):
    """The same refusal `build` makes, on the same reasoning."""
    with pytest.raises(ValueError) as e:
        tf_tree.open(create=CONFIG, mode="rw", name="cfg_arena2", capacity=64)
    assert "config carries" in str(e.value)
