"""The Python surface (`docs/PHASE3.md` §3, §4, §5)."""

import numpy as np
import pytest
import tf_tree


@pytest.fixture
def tree():
    """A two-edge chain with two samples on the first edge.

    Module-scoped fixtures defined inline are avoided deliberately: pytest 9.1
    can execute an inline autouse fixture twice under ``--doctest-modules``
    (§10.1), and a fixture that builds an arena twice would be a confusing
    failure rather than an obvious one.
    """
    t = tf_tree.build([("map", "base"), ("base", "cam")])
    tf_tree.push(t, "base", "map", 1_000, [1.0, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0])
    tf_tree.push(t, "base", "map", 2_000, [1.0, 0.0, 0.0, 0.0, 3.0, 4.0, 5.0])
    return t


def test_scalar_lookup_returns_a_4x4(tree):
    p = tree.plan("map", "base")
    m = p.at(1_500)
    assert m.shape == (4, 4)
    assert m.dtype == np.float64
    # Midway between the two samples, so the translation is their mean.
    np.testing.assert_allclose(m[:3, 3], [2.0, 3.0, 4.0])
    # A rigid transform's bottom row is exact, not approximately right.
    assert m[3, 0] == 0.0 and m[3, 1] == 0.0 and m[3, 2] == 0.0 and m[3, 3] == 1.0


def test_batch_equals_scalar_bit_for_bit(tree):
    """§11.1: ``at(t)`` must equal ``at([t])[0]`` *exactly*.

    Not approximately. The batch path uses resumable cursors and the scalar path
    does not, so any divergence would be two implementations of the same
    interpolation drifting apart — and a tolerance-based check would hide it
    until the drift grew.
    """
    p = tree.plan("map", "base")
    stamps = np.array([1_000, 1_250, 1_500, 1_750, 2_000], dtype=np.int64)
    batch = p.at(stamps)
    assert batch.shape == (5, 4, 4)
    for i, s in enumerate(stamps):
        np.testing.assert_array_equal(batch[i], p.at(int(s)))


def test_at_into_writes_the_same_values_and_allocates_nothing(tree):
    p = tree.plan("map", "base")
    stamps = np.array([1_000, 1_500, 2_000], dtype=np.int64)
    expected = p.at(stamps)

    out = np.empty((3, 4, 4), dtype=np.float64)
    before = out.__array_interface__["data"][0]
    p.at_into(stamps, out)
    np.testing.assert_array_equal(out, expected)
    # Written in place: the caller's buffer, not a replacement for it.
    assert out.__array_interface__["data"][0] == before


def test_a_float_stamp_is_refused_with_the_measurement(tree):
    """§3: the rejection carries the number, not an opinion.

    Users argue with rules and accept measurements, so the exception states the
    238 ns ULP rather than asserting that floats are unsuitable.
    """
    p = tree.plan("map", "base")
    with pytest.raises(TypeError, match="238 ns"):
        p.at(1.5)


def test_from_sec_is_the_only_route_from_float_seconds():
    assert tf_tree.from_sec(1.5) == 1_500_000_000
    with pytest.raises(ValueError):
        tf_tree.from_sec(float("nan"))


def test_a_wrong_shaped_out_is_refused_before_anything_is_written(tree):
    """§5.3: a half-written output is worse than none — it looks like data."""
    p = tree.plan("map", "base")
    stamps = np.array([1_000, 1_500, 2_000], dtype=np.int64)

    sentinel = -12345.5
    out = np.full((2, 4, 4), sentinel, dtype=np.float64)
    with pytest.raises(tf_tree.BufferError):
        p.at_into(stamps, out)
    assert np.all(out == sentinel), "the buffer was written before validation"


def test_a_non_contiguous_out_is_refused_rather_than_silently_copied(tree):
    """A silent copy would defeat the whole purpose while appearing to work.

    The user would ship it and wonder why their profile did not change.
    """
    p = tree.plan("map", "base")
    stamps = np.array([1_000, 1_500, 2_000], dtype=np.int64)
    strided = np.empty((3, 4, 8), dtype=np.float64)[:, :, ::2]
    assert not strided.flags["C_CONTIGUOUS"]
    with pytest.raises(tf_tree.BufferError):
        p.at_into(stamps, strided)


def test_an_unknown_frame_names_itself(tree):
    with pytest.raises(tf_tree.FrameNotDeclaredError, match="nope"):
        tree.plan("map", "nope")


