"""Extrapolation you cannot fail to notice (`docs/decisions/0039`).

`ExtrapPolicy` has three variants and all three work in the engine's sampler;
until `0039` every fold site passed the `Error` literal and no consumer could
select any of them. `Plan.at_extrapolating` is the Python half of reaching
them, and its shape is the safety property: it returns `(poses, by_ns)` and
there is no spelling that returns the pose alone, so a held pose cannot pass
for a fresh one by omission.

**The fixture is the frozen tag-1 arena**, `testdata/frozen/sensor_domain.tft`
— two dynamic edges at 16 samples 10 ms apart, so `0..150 ms` is answerable and
anything past 150 ms is extrapolation. It is used rather than a
`tf_tree.build` chain for two reasons: the window is a committed fact rather
than something each test re-establishes by pushing, and it carries `domain=1`,
so the tag on the plan handle has to reach `at_extrapolating_tagged` for any of
this to answer at all. A tag-0 arena cannot tell "carried the handle's tag"
from "hard-coded `SystemDomain`" — `docs/decisions/0038`'s whole finding.
"""

import pathlib

import numpy as np
import pytest
import tf_tree

#: The committed tag-1 arena. Resolved from this file, as `test_domains.py`
#: resolves it: the suite is only runnable from the checkout, so the path is
#: always there and a skip would be a silent vacuous pass.
FIXTURE = (
    pathlib.Path(__file__).parents[2] / "testdata" / "frozen" / "sensor_domain.tft"
)

#: The newest stamp every dynamic edge in the fixture has data for: 16 samples
#: 10 ms apart starting at zero.
NEWEST_NS = 150_000_000
#: 50 ms past that — the position a control loop running faster than its state
#: estimate is in on every tick.
PAST_NS = 200_000_000


@pytest.fixture
def plan():
    """`lidar <- map`: two dynamic edges and one static, in `SENSOR_DOMAIN`.

    A composed route on purpose. A single-edge plan would make `by_ns` the
    distance to *the* edge, where the value `0039` defines is the minimum over
    the route's dynamic edges — and a static step, which has no samples, must
    contribute nothing to it.
    """
    assert FIXTURE.is_file(), (
        f"{FIXTURE} is missing; regenerate it with `cargo run -p tf_tree "
        f"--features shm --example gen_domain_fixture`"
    )
    tree = tf_tree.open_file(str(FIXTURE))
    return tree.plan("lidar", "map", domain=tf_tree.SENSOR_DOMAIN)


def test_the_three_policies_differ_at_the_same_stamp(plan):
    """`0039` step 3's verification, through Python.

    `"error"` refuses, `"hold"` returns the newest pose, and
    `"constant_twist"` returns a **different** pose at the same stamp. The
    third assertion is the one that earns the test: a binding that ignored its
    `policy` argument and always held would satisfy the first two.

    Mutant: hard-code `ExtrapPolicy::Hold` in `PyPlan::at_extrapolating` =>
    `the two policies must differ` fails, and so does the `"error"` arm.
    """
    with pytest.raises(tf_tree.ExtrapolationError):
        plan.at_extrapolating(PAST_NS, "error")

    held, by_ns = plan.at_extrapolating(PAST_NS, "hold")
    assert held.shape == (4, 4)
    assert by_ns == PAST_NS - NEWEST_NS

    # "Held" means "the pose the plan gives at the newest common stamp", so it
    # is asserted against the engine's own answer there rather than against a
    # hand-written matrix — which would be a second implementation of it.
    assert np.array_equal(held, plan.at(NEWEST_NS))

    twisted, twist_by_ns = plan.at_extrapolating(PAST_NS, "constant_twist")
    assert twist_by_ns == by_ns, "the distance is the arena's, not the policy's"
    assert not np.array_equal(twisted, held), (
        "the two policies must differ, or the argument is being ignored"
    )
    # Still a rigid transform and not a corrupted one.
    assert np.allclose(twisted[3], [0.0, 0.0, 0.0, 1.0])
    assert np.allclose(twisted[:3, :3] @ twisted[:3, :3].T, np.eye(3))


def test_at_still_refuses_and_is_not_a_mode(plan):
    """`at` is untouched. Extrapolation is a second entry point, not a flag.

    `0039`: *"`Plan::at` is untouched, still passes `Error`, and remains what
    the README's hot loop shows."* A caller that never asks for extrapolation
    must not be able to receive it, so the refusal has to survive this whole
    change — and `at` must keep returning a bare array, not a tuple.
    """
    with pytest.raises(tf_tree.ExtrapolationError):
        plan.at(PAST_NS)
    assert plan.at(NEWEST_NS).shape == (4, 4)


