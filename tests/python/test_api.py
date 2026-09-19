"""The Python surface (`docs/PHASE3.md` §3, §4, §5)."""

import numpy as np
import pytest
import tf_tree


@pytest.fixture
def tree():
    """A two-edge chain with two samples on the first edge."""
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
    """§11.1: ``at(t)`` must equal ``at([t])[0]`` *exactly*."""
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
    """§3: the rejection carries the number, not an opinion."""
    p = tree.plan("map", "base")
    with pytest.raises(TypeError, match="238 ns"):
        p.at(1.5)


POSE7 = [1.0, 0.0, 0.0, 0.0, 9.0, 9.0, 9.0]

# `(id, call, a stamp the call accepts)` for every entry point that takes a scalar stamp
# without going through `Plan.at`'s dispatch.
SCALAR_STAMP_ENTRY_POINTS = [
    ("Tree.lookup", lambda t, s: t.lookup("map", "base", s), 1_500),
    ("Publisher.push", lambda t, s: t.publisher("base", "map").push(s, POSE7), 3_000),
    ("tf_tree.push", lambda t, s: tf_tree.push(t, "base", "map", s, POSE7), 3_000),
    ("adaptive start", lambda t, s: t.plan("map", "base").adaptive(s, 2_000), 1_500),
    ("adaptive end", lambda t, s: t.plan("map", "base").adaptive(1_000, s), 1_500),
]
SCALAR_STAMP_IDS = [row[0] for row in SCALAR_STAMP_ENTRY_POINTS]


@pytest.mark.parametrize(
    "call", [row[1] for row in SCALAR_STAMP_ENTRY_POINTS], ids=SCALAR_STAMP_IDS
)
@pytest.mark.parametrize("stamp", [1.5, np.float64(1500.0)], ids=["float", "f64"])
def test_every_scalar_stamp_refuses_a_float_with_the_measurement(tree, call, stamp):
    """§3 is NORMATIVE, and §14 ticked "no `float` stamp accepted anywhere"."""
    with pytest.raises(TypeError, match="238 ns"):
        call(tree, stamp)


@pytest.mark.parametrize(
    ("call", "ok"),
    [row[1:] for row in SCALAR_STAMP_ENTRY_POINTS],
    ids=SCALAR_STAMP_IDS,
)
def test_every_scalar_stamp_accepts_a_numpy_int64(tree, call, ok):
    """The other half of §3's list, on the same five entry points."""
    t = np.array([0, ok], dtype=np.int64)[1]
    assert isinstance(t, np.int64) and not isinstance(t, int)
    call(tree, t)


# Every scalar-stamp route, `Plan.at`'s dispatch included this time: the numpy
# float scalars below reach `stamp_from_any` through all of them.
NUMPY_FLOAT_ROUTES = [
    *SCALAR_STAMP_ENTRY_POINTS,
    ("Plan.at", lambda t, s: t.plan("map", "base").at(s), 1_500),
    (
        "Plan.at_into mat4",
        lambda t, s: t.plan("map", "base").at_into(s, np.zeros((4, 4))),
        1_500,
    ),
    (
        "Plan.at_into quat",
        lambda t, s: t.plan("map", "base").at_into(s, np.zeros(7), layout="quat"),
        1_500,
    ),
]


@pytest.mark.parametrize(
    "call",
    [row[1] for row in NUMPY_FLOAT_ROUTES],
    ids=[row[0] for row in NUMPY_FLOAT_ROUTES],
)
@pytest.mark.parametrize(
    "stamp",
    [np.float32(1500.0), np.float16(1500.0)],
    ids=["f32", "f16"],
)
def test_a_numpy_float_scalar_that_is_not_a_float_meets_the_measurement(
    tree, call, stamp
):
    """§3's refusal for the numpy float scalars that do not subclass ``float``."""
    assert not isinstance(stamp, float)
    with pytest.raises(TypeError, match="238 ns"):
        call(tree, stamp)


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
    """A silent copy would defeat the whole purpose while appearing to work."""
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
    """§5.1: nothing hands Python a view into arena memory."""
    p = tree.plan("map", "base")
    a = p.at(1_500)
    b = p.at(1_500)
    assert a.__array_interface__["data"][0] != b.__array_interface__["data"][0]
    a[0, 0] = 99.0
    assert b[0, 0] != 99.0


