"""Every message this binding raises is prose, in the caller's own names."""

import pickle
import re
import tempfile

import numpy as np
import pytest
import tf_tree
from conftest import LONG_CHILD, _stub_annotations

# One predicate for the two Linux-only paths (`has_shared_memory()` is
# `cfg!(target_os = "linux")`): the served arena and the frozen `.tft`.
shm = pytest.mark.skipif(
    not tf_tree.has_shared_memory(),
    reason="needs the mmap-backed arena: shared trees and .tft files are Linux-only",
)

# `EdgeId(3)`, `FrameId(7)`: a Rust newtype id as `Debug` writes it.
RUST_ID = re.compile(r"Id\(\d+\)")
# `NonMonotonicStamp { last: 1000, got: 500 }`: a Rust struct literal.
RUST_STRUCT = re.compile(r"\w+ \{ ")

POSE = [1.0, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0]
POSE_B = [1.0, 0.0, 0.0, 0.0, 3.0, 4.0, 5.0]

# Distinctive names, so a stray substring of the prose cannot satisfy a check.
EDGES = [("world_a", "chassis_b"), ("chassis_b", "sensor_c")]


def _chain():
    """Two edges; only the first has samples, so a live and a silent edge exist."""
    t = tf_tree.build(EDGES)
    tf_tree.push(t, "chassis_b", "world_a", 1_000, POSE)
    tf_tree.push(t, "chassis_b", "world_a", 2_000, POSE_B)
    return t


def _extrapolation():
    _chain().plan("world_a", "chassis_b").at(9_000_000)


def _no_data():
    _chain().plan("world_a", "sensor_c").at(1_500)


def _unknown_frame_through_lookup():
    # `UnknownFrame` holds a BLAKE3 prefix, which does not invert.
    _chain().lookup("world_a", "ghost_frame", 1_500)


def _unknown_frame_through_plan():
    _chain().plan("ghost_frame", "world_a")


def _unknown_frame_through_span():
    _chain().span("ghost_frame", "world_a")


def _disconnected():
    # Toward `chassis_b`, so `target`, `source` and `cut_at` are three different frames.
    t = tf_tree.build([("world_a", "chassis_b"), ("orphan_d", "sensor_c")])
    t.plan("chassis_b", "sensor_c")


def _too_deep():
    # A previously-unhandled variant: it used to raise a Rust struct dump.
    t = tf_tree.build(
        [("world_a", "chassis_b")] + [(f"f{i}", f"f{i + 1}") for i in range(48)],
        capacity=8,
    )
    t.plan("f0", "f48")


def _too_long_a_walk():
    # 80 links is past `MAX_PATH_EDGES`: the walk refuses before `fold`.
    t = tf_tree.build(
        [("world_a", "chassis_b")] + [(f"g{i}", f"g{i + 1}") for i in range(80)],
        capacity=8,
    )
    t.plan("g0", "g80")


def _at_the_seam():
    # Exactly `MAX_PATH_EDGES` links: the seam between the two sentences; the walk
    # accepts 64 edges and `fold` reports `depth == 64`.
    t = tf_tree.build(
        [("world_a", "chassis_b")] + [(f"h{i}", f"h{i + 1}") for i in range(64)],
        capacity=8,
    )
    t.plan("h0", "h64")


def _non_monotonic_push():
    t = _chain()
    with t.publisher("chassis_b", "world_a") as pub:
        pub.push(500, POSE)


def _non_monotonic_push_many():
    t = _chain()
    with t.publisher("chassis_b", "world_a") as pub:
        pub.push_many(
            np.array([9_000, 8_000], dtype=np.int64),
            np.tile(np.array(POSE), (2, 1)),
        )


def _non_monotonic_module_push():
    t = _chain()
    tf_tree.push(t, "chassis_b", "world_a", 500, POSE)


def _claim_reversed_pair():
    # `publisher(child, parent)`, passed the wrong way round.
    _chain().publisher("world_a", "chassis_b")


