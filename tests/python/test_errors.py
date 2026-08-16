"""Every message this binding raises is prose, in the caller's own names.

`docs/API.md` R5 lets the Rust errors be `Copy`, `String`-free identifiers by
promising the prose lives in a separate layer. `crates/tf_tree_py/src/errors.rs`
*is* that layer for Python, and for three phases it was only half doing the job:
``edge EdgeId(3)`` and ``NonMonotonicStamp { last: 1000, got: 500 }`` reached
users as if they were sentences. Neither is: a Python caller is never handed an
``EdgeId``, the surface offers no way to invert one, and there is no struct
behind that second spelling to catch and inspect. The information a caller can
act on is the two frame names they typed.

So this file is one table and one assertion applied to all of it. It is
deliberately **not** a set of exact-message tests — pinning wording would make
every improvement a test edit, which is how message tests end up deleted. It
pins the three properties that make a message usable:

* no Rust id (``EdgeId(3)``, ``FrameId(7)``) anywhere in it;
* no Rust struct or enum literal (``Foo { bar: 1 }``);
* the frame name the test actually used appears in it.

The table doubles as the exception-*type* contract, which R5 makes the part a
caller programs against: each row names the class, so a refactor that improves a
message but reroutes it to a different class fails here.
"""

import re

import numpy as np
import pytest
import tf_tree

# `EdgeId(3)`, `FrameId(7)` — a Rust newtype id as `Debug` writes it.
RUST_ID = re.compile(r"Id\(\d+\)")
# `NonMonotonicStamp { last: 1000, got: 500 }` — a Rust struct literal. Loose on
# purpose: the point is that no message ever grows a `{ ` at all, and a message
# that legitimately needed a brace would be worth looking at anyway.
RUST_STRUCT = re.compile(r"\w+ \{ ")

POSE = [1.0, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0]
POSE_B = [1.0, 0.0, 0.0, 0.0, 3.0, 4.0, 5.0]

# Distinctive names, so "the message contains 'chassis_b'" cannot be satisfied
# by a stray substring of the prose itself. `map`/`base` would be: "base" occurs
# in ordinary English and one of these messages says "the builder".
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
    # The one arm whose identity the error cannot carry: `UnknownFrame` holds a
    # BLAKE3 prefix and BLAKE3 does not invert. `Tree.lookup` is where both
    # names are still in scope, so this asserts the call site catches it rather
    # than letting a hash through.
    _chain().lookup("world_a", "ghost_frame", 1_500)


def _unknown_frame_through_plan():
    _chain().plan("ghost_frame", "world_a")


def _unknown_frame_through_span():
    _chain().span("ghost_frame", "world_a")


def _disconnected():
    t = tf_tree.build([("world_a", "chassis_b"), ("orphan_d", "sensor_c")])
    t.plan("world_a", "sensor_c")


def _too_deep():
    # A previously-unhandled variant, and the reason it is in the table: before
    # the `other =>` catch-all was deleted this raised the text
    # `TreeTooDeep { depth: 16 }`. `MAX_DEPTH` is 16 (`tf_tree_core`), so 24
    # links is comfortably past it without depending on the exact value.
    t = tf_tree.build(
        [("world_a", "chassis_b")] + [(f"f{i}", f"f{i + 1}") for i in range(24)],
        capacity=8,
    )
    t.plan("f0", "f24")


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
    # `publisher(child, parent)`; this passes them the wrong way round. The
    # child of the reversed pair is the root, so this is `ClaimApiError::NoEdge`
    # rather than `ParentMismatch` — which is why that arm names both arguments.
    _chain().publisher("world_a", "chassis_b")


def _claim_wrong_parent():
    # `sensor_c` is attached to `chassis_b`, not to `world_a`:
    # `ClaimApiError::ParentMismatch`, the one arm that has to report what the
    # arena says as well as what was asked for.
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


CASES = [
    # (trigger, exception class, names that must appear in the message)
    (_extrapolation, tf_tree.ExtrapolationError, ("world_a", "chassis_b")),
    (_no_data, tf_tree.NoDataError, ("chassis_b", "sensor_c")),
    (_unknown_frame_through_lookup, tf_tree.FrameNotDeclaredError, ("ghost_frame",)),
    (_unknown_frame_through_plan, tf_tree.FrameNotDeclaredError, ("ghost_frame",)),
    (_unknown_frame_through_span, tf_tree.FrameNotDeclaredError, ("ghost_frame",)),
    (_disconnected, tf_tree.DisconnectedError, ("world_a", "sensor_c")),
    (_too_deep, tf_tree.TfTreeError, ()),
    (_non_monotonic_push, tf_tree.TfTreeError, ("world_a", "chassis_b")),
    (_non_monotonic_push_many, tf_tree.TfTreeError, ("world_a", "chassis_b")),
    (_non_monotonic_module_push, tf_tree.TfTreeError, ("world_a", "chassis_b")),
    (_claim_reversed_pair, tf_tree.TfTreeError, ("world_a", "chassis_b")),
    # All three names: the two the caller typed, and `chassis_b` — the parent
    # the arena actually records, which is the fact they did not have.
    (_claim_wrong_parent, tf_tree.TfTreeError, ("sensor_c", "world_a", "chassis_b")),
    (
        _derivatives_unavailable,
        tf_tree.DerivativesUnavailableError,
        ("world_a", "chassis_b"),
    ),
    (_no_segment, tf_tree.NoSegmentError, ("world_a", "chassis_b")),
    (_span_of_a_silent_edge, tf_tree.NoDataError, ("chassis_b", "sensor_c")),
]