def test_no_result_aliases_the_tree(tree):
    """§5.1: nothing hands Python a view into arena memory.

    Two lookups at the same stamp must be independent arrays — if either aliased
    the ring, a concurrent publisher would mutate a value the caller already
    holds, which is the torn-pose failure this rule exists to prevent.
    """
    p = tree.plan("map", "base")
    a = p.at(1_500)
    b = p.at(1_500)
    assert a.__array_interface__["data"][0] != b.__array_interface__["data"][0]
    a[0, 0] = 99.0
    assert b[0, 0] != 99.0


@pytest.fixture
def curved():
    """A tree whose path actually curves.

    The plain `tree` fixture rotates not at all and translates linearly, so
    LERP between any two of its samples is **exact** — an adaptive subdivision
    returns two knots and reconstructs perfectly at any tolerance whatsoever.
    A tolerance test on that fixture passes even when the tolerance is ignored,
    which is how this one was first written and how it was caught.

    Here the rotation sweeps a radian about Z while the translation follows the
    arc, so the geodesic genuinely departs from the chord.
    """
    t = tf_tree.build([("map", "base")])
    for k in range(21):
        u = k / 20.0
        theta = u * 1.0
        # Rotation about Z by `theta`, translation on the unit circle.
        t_ = tf_tree.push(
            t,
            "base",
            "map",
            1_000 + k * 1_000,
            [
                float(np.cos(theta / 2)),
                0.0,
                0.0,
                float(np.sin(theta / 2)),
                float(np.cos(theta)),
                float(np.sin(theta)),
                0.0,
            ],
        )
        assert t_ is None
    return t


def test_adaptive_reconstructs_within_tolerance(curved):
    """The knots must actually bound the error they claim to (§4.2, §5.6).

    Asserting only "some knots came back" would pass for any subdivision at
    all. So this reconstructs by LERP *between* the knots and compares against
    a dense exact evaluation — the property a consumer relies on.
    """
    p = curved.plan("map", "base")
    lo, hi = 1_000, 21_000
    stamps, poses = p.adaptive(lo, hi, lin=1e-4, ang=1e-4)

    assert stamps.shape[0] == poses.shape[0]
    assert poses.shape[1:] == (4, 4)
    assert stamps[0] == lo and stamps[-1] == hi
    assert np.all(np.diff(stamps) > 0), "knots must be strictly increasing"

    probe = np.linspace(lo, hi, 200).astype(np.int64)
    exact = p.at(probe)
    for i, t in enumerate(probe):
        j = int(np.searchsorted(stamps, t, side="right")) - 1
        j = min(max(j, 0), len(stamps) - 2)
        span = stamps[j + 1] - stamps[j]
        u = 0.0 if span == 0 else (t - stamps[j]) / span
        lerped = poses[j][:3, 3] * (1 - u) + poses[j + 1][:3, 3] * u
        err = float(np.max(np.abs(lerped - exact[i][:3, 3])))
        assert err < 1e-2, f"reconstruction at {t} was off by {err}"


def test_a_tighter_tolerance_needs_more_knots(curved):
    """**This is the test that catches an ignored tolerance.**

    A subdivision that hard-codes its bound still returns increasing stamps and
    still reconstructs, so the test above passes. Only the *response* to the
    tolerance distinguishes it — and on a curved path a 1e-6 bound must cost
    strictly more knots than a 1e-1 one.
    """
    p = curved.plan("map", "base")
    tight, _ = p.adaptive(1_000, 21_000, lin=1e-6, ang=1e-6)
    loose, _ = p.adaptive(1_000, 21_000, lin=1e-1, ang=1e-1)
    assert len(loose) < len(tight), (
        f"tolerance had no effect: {len(loose)} knots at 1e-1 vs "
        f"{len(tight)} at 1e-6 — the subdivision is ignoring its bound"
    )


def test_a_nonsense_tolerance_is_refused(tree):
    p = tree.plan("map", "base")
    for bad in ({"lin": 0.0}, {"lin": -1.0}, {"ang": float("nan")}):
        with pytest.raises(ValueError):
            p.adaptive(1_000, 2_000, **bad)