def _claim_wrong_parent():
    # `sensor_c` is attached to `chassis_b`: `ClaimApiError::ParentMismatch`.
    _chain().publisher("sensor_c", "world_a")


def _derivatives_unavailable():
    t = tf_tree.build([("world_a", "chassis_b")], interp="lerpslerp")
    tf_tree.push(t, "chassis_b", "world_a", 1_000, POSE)
    tf_tree.push(t, "chassis_b", "world_a", 2_000, POSE_B)
    t.plan("world_a", "chassis_b").at(1_500, layout="quat_twist")


def _no_segment():
    t = tf_tree.build([("world_a", "chassis_b")])
    tf_tree.push(t, "chassis_b", "world_a", 1_000, POSE)
    t.plan("world_a", "chassis_b").at(1_000, layout="quat_twist")


def _span_of_a_silent_edge():
    _chain().span("world_a", "sensor_c")


def _build_a_cycle():
    # The cheapest mistake in the API; it used to answer with a struct literal.
    tf_tree.build([("world_a", "chassis_b"), ("chassis_b", "world_a")])


def _build_two_parents_for_one_frame():
    # `BuildError::DuplicateEdge`: `Display` reports a hash.
    tf_tree.build([("world_a", "chassis_b"), ("sensor_c", "chassis_b")])


CASES = [
    # (trigger, exception class, names that must appear in the message)
    (_extrapolation, tf_tree.ExtrapolationError, ("world_a", "chassis_b")),
    (_no_data, tf_tree.NoDataError, ("chassis_b", "sensor_c")),
    (_unknown_frame_through_lookup, tf_tree.FrameNotDeclaredError, ("ghost_frame",)),
    (_unknown_frame_through_plan, tf_tree.FrameNotDeclaredError, ("ghost_frame",)),
    (_unknown_frame_through_span, tf_tree.FrameNotDeclaredError, ("ghost_frame",)),
    (_disconnected, tf_tree.DisconnectedError, ("chassis_b", "sensor_c", "world_a")),
    (_too_deep, tf_tree.TfTreeError, ()),
    (_too_long_a_walk, tf_tree.TfTreeError, ()),
    (_at_the_seam, tf_tree.TfTreeError, ()),
    (_non_monotonic_push, tf_tree.NonMonotonicStampError, ("world_a", "chassis_b")),
    (
        _non_monotonic_push_many,
        tf_tree.NonMonotonicStampError,
        ("world_a", "chassis_b"),
    ),
    (
        _non_monotonic_module_push,
        tf_tree.NonMonotonicStampError,
        ("world_a", "chassis_b"),
    ),
    (_claim_reversed_pair, tf_tree.TfTreeError, ("world_a", "chassis_b")),
    # All three names, including `chassis_b`, the parent the arena records.
    (_claim_wrong_parent, tf_tree.TfTreeError, ("sensor_c", "world_a", "chassis_b")),
    (
        _derivatives_unavailable,
        tf_tree.DerivativesUnavailableError,
        ("world_a", "chassis_b"),
    ),
    (_no_segment, tf_tree.NoSegmentError, ("world_a", "chassis_b")),
    (_span_of_a_silent_edge, tf_tree.NoDataError, ("chassis_b", "sensor_c")),
    # The two entry points every program calls first.
    (_build_a_cycle, tf_tree.TfTreeError, ("world_a", "chassis_b")),
    (
        _build_two_parents_for_one_frame,
        tf_tree.TfTreeError,
        ("chassis_b", "world_a", "sensor_c"),
    ),
]


@pytest.mark.parametrize(
    "trigger,exc_type,names",
    CASES,
    ids=[c[0].__name__.lstrip("_") for c in CASES],
)
def test_a_message_carries_frame_names_and_no_rust_internals(trigger, exc_type, names):
    """The whole point of the prose layer, checked on every message it reaches."""
    with pytest.raises(exc_type) as excinfo:
        trigger()
    _assert_prose(str(excinfo.value), names)


