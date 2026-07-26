"""Free-threading (`docs/PHASE3.md` §1.2, §7).

What these tests can and cannot prove is worth stating, because §1.2's premise
no longer holds on the pinned toolchain.

``test_importing_tf_tree_does_not_re_enable_the_gil`` checks the property users
actually care about: after ``import tf_tree`` the process is still running
without a GIL. **It cannot check the ``gil_used = false`` declaration**, because
PyO3 0.29 defaults that flag to ``false`` when the attribute is absent
(``module.rs:394``) — removing the attribute leaves this test passing. It is
kept for what it does prove: that no *other* import-time effect re-enables the
GIL, which is a real regression and one nothing else would catch.

The test that carries the weight is the second one. Under free-threading the
claim is not "we declared ourselves safe" but "eight threads evaluating one
plan agree with the single-threaded answer", and that is checked against
values, not against the absence of a crash.
"""

import sys
import threading

import numpy as np
import pytest
import tf_tree

freethreaded = pytest.mark.skipif(
    not hasattr(sys, "_is_gil_enabled") or sys._is_gil_enabled(),
    reason="requires a free-threaded interpreter (python3.14t)",
)


@freethreaded
def test_importing_tf_tree_does_not_re_enable_the_gil():
    assert not sys._is_gil_enabled(), (
        "importing tf_tree re-enabled the GIL for this process: the module is "
        "missing gil_used = false, or a pyclass is not Send + Sync"
    )


@freethreaded
def test_threads_share_one_plan_without_corruption():
    """A ``Plan`` is ``Copy`` and frozen, so concurrent evaluation is the point.

    Each thread checks its own results, so a torn read shows up as a wrong
    value rather than only as a crash — the failure mode that would otherwise
    be invisible until a robot did something inexplicable.
    """
    tree = tf_tree.build([("map", "base")])
    tf_tree.push(tree, "base", "map", 1_000, [1.0, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0])
    tf_tree.push(tree, "base", "map", 2_000, [1.0, 0.0, 0.0, 0.0, 3.0, 4.0, 5.0])
    plan = tree.plan("map", "base")

    stamps = np.array([1_000, 1_500, 2_000], dtype=np.int64)
    expected = plan.at(stamps)
    errors: list[str] = []

    def worker():
        try:
            for _ in range(2_000):
                got = plan.at(stamps)
                if not np.array_equal(got, expected):
                    errors.append(f"diverged: {got[:, :3, 3]}")
                    return
        except Exception as e:  # noqa: BLE001 - reported, not swallowed
            errors.append(repr(e))

    threads = [threading.Thread(target=worker) for _ in range(8)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    assert not errors, errors[:3]