def test_a_plan_keeps_its_tree_alive():
    """**A `Plan` outliving its `Tree` must not read freed memory.**

    This was a real use-after-free: `PyPlan` held a raw `*const Tree` justified
    by a doc comment claiming Python held a reference. It did not. Dropping the
    tree freed the arena while the plan still pointed into it, and — worse than
    a crash — the read *succeeded*, because the allocation was still mapped. It
    surfaced as a nonsense `UnknownEdge` rather than as anything obviously
    wrong.

    So the assertion is not "it does not crash": it is that the lookup still
    returns the *right numbers* after the tree is dropped, which is only
    possible if the arena is genuinely still alive.
    """
    import gc

    tree = tf_tree.build([("map", "base")])
    tf_tree.push(tree, "base", "map", 1_000, [1.0, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0])
    tf_tree.push(tree, "base", "map", 2_000, [1.0, 0.0, 0.0, 0.0, 3.0, 4.0, 5.0])
    plan = tree.plan("map", "base")
    before = plan.at(1_500)

    del tree
    gc.collect()

    np.testing.assert_array_equal(plan.at(1_500), before)
    np.testing.assert_allclose(plan.at(1_500)[:3, 3], [2.0, 3.0, 4.0])


def test_publisher_round_trips_through_the_context_manager():
    tree = tf_tree.build([("map", "base")])
    with tree.publisher("base", "map") as pub:
        pub.push(1_000, [1.0, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0])
        pub.push(2_000, [1.0, 0.0, 0.0, 0.0, 3.0, 4.0, 5.0])
    p = tree.plan("map", "base")
    np.testing.assert_allclose(p.at(1_500)[:3, 3], [2.0, 3.0, 4.0])


def test_a_released_publisher_refuses_to_publish():
    """§4.3: the context manager is the documented form, so leaving it must
    actually release — not merely stop being convenient.

    A claim held past its scope is a claim no other process can take, and the
    symptom is a peer that cannot publish for a reason nothing reports.
    """
    tree = tf_tree.build([("map", "base")])
    with tree.publisher("base", "map") as pub:
        pub.push(1_000, [1.0, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0])
    with pytest.raises(tf_tree.TfTreeError, match="already released"):
        pub.push(2_000, [1.0, 0.0, 0.0, 0.0, 3.0, 4.0, 5.0])

    # And the edge is genuinely free again: a second claim succeeds.
    with tree.publisher("base", "map") as second:
        second.push(2_000, [1.0, 0.0, 0.0, 0.0, 3.0, 4.0, 5.0])


def test_push_many_matches_a_loop_of_push():
    tree_a = tf_tree.build([("map", "base")])
    tree_b = tf_tree.build([("map", "base")])
    stamps = np.arange(1_000, 1_000 + 32 * 100, 100, dtype=np.int64)
    poses = np.zeros((32, 7))
    poses[:, 0] = 1.0
    poses[:, 4] = np.arange(32, dtype=np.float64)

    with tree_a.publisher("base", "map") as pub:
        pub.push_many(stamps, poses)
    with tree_b.publisher("base", "map") as pub:
        for i, s in enumerate(stamps):
            pub.push(int(s), list(poses[i]))

    pa = tree_a.plan("map", "base")
    pb = tree_b.plan("map", "base")
    np.testing.assert_array_equal(pa.at(stamps), pb.at(stamps))


def test_push_many_names_the_sample_it_rejected():
    """A batch that fails partway is not a batch that failed.

    The samples before the bad one *were* published, so an error naming only
    the batch would leave the caller unable to tell how far it got.
    """
    tree = tf_tree.build([("map", "base")])
    stamps = np.array([3_000, 2_000], dtype=np.int64)  # non-monotonic
    poses = np.zeros((2, 7))
    poses[:, 0] = 1.0
    with (
        tree.publisher("base", "map") as pub,
        pytest.raises(tf_tree.TfTreeError, match="sample 1"),
    ):
        pub.push_many(stamps, poses)


def test_a_publisher_keeps_its_tree_alive():
    """The same lifetime guarantee `Plan` needed, for the same reason."""
    import gc

    tree = tf_tree.build([("map", "base")])
    pub = tree.publisher("base", "map")
    plan = tree.plan("map", "base")
    del tree
    gc.collect()
    pub.push(1_000, [1.0, 0.0, 0.0, 0.0, 7.0, 8.0, 9.0])
    np.testing.assert_allclose(plan.latest()[:3, 3], [7.0, 8.0, 9.0])