@pytest.fixture
def curved():
    """A tree whose path actually curves."""
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
    """The knots must actually bound the error they claim to (§4.2, §5.6)."""
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
    """**This is the test that catches an ignored tolerance.**"""
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
    """**A `Plan` outliving its `Tree` must not read freed memory.**"""
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
    """§4.3: the context manager is the documented form, so leaving it must actually
    release — not merely stop being convenient.
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
    """A batch that fails partway is not a batch that failed."""
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
    """A publisher outlives the `Tree` object it was claimed from."""
    import gc

    tree = tf_tree.build([("map", "base")])
    pub = tree.publisher("base", "map")
    plan = tree.plan("map", "base")
    del tree
    gc.collect()
    pub.push(1_000, [1.0, 0.0, 0.0, 0.0, 7.0, 8.0, 9.0])
    np.testing.assert_allclose(plan.latest()[:3, 3], [7.0, 8.0, 9.0])


class _FakeCudaArray:
    """An object that claims to live on a CUDA device."""

    def __dlpack_device__(self):
        return (2, 0)  # kDLCUDA


class _FakePinned:
    """Pinned host memory, which reports `kDLCUDAHost` and *is* writable."""

    def __dlpack_device__(self):
        return (3, 0)


def test_device_memory_is_refused_with_an_actionable_message(tree):
    """§5.5: never attempt the write."""
    p = tree.plan("map", "base")
    stamps = np.array([1_000, 1_500], dtype=np.int64)
    with pytest.raises(tf_tree.BufferError) as e:
        p.at_into(stamps, _FakeCudaArray())
    msg = str(e.value)
    assert "device type 2" in msg
    assert "pin_memory" in msg, "the error must name the fix, not just the fault"


def test_a_host_device_type_is_not_refused_for_being_dlpack(tree):
    """Pinned host memory must pass the *device* check."""
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
    """§4.2's convenience must not be a *different* answer."""
    p = tree.plan("map", "base")
    for stamp in (1_000, 1_500, 2_000):
        np.testing.assert_array_equal(tree.lookup("map", "base", stamp), p.at(stamp))


def test_lookup_reports_an_unknown_frame(tree):
    with pytest.raises(tf_tree.FrameNotDeclaredError):
        tree.lookup("map", "nope", 1_500)


def test_an_in_process_tree_has_no_instance_uuid(tree):
    """All-zero is the "not a shared instance" sentinel, and `__repr__` hides it."""
    assert tree.instance_uuid() == "0" * 32
    assert "instance=" not in repr(tree)
    assert "shared=False" in repr(tree)


def test_reprs_spell_booleans_the_python_way(tree):
    """A repr is read by a Python programmer."""
    assert "writable=True" in repr(tree)
    with tree.publisher("base", "map") as pub:
        assert "held=True" in repr(pub)
    assert "held=False" in repr(pub)


def test_at_into_accepts_a_scalar_stamp_and_a_4x4(tree):
    """The allocation-free scalar path (§5.2)."""
    p = tree.plan("map", "base")
    out = np.empty((4, 4))
    p.at_into(1_500, out)
    np.testing.assert_array_equal(out, p.at(1_500))

    # Reusing the buffer is the whole point, so a second call must overwrite
    # rather than accumulate.
    p.at_into(1_000, out)
    np.testing.assert_array_equal(out, p.at(1_000))


def test_at_into_rejects_a_scalar_stamp_with_a_batch_buffer(tree):
    """A shape mismatch is refused, not reinterpreted."""
    p = tree.plan("map", "base")
    with pytest.raises(tf_tree.BufferError):
        p.at_into(1_500, np.empty((1, 4, 4)))
    with pytest.raises(tf_tree.BufferError):
        p.at_into(1_500, np.empty((3, 4)))


def test_at_into_still_rejects_a_non_contiguous_scalar_buffer(tree):
    """Non-contiguous is refused rather than silently copied."""
    p = tree.plan("map", "base")
    with pytest.raises(tf_tree.BufferError):
        p.at_into(1_500, np.empty((4, 8))[:, ::2])


