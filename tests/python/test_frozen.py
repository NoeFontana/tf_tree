"""The offline API (`docs/PHASE5.md` §4)."""

import os
import pathlib
import threading
import time

import numpy as np
import pytest
import tf_tree

# Every test in this module goes through the frozen `.tft` path, which is
# `#[cfg(all(feature = "shm", target_os = "linux"))]` in the facade.
pytestmark = pytest.mark.skipif(
    not tf_tree.has_shared_memory(),
    reason="frozen .tft files need the mmap-backed arena (Linux only)",
)

MS = 1_000_000

# Three edges with *deliberately different* retained windows, staggered by 100 ms.
EDGES = [("map", "odom"), ("odom", "base_link"), ("base_link", "lidar")]
N = 100
STEP = 10 * MS
OFFSETS = [0, 100 * MS, 200 * MS]
# The intersection those three windows have: the last edge to start and the first to
# stop.
COMMON = (200 * MS, (N - 1) * STEP)


def _poses(seed: float) -> np.ndarray:
    """`(N, 7)` quaternion-and-translation samples that repeat in no component."""
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
    """§4.1: the same call, the same object, the same bits."""
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
    """§4.1 names ``plan``, ``at``, ``at_into``, ``adaptive`` and ``latest``."""
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
    """§2.4: `AttachMode` is implicitly and permanently `ReadOnly`."""
    assert frozen.is_writable() is False
    with pytest.raises(tf_tree.TfTreeError):
        frozen.publisher("odom", "map")


def test_span_is_the_intersection_of_the_retained_windows(frozen):
    """§4.2: `span` is `LatestCommon` generalised to a range."""
    assert frozen.span("map", "lidar") == COMMON
    # Each single-edge span is its own window, and none of them equals the
    # intersection — otherwise the assertion above would be trivially true.
    assert frozen.span("map", "odom") == (0, (N - 1) * STEP)
    assert frozen.span("base_link", "lidar") == (200 * MS, (N - 1) * STEP + 200 * MS)
    assert frozen.span("map", "odom") != COMMON


def test_span_answers_at_the_ends_it_reports(frozen):
    """The interval means what it says: answerable inside, refused outside."""
    t0, t1 = frozen.span("map", "lidar")
    plan = frozen.plan("map", "lidar")
    assert plan.at(t0).shape == (4, 4)
    assert plan.at(t1).shape == (4, 4)
    with pytest.raises(tf_tree.ExtrapolationError):
        plan.at(t0 - 1)
    with pytest.raises(tf_tree.ExtrapolationError):
        plan.at(t1 + 1)


def test_span_of_an_empty_plan_is_none(frozen):
    """`None` means unbounded, and is not the same as an empty interval."""
    assert frozen.span("map", "map") is None


def test_span_names_the_frames_of_the_edge_that_has_never_published():
    """The answer to "why did my lookup fail at t" is nearly always this (§4.2)."""
    t = tf_tree.build(EDGES, capacity=64)
    stamps = np.arange(N, dtype=np.int64) * STEP
    # Publish on the first two edges so exactly one edge — `base_link -> lidar` — is
    # silent.
    for j, (parent, child) in enumerate(EDGES[:2]):
        with t.publisher(child, parent) as p:
            p.push_many(stamps, _poses(1.0 + j))
    with pytest.raises(tf_tree.NoDataError) as e:
        t.span("map", "lidar")
    msg = str(e.value)
    # The *pair*, in edge order.
    assert '"base_link" -> "lidar"' in msg, msg
    assert "EdgeId" not in msg, msg
    # `span_impl`'s own contribution, and the only assertion here that
    # `lookup_err`'s shared arm does not already satisfy.
    assert "on the path from" in msg, msg


def test_open_file_of_a_missing_path_raises_filenotfound(tmp_path):
    """The errno path is a real `OSError` subclass, not our hierarchy."""
    missing = tmp_path / "absent.tft"
    with pytest.raises(FileNotFoundError) as e:
        tf_tree.open_file(str(missing))
    # The path is in the exception, not only in the message: a dataloader that
    # opens sixteen files needs to know which one.
    assert e.value.filename == str(missing)


def test_open_file_of_a_file_that_is_not_a_tft_says_so(tmp_path):
    """A foreign file is refused, in our exception hierarchy, naming the path."""
    junk = tmp_path / "not.tft"
    junk.write_bytes(b"PK\x03\x04" + os.urandom(8192))
    with pytest.raises(tf_tree.TfTreeError) as e:
        tf_tree.open_file(str(junk))
    assert str(junk) in str(e.value)
    assert ".tft" in str(e.value)


@pytest.mark.parametrize(
    ("field", "offset", "variant"),
    [("layout_hash", 12, "LayoutMismatch"), ("format_version", 8, "VersionMismatch")],
)
def test_a_damaged_arena_header_reports_the_engines_reason_as_prose(
    live, tmp_path, field, offset, variant
):
    """One flipped bit in a field of the *arena* header inside a ``.tft``."""
    path = tmp_path / f"bad_{field}.tft"
    live.freeze(str(path), source="synthetic")
    data = bytearray(path.read_bytes())
    # `FrozenHeader` (PHASE5.md §2.3): `arena_off` is the u64 at offset 32.
    arena_off = int.from_bytes(data[32:40], "little")
    data[arena_off + offset] ^= 1
    path.write_bytes(bytes(data))

    with pytest.raises(tf_tree.TfTreeError) as e:
        tf_tree.open_file(str(path))
    msg = str(e.value)
    assert str(path) in msg, msg
    assert "{" not in msg and "}" not in msg, msg
    assert "raw" not in msg, msg
    assert variant in msg, msg
    if field == "layout_hash":
        assert "re-freez" in msg.lower(), msg


def test_freeze_replaces_the_path_atomically_and_leaves_no_litter(live, tmp_path):
    """The temporary is a *sibling* and is renamed over the target (§2.3)."""
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
    """§4.3's rule is right; §4.3's *reason* does not apply to a `.tft`."""
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
    """`freeze` and `open_file` take a `pathlib.Path`, not only a `str`."""
    path = tmp_path / "pathlike.tft"
    live.freeze(path)
    assert path.exists()
    tree = tf_tree.open_file(path)
    assert tree.plan("map", "lidar").at(int(COMMON[0])).shape == (4, 4)

    # `OSError.filename` stays a `str`, as CPython's own does for a `str` argument —
    # PyO3 would have made it a `PosixPath` had `frozen_err` handed back a `PathBuf`
    # instead of an `OsString`.
    with pytest.raises(FileNotFoundError) as e:
        tf_tree.open_file(tmp_path / "absent.tft")
    assert e.value.filename == str(tmp_path / "absent.tft")
    assert isinstance(e.value.filename, str)


def test_freeze_releases_the_gil_for_the_copy(tmp_path):
    """A freeze must not stop every other thread in the process for its duration."""
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

    # If a future host freezes 40 MB so fast that the GIL-held case would stall less
    # than the scheduler noise, this test can no longer tell the two apart — say so
    # instead of passing vacuously.
    if wall < 0.020:
        pytest.skip(f"freeze took {wall * 1e3:.1f} ms: too fast to discriminate")

    assert stall < 0.5 * wall, (
        f"a concurrent thread stalled {stall * 1e3:.1f} ms across a "
        f"{wall * 1e3:.1f} ms freeze: the GIL was held for the copy"
    )