class _FakeCudaArray:
    """An object that claims to live on a CUDA device.

    Stands in for `torch.empty(..., device="cuda")` without a GPU or a CUDA
    runtime, both of which decision D8 forbids as dependencies. What is under
    test is the *classification*, and that reads only `__dlpack_device__`.
    """

    def __dlpack_device__(self):
        return (2, 0)  # kDLCUDA


class _FakePinned:
    """Pinned host memory, which reports `kDLCUDAHost` and *is* writable."""

    def __dlpack_device__(self):
        return (3, 0)


def test_device_memory_is_refused_with_an_actionable_message(tree):
    """§5.5: never attempt the write.

    A CPU store to a `cudaMalloc` pointer is undefined — not slow, undefined —
    so the only acceptable outcome is a refusal. And the message has to say
    what to do instead, because "wrong device" tells a user nothing they did
    not already suspect.
    """
    p = tree.plan("map", "base")
    stamps = np.array([1_000, 1_500], dtype=np.int64)
    with pytest.raises(tf_tree.BufferError) as e:
        p.at_into(stamps, _FakeCudaArray())
    msg = str(e.value)
    assert "device type 2" in msg
    assert "pin_memory" in msg, "the error must name the fix, not just the fault"


def test_a_host_device_type_is_not_refused_for_being_dlpack(tree):
    """Pinned host memory must pass the *device* check.

    It then fails the layout check, which is the correct second failure — this
    fake exposes no buffer. The point is that it is refused for the right
    reason: a blanket rejection of anything with `__dlpack_device__` would lock
    out every legitimate pinned buffer.
    """
    p = tree.plan("map", "base")
    stamps = np.array([1_000, 1_500], dtype=np.int64)
    with pytest.raises(tf_tree.BufferError) as e:
        p.at_into(stamps, _FakePinned())
    assert "device type" not in str(e.value), (
        "pinned host memory was rejected as device memory"
    )


def test_a_plain_numpy_array_still_works(tree):
    """The device check must not have broken the ordinary path."""
    p = tree.plan("map", "base")
    stamps = np.array([1_000, 1_500, 2_000], dtype=np.int64)
    out = np.empty((3, 4, 4))
    p.at_into(stamps, out)
    np.testing.assert_array_equal(out, p.at(stamps))


def test_lookup_matches_a_compiled_plan_exactly(tree):
    """§4.2's convenience must not be a *different* answer.

    It goes through a per-thread plan cache rather than a fresh compile, so a
    stale or mis-keyed entry would show up as a wrong transform here and
    nowhere else.
    """
    p = tree.plan("map", "base")
    for stamp in (1_000, 1_500, 2_000):
        np.testing.assert_array_equal(tree.lookup("map", "base", stamp), p.at(stamp))


def test_lookup_reports_an_unknown_frame(tree):
    with pytest.raises(tf_tree.FrameNotDeclaredError):
        tree.lookup("map", "nope", 1_500)


def test_an_in_process_tree_has_no_instance_uuid(tree):
    """All-zero is the "not a shared instance" sentinel, and `__repr__` hides it.

    Showing 32 zeros on an in-process tree reads like a bug in the uuid rather
    than like the absence of one.
    """
    assert tree.instance_uuid() == "0" * 32
    assert "instance=" not in repr(tree)
    assert "shared=False" in repr(tree)


def test_reprs_spell_booleans_the_python_way(tree):
    """A repr is read by a Python programmer.

    Rust's `false` in a repr looks like a stringly-typed field rather than a
    bool, and someone will eventually compare against the string.
    """
    assert "writable=True" in repr(tree)
    with tree.publisher("base", "map") as pub:
        assert "held=True" in repr(pub)
    assert "held=False" in repr(pub)


def test_at_into_accepts_a_scalar_stamp_and_a_4x4(tree):
    """The allocation-free scalar path (§5.2).

    A control loop does **one** lookup per tick and cannot batch, so `at`'s
    allocation is paid every tick forever. Measured on a depth-3 chain, release
    build: `at` 224 ns, `at_into` **173 ns** — and nothing allocated.
    """
    p = tree.plan("map", "base")
    out = np.empty((4, 4))
    p.at_into(1_500, out)
    np.testing.assert_array_equal(out, p.at(1_500))

    # Reusing the buffer is the whole point, so a second call must overwrite
    # rather than accumulate.
    p.at_into(1_000, out)
    np.testing.assert_array_equal(out, p.at(1_000))


