"""The offline API (`docs/PHASE5.md` §4).

§4.1 is NORMATIVE that there is *no* offline API: a `.tft` opens into the same
``Tree`` a live arena does, and the same calls return the same results. A test
of "the same" that used a tolerance would pass even if the frozen path had
quietly acquired its own interpolation, so the comparison here is bit-for-bit.
"""

import os
import pathlib
import threading
import time

import numpy as np
import pytest
import tf_tree

# Every test in this module goes through the frozen `.tft` path, which is
# `#[cfg(all(feature = "shm", target_os = "linux"))]` in the facade. The binding
# keeps `open_file` and `Tree.freeze` *present* elsewhere so a portable script
# gets an explanation instead of an `AttributeError`, but "it refuses with a
# message" is a different assertion from every one below — so skip, the way
# `test_shared.py` already does off the same predicate (`has_shared_memory()` is
# `cfg!(target_os = "linux")`). Without this the suite reports eleven failures on
# macOS and the real signal is buried.
pytestmark = pytest.mark.skipif(
    not tf_tree.has_shared_memory(),
    reason="frozen .tft files need the mmap-backed arena (Linux only)",
)

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


def test_span_of_an_empty_plan_is_none(frozen):
    """`None` means unbounded, and is not the same as an empty interval.

    This is the **empty** ``lookup(x, x)`` plan — ``len == 0``, so `span`'s loop
    body never runs. It is *not* the all-static path, which reaches a different
    branch (`Step::Static`) that no plan built through ``tf_tree.build`` can
    contain, because the Python builder declares only dynamic edges. That branch
    is covered where it is reachable, by
    ``span_skips_static_steps_and_is_bounded_by_the_dynamic_one`` in
    ``crates/tf_tree/tests/behavior.rs``. An earlier version of this test claimed
    the static case and asserted the empty one.

    Mutant: return ``Some((0, 0))`` for the stepless plan in ``Plan::span``. A
    caller's ``t0 <= t <= t1`` would then be false everywhere for a path that
    answers everywhere.
    """
    assert frozen.span("map", "map") is None


def test_span_names_the_frames_of_the_edge_that_has_never_published():
    """The answer to "why did my lookup fail at t" is nearly always this (§4.2).

    And the answer has to be *actionable*: an ``EdgeId(2)`` is not, because the
    Python surface exposes no way to turn an edge id back into the names the
    caller typed.

    **This test used to be the only place that was true**, and its mutant note
    used to say so. Since ``lookup_err`` grew ``edge_label``, *every* message
    resolves ids — ``tests/python/test_errors.py`` is the general check — so
    what is left for ``span_impl``'s own arm is the phrase that places the
    silent edge **on the path that was asked for**, which is the half of §4.2's
    question an edge named on its own does not answer.

    Mutant: drop the ``LookupError::NoData`` arm in ``span_impl`` and let it
    fall through to ``lookup_err``. **Applied and run**: ``1 failed, 139
    passed`` — the type and the two name assertions still pass, because
    ``lookup_err`` names the edge too now (``edge "base_link" -> "lidar" has no
    samples yet``), and ``"on the path from"`` is the one that fails. That third
    assertion is what makes this a test of ``span_impl`` rather than of the
    shared layer.
    Mutant: ``continue`` past an empty ring in ``Plan::span`` instead of
    propagating; this then gets a tuple instead of an exception.
    """
    t = tf_tree.build(EDGES, capacity=64)
    stamps = np.arange(N, dtype=np.int64) * STEP
    # Publish on the first two edges so exactly one edge — `base_link -> lidar` —
    # is silent. Leaving two silent would make the assertion below pass on
    # whichever the walk happened to reach first, which is not a property.
    for j, (parent, child) in enumerate(EDGES[:2]):
        with t.publisher(child, parent) as p:
            p.push_many(stamps, _poses(1.0 + j))
    with pytest.raises(tf_tree.NoDataError) as e:
        t.span("map", "lidar")
    msg = str(e.value)
    # The *pair*, in edge order. `map` and `lidar` are the query's own endpoints
    # and the message echoes them, so asserting either name alone would be
    # satisfied by a message that named no edge at all; `"base_link" -> "lidar"`
    # can only come from the edge record.
    assert '"base_link" -> "lidar"' in msg, msg
    assert "EdgeId" not in msg, msg
    # `span_impl`'s own contribution, and the only assertion here that
    # `lookup_err`'s shared arm does not already satisfy.
    assert "on the path from" in msg, msg


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
    """A foreign file is refused, in our exception hierarchy, naming the path.

    Mutant: map ``FrozenFileError::Frozen`` to ``PyValueError`` instead of
    ``TfTreeError`` — the mistake this arm's ordering invites, since
    ``TfTreeError`` is not a ``ValueError``. That is the mutant this test kills.

    It does **not** kill "return ``Ok`` for a bad magic in ``FrozenArena::open``":
    the junk below still fails a later container check and the fall-through arm's
    message still contains ``.tft``, so both assertions hold. That mutant is
    caught by ``frozen::tests::a_foreign_file_is_not_a_tft`` under
    ``just shm-check``, which is the right place for it — it is a property of the
    arena, not of the binding.
    """
    junk = tmp_path / "not.tft"
    junk.write_bytes(b"PK\x03\x04" + os.urandom(8192))
    with pytest.raises(tf_tree.TfTreeError) as e:
        tf_tree.open_file(str(junk))
    assert str(junk) in str(e.value)
    assert ".tft" in str(e.value)