def test_an_in_window_stamp_reports_zero_under_every_policy(plan):
    """`by_ns == 0` is what says the answer was interpolated, not invented.

    Inside every edge's window there is nothing to extrapolate, so the three
    policies must agree and the distance must be zero. Without this half a
    caller could not tell an extrapolating call that did not need to
    extrapolate from one that did — which is the whole use of the number.
    """
    plain = plan.at(75_000_000)
    for policy in ("error", "hold", "constant_twist"):
        poses, by_ns = plan.at_extrapolating(75_000_000, policy)
        assert by_ns == 0, f"{policy} extrapolated inside the window"
        assert np.array_equal(poses, plain), f"{policy} changed an interpolated answer"


def test_a_batch_gets_one_distance_per_stamp(plan):
    """`by_ns` is an `(N,)` array, and the reason is in the values.

    The distance is `max(0, stamp - newest_common)`, so it is a function of the
    stamp. This batch straddles the newest sample: the first three elements are
    interpolated and the last two are not, in one call. A scalar return would
    have to be a `max` — marking the fresh elements stale — or a `min`, marking
    the stale ones fresh, and the second is precisely the failure this surface
    exists to prevent.

    Mutant: return `by_ns.max()` as a scalar => the dtype and per-element
    assertions below both fail.
    """
    stamps = np.array([0, 75_000_000, NEWEST_NS, 175_000_000, PAST_NS], dtype=np.int64)
    poses, by_ns = plan.at_extrapolating(stamps, "hold")

    assert poses.shape == (5, 4, 4)
    assert by_ns.shape == (5,)
    assert by_ns.dtype == np.int64
    assert list(by_ns) == [0, 0, 0, 25_000_000, 50_000_000]

    # And the batch agrees with the scalar form element by element, which is
    # what makes one of them checkable against the other rather than two
    # independent implementations.
    for i, t in enumerate(stamps):
        pose_i, by_i = plan.at_extrapolating(int(t), "hold")
        assert np.array_equal(poses[i], pose_i)
        assert by_ns[i] == by_i

    # The held tail really is held: every element past the newest sample is the
    # same pose, and it is the pose at the newest sample.
    assert np.array_equal(poses[3], poses[4])
    assert np.array_equal(poses[3], plan.at(NEWEST_NS))


def test_a_batch_under_constant_twist_moves_where_hold_stands_still(plan):
    """The batch carries the policy too, not only the scalar path.

    Two entry points share one `policy` argument and only one of them is the
    one a NumPy user calls. Reverting the batch half to `Hold` would leave
    `test_the_three_policies_differ_at_the_same_stamp` green.
    """
    stamps = np.array([175_000_000, PAST_NS], dtype=np.int64)
    held, _ = plan.at_extrapolating(stamps, "hold")
    twisted, _ = plan.at_extrapolating(stamps, "constant_twist")

    assert np.array_equal(held[0], held[1]), "holding does not move"
    assert not np.array_equal(twisted[0], twisted[1]), "a twist does"
    assert not np.allclose(twisted, held)


def test_the_error_policy_raises_from_a_batch_too(plan):
    """A refusal part-way through a batch is an exception, not a short array.

    Both output arrays are allocated inside the binding and dropped on failure,
    so there is no half-filled buffer for a caller to mistake for data — the
    partial-write question `at_into` has to answer does not arise here.
    """
    stamps = np.array([0, 75_000_000, PAST_NS], dtype=np.int64)
    with pytest.raises(tf_tree.ExtrapolationError):
        plan.at_extrapolating(stamps, "error")


def test_the_policy_is_required_and_its_vocabulary_is_closed(plan):
    """Opt-in per query, and never guessed at.

    No default, because a default would make extrapolation something a caller
    can get without asking for it. And an unrecognised spelling is refused
    rather than resolved to the nearest thing: the three policies differ in
    what the answer *is*, not in how it is written, so serving a different one
    is a wrong pose rather than a wrong format.
    """
    with pytest.raises(TypeError):
        plan.at_extrapolating(PAST_NS)
    with pytest.raises(ValueError) as e:
        plan.at_extrapolating(PAST_NS, "constant-twist")
    assert "constant_twist" in str(e.value), "the message names the spellings"


def test_a_float_stamp_still_meets_the_measurement(plan):
    """`at`'s stamp discipline, unchanged on the new entry point.

    `docs/PHASE3.md` §3 is NORMATIVE that a `float` stamp raises a `TypeError`
    carrying the 238 ns ULP, and that an `np.int64` scalar is accepted. Both
    depend on the array cast being *fallen through* rather than `else`-d, which
    is easy to get wrong in a new method and invisible until somebody passes a
    float.
    """
    with pytest.raises(TypeError) as e:
        plan.at_extrapolating(0.15, "hold")
    assert "238" in str(e.value)

    poses, by_ns = plan.at_extrapolating(np.int64(PAST_NS), "hold")
    assert poses.shape == (4, 4)
    assert by_ns == PAST_NS - NEWEST_NS