def test_at_into_rejects_a_scalar_stamp_with_a_batch_buffer(tree):
    """A shape mismatch is refused, not reinterpreted.

    `(1, 4, 4)` holds the same sixteen doubles as `(4, 4)`, so writing into it
    would "work" — and then the caller's next `out[i]` would index an array that
    is one dimension off from what they think.
    """
    p = tree.plan("map", "base")
    with pytest.raises(tf_tree.BufferError):
        p.at_into(1_500, np.empty((1, 4, 4)))
    with pytest.raises(tf_tree.BufferError):
        p.at_into(1_500, np.empty((3, 4)))


def test_at_into_still_rejects_a_non_contiguous_scalar_buffer(tree):
    """Non-contiguous is refused rather than silently copied.

    A silent copy would defeat the point of the method while appearing to work,
    and the user would ship it and wonder why their profile did not change.
    """
    p = tree.plan("map", "base")
    with pytest.raises(tf_tree.BufferError):
        p.at_into(1_500, np.empty((4, 8))[:, ::2])


@pytest.mark.parametrize(
    ("stamps", "shape"),
    [(1_500, (4, 4)), (np.array([1_500], dtype=np.int64), (1, 4, 4))],
)
def test_at_into_refuses_a_non_writable_buffer(tree, stamps, shape):
    """**A read-only buffer must be refused, not written and not faulted.**

    `as_slice_mut` checks neither `NPY_ARRAY_WRITEABLE` nor aliasing, so without
    an explicit check a `flags.writeable = False` array was *silently
    overwritten* — and a read-only `np.memmap`, which is a `PROT_READ` page,
    took `SIGSEGV` inside what looks like an ordinary lookup. §5.5's rule is
    refuse rather than fault, and it applies to host memory the caller cannot
    write exactly as much as to device memory.

    Both the scalar and the batch path had this; both are checked here.
    """
    p = tree.plan("map", "base")
    out = np.zeros(shape)
    out.flags.writeable = False
    with pytest.raises(tf_tree.BufferError, match="not writable"):
        p.at_into(stamps, out)
    # And nothing was written on the way to the refusal.
    assert not out.any()


def test_at_into_refuses_a_read_only_memmap_instead_of_faulting(tree, tmp_path):
    """The same check, against memory the process genuinely cannot write.

    `flags.writeable = False` is NumPy's own bookkeeping and a store to it would
    "work"; a read-only `mmap` is enforced by the MMU and a store is `SIGSEGV`.
    This is the case that turns a missing check into a crash, so it is tested
    against the real thing rather than the flag alone.
    """
    path = tmp_path / "ro.bin"
    path.write_bytes(b"\0" * 128)
    m = np.memmap(path, dtype=np.float64, mode="r", shape=(4, 4))
    p = tree.plan("map", "base")
    with pytest.raises(tf_tree.BufferError, match="not writable"):
        p.at_into(1_500, m)


def test_at_into_errors_name_the_argument_that_is_wrong(tree):
    """A mismatch must blame `out`, not the `stamps` the caller got right.

    Probing `out` before dispatching on `stamps` produced two misleading
    messages: a scalar stamp with an `(1, 4, 4)` buffer reported "stamps must be
    an (N,) int64 array", and an `(N,)` array with a `(4, 4)` buffer escaped as
    numpy's own `TypeError: only integer scalar arrays can be converted to a
    scalar index` — leaked from `stamp_from_any`, and not even a `BufferError`.
    """
    p = tree.plan("map", "base")
    with pytest.raises(tf_tree.BufferError, match=r"scalar stamp needs out"):
        p.at_into(1_500, np.empty((1, 4, 4)))
    with pytest.raises(tf_tree.BufferError, match=r"\(N, 4, 4\)"):
        p.at_into(np.array([1_500], dtype=np.int64), np.empty((4, 4)))


def test_at_into_refuses_a_non_numpy_buffer_and_says_so(tree):
    """`PHASE3.md` §5.5 steps 2-4 are not implemented, and the error admits it.

    The spec's acquisition order goes through the buffer protocol; `at_into`
    casts to `numpy.ndarray` instead. A `memoryview` with a perfectly correct
    `(4, 4)` float64 layout is therefore refused, and the message names
    `np.asarray(...)` rather than leaving the caller to debug a buffer that is
    fine.
    """
    p = tree.plan("map", "base")
    mv = memoryview(bytearray(128)).cast("d", (4, 4))
    with pytest.raises(tf_tree.BufferError, match="numpy array"):
        p.at_into(1_500, mv)
