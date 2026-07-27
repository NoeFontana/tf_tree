"""The offline API (`docs/PHASE5.md` §4).

§4.1 is NORMATIVE that there is *no* offline API: a `.tft` opens into the same
``Tree`` a live arena does, and the same calls return the same results. A test
of "the same" that used a tolerance would pass even if the frozen path had
quietly acquired its own interpolation, so the comparison here is bit-for-bit.
"""

import os
import pathlib

import numpy as np
import pytest
import tf_tree

MS = 1_000_000

# Three edges with *deliberately different* retained windows, staggered by
# 100 ms. A fixture whose edges shared one window would make `span`'s
# intersection equal to every edge's own window, and the test could not tell a
# `max`/`min` intersection from picking the first edge and ignoring the rest.
EDGES = [("map", "odom"), ("odom", "base_link"), ("base_link", "lidar")]
N = 100
STEP = 10 * MS
OFFSETS = [0, 100 * MS, 200 * MS]
# The intersection those three windows have: the last edge to start and the
# first to stop. Both ends therefore come from a *different* edge than the
# other, which is what makes the test non-degenerate.
COMMON = (200 * MS, (N - 1) * STEP)


def _poses(seed: float) -> np.ndarray:
    """`(N, 7)` quaternion-and-translation samples that repeat in no component.

    Every sample has a rotation *and* a translation on all three axes, driven by
    irrational multiples of the sample index, and none of them is the identity.
    Identity poses — or pure translations — would make "the frozen bytes answer
    like the live ones" true for reasons that have nothing to do with the freeze
    working.
    """
    i = np.arange(N, dtype=np.float64)
    angle = 0.031 * seed * (i + 1) * np.sqrt(2.0)
    axis = np.array([1.0, -2.0, 3.0]) * seed
    axis = axis / np.linalg.norm(axis)
    s = np.sin(angle / 2.0)
    out = np.empty((N, 7), dtype=np.float64)
    out[:, 0] = np.cos(angle / 2.0)
    out[:, 1] = axis[0] * s
    out[:, 2] = axis[1] * s
    out[:, 3] = axis[2] * s
    out[:, 4] = 0.11 * seed * i
    out[:, 5] = -0.07 * i + seed
    out[:, 6] = np.sin(0.19 * seed * i) * 2.0
    return out


@pytest.fixture
def live() -> tf_tree.Tree:
    """A three-edge chain, `map -> odom -> base_link -> lidar`."""
    t = tf_tree.build(EDGES, capacity=1024)
    for j, (parent, child) in enumerate(EDGES):
        stamps = (np.arange(N, dtype=np.int64) * STEP) + OFFSETS[j]
        with t.publisher(child, parent) as p:
            p.push_many(stamps, _poses(1.0 + j))
    return t


@pytest.fixture
def frozen(live: tf_tree.Tree, tmp_path: pathlib.Path) -> tf_tree.Tree:
    path = tmp_path / "run.tft"
    live.freeze(str(path), source="synthetic")
    return tf_tree.open_file(str(path))


def _query_stamps() -> np.ndarray:
    """Stamps strictly inside the common window, on and off the sample grid."""
    return np.arange(COMMON[0], COMMON[1], 7 * MS, dtype=np.int64)


def test_the_frozen_file_answers_bit_for_bit_like_the_live_tree(live, frozen):
    """§4.1: the same call, the same object, the same bits.

    Mutant: in ``crates/tf_tree_py/src/offline.rs``, make ``open_file`` return
    ``crate::tree::build(...)`` (an empty tree) instead of ``open_frozen`` — or
    drop the ``rename`` in ``Tree::freeze_to`` so nothing lands at the path.
    Either way this fails. The weaker mutant — swapping
    ``assert_array_equal`` for ``assert_allclose`` — is the one this test is
    written *against*: a tolerance would hide a second interpolation.
    """
    stamps = _query_stamps()
    want = live.plan("map", "lidar").at(stamps)
    got = frozen.plan("map", "lidar").at(stamps)
    np.testing.assert_array_equal(got, want)

    # The fixture has to be worth comparing: distinct poses at every stamp, and
    # not the identity anywhere.
    assert want.shape == (len(stamps), 4, 4)
    assert len({tuple(m[:3, 3]) for m in want}) == len(stamps)
    assert not np.allclose(want[0], np.eye(4))