def test_the_pose_never_arrives_without_the_distance(plan):
    """The property `0039` is for, asserted as a property of the surface.

    In Rust it is a type with no pose-only accessor. Here it is a 2-tuple: the
    only way to reach the pose is to receive the distance in the same value,
    and dropping it takes a deliberate `[0]`. This test is what notices if
    somebody later "simplifies" the return to a bare array.
    """
    result = plan.at_extrapolating(PAST_NS, "hold")
    assert isinstance(result, tuple) and len(result) == 2
    assert isinstance(result[1], int), "the scalar distance is a plain int"
    assert result[1] > 0


def test_at_extrapolating_into_writes_the_callers_buffers(plan):
    """R2's `_into` form, which `0039`'s binding step left out.

    `docs/API.md` R2 is NORMATIVE that every batch entry point has one, and its
    justification names this caller: the allocation "is noise at n = 65536 and
    half the call at n = 64 — and n = 64 is the control loop". Extrapolation is
    the method a controller reaches for, so it was the one path the rule was
    written about and the one that did not obey it.
    """
    stamps = np.array([50_000_000, 200_000_000, 250_000_000], dtype=np.int64)
    want_poses, want_by = plan.at_extrapolating(stamps, "constant_twist")

    poses = np.zeros((3, 4, 4))
    by_ns = np.zeros(3, dtype=np.int64)
    plan.at_extrapolating_into(stamps, "constant_twist", poses, by_ns)

    assert np.array_equal(poses, want_poses)
    assert np.array_equal(by_ns, want_by)
    # The distance still comes back per element, not collapsed.
    assert by_ns[0] == 0 and by_ns[-1] > 0


def test_at_extrapolating_takes_a_layout_like_at_does(plan):
    """The asymmetry against C, closed: a C caller could extrapolate into a
    non-mat4 layout and a Python caller could not."""
    stamps = np.array([200_000_000], dtype=np.int64)
    quat, by_ns = plan.at_extrapolating(stamps, "hold", layout="quat")
    assert quat.shape == (1, 7)
    assert by_ns.shape == (1,)

    into = np.zeros((1, 7))
    dist = np.zeros(1, dtype=np.int64)
    plan.at_extrapolating_into(stamps, "hold", into, dist, layout="quat")
    assert np.array_equal(into, quat)

    # A scalar keeps `at`'s shapes too.
    one, d = plan.at_extrapolating(200_000_000, "hold")
    assert one.shape == (4, 4) and isinstance(d, int)


def test_a_twist_layout_cannot_carry_an_extrapolated_pose(plan):
    """The same refusal the C ABI makes, for the same reason: there is no
    extrapolating `at_with_derivatives`, so a twist would be computed under
    `error` beside a pose computed under the caller's policy — two policies in
    one 13-float row."""
    stamps = np.array([200_000_000], dtype=np.int64)
    for layout in ("quat_twist", "affine32"):
        with pytest.raises(ValueError) as e:
            plan.at_extrapolating(stamps, "hold", layout=layout)
        assert "extrapolat" in str(e.value).lower()


def test_by_ns_may_not_alias_stamps(plan):
    """The one `_into` form whose input and an output share a dtype.

    `stamps` and `by_ns` are both int64, so `f(stamps=a, .., by_ns=a)` satisfies
    every shape and dtype check — and would give the fold `&[i64]` and
    `&mut [i64]` over one allocation, which is undefined behaviour reached from
    safe Python. Every other `_into` on this binding is safe from it by dtype
    alone, which is why no shared helper looks for it.
    """
    stamps = np.array([50_000_000, 200_000_000], dtype=np.int64)
    poses = np.zeros((2, 4, 4))

    with pytest.raises(tf_tree.BufferError) as e:
        plan.at_extrapolating_into(stamps, "hold", poses, stamps)
    assert "alias" in str(e.value)

    # A view of the same memory is refused too — the check is on byte ranges,
    # not on array identity.
    with pytest.raises(tf_tree.BufferError):
        plan.at_extrapolating_into(stamps, "hold", poses, stamps[:])

    # And a separate buffer of the same shape is fine.
    by_ns = np.zeros(2, dtype=np.int64)
    plan.at_extrapolating_into(stamps, "hold", poses, by_ns)
    assert by_ns[-1] > 0