def test_freeze_replaces_the_path_atomically_and_leaves_no_litter(live, tmp_path):
    """The temporary is a *sibling* and is renamed over the target (§2.3).

    **The inode is the assertion.** An earlier version of this test froze twice
    and compared the numbers it read back, which passes just as well when
    ``freeze_to`` writes straight to ``path`` — so it asserted nothing about the
    property it is named for, and the whole Rust suite passed with the rename
    deleted. `rename` replaces the directory entry, so a re-freeze *must* leave a
    different inode at the same path; an in-place rewrite keeps it.

    That is the difference that matters in practice. ``write_frozen`` sizes the
    file with ``ftruncate`` first, so a freeze that dies at 60 % of a 233 MB copy
    leaves a **full-length** zero-tailed file — harmless at a temporary name that
    is then unlinked, fatal at ``path``, where next week's ``open_file`` gets
    ``BadMagic`` instead of last week's good index. It is also what lets the third
    assertion below hold: re-freezing over a path that is *currently mapped* by a
    live tree cannot corrupt that mapping, because the mapping keeps the old
    inode.

    Mutant: in ``Tree::freeze_to``, ``File::create(path)`` instead of
    ``File::create(&tmp)`` with the ``rename`` deleted. The inode assertion fails.
    """
    path = tmp_path / "run.tft"
    live.freeze(path)
    first_ino = os.stat(path).st_ino
    # Hold the first image open across the second freeze: this is the mapping the
    # rename exists to protect.
    held = tf_tree.open_file(path)
    first = held.plan("map", "lidar").at(int(COMMON[0]))

    live.freeze(path)

    assert os.stat(path).st_ino != first_ino, (
        "freeze rewrote the target in place: a partial write would have been "
        "visible at `path`, and the mapping held open above would have moved "
        "under its reader"
    )
    np.testing.assert_array_equal(
        held.plan("map", "lidar").at(int(COMMON[0])),
        first,
        err_msg="the mapping open across the freeze changed answers",
    )
    again = tf_tree.open_file(path).plan("map", "lidar").at(int(COMMON[0]))
    np.testing.assert_array_equal(again, first)
    # No litter: the temporary is gone, and it was a sibling rather than a file
    # in `/tmp` (a rename across filesystems is not atomic).
    assert [p.name for p in tmp_path.iterdir()] == ["run.tft"]


@pytest.mark.filterwarnings(
    "ignore:This process .* is multi-threaded:DeprecationWarning"
)
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

    The ``filterwarnings`` mark is not cosmetic. CPython 3.12+ raises
    ``DeprecationWarning`` from ``os.fork()`` in a multi-threaded process, and
    whether this process has a thread by now depends on which modules pytest has
    imported — so the warning appears in some runs and not others. Under
    ``-W error`` that is the difference between a pass and a failure for a reason
    that has nothing to do with the property. The child does allocate (numpy) and
    take locks between the fork and ``os._exit``, which is exactly what the
    warning is about; the fork is kept because §4.3's claim *is* about ``fork``,
    and the parent waits on a pipe so a wedged child fails the suite rather than
    hanging it forever.
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