def test_every_online_method_works_unchanged_on_a_frozen_tree(frozen, live):
    """§4.1 names ``plan``, ``at``, ``at_into``, ``adaptive`` and ``latest``.

    Mutant: none available — this is structurally guarded. `open_file` hands
    back the ordinary `Tree` pyclass, so there is no frozen-only code path that
    could implement one of these differently; the test's value is that the
    guarantee is *checked* rather than argued, and it would catch a future
    `open_file` that returned a different class.
    """
    plan = frozen.plan("map", "lidar")
    stamps = _query_stamps()

    out = np.empty((len(stamps), 4, 4), dtype=np.float64)
    plan.at_into(stamps, out)
    np.testing.assert_array_equal(out, plan.at(stamps))

    scalar = np.empty((4, 4), dtype=np.float64)
    plan.at_into(int(stamps[0]), scalar)
    np.testing.assert_array_equal(scalar, plan.at(int(stamps[0])))

    knots, poses = plan.adaptive(int(COMMON[0]), int(COMMON[1]))
    assert knots.shape[0] == poses.shape[0] >= 2
    assert np.all(np.diff(knots) > 0)

    np.testing.assert_array_equal(plan.latest(), live.plan("map", "lidar").latest())
    assert plan.depth() == live.plan("map", "lidar").depth() == 3
    assert frozen.lookup("map", "lidar", int(stamps[3])).shape == (4, 4)


def test_a_frozen_tree_is_permanently_read_only(frozen):
    """§2.4: `AttachMode` is implicitly and permanently `ReadOnly`.

    Mutant: delete the ``if !self.arena.is_writable()`` guard in
    ``Tree::claim`` (`crates/tf_tree/src/tree.rs`). The claim's CAS then reaches
    a ``PROT_READ`` page and the interpreter dies with SIGSEGV instead of
    raising — which is the whole point of refusing rather than faulting, and it
    is why this asserts an exception rather than merely a flag.
    """
    assert frozen.is_writable() is False
    with pytest.raises(tf_tree.TfTreeError):
        frozen.publisher("odom", "map")


def test_span_is_the_intersection_of_the_retained_windows(frozen):
    """§4.2: `span` is `LatestCommon` generalised to a range.

    Mutant: in ``span_impl``, replace ``lo.max(oldest)`` with ``lo.min(oldest)``
    (or ``hi.min(newest)`` with ``hi.max(newest)``). The fixture's three edges
    start and stop 100 ms apart, so either edit moves an end by 100 ms and this
    fails. Taking only the first step's window fails the same way.
    """
    assert frozen.span("map", "lidar") == COMMON
    # Each single-edge span is its own window, and none of them equals the
    # intersection — otherwise the assertion above would be trivially true.
    assert frozen.span("map", "odom") == (0, (N - 1) * STEP)
    assert frozen.span("base_link", "lidar") == (200 * MS, (N - 1) * STEP + 200 * MS)
    assert frozen.span("map", "odom") != COMMON


def test_span_answers_at_the_ends_it_reports(frozen):
    """The interval means what it says: answerable inside, refused outside.

    Mutant: widen the reported window by one nanosecond in ``span_impl`` —
    ``hi.min(newest) + 1`` — and the first ``at`` past the end below stops
    raising.
    """
    t0, t1 = frozen.span("map", "lidar")
    plan = frozen.plan("map", "lidar")
    assert plan.at(t0).shape == (4, 4)
    assert plan.at(t1).shape == (4, 4)
    with pytest.raises(tf_tree.ExtrapolationError):
        plan.at(t0 - 1)
    with pytest.raises(tf_tree.ExtrapolationError):
        plan.at(t1 + 1)


def test_span_of_an_all_static_path_is_none(frozen):
    """`None` means unbounded, and is not the same as an empty interval.

    Mutant: return ``Some((0, 0))`` for the stepless plan in ``span_impl``. A
    caller's ``t0 <= t <= t1`` would then be false everywhere for a path that
    answers everywhere.
    """
    assert frozen.span("map", "map") is None


def test_span_names_the_edge_that_has_never_published():
    """The answer to "why did my lookup fail at t" is nearly always this (§4.2).

    Mutant: in ``span_impl``, ``continue`` past an empty ring instead of
    raising. `span` would then report the *other* edges' intersection — a window
    over which the path is not answerable at all — and this test would get a
    tuple instead of an exception.
    """
    t = tf_tree.build(EDGES, capacity=64)
    stamps = np.arange(N, dtype=np.int64) * STEP
    with t.publisher("odom", "map") as p:
        p.push_many(stamps, _poses(1.0))
    with pytest.raises(tf_tree.NoDataError):
        t.span("map", "lidar")