# --- The fields a handler branches on (`docs/decisions/0058`)

# What each raising row's instance carries, exactly: `vars(e)` is compared whole.
# A row absent from this table must carry nothing.
ATTRIBUTES = {
    _extrapolation: {
        "edge": ("world_a", "chassis_b"),
        "requested": 9_000_000,
        "oldest": 1_000,
        "newest": 2_000,
        "domain": 0,
    },
    _no_data: {"edge": ("chassis_b", "sensor_c")},
    _unknown_frame_through_lookup: {"name": "ghost_frame"},
    _unknown_frame_through_plan: {"name": "ghost_frame"},
    _unknown_frame_through_span: {"name": "ghost_frame"},
    _disconnected: {"target": "chassis_b", "source": "sensor_c", "cut_at": "world_a"},
    _derivatives_unavailable: {"edge": ("world_a", "chassis_b")},
    _no_segment: {"edge": ("world_a", "chassis_b")},
    _span_of_a_silent_edge: {"edge": ("chassis_b", "sensor_c")},
    # `push_many` publishes 9000 then refuses 8000, so `last` is the stamp just written.
    _non_monotonic_push: {"edge": ("world_a", "chassis_b"), "last": 2_000, "got": 500},
    _non_monotonic_push_many: {
        "edge": ("world_a", "chassis_b"),
        "last": 9_000,
        "got": 8_000,
    },
    _non_monotonic_module_push: {
        "edge": ("world_a", "chassis_b"),
        "last": 2_000,
        "got": 500,
    },
}


def _raised(trigger):
    try:
        trigger()
    except tf_tree.TfTreeError as e:
        return e
    raise AssertionError(f"{trigger.__name__} did not raise")


@pytest.mark.parametrize(
    "trigger,exc_type",
    [(c[0], c[1]) for c in CASES],
    ids=[c[0].__name__.lstrip("_") for c in CASES],
)
def test_a_raised_exception_carries_exactly_its_class_attributes(trigger, exc_type):
    """`0058` §1: each attribute is on every instance of its class and no other,
    with its value asserted.
    """
    e = _raised(trigger)
    assert type(e) is exc_type
    assert vars(e) == ATTRIBUTES.get(trigger, {}), (trigger.__name__, vars(e))
    assert len(e.args) == 1, e.args
    assert str(e) == e.args[0]


def test_every_class_a_row_raises_annotates_exactly_what_the_instance_carries():
    """The stub against the mapper, on raised instances (`0058` step 2)."""
    for trigger, _, _ in CASES:
        e = _raised(trigger)
        annotated = set(_stub_annotations(type(e).__name__))
        assert annotated == set(vars(e)), (trigger.__name__, annotated, vars(e))


def test_a_caller_constructed_exception_carries_no_attributes():
    """`0058` question 6: the stub stays precise and nothing defaults to ``None``."""
    built = tf_tree.ExtrapolationError("m")
    assert not hasattr(built, "requested")
    assert vars(built) == {}
    assert _stub_annotations("ExtrapolationError")["requested"] == "int"


def test_a_push_error_names_the_stored_edge_not_the_typed_one():
    """`NonMonotonicStampError.edge` is resolved from the variant's `EdgeId`."""
    t = tf_tree.build([("world_a", LONG_CHILD)])
    tf_tree.push(t, LONG_CHILD, "world_a", 1_000, POSE)
    with pytest.raises(tf_tree.NonMonotonicStampError) as excinfo:
        tf_tree.push(t, LONG_CHILD, "world_a", 500, POSE)
    e = excinfo.value
    assert e.edge == t.edges()[0]
    assert e.edge != ("world_a", LONG_CHILD)
    assert len(e.edge[1].encode()) == 48