@pytest.mark.parametrize(
    "trigger,exc_type,names",
    CASES,
    ids=[c[0].__name__.lstrip("_") for c in CASES],
)
def test_a_message_carries_frame_names_and_no_rust_internals(trigger, exc_type, names):
    """The whole point of the prose layer, checked on every message it reaches.

    ``_too_deep`` carries no name assertion on purpose: `TreeTooDeep` is a
    property of the *path length* and the error holds no frame or edge id at
    all, so demanding a name would be demanding an invention. It is in the table
    for the other two assertions — it is the variant the deleted ``other =>``
    catch-all used to render as ``TreeTooDeep { depth: 16 }``, so it is the row
    that fails if the catch-all comes back.

    Mutant: restore ``other => TfTreeError::new_err(format!("{other:?}"))`` at
    the end of ``lookup_err`` and delete the ``TreeTooDeep`` arm. **Applied and
    run**: ``1 failed, 139 passed`` — ``too_deep`` fails on ``RUST_STRUCT`` with
    ``TreeTooDeep { depth: 16 }`` and nothing else in the suite moves, which is
    exactly the shape of the bug: a variant ships a Debug dump while every
    handled one looks fine.

    Mutant: make ``edge_label`` return ``format!("edge #{}", edge.get())``.
    **Applied and run**: ``7 failed, 133 passed`` — five rows here
    (``extrapolation``, ``no_data``, ``derivatives_unavailable``,
    ``no_segment``, ``span_of_a_silent_edge``) fail on the missing name, plus
    the fallback test below and ``test_frozen.py``'s span test. **None fails on
    ``RUST_ID``**, which is why the name assertion is here and not left to the
    two regexes: an id can be scrubbed without a name appearing.
    """
    with pytest.raises(exc_type) as excinfo:
        trigger()
    msg = str(excinfo.value)

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


def test_a_stale_id_degrades_to_an_index_and_a_reason_not_to_a_debug_dump():
    """Name resolution can fail, and the fallback has to say *why*.

    A fork child is the reachable case: the shared mapping is ``MADV_DONTFORK``,
    so `Tree::view` substitutes a zeroed poison arena and every name in it reads
    absent. An in-process tree cannot be detached, so this checks the shape of
    the fallback the only way a test without shared memory can — by asserting
    the sentence exists in the module that would produce it, which is not
    something pytest can do. What it *can* pin is the property that matters at
    this tier: the tree used here is not detached, every id resolves, and no
    message falls back at all.

    Kept as its own test rather than folded into the table because it is an
    assertion about the *absence* of the fallback, and a row that passes by
    never being reached is worse than no row.
    """
    t = _chain()
    with pytest.raises(tf_tree.NoDataError) as excinfo:
        t.plan("world_a", "sensor_c").at(1_500)
    msg = str(excinfo.value)
    assert "name unavailable" not in msg, msg
    assert "edge #" not in msg, msg
    assert "frame #" not in msg, msg


def test_a_damaged_tft_is_described_and_not_dumped(tmp_path):
    """The one error enum in this binding that *can* be matched exhaustively.

    `tf_tree_arena::FrozenError` carries no ``#[non_exhaustive]`` — unlike
    `LookupError`, `PushError` and `FrozenFileError` — so ``frozen_err`` can
    enumerate all nine variants and a tenth is a compile error there. That is
    the property this whole file wants and only this one enum can give. Six of
    the nine used to reach Python through a ``{other:?}``; this is the cheapest
    of them to trigger, and the one whose Debug was an actual struct literal.

    Mutant: restore ``other => format!("could not be opened as a .tft:
    {other:?}")`` in place of the five enumerated arms. **Applied and run**:
    ``1 failed, 139 passed`` — this test fails on ``RUST_STRUCT`` with
    ``SizeMismatch { actual: 1094400, expected: 2188800 }``, and nothing else in
    the suite moves.
    """
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
    # The path is what a caller greps for, and the byte counts are what tell a
    # truncated write apart from a wrong-build file.
    assert str(half) in msg
    assert "bytes" in msg


def test_a_batch_push_keeps_the_scalar_sentence_and_only_prefixes_it():
    """`push_many` names the sample it rejected *and* says the same thing after.

    The samples before the bad one were published, so the index is load-bearing
    — but an index alone was the whole message once the Debug dump is taken
    away. Prefixing rather than re-wording is what keeps the two paths from
    drifting into two explanations of one failure.

    Mutant: give ``push_many`` its own sentence
    (``"sample {i} (stamp {stamp}) was rejected"``) instead of ``push_msg``.
    **Applied and run**: ``2 failed, 138 passed`` — this test and the
    ``non_monotonic_push_many`` row above. ``test_api.py``'s
    ``test_push_many_names_the_sample_it_rejected`` passes under the mutant,
    because it only matches ``sample 1``; that is the gap this closes.
    """
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
    # The scalar failure is against a newest of 2000, the batch's against the
    # 9000 it just published, so only the shape of the sentence can be equal.
    assert str(batch.value)[len(prefix) :].replace("9000", "2000") == str(scalar.value)
