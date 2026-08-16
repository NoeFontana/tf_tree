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

**The table's own hole was `build` and `open`.** It reached fifteen triggers
without one of them being the two calls every program makes *first*, and both
forwarded a `Display` that embeds `Debug`: `tf_tree.build([("a","b"),("b","a")])`
— a typo-grade mistake — raised ``topology error: WouldCreateCycle { child:
FrameId(1) }``, which matches both regexes below. A "general regression check"
with a hole exactly the shape of the entry points is worth less than it looks,
so the rows are here now and the shared-arena half is the test after the table.
"""

import re
import tempfile

import numpy as np
import pytest
import tf_tree

# One predicate for the two Linux-only paths this file touches, because it *is*
# one predicate: `has_shared_memory()` is `cfg!(target_os = "linux")`, and the
# served arena and the frozen `.tft` are both `#[cfg(all(feature = "shm",
# target_os = "linux"))]` in the facade. The reason names both — it used to say
# only "share a tree between processes", which is not why a `.tft` row skips.
#
# A mark rather than a `pytestmark`: every other test here is a prose assertion
# that never opens an arena, and off-Linux those are the ones worth running.
# Unguarded, the three below *fail* there rather than skip — `open_file` and
# `Tree.freeze` refuse with a message naming the platform (`SUPPORT.md`, "What
# is not supported") — and bury the rest of the file's signal. `test_frozen.py`
# made the same argument for its whole module.
shm = pytest.mark.skipif(
    not tf_tree.has_shared_memory(),
    reason="needs the mmap-backed arena: shared trees and .tft files are Linux-only",
)

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


def _build_a_cycle():
    # The cheapest mistake in the API and the one that used to answer with a
    # struct literal. There is no tree yet, so the names come from the list the
    # caller passed in — which is the only place they exist.
    tf_tree.build([("world_a", "chassis_b"), ("chassis_b", "world_a")])


def _build_two_parents_for_one_frame():
    # `BuildError::DuplicateEdge`, whose Rust `Display` reports the child's
    # 64-bit hash — a number that does not invert and that the caller cannot
    # match against anything they typed.
    tf_tree.build([("world_a", "chassis_b"), ("sensor_c", "chassis_b")])


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
    # The two entry points every program calls first. Both names, in both rows:
    # a cycle is a chain and naming one end of it does not show it, and the
    # duplicate row names the child *and* the two parents that claim it.
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

    Mutant: give ``build`` back ``.map_err(|e|
    TfTreeError::new_err(format!("{e}")))`` — the spelling both entry points
    shipped with. **Applied, rebuilt and run**: ``2 failed, 142 passed`` — the
    two ``build_*`` rows, on ``a Rust newtype id reached a Python message:
    'topology error: WouldCreateCycle { child: FrameId(1) }'`` and on
    ``'chassis_b' missing from 'two edges declare the same child (name hash
    0x…)'``. The second is the more interesting failure: it is not a Debug dump
    at all, so only the *name* assertion catches it.

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
    _assert_prose(str(excinfo.value), names)


def _assert_prose(msg, names):
    """The three properties, in one place so the table is not the only caller.

    The shared-arena rows below cannot be table rows — they need a fixture, and
    the table is a list of zero-argument callables — but they are checking the
    same three things, and a second copy of these assertions is how the two
    would drift into checking different ones.
    """
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
def test_open_reports_a_bad_edge_list_the_way_build_does(runtime_dir):
    """`tf_tree.open(create=...)` is `build` behind one `From` impl.

    ``OpenError::Build(BuildError)`` forwards its `Display`, so every Debug dump
    in `BuildError` reached this call too — and this is the call a *deployed*
    consumer makes, where `build` is mostly a test and notebook entry point.
    One mapper serves both; this is the row that says so.

    Mutant: give ``open_arena`` back ``.map_err(|e|
    TfTreeError::new_err(format!("{e}")))``. Applied, rebuilt, run:
    ``1 failed, 144 passed`` — this test, on ``a Rust newtype id reached a
    Python message: 'topology error: WouldCreateCycle { child: FrameId(1) }'``.
    It trips ``RUST_ID`` before ``RUST_STRUCT`` because that dump is both at
    once. Nothing else moves: ``build``'s own rows keep passing under it, which
    is the shape of the hole this row closes.
    """
    with pytest.raises(tf_tree.TfTreeError) as excinfo:
        tf_tree.open(
            mode="rw", create=[("world_a", "chassis_b"), ("chassis_b", "world_a")]
        )
    _assert_prose(str(excinfo.value), ("world_a", "chassis_b"))


def test_an_unknown_frame_is_not_interned_by_the_message_that_names_it():
    """**Formatting an error must not change the arena.**

    `Tree.lookup` catches `UnknownFrame` and probes both names, because the
    error carries a BLAKE3 prefix and BLAKE3 does not invert. The probe that
    shipped was ``self.inner.frame(n).is_err()`` — and `Tree::frame` is not a
    read: on a writable tree its last line is ``self.view().intern(name)``,
    which publishes a `FrameRecord` with a `compare_exchange`. So the *error
    path* declared the frame the caller had misspelled, permanently: ids are
    append-only and never recycled (D10), so N typos consume N slots that every
    participant shares, and the ingest bridge's own `Tree::frame()` starts
    failing `CapacityExceeded`.

    It also defeated itself. The intern *succeeds*, so ``.is_err()`` is false,
    ``find`` yields ``None``, and the message falls through to the hash it was
    added to replace.

    ``frame_headroom`` is what makes either half visible: below the capacity
    pre-check in `intern_core` a failed intern writes nothing, so on a tree with
    no spare slots — which is every tree `tf_tree.build` made before this
    keyword existed — the bug is invisible and the old test passes.

    The arena assertion comes first because it is the one that matters: a
    message can be fixed in an afternoon and a consumed frame slot is gone for
    the life of the arena.

    Mutant: restore ``.find(|n| self.inner.frame(n).is_err())``. Applied,
    rebuilt, run: ``1 failed, 144 passed`` — this test, on ``['world_a',
    'chassis_b', 'sensor_c'] -> ['world_a', 'chassis_b', 'sensor_c',
    'ghost_frame']``. Under the same build, three misspelled reads in a row
    leave ``['world_a', 'chassis_b', 'sensor_c', 'ghost_frame', 'typo2',
    'typo3']`` — one slot each, and the arena has eight — while all three
    messages read ``no frame with hash 0xb0c2b770bd9545ed`` and its siblings.
    That is the second half of the defect: the intern *succeeds*, so the probe
    finds nothing wrong and prints the hash it was added to replace. The prose
    assertion below is what catches it once the arena assertion is satisfied.
    """
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
    """The premise of the test above, checked instead of assumed.

    That test proves nothing if ``frame_headroom=8`` never reaches
    ``ArenaLayout``: with no spare slot the intern the mutant performs fails the
    capacity pre-check in `intern_core`, writes nothing, and the guard passes
    against the very code it exists to catch. A keyword that is accepted and
    dropped would be invisible there.

    **The `.tft` size is the only observation of `max_frames` the Python surface
    offers, and it is a real one**: headroom widens the frame table, the intern
    table and both topology blocks, and `Tree.freeze` writes the whole arena.
    There is deliberately no "intern a name and watch `frames()` grow" assertion
    to pair with it — **after this PR no Python entry point interns at all**.
    `plan`, `publisher`, `push`, `span` and `lookup`'s probe all resolve through
    the read-only `resolve_frame`, which is the fix itself. The knob exists for
    *peers*: a Rust or C process, or the ROS ingest bridge, calling
    `Tree::frame()` on an arena this binding created. Exercising that needs a
    second process and belongs in `crates/tf_tree/tests/`.

    The second assertion is the other half of the same question, and it is
    `named_frame_in`'s: a headroom slot is zeroed, `frame_record` bounds against
    ``max_frames`` rather than ``frame_count``, and an unfiltered walk would
    report the reserved slots as frames named ``""``.

    Measured, this build, three edges: 2262912 B at ``frame_headroom=0``,
    2263552 at 4, 2264320 at 8, 2274048 at 64 — and ``frames()`` is the same
    three names at every one of them.

    Mutant: drop ``.frame_headroom(frame_headroom)`` from ``build`` so the
    keyword is parsed and discarded. Applied, rebuilt, run: ``1 failed, 144
    passed`` — this test, on ``[2262912, 2262912, 2262912]``. **The interning
    guard above passes under that same mutant**, which is the whole reason this
    test exists: it is the one that fails when the other one stops meaning
    anything.
    """
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


@shm
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