def test_open_file_of_a_missing_path_raises_filenotfound(tmp_path):
    """The errno path is a real `OSError` subclass, not our hierarchy.

    A Python caller already writes ``except FileNotFoundError``; making them
    learn ``TfTreeError`` for a missing file buys nothing.

    Mutant: drop the ``FrozenFileError::Path`` arm in ``frozen_err`` and let it
    fall through to ``TfTreeError``. ``FileNotFoundError`` is not raised and
    this fails.
    """
    missing = tmp_path / "absent.tft"
    with pytest.raises(FileNotFoundError) as e:
        tf_tree.open_file(str(missing))
    # The path is in the exception, not only in the message: a dataloader that
    # opens sixteen files needs to know which one.
    assert e.value.filename == str(missing)


def test_open_file_of_a_file_that_is_not_a_tft_says_so(tmp_path):
    """Mutant: return ``Ok`` for a bad magic in ``FrozenArena::open``.

    Then a directory of PNGs would map as an arena. Also killed by mapping
    ``FrozenFileError::Frozen`` to ``PyValueError`` instead of ``TfTreeError``,
    which is the mistake this arm's ordering invites.
    """
    junk = tmp_path / "not.tft"
    junk.write_bytes(b"PK\x03\x04" + os.urandom(8192))
    with pytest.raises(tf_tree.TfTreeError) as e:
        tf_tree.open_file(str(junk))
    assert str(junk) in str(e.value)
    assert ".tft" in str(e.value)


def test_freeze_replaces_the_path_atomically_and_leaves_no_litter(live, tmp_path):
    """The temporary is a *sibling* and is renamed over the target (§2.3).

    Mutant: in ``Tree::freeze_to``, write straight to ``path`` instead of to
    ``temp_sibling`` and renaming. The directory listing below then still has
    one entry, so what kills this is the second assertion — freezing twice over
    a live target and reopening it — plus the absence of any dotfile.
    """
    path = tmp_path / "run.tft"
    live.freeze(str(path))
    first = tf_tree.open_file(str(path)).plan("map", "lidar").at(int(COMMON[0]))
    live.freeze(str(path))
    again = tf_tree.open_file(str(path)).plan("map", "lidar").at(int(COMMON[0]))
    np.testing.assert_array_equal(again, first)
    assert [p.name for p in tmp_path.iterdir()] == ["run.tft"]


def test_a_forked_child_can_query_a_tree_opened_before_the_fork(frozen):
    """§4.3's rule is right; §4.3's *reason* does not apply to a `.tft`.

    §4.3 says to open lazily because Phase 3's ``register_at_fork`` poisoning
    applies here too. It does not: ``fork_gen_for`` returns ``None`` for
    ``ArenaBacking::Frozen`` on purpose, because a ``MAP_PRIVATE | PROT_READ``
    mapping is inherited intact and poisoning it would break
    ``multiprocessing`` for offline users to defend against a hazard they do not
    have. The rule survives for a different reason — a ``Tree`` cannot be
    pickled, and 3.14's default start method on Linux is ``forkserver`` — and
    that reason is in ``open_file``'s docstring.

    Mutant: give ``ArenaBacking::Frozen`` the ``Mapped`` arm's body in
    ``fork_gen_for`` (`crates/tf_tree/src/tree.rs`). The child's guard is then
    poisoned and answers ``ChildDetached``, so it reports failure here.
    """
    stamps = _query_stamps()
    want = frozen.plan("map", "lidar").at(stamps)

    read_fd, write_fd = os.pipe()
    pid = os.fork()
    if pid == 0:  # pragma: no cover — the child never returns to pytest
        ok = b"0"
        try:
            got = frozen.plan("map", "lidar").at(stamps)
            ok = b"1" if np.array_equal(got, want) else b"0"
        finally:
            os.write(write_fd, ok)
            os._exit(0)
    os.close(write_fd)
    verdict = os.read(read_fd, 1)
    os.close(read_fd)
    _, status = os.waitpid(pid, 0)
    assert os.waitstatus_to_exitcode(status) == 0
    assert verdict == b"1", "the inherited .tft mapping stopped answering in the child"
