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
    """A publisher outlives the `Tree` object it was claimed from.

    `Plan` gets this from a `Py<PyTree>` — a CPython refcount on the wrapper.
    `Publisher` gets it from somewhere else since `docs/decisions/0017` step 6:
    it holds an `OwnedWriter`, whose `Arc<Tree>` keeps the **arena** alive
    rather than the Python object. The observable behaviour is identical, which
    is why this test did not change when the mechanism did.

    **No mutant is claimed for this one, deliberately.** The mutation that
    breaks it — deleting `OwnedWriter`'s `Arc<Tree>` — produces a
    use-after-free that a release CPython extension does not reliably notice,
    so a "Mutant:" note here would be a claim nothing checked. The gates that
    do check it are `crates/tf_tree/tests/owned_writer.rs` under `just miri`
    (`0017` step 2) and `a_publisher_outlives_the_tree_handle_it_came_from` in
    `crates/tf_tree_c/tests/publish.rs` under `just c-abi-check`'s ASan row.
    """
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


# ---------------------------------------------------------------------------
# Introspection (`docs/PHASE5.md` §4.4 item 2, `docs/API.md` §3.2)
# ---------------------------------------------------------------------------


def test_frames_lists_every_declared_frame_in_declaration_order(tree):
    """`TreeBuilder` interns names in edge-declaration order, parent then child.

    So the order is not an accident of a hash table and is worth asserting: it
    is what makes `frames()[0]` the root of a tree a user just built, which is
    the first thing anyone prints.

    The exact list is also what pins frame id 0 — the root sentinel — out of the
    answer, so there is no separate sentinel test: any bound that reaches slot 0
    changes this list.

    Mutant: iterate `1..frame_count` instead of `1..=frame_count` in
    ``frames_impl``. Applied: the last frame disappears and this fails with
    ``['map', 'base'] != ['map', 'base', 'cam']`` — an off-by-one a set
    comparison would catch only incidentally, through the count.
    """
    assert tree.frames() == ["map", "base", "cam"]


def test_edges_are_parent_child_pairs_and_exclude_the_sentinel(tree):
    """`(parent, child)` — `build`'s order, not `publisher`'s — and no slot 0.

    **One exact-list assertion, deliberately, because it is what kills the
    mutants.** A first revision of this file spread the property over three
    tests (a sentinel test, a tuple-shape test, and this one); the review found
    that the two extra tests killed nothing the equality below does not, while
    their docstrings argued they were the only guard. They are folded in here
    instead, because the argument is worth keeping and a second assertion of it
    is not.

    Order. An edge list silently reversed still builds a perfectly valid tree,
    just upside down, so a test that only checked "two pairs of strings came
    back" would pass against that bug. The rebuild below is the other half: the
    rebuilt tree's `edges()` is derived the same way and would agree with itself
    either way — it is the *frames* that give it away, since `map` is the root
    of one tree and a leaf of the other.

    The rebuild is **the graph only**, and this fixture is where that is safe:
    every edge a `tf_tree.build` tree can hold is dynamic. `edges()` does not
    report an edge's kind and `tf_tree.build` cannot declare a static one, so
    the same round trip over a `.tft` or a peer-built arena silently converts
    every static edge into an empty dynamic one. That limit is documented on
    `Tree.edges`; it is not asserted here because no Python surface can build
    the tree that would show it.

    Sentinel. Frame id 0 is the root sentinel and edge id 0 is reserved
    (`edge_count` is stored as *declared + 1* for that reason, an off-by-one
    that has already cost `tf_tree_c::unstable` a test). **The loop bound is not
    what protects the edge list, which is why that mutant needs two edits**: a
    zeroed edge slot names frame 0 at both ends and `FrameId::new(0)` is `None`,
    so ``named_edge_in`` declines it whatever the bound is. It is the `None`
    *propagation* that is load-bearing, and the tempting refactor is precisely
    to replace it with `Tree::edge_name`'s `"<root>"` fallback so that no entry
    is ever dropped.

    Mutants, all three run against this one assertion pair:

    * swap to `(child, parent)` in ``named_edge_in`` — fails at index 0 with
      ``('base', 'map') != ('map', 'base')``;
    * `for raw in 0..count` in ``edges_impl`` **and** a `"<root>"` fallback in
      ``named_edge_in`` — fails with a leading ``('<root>', '<root>')``. Either
      edit alone changes nothing observable;
    * append a per-edge sample count to the tuple in ``edges_impl``, the exact
      shape `docs/PHASE5.md` §4.2's amendment refuses — fails on the tuple
      arity.
    """
    assert tree.edges() == [("map", "base"), ("base", "cam")]
    rebuilt = tf_tree.build(tree.edges())
    assert rebuilt.edges() == tree.edges()
    assert rebuilt.frames() == tree.frames()


