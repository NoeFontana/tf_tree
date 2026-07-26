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