@pytest.mark.parametrize(
    ("stamps", "shape"),
    [(1_500, (4, 4)), (np.array([1_500], dtype=np.int64), (1, 4, 4))],
)
def test_at_into_refuses_a_non_writable_buffer(tree, stamps, shape):
    """**A read-only buffer must be refused, not written and not faulted.**"""
    p = tree.plan("map", "base")
    out = np.zeros(shape)
    out.flags.writeable = False
    with pytest.raises(tf_tree.BufferError, match="not writable"):
        p.at_into(stamps, out)
    # And nothing was written on the way to the refusal.
    assert not out.any()


def test_at_into_refuses_a_read_only_memmap_instead_of_faulting(tree, tmp_path):
    """The same check, against memory the process genuinely cannot write."""
    path = tmp_path / "ro.bin"
    path.write_bytes(b"\0" * 128)
    m = np.memmap(path, dtype=np.float64, mode="r", shape=(4, 4))
    p = tree.plan("map", "base")
    with pytest.raises(tf_tree.BufferError, match="not writable"):
        p.at_into(1_500, m)


def test_at_into_errors_name_the_argument_that_is_wrong(tree):
    """A mismatch must blame `out`, not the `stamps` the caller got right."""
    p = tree.plan("map", "base")
    with pytest.raises(tf_tree.BufferError, match=r"scalar stamp needs out"):
        p.at_into(1_500, np.empty((1, 4, 4)))
    with pytest.raises(tf_tree.BufferError, match=r"\(N, 4, 4\)"):
        p.at_into(np.array([1_500], dtype=np.int64), np.empty((4, 4)))


def test_at_into_refuses_a_non_numpy_buffer_and_says_so(tree):
    """`PHASE3.md` §5.5 steps 2-4 are not implemented, and the error admits it."""
    p = tree.plan("map", "base")
    mv = memoryview(bytearray(128)).cast("d", (4, 4))
    with pytest.raises(tf_tree.BufferError, match="numpy array"):
        p.at_into(1_500, mv)


# ---------------------------------------------------------------------------
# Introspection (`docs/PHASE5.md` §4.4 item 2, `docs/API.md` §3.2)
# ---------------------------------------------------------------------------


def test_frames_lists_every_declared_frame_in_declaration_order(tree):
    """`TreeBuilder` interns names in edge-declaration order, parent then child."""
    assert tree.frames() == ["map", "base", "cam"]


def test_edges_are_parent_child_pairs_and_exclude_the_sentinel(tree):
    """`(parent, child)` — `build`'s order, not `publisher`'s — and no slot 0."""
    assert tree.edges() == [("map", "base"), ("base", "cam")]
    rebuilt = tf_tree.build(tree.edges())
    assert rebuilt.edges() == tree.edges()
    assert rebuilt.frames() == tree.frames()


def test_plan_edges_names_the_edges_the_plan_samples(tree):
    """One entry per `Step::Dyn`, in fold order."""
    p = tree.plan("map", "cam")
    assert p.depth() == 2
    assert p.edges() == [("map", "base"), ("base", "cam")]


def test_plan_edges_report_identity_not_direction(tree):
    """A plan and its reverse sample the same edges."""
    forward = tree.plan("map", "cam").edges()
    backward = tree.plan("cam", "map").edges()
    assert sorted(backward) == sorted(forward)
    assert set(forward) <= set(tree.edges())


def test_plan_edges_of_a_self_plan_is_empty(tree):
    """`lookup(x, x)` compiles to a plan with no steps, so it samples nothing."""
    p = tree.plan("base", "base")
    assert p.depth() == 0
    assert p.edges() == []
    assert tree.span("base", "base") is None


# --------------------------------------------------------------------------- layout=
# (`docs/PHASE5.md` §4.4 item 1, `docs/API.md` §6 row 7)
# ---------------------------------------------------------------------------