def test_plan_edges_names_the_edges_the_plan_samples(tree):
    """One entry per `Step::Dyn`, in fold order.

    Every edge a `tf_tree.build` tree can hold is dynamic — the binding has no
    way to declare a static one — so here `len(plan.edges()) == plan.depth()`.
    That equality is *not* the promise (a folded static run makes it strictly
    less) and the docstring on `Plan.edges` says so; it is asserted because on
    this tree it is exactly what "the plan samples every step" means.

    Mutant: seed ``plan_edges_impl`` with `EdgeId(1)` before the loop — one edge
    counted twice, which the exact-list comparison catches and a set comparison
    would not. Applied: this test and the self-plan one below both fail.
    """
    p = tree.plan("map", "cam")
    assert p.depth() == 2
    assert p.edges() == [("map", "base"), ("base", "cam")]


def test_plan_edges_report_identity_not_direction(tree):
    """A plan and its reverse sample the same edges.

    `Step::Dyn` carries an `inverted` flag and this deliberately does not report
    it: the pair is the edge's *identity* — the same identity `Tree.edges()`
    hands out — and `map -> cam` and `cam -> map` traverse one topology, not
    two. Reporting the traversal direction instead would make
    ``set(plan.edges()) <= set(tree.edges())`` false for half of all plans,
    which is the property that makes this list joinable with the tree's at all.

    Mutant: emit `(child, parent)` when `inverted` in ``plan_edges_impl``.
    Applied: the two plans stop agreeing —
    ``[('base', 'map'), ('cam', 'base')]`` against
    ``[('base', 'cam'), ('map', 'base')]`` once sorted.
    """
    forward = tree.plan("map", "cam").edges()
    backward = tree.plan("cam", "map").edges()
    assert sorted(backward) == sorted(forward)
    assert set(forward) <= set(tree.edges())


def test_plan_edges_of_a_self_plan_is_empty(tree):
    """`lookup(x, x)` compiles to a plan with no steps, so it samples nothing.

    An empty list, not an error: the identity path is answerable at every stamp
    precisely because there is nothing on it to sample. This is the same edge
    case `Tree.span` returns `None` for, and the two agree.

    Mutant: seed ``plan_edges_impl`` with `EdgeId(1)` before the loop. Applied:
    ``[('map', 'base')] != []`` here, and the depth-2 test above fails too — a
    plan that reports an edge it does not sample is the failure this pair of
    tests brackets from both ends.
    """
    p = tree.plan("base", "base")
    assert p.depth() == 0
    assert p.edges() == []
    assert tree.span("base", "base") is None


# ---------------------------------------------------------------------------
# layout= (`docs/PHASE5.md` §4.4 item 1, `docs/API.md` §6 row 7)
# ---------------------------------------------------------------------------


@pytest.fixture
def twistable():
    """A tree whose edges can answer a twist.

    ``tf_tree.build``'s default is ``interp="lerpslerp"``, which is
    ``tf2``-compatible and has **no exact body twist** — so the default tree is
    the one that *refuses* ``layout="quat_twist"``. That refusal has its own
    test below; this fixture is the other half.
    """
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
    """R4: the layout is stated, and what comes back is what was stated.

    Mutant: return ``Layout::elems()`` for the wrong variant (swap ``Quat``'s 7
    and ``QuatTwist``'s 13) => two rows fail on shape.
    """
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
    """§11.1, extended to every layout.

    The scalar ``mat4`` path is a genuinely separate implementation, because it
    is the measured hot path and predates this; the other three are deliberately
    a one-element batch so there is nothing that *can* diverge. Both claims are
    checked here, and the ``mat4`` row is the one that could actually fail:
    it is the only layout with two implementations to keep in step.

    Mutant: make the scalar ``mat4`` path write ``write_quat`` instead of
    ``write_mat4`` => the ``mat4`` row fails.
    """
    p = twistable.plan("map", "base")
    t = 1_500_000_000
    one = p.at(t, layout=name)
    many = p.at(np.array([t], dtype=np.int64), layout=name)
    np.testing.assert_array_equal(one.reshape(-1), many[0].reshape(-1))