def _assert_prose(msg, names):
    """The three properties, in one place."""
    assert not RUST_ID.search(msg), (
        f"a Rust newtype id reached a Python message: {msg!r}. "
        "Route the id through errors.rs's edge_label/frame_label."
    )
    assert not RUST_STRUCT.search(msg), (
        f"a Rust struct literal reached a Python message: {msg!r}. "
        "Something is formatting an error with Debug instead of prose."
    )
    for name in names:
        assert name in msg, f"{name!r} missing from {msg!r}"


@pytest.fixture
def runtime_dir(monkeypatch):
    """A scratch rendezvous directory, so a test cannot collide with a robot."""
    with tempfile.TemporaryDirectory(prefix="tf_tree_py_") as d:
        monkeypatch.setenv("TF_TREE_RUNTIME_DIR", d)
        yield d


@shm
def test_the_two_too_deep_sentences_name_the_bound_that_refused():
    """One error variant carries two bounds, so the message has to choose."""
    with pytest.raises(tf_tree.TfTreeError) as past_compiled:
        _too_deep()
    assert "compiles to 48 steps and a plan holds 32" in str(past_compiled.value)

    with pytest.raises(tf_tree.TfTreeError) as past_walk:
        _too_long_a_walk()
    assert "longer than the 64 edges a lookup walks" in str(past_walk.value)
    assert "compiles to" not in str(past_walk.value)

    with pytest.raises(tf_tree.TfTreeError) as seam:
        _at_the_seam()
    assert "compiles to 64 steps and a plan holds 32" in str(seam.value)
    assert "edges a lookup walks" not in str(seam.value), (
        "the walk accepted this path; it must not be blamed for it"
    )

    # No remedy the caller cannot reach: `build` declares every edge dynamic.
    for exc in (past_compiled, past_walk, seam):
        assert "static_edge" not in str(exc.value)


def test_open_reports_a_bad_edge_list_the_way_build_does(runtime_dir):
    """`tf_tree.open(create=...)` is `build` behind one `From` impl."""
    with pytest.raises(tf_tree.TfTreeError) as excinfo:
        tf_tree.open(
            mode="rw", create=[("world_a", "chassis_b"), ("chassis_b", "world_a")]
        )
    _assert_prose(str(excinfo.value), ("world_a", "chassis_b"))


def test_an_unknown_frame_is_not_interned_by_the_message_that_names_it():
    """Formatting an error must not change the arena."""
    tree = tf_tree.build(EDGES, frame_headroom=8)
    before = tree.frames()

    with pytest.raises(tf_tree.FrameNotDeclaredError) as excinfo:
        tree.lookup("world_a", "ghost_frame", 1_500)

    assert tree.frames() == before, (
        "a failed lookup interned the name it was complaining about: "
        f"{before} -> {tree.frames()}. Error formatting is a read."
    )
    _assert_prose(str(excinfo.value), ("ghost_frame",))


@shm
def test_frame_headroom_reaches_the_arena_and_stays_out_of_the_frame_list(tmp_path):
    """The premise of the test above, checked instead of assumed."""
    sizes = []
    for headroom in (0, 8, 64):
        tree = tf_tree.build(EDGES, frame_headroom=headroom)
        assert tree.frames() == ["world_a", "chassis_b", "sensor_c"], (
            f"frame_headroom={headroom} put reserved slots in frames(): {tree.frames()}"
        )
        path = tmp_path / f"headroom_{headroom}.tft"
        tree.freeze(path)
        sizes.append(path.stat().st_size)

    assert sizes[0] < sizes[1] < sizes[2], (
        f"frame_headroom did not reach the arena layout: {sizes} bytes for "
        "0, 8 and 64 spare frame slots. A keyword that is parsed and dropped "
        "makes the interning guard above vacuous."
    )