@pytest.fixture
def twistable():
    """A tree whose edges can answer a twist."""
    t = tf_tree.build([("map", "base")], interp="sclerp")
    tf_tree.push(t, "base", "map", 1_000_000_000, [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
    tf_tree.push(t, "base", "map", 2_000_000_000, [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0])
    return t


LAYOUT_SHAPES = [
    ("mat4", (4, 4), (3, 4, 4), np.float64),
    ("quat", (7,), (3, 7), np.float64),
    ("affine32", (12,), (3, 12), np.float32),
    ("quat_twist", (13,), (3, 13), np.float64),
]


@pytest.mark.parametrize(("name", "scalar", "batch", "dtype"), LAYOUT_SHAPES)
def test_every_layout_has_the_shape_and_dtype_it_advertises(
    twistable, name, scalar, batch, dtype
):
    """R4: the layout is stated, and what comes back is what was stated."""
    p = twistable.plan("map", "base")
    stamps = np.array([1_000_000_000, 1_500_000_000, 2_000_000_000], dtype=np.int64)
    one = p.at(1_500_000_000, layout=name)
    many = p.at(stamps, layout=name)
    assert one.shape == scalar and one.dtype == dtype
    assert many.shape == batch and many.dtype == dtype


@pytest.mark.parametrize(("name", "scalar", "batch", "dtype"), LAYOUT_SHAPES)
def test_a_scalar_layout_call_is_the_one_element_batch_bit_for_bit(
    twistable, name, scalar, batch, dtype
):
    """§11.1, extended to every layout."""
    p = twistable.plan("map", "base")
    t = 1_500_000_000
    one = p.at(t, layout=name)
    many = p.at(np.array([t], dtype=np.int64), layout=name)
    np.testing.assert_array_equal(one.reshape(-1), many[0].reshape(-1))


def test_the_twist_layout_is_the_quat_layout_plus_six(twistable):
    """``quat_twist`` is ``quat`` with the body twist appended, not a re-derived pose."""
    p = twistable.plan("map", "base")
    stamps = np.array([1_250_000_000, 1_500_000_000, 1_750_000_000], dtype=np.int64)
    pose = p.at(stamps, layout="quat")
    twist = p.at(stamps, layout="quat_twist")
    np.testing.assert_array_equal(twist[:, :7], pose)
    np.testing.assert_allclose(twist[:, 7:10], 0.0, atol=1e-12)
    np.testing.assert_allclose(twist[:, 10:13], [[1.0, 0.0, 0.0]] * 3, atol=1e-9)


def test_lerpslerp_refuses_a_twist_rather_than_finite_differencing_it():
    """`docs/PHASE5.md` §4.4 item 1: the typed error, not a plausible number."""
    t = tf_tree.build([("map", "base")], interp="lerpslerp")
    tf_tree.push(t, "base", "map", 1_000, [1.0, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0])
    tf_tree.push(t, "base", "map", 2_000, [1.0, 0.0, 0.0, 0.0, 3.0, 4.0, 5.0])
    p = t.plan("map", "base")
    stamps = np.array([1_500], dtype=np.int64)
    with pytest.raises(tf_tree.DerivativesUnavailableError):
        p.at(stamps, layout="quat_twist")
    # The pose layouts over the same edge are unaffected: it is the derivative
    # that does not exist, not the transform.
    assert p.at(stamps, layout="quat").shape == (1, 7)


def test_the_default_interp_is_the_engines_own(tree):
    """`docs/PROJECT.md` §5 D5: ScLerp is the default, and Python is not exempt."""
    assert tree.plan("map", "base").at(1_500, layout="quat_twist").shape == (13,)

    # A 90-degree yaw with an offset lever arm: LERP+SLERP and the SE(3) screw
    # geodesic put the midpoint in different places.
    q0 = [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]
    q1 = [0.7071067811865476, 0.0, 0.0, 0.7071067811865476, 2.0, 0.0, 0.0]
    mid = {}
    for name in ("default", "lerpslerp"):
        kw = {} if name == "default" else {"interp": "lerpslerp"}
        t = tf_tree.build([("map", "base")], **kw)
        tf_tree.push(t, "base", "map", 1_000, q0)
        tf_tree.push(t, "base", "map", 2_000, q1)
        mid[name] = t.plan("map", "base").at(1_500, layout="quat")
    assert not np.allclose(mid["default"], mid["lerpslerp"])


def test_a_float_stamp_is_still_refused_with_the_measurement_under_a_layout(tree):
    """§3 is NORMATIVE and does not have a per-layout exception."""
    p = tree.plan("map", "base")
    for layout in ("mat4", "quat", "affine32", "quat_twist"):
        with pytest.raises(TypeError, match="238 ns"):
            p.at(1.5, layout=layout)


# `(layout, elems, dtype)` for the three non-`mat4` layouts.
LAYOUT_OUT = [
    ("quat", 7, np.float64),
    ("affine32", 12, np.float32),
    ("quat_twist", 13, np.float64),
]


@pytest.mark.parametrize(("layout", "elems", "dtype"), LAYOUT_OUT)
def test_at_into_refuses_a_float_stamp_with_the_measurement_too(
    twistable, layout, elems, dtype
):
    """§3 is NORMATIVE and does not have a per-*method* exception either."""
    p = twistable.plan("map", "base")
    out = np.zeros(elems, dtype=dtype)
    with pytest.raises(TypeError, match="238 ns"):
        p.at_into(1.5, out, layout=layout)
    assert not out.any(), "the buffer was written before the stamp was validated"


@pytest.mark.parametrize(("layout", "elems", "dtype"), LAYOUT_OUT)
def test_the_layout_path_reports_a_bad_stamps_array_exactly_as_at_does(
    twistable, layout, elems, dtype
):
    """The price the two tests around this one charged, pinned so it stays paid on purpose."""
    p = twistable.plan("map", "base")
    out = np.zeros((1, elems), dtype=dtype)
    bad = np.array([1_500_000_000.0])  # float64, not int64
    with pytest.raises(TypeError) as into_exc:
        p.at_into(bad, out, layout=layout)
    with pytest.raises(TypeError) as at_exc:
        p.at(bad, layout=layout)
    assert str(into_exc.value) == str(at_exc.value)
    assert not out.any(), "the buffer was written before the stamp was validated"


@pytest.mark.parametrize(("layout", "elems", "dtype"), LAYOUT_OUT)
def test_a_numpy_int64_scalar_is_an_accepted_stamp(twistable, layout, elems, dtype):
    """§3 lists the accepted stamp types: ``int``, an ``np.int64`` scalar, and a
    C-contiguous ``np.int64`` array.
    """
    p = twistable.plan("map", "base")
    out = np.zeros(elems, dtype=dtype)
    t = np.array([1_250_000_000, 1_500_000_000], dtype=np.int64)[1]
    assert isinstance(t, np.int64) and not isinstance(t, int)
    p.at_into(t, out, layout=layout)
    np.testing.assert_array_equal(out, p.at(1_500_000_000, layout=layout))
    # `at` has always accepted it; the two must not disagree about what a stamp
    # is, which is the whole reason this fix was "match the pose path exactly".
    np.testing.assert_array_equal(p.at(t, layout=layout), out)


# The default layout, spelled both ways: `layout="mat4"` goes back to the same body as
# no keyword at all, so one row alone would leave the other spelling unpinned.
MAT4_SPELLINGS = [{}, {"layout": "mat4"}]
MAT4_IDS = ["default", "explicit"]


@pytest.mark.parametrize("kw", MAT4_SPELLINGS, ids=MAT4_IDS)
def test_the_mat4_path_refuses_a_float_stamp_with_the_measurement(twistable, kw):
    """§3 on the default overload, which is the one most callers write."""
    p = twistable.plan("map", "base")
    out = np.zeros((4, 4))
    with pytest.raises(TypeError, match="238 ns"):
        p.at_into(1.5, out, **kw)
    assert not out.any(), "the buffer was written before the stamp was validated"


@pytest.mark.parametrize("kw", MAT4_SPELLINGS, ids=MAT4_IDS)
def test_the_mat4_path_accepts_a_numpy_int64_scalar(twistable, kw):
    """§3's middle accepted type, on the default overload."""
    p = twistable.plan("map", "base")
    out = np.zeros((4, 4))
    t = np.array([1_250_000_000, 1_500_000_000], dtype=np.int64)[1]
    assert isinstance(t, np.int64) and not isinstance(t, int)
    p.at_into(t, out, **kw)
    np.testing.assert_array_equal(out, p.at(1_500_000_000))
    np.testing.assert_array_equal(p.at(t), out)


@pytest.mark.parametrize("kw", MAT4_SPELLINGS, ids=MAT4_IDS)
def test_the_mat4_path_reports_a_bad_stamps_array_exactly_as_at_does(twistable, kw):
    """The price, on the default overload too, and charged on purpose."""
    p = twistable.plan("map", "base")
    out = np.zeros((1, 4, 4))
    bad = np.array([1_500_000_000.0])
    with pytest.raises(TypeError) as into_exc:
        p.at_into(bad, out, **kw)
    with pytest.raises(TypeError) as at_exc:
        p.at(bad)
    assert str(into_exc.value) == str(at_exc.value)
    assert not out.any(), "the buffer was written before the stamp was validated"


# --------------------------------------------------------------------------- The twist
# layout's second refusal (`docs/API.md` R5)
# ---------------------------------------------------------------------------


def test_a_single_sample_has_a_pose_but_no_segment_to_differentiate():
    """``NoSegmentError``, and it is a *type* because the response differs."""
    t = tf_tree.build([("map", "base")], interp="sclerp")
    tf_tree.push(t, "base", "map", 1_000_000_000, [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
    p = t.plan("map", "base")
    # The pose is fine. That is the entire distinction from `NoDataError`.
    assert p.at(1_000_000_000, layout="quat").shape == (7,)
    with pytest.raises(tf_tree.NoSegmentError):
        p.at(1_000_000_000, layout="quat_twist")


def test_two_equal_stamps_bracket_a_zero_length_segment():
    """The second cause, and the one that is legal rather than merely early."""
    t = tf_tree.build([("map", "base")], interp="sclerp")
    tf_tree.push(t, "base", "map", 5, [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
    tf_tree.push(t, "base", "map", 5, [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0])
    p = t.plan("map", "base")
    assert p.at(5, layout="quat").shape == (7,)
    with pytest.raises(tf_tree.NoSegmentError):
        p.at(5, layout="quat_twist")


def test_no_segment_reaches_every_twist_entry_point():
    """The scalar, batch and ``_into`` paths are three call sites; one arm."""
    t = tf_tree.build([("map", "base")], interp="sclerp")
    tf_tree.push(t, "base", "map", 1_000_000_000, [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
    p = t.plan("map", "base")
    s = 1_000_000_000
    with pytest.raises(tf_tree.NoSegmentError):
        p.at(s, layout="quat_twist")
    with pytest.raises(tf_tree.NoSegmentError):
        p.at(np.array([s], dtype=np.int64), layout="quat_twist")
    with pytest.raises(tf_tree.NoSegmentError):
        p.at_into(s, np.zeros(13), layout="quat_twist")


def test_an_unknown_layout_is_refused_and_lists_the_ones_that_exist(tree):
    """R4 has no silently-wrong default, so a typo is an error rather than a guess."""
    p = tree.plan("map", "base")
    with pytest.raises(ValueError, match="quat_twist"):
        p.at(1_500, layout="matrix4")


def test_at_into_serves_every_layout_and_validates_before_writing(twistable):
    """R2's corollary: every batch entry point has an ``_into`` form."""
    p = twistable.plan("map", "base")
    stamps = np.array([1_250_000_000, 1_750_000_000], dtype=np.int64)
    for name, _scalar, _batch, dtype in LAYOUT_SHAPES:
        want = p.at(stamps, layout=name)
        out = np.zeros(want.shape, dtype=dtype)
        p.at_into(stamps, out, layout=name)
        np.testing.assert_array_equal(out, want)

        # Too small: refused, and the buffer is untouched.
        bad = np.zeros((len(stamps), 3), dtype=dtype)
        with pytest.raises(tf_tree.BufferError):
            p.at_into(stamps, bad, layout=name)
        assert not bad.any()

        # Right size, wrong shape.
        flat = np.zeros(want.size, dtype=dtype)
        with pytest.raises(tf_tree.BufferError):
            p.at_into(stamps, flat, layout=name)
        assert not flat.any()


def test_at_into_refuses_the_wrong_dtype_for_a_layout(twistable):
    """``affine32`` is the one ``float32`` layout, and a ``float64`` buffer for it is a
    silent halving of precision if it is accepted.
    """
    p = twistable.plan("map", "base")
    stamps = np.array([1_500_000_000], dtype=np.int64)
    with pytest.raises(tf_tree.BufferError, match="float32"):
        p.at_into(stamps, np.zeros((1, 12), dtype=np.float64), layout="affine32")
    with pytest.raises(tf_tree.BufferError, match="float64"):
        p.at_into(stamps, np.zeros((1, 13), dtype=np.float32), layout="quat_twist")


def test_a_scalar_layout_write_needs_a_one_dimensional_buffer(twistable):
    """The scalar overload's ``out`` is ``(elems,)``, matching what ``at`` returns for a
    scalar stamp — not ``(1, elems)``.
    """
    p = twistable.plan("map", "base")
    out = np.zeros(13, dtype=np.float64)
    p.at_into(1_500_000_000, out, layout="quat_twist")
    np.testing.assert_array_equal(out, p.at(1_500_000_000, layout="quat_twist"))
    with pytest.raises(tf_tree.BufferError):
        p.at_into(
            1_500_000_000, np.zeros((1, 13), dtype=np.float64), layout="quat_twist"
        )


def test_an_unknown_interp_is_refused(tree):
    """Mutant: default an unknown name to ``ScLerp`` => nothing raises."""
    with pytest.raises(ValueError, match="sclerp"):
        tf_tree.build([("map", "base")], interp="screw")


def test_open_validates_interp_even_with_nothing_to_create():
    """The same typo must be the same error in both creation calls."""
    with pytest.raises(ValueError, match="sclerp"):
        tf_tree.open(name="tf_tree_test_no_such_arena", interp="screw")


# ---------------------------------------------------------------------------
# `docs/PHASE3.md` §4.2: verify METH_FASTCALL rather than assuming it
# ---------------------------------------------------------------------------


def _ml_flags(cls: type, name: str) -> int:
    """The ``PyMethodDef::ml_flags`` CPython holds for ``cls.name``."""
    import ctypes

    voidp = ctypes.POINTER(ctypes.c_void_p)
    descr = getattr(cls, name)
    # `d_qualname` is filled on first access — touch it before reading slots.
    signature = (id(cls), id(descr.__name__), id(descr.__qualname__))
    base = id(descr)

    def slot(offset: int) -> int:
        return ctypes.cast(base + offset, voidp)[0] or 0

    # `d_method` is the fourth slot from `head`, so `head + 32` is the first
    # byte past it and may not exceed the object.
    for head in range(0, type(descr).__basicsize__ - 32 + 1, 8):
        if tuple(slot(head + 8 * i) for i in range(3)) != signature:
            continue
        # Three known pointers in a row cannot be a coincidence, so the walk is
        # committed here: if the name does not read back, the structure moved and the
        # answer is the AssertionError, not a further search.
        method_def = slot(head + 24)
        if method_def:
            ml_name = ctypes.cast(method_def, ctypes.POINTER(ctypes.c_char_p))[0]
            if ml_name == name.encode():
                return ctypes.cast(method_def + 16, ctypes.POINTER(ctypes.c_int))[0]
        break
    raise AssertionError(
        f"could not locate PyMethodDef for {cls.__name__}.{name}: CPython's "
        "descriptor layout has moved and this probe needs updating"
    )


METH_VARARGS = 0x0001
METH_KEYWORDS = 0x0002
METH_NOARGS = 0x0004
METH_FASTCALL = 0x0080


def test_the_hot_methods_are_emitted_as_meth_fastcall():
    """§4.2: "**Verify** that PyO3 actually emits ``METH_FASTCALL`` for these signatures
    rather than assuming it; if it does not, that is 29 ns and worth a hand-written
    shim."
    """
    from tf_tree import _core

    for cls, name in (
        (tf_tree.Plan, "at"),
        (tf_tree.Plan, "at_into"),
        (_core.Publisher, "push"),
    ):
        flags = _ml_flags(cls, name)
        assert flags & METH_FASTCALL, f"{name}: ml_flags={flags:#x}, no METH_FASTCALL"
        assert not flags & METH_VARARGS, f"{name}: ml_flags={flags:#x} is METH_VARARGS"

    latest = _ml_flags(tf_tree.Plan, "latest")
    assert latest & METH_NOARGS, f"latest: ml_flags={latest:#x}"


# --------------------------------------------------------------------------- Exact
# stamp converters (`docs/API.md` §5.1, §6 row 9)
# ---------------------------------------------------------------------------

# The twin of `crates/tf_tree_c/tests/abi.rs::PARTS_TABLE`, and it must stay identical
# to it. `(sec, nanosec, expected)`, where `None` means **refused**.
PARTS_TABLE = [
    (0, 0, 0),
    (1_700_000_000, 123_456_789, 1_700_000_000_123_456_789),
    (-1, 999_999_999, -1),
    (-1, 0, -1_000_000_000),
    # Exactly `i64::MIN`. `-9_223_372_037 * 1e9` alone is below it, so a staged
    # `checked_mul`/`checked_add` would refuse this *representable* stamp.
    (-9_223_372_037, 145_224_192, -(2**63)),
    (-9_223_372_037, 145_224_191, None),
    (9_223_372_036, 854_775_807, 2**63 - 1),
    (9_223_372_036, 854_775_808, None),
    (0, 1_000_000_000, None),
    (0, 2**32 - 1, None),
]


@pytest.mark.parametrize(("sec", "nanosec", "want"), PARTS_TABLE)
def test_from_parts_agrees_with_rust_including_the_refusals(sec, nanosec, want):
    """Mutant: normalise out-of-range nanoseconds (``divmod`` into ``sec``) instead of
    refusing => the ``(0, 1_000_000_000)`` row returns a number. Mutant: compute the sum
    in ``i64`` with ``wrapping_add`` => the two boundary refusals return wrapped stamps.
    """
    if want is None:
        with pytest.raises(ValueError):
            tf_tree.from_parts(sec, nanosec)
    else:
        assert tf_tree.from_parts(sec, nanosec) == want


def test_from_parts_refuses_a_negative_nanosecond():
    """A negative nanosecond field means a *relative* interval is being converted as an
    instant — POSIX permits one only there. It is not expressible in Rust's
    ``from_parts`` (whose field is ``u32``) and is refused here for the same reason
    ``Stamp::from_timespec`` refuses it.
    """
    with pytest.raises(ValueError, match=r"\[0, 1000000000\)"):
        tf_tree.from_parts(0, -1)


class _RosTime:
    """A duck for `builtin_interfaces/Time`."""

    def __init__(self, sec, nanosec):
        self.sec = sec
        self.nanosec = nanosec


@pytest.mark.parametrize(("sec", "nanosec", "want"), PARTS_TABLE)
def test_from_ros_is_from_parts_over_a_message(sec, nanosec, want):
    """Mutant: convert via ``sec + nanosec / 1e9`` seconds and multiply back (the
    ``to_sec()`` round trip §5.1 forbids) => row 2 comes back as 1700000000123456768 and
    the test fails.
    """
    msg = _RosTime(sec, nanosec)
    if want is None:
        with pytest.raises(ValueError):
            tf_tree.from_ros(msg)
    else:
        assert tf_tree.from_ros(msg) == want


def test_from_ros_says_what_it_wanted_when_handed_the_wrong_object():
    """Mutant: let the ``getattr`` error propagate unchanged => an ``AttributeError`` is
    raised instead of the ``TypeError`` this asserts, and the message never names
    ``.nanosec``.
    """
    with pytest.raises(TypeError, match="nanosec"):
        tf_tree.from_ros(object())

    class _RclpyTimeish:
        nanoseconds = 5

    with pytest.raises(TypeError, match="nanoseconds"):
        tf_tree.from_ros(_RclpyTimeish())


def test_from_sec_still_exists_and_still_names_its_exact_siblings():
    """`from_sec` is kept and kept lossy (§5.1); what it gains is somewhere to point. The
    docstring is the thing a user reads at the moment they are about to use it, so the
    pointer belongs there.
    """
    assert tf_tree.from_sec(1.5) == 1_500_000_000
    doc = tf_tree.from_sec.__doc__ or ""
    assert "from_parts" in doc and "from_ros" in doc