def test_the_twist_layout_is_the_quat_layout_plus_six(twistable):
    """``quat_twist`` is ``quat`` with the body twist appended, not a re-derived
    pose.

    The first seven elements must be **bit-identical** to what ``quat`` writes:
    the two go through different folds (``fold_batch`` against
    ``fold_batch_with_twist``), and a pose that differed in the last bit between
    them would mean two implementations of the same interpolation had drifted.

    The twist itself is checked against the motion the fixture publishes: one
    metre of +x over one second, no rotation, so the body linear velocity is
    ``[1, 0, 0]`` m/s and the angular part is zero.

    Mutant: write the twist as ``[v, w]`` instead of ``[w, v]`` => the angular
    assertion sees 1.0 and fails.
    """
    p = twistable.plan("map", "base")
    stamps = np.array([1_250_000_000, 1_500_000_000, 1_750_000_000], dtype=np.int64)
    pose = p.at(stamps, layout="quat")
    twist = p.at(stamps, layout="quat_twist")
    np.testing.assert_array_equal(twist[:, :7], pose)
    np.testing.assert_allclose(twist[:, 7:10], 0.0, atol=1e-12)
    np.testing.assert_allclose(twist[:, 10:13], [[1.0, 0.0, 0.0]] * 3, atol=1e-9)


def test_lerpslerp_refuses_a_twist_rather_than_finite_differencing_it(tree):
    """`docs/PHASE5.md` §4.4 item 1: the typed error, not a plausible number.

    ``tree`` is the default fixture, so its edges are ``LerpSlerp`` — ``tf2``'s
    interpolator, which has no exact body twist. A layout that quietly changed
    meaning per interpolator would be the quaternion-order trap moved into the
    time axis, so this is a refusal and it is **typed**: a caller branches on it
    to decide whether to re-declare the edge or ask for a pose.

    Mutant: map ``LookupError::DerivativesUnavailable`` to the generic
    ``TfTreeError`` (delete the arm added to ``lookup_err``) => ``raises`` no
    longer matches the subclass and the test fails.
    """
    p = tree.plan("map", "base")
    stamps = np.array([1_500], dtype=np.int64)
    with pytest.raises(tf_tree.DerivativesUnavailableError):
        p.at(stamps, layout="quat_twist")
    # The pose layouts over the same edge are unaffected: it is the derivative
    # that does not exist, not the transform.
    assert p.at(stamps, layout="quat").shape == (1, 7)


def test_an_unknown_layout_is_refused_and_lists_the_ones_that_exist(tree):
    """R4 has no silently-wrong default, so a typo is an error rather than a
    guess.

    Mutant: add ``other => Ok(Layout::Mat4)`` to ``layout_from_str`` => no
    exception is raised and the test fails.
    """
    p = tree.plan("map", "base")
    with pytest.raises(ValueError, match="quat_twist"):
        p.at(1_500, layout="matrix4")


def test_at_into_serves_every_layout_and_validates_before_writing(twistable):
    """R2's corollary: every batch entry point has an ``_into`` form.

    Also the §5.3 rule, per layout: a buffer of the wrong shape is refused with
    nothing written, so a rejected call leaves the caller's array as it was.

    **The flat buffer is the case that carries the load.** A too-small one is
    refused by the engine's own ``BufferTooSmall`` whatever this binding does;
    a *flat* buffer of exactly the right element count is not, because the
    engine sees a slice and cannot see a shape. Only the binding can refuse it,
    and it must — a caller who passes ``(N*13,)`` believing it is ``(N, 13)``
    gets the right bytes today and an off-by-a-transpose the moment they index
    it.

    Mutant: drop the ``check_out`` shape comparison => the flat buffer is
    accepted and the test fails. (Dropping it does *not* break the too-small
    case, which is why that assertion alone would not have been a test.)
    """
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
    """``affine32`` is the one ``float32`` layout, and a ``float64`` buffer for
    it is a silent halving of precision if it is accepted.

    Mutant: take the ``f64`` branch for ``affine32`` too (``is_f32()`` =>
    ``false``) => the ``float64`` buffer is accepted by the binding and the
    engine refuses it as ``WrongElementType``, which reaches Python as the base
    ``TfTreeError`` rather than ``BufferError``, so the first block fails.
    """
    p = twistable.plan("map", "base")
    stamps = np.array([1_500_000_000], dtype=np.int64)
    with pytest.raises(tf_tree.BufferError, match="float32"):
        p.at_into(stamps, np.zeros((1, 12), dtype=np.float64), layout="affine32")
    with pytest.raises(tf_tree.BufferError, match="float64"):
        p.at_into(stamps, np.zeros((1, 13), dtype=np.float32), layout="quat_twist")