def test_the_path_arguments_accept_os_pathlike(live, tmp_path):
    """`freeze` and `open_file` take a `pathlib.Path`, not only a `str`.

    A dataloader is precisely where paths arrive as `Path`, and these are the
    binding's first filesystem-path arguments, so there was no earlier spelling
    to stay consistent with.

    Mutant: change either signature back to `&str`. `open_file(pathlib.Path(...))`
    then raises ``TypeError: 'PosixPath' object cannot be converted to 'PyString'``
    and this fails.
    """
    path = tmp_path / "pathlike.tft"
    live.freeze(path)
    assert path.exists()
    tree = tf_tree.open_file(path)
    assert tree.plan("map", "lidar").at(int(COMMON[0])).shape == (4, 4)

    # `OSError.filename` stays a `str`, as CPython's own does for a `str`
    # argument — PyO3 would have made it a `PosixPath` had `frozen_err` handed
    # back a `PathBuf` instead of an `OsString`.
    with pytest.raises(FileNotFoundError) as e:
        tf_tree.open_file(tmp_path / "absent.tft")
    assert e.value.filename == str(tmp_path / "absent.tft")
    assert isinstance(e.value.filename, str)


def test_freeze_releases_the_gil_for_the_copy(tmp_path):
    """A freeze must not stop every other thread in the process for its duration.

    `PyPlan.at_into` releases the GIL above 1 µs of estimated work; a freeze is
    four orders of magnitude past that and was holding it. Measured on the host
    that wrote this test, over a 39.9 MB arena: with the GIL held the heartbeat
    thread's worst gap was **89–124 ms against an 89–124 ms freeze** — it did not
    run at all — and with the GIL released it was **1.2–5.8 ms against a 52–68 ms
    freeze**. The threshold below sits between those two populations with roughly
    a five-fold margin on each side.

    A *sleeping* thread is the probe rather than a spinning one on purpose: a spin
    loop's own allocation makes its idle gaps noisy enough (up to 4.9 ms here) to
    swamp the signal, and "the thread servicing the progress bar or the socket" is
    the case that actually matters.

    Mutant: drop the ``py.detach`` in ``freeze_impl`` and call ``freeze_to``
    directly. The stall becomes the whole freeze and this fails.

    On a free-threaded build there is no GIL to hold, so this passes for a reason
    unrelated to the fix — it is a real assertion only under `just py-test`, not
    under `just py-test-freethreaded`.
    """
    # 32 edges x 16384 slots: big enough that the freeze dominates scheduler
    # noise, small enough (~40 MB) to be polite about disk.
    edges = [(f"f{i}", f"f{i + 1}") for i in range(32)]
    t = tf_tree.build(edges, capacity=16384)
    stamps = np.arange(4, dtype=np.int64) * MS
    poses = np.zeros((4, 7), dtype=np.float64)
    poses[:, 0] = 1.0
    with t.publisher("f1", "f0") as p:
        p.push_many(stamps, poses)

    gaps: list[float] = []
    stop = threading.Event()

    def heartbeat() -> None:
        prev = time.perf_counter()
        while not stop.is_set():
            time.sleep(0.001)
            now = time.perf_counter()
            gaps.append(now - prev)
            prev = now

    path = tmp_path / "gil.tft"
    th = threading.Thread(target=heartbeat)
    th.start()
    try:
        time.sleep(0.05)  # let the thread reach steady state
        gaps.clear()
        t0 = time.perf_counter()
        t.freeze(path)
        wall = time.perf_counter() - t0
        stall = max(gaps)
    finally:
        stop.set()
        th.join()
        path.unlink(missing_ok=True)

    # If a future host freezes 40 MB so fast that the GIL-held case would stall
    # less than the scheduler noise, this test can no longer tell the two apart —
    # say so instead of passing vacuously.
    if wall < 0.020:
        pytest.skip(f"freeze took {wall * 1e3:.1f} ms: too fast to discriminate")

    assert stall < 0.5 * wall, (
        f"a concurrent thread stalled {stall * 1e3:.1f} ms across a "
        f"{wall * 1e3:.1f} ms freeze: the GIL was held for the copy"
    )