def test_a_stale_id_degrades_to_an_index_and_a_reason_not_to_a_debug_dump():
    """Name resolution can fail, and the fallback has to say why."""
    t = _chain()
    with pytest.raises(tf_tree.NoDataError) as excinfo:
        t.plan("world_a", "sensor_c").at(1_500)
    msg = str(excinfo.value)
    assert "name unavailable" not in msg, msg
    assert "edge #" not in msg, msg
    assert "frame #" not in msg, msg
    # The attribute resolved too (`0058` §2), not the `None` of a failed resolution.
    assert excinfo.value.edge == ("chassis_b", "sensor_c")
    assert excinfo.value.edge in t.edges()


@shm
def test_a_damaged_tft_is_described_and_not_dumped(tmp_path):
    """The one error enum in this binding that can be matched exhaustively."""
    tree = tf_tree.build(EDGES)
    tf_tree.push(tree, "chassis_b", "world_a", 1_000, POSE)
    whole = tmp_path / "whole.tft"
    tree.freeze(whole)

    half = tmp_path / "half.tft"
    half.write_bytes(whole.read_bytes()[: whole.stat().st_size // 2])
    with pytest.raises(tf_tree.TfTreeError) as excinfo:
        tf_tree.open_file(half)
    msg = str(excinfo.value)

    assert not RUST_STRUCT.search(msg), msg
    assert not RUST_ID.search(msg), msg
    # The path is what a caller greps for; byte counts tell truncation from mismatch.
    assert str(half) in msg
    assert "bytes" in msg


def test_a_batch_push_keeps_the_scalar_sentence_and_only_prefixes_it():
    """`push_many` names the rejected sample and says the same thing after."""
    t = _chain()
    with t.publisher("chassis_b", "world_a") as pub:
        with pytest.raises(tf_tree.TfTreeError) as scalar:
            pub.push(500, POSE)
        with pytest.raises(tf_tree.TfTreeError) as batch:
            pub.push_many(
                np.array([9_000, 500], dtype=np.int64),
                np.tile(np.array(POSE), (2, 1)),
            )

    prefix = "sample 1 (stamp 500): "
    assert str(batch.value).startswith(prefix)
    # Different newest stamps, so only the sentence's shape can be equal.
    assert str(batch.value)[len(prefix) :].replace("9000", "2000") == str(scalar.value)


# --- An exception has to leave the process it was raised in

# : Every exception class the package exports.
EXPECTED_EXCEPTION_COUNT = 15


def _exception_classes():
    return sorted(
        (
            obj
            for obj in vars(tf_tree).values()
            if isinstance(obj, type) and issubclass(obj, BaseException)
        ),
        key=lambda c: c.__name__,
    )


def test_every_exception_class_pickles_as_itself():
    """A worker's exception is pickled to reach its parent."""
    classes = _exception_classes()
    assert len(classes) == EXPECTED_EXCEPTION_COUNT, [c.__name__ for c in classes]
    for cls in classes:
        assert cls.__module__ == "tf_tree", (
            f"{cls.__name__}.__module__ is {cls.__module__!r}; pickle resolves a "
            "class by importing its module, so anything but 'tf_tree' cannot "
            "cross a process boundary"
        )
        back = pickle.loads(pickle.dumps(cls("m")))
        assert type(back) is cls and back.args == ("m",)
    # The rename is of a spelling; the private module hands out the same classes.
    from tf_tree import _core

    assert all(getattr(_core, c.__name__) is c for c in classes)


@pytest.mark.parametrize(
    "trigger,exc_type",
    [(c[0], c[1]) for c in CASES],
    ids=[c[0].__name__.lstrip("_") for c in CASES],
)
def test_a_raised_exception_survives_a_pickle_round_trip(trigger, exc_type):
    """The instances a caller actually catches."""
    with pytest.raises(exc_type) as excinfo:
        trigger()
    back = pickle.loads(pickle.dumps(excinfo.value))
    assert type(back) is type(excinfo.value)
    assert back.args == excinfo.value.args
    # `0058` §5: attributes live in `__dict__`, which `__reduce__` carries.
    assert vars(back) == vars(excinfo.value)