def test_a_scalar_layout_write_needs_a_one_dimensional_buffer(twistable):
    """The scalar overload's ``out`` is ``(elems,)``, matching what ``at``
    returns for a scalar stamp — not ``(1, elems)``.

    Mutant: build ``want`` as ``[1, e]`` for the scalar case => the first call
    raises and the test fails.
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


# ---------------------------------------------------------------------------
# Exact stamp converters (`docs/API.md` §5.1, §6 row 9)
# ---------------------------------------------------------------------------

# The twin of `crates/tf_tree_c/tests/abi.rs::PARTS_TABLE`, and it must stay
# identical to it. `(sec, nanosec, expected)`, where `None` means **refused**.
# A converter that agrees with Rust on the successes and disagrees at the edges
# is the bug this row exists to prevent, and it is invisible to any test that
# only checks the middle.
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
    """Mutant: normalise out-of-range nanoseconds (``divmod`` into ``sec``)
    instead of refusing => the ``(0, 1_000_000_000)`` row returns a number.
    Mutant: compute the sum in ``i64`` with ``wrapping_add`` => the two
    boundary refusals return wrapped stamps.
    """
    if want is None:
        with pytest.raises(ValueError):
            tf_tree.from_parts(sec, nanosec)
    else:
        assert tf_tree.from_parts(sec, nanosec) == want


def test_from_parts_refuses_a_negative_nanosecond():
    """A negative nanosecond field means a *relative* interval is being
    converted as an instant — POSIX permits one only there. It is not
    expressible in Rust's ``from_parts`` (whose field is ``u32``) and is
    refused here for the same reason ``Stamp::from_timespec`` refuses it.

    Mutant: use ``nanosec % 1_000_000_000`` for the range test => ``-1``
    becomes a legal input.
    """
    with pytest.raises(ValueError, match=r"\[0, 1000000000\)"):
        tf_tree.from_parts(0, -1)


class _RosTime:
    """A duck for `builtin_interfaces/Time`.

    `rclpy` is not a dependency of this wheel and must not become one; the
    message is two integer fields and the converter reads exactly those, so a
    stand-in with the same two attributes exercises the real path.
    """

    def __init__(self, sec, nanosec):
        self.sec = sec
        self.nanosec = nanosec


@pytest.mark.parametrize(("sec", "nanosec", "want"), PARTS_TABLE)
def test_from_ros_is_from_parts_over_a_message(sec, nanosec, want):
    """Mutant: convert via ``sec + nanosec / 1e9`` seconds and multiply back
    (the ``to_sec()`` round trip §5.1 forbids) => row 2 comes back as
    1700000000123456768 and the test fails.
    """
    msg = _RosTime(sec, nanosec)
    if want is None:
        with pytest.raises(ValueError):
            tf_tree.from_ros(msg)
    else:
        assert tf_tree.from_ros(msg) == want


def test_from_ros_says_what_it_wanted_when_handed_the_wrong_object():
    """Mutant: let the ``getattr`` error propagate unchanged => an
    ``AttributeError`` is raised instead of the ``TypeError`` this asserts, and
    the message never names ``.nanosec``.
    """
    with pytest.raises(TypeError, match="nanosec"):
        tf_tree.from_ros(object())

    class _RclpyTimeish:
        nanoseconds = 5

    with pytest.raises(TypeError, match="nanoseconds"):
        tf_tree.from_ros(_RclpyTimeish())


def test_from_sec_still_exists_and_still_names_its_exact_siblings():
    """`from_sec` is kept and kept lossy (§5.1); what it gains is somewhere to
    point. The docstring is the thing a user reads at the moment they are about
    to use it, so the pointer belongs there.

    Mutant: delete ``from_parts`` from the docstring => the assertion fails.
    """
    assert tf_tree.from_sec(1.5) == 1_500_000_000
    doc = tf_tree.from_sec.__doc__ or ""
    assert "from_parts" in doc and "from_ros" in doc
