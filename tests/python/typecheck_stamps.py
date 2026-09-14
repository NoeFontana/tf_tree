# pyright: strict, reportUnnecessaryTypeIgnoreComment=true
"""The stub's stamp types, checked at a call site (`docs/PHASE3.md` §3, §9).

**Read by `pyright` in `just py-lint`, never run and never collected by
pytest** (its name does not start with `test_`). `pyright python` checks the
stub *itself*, and `tests/python/test_stubs.py` checks only that names exist,
so nothing looked at what a caller's type checker says about a call — and the
stub typed every scalar stamp as `int` while the runtime accepts `np.int64`
(§3's list). `p.at(np.int64(t))` was a strict-mode error on correct code; so is
any stamp from `stamps.max()`. (`stamps[i]` is not: numpy types an index into
`NDArray[np.int64]` as `Any`, so that spelling was never the evidence.)

Two kinds of line:

* **accepted**, which must type-check clean;
* **refused**, carrying `# pyright: ignore[<rule>]`. With
  `reportUnnecessaryTypeIgnoreComment` on, that comment is itself an error the
  moment the call stops being an error — so these lines pin that a float stamp
  is still refused *by the stub*, and cannot quietly start passing.

Mutants, each applied to `python/tf_tree/_core.pyi` and checked with
`.venv/bin/pyright tests/python/typecheck_stamps.py`:

* The stub as it was before `np.int64` reached it => 25 errors, on the
  accepted lines.
* `Tree.lookup`'s `stamp_ns` back to `int` => one error, `Argument of type
  "int64" cannot be assigned to parameter "stamp_ns" of type "int" in function
  "lookup"`.
* `Publisher.push`'s `stamp_ns` widened to `int | np.int64 | float` => one
  error, `Unnecessary "# pyright: ignore" rule: "reportArgumentType"`, on the
  `pub.push(1.5, ...)` line.
* **An unexpected pass, recorded:** `Plan.at`'s *first* overload alone back to
  `stamps: int` leaves this file clean, because `p.at(t)` then resolves through
  the `layout: F64Layout | None = ...` overload, which returns the same type.
  So the per-overload spelling is not pinned here, only what a caller gets.
"""

import numpy as np
import tf_tree
from numpy.typing import NDArray
from typing_extensions import assert_type


def accepted(tree: tf_tree.Tree, stamps: NDArray[np.int64]) -> None:
    p = tree.plan("map", "base")
    t = np.int64(1_500)
    top = stamps.max()

    assert_type(p.at(t), NDArray[np.float64])
    assert_type(p.at(top), NDArray[np.float64])
    assert_type(p.at(t, layout="affine32"), NDArray[np.float32])
    assert_type(p.at(t, layout="quat"), NDArray[np.float64])
    p.at_into(t, np.zeros((4, 4)))
    p.at_into(t, np.zeros(7), layout="quat")
    assert_type(p.at_extrapolating(t, "hold"), tuple[NDArray[np.float64], int])
    p.at_extrapolating_into(t, "hold", np.zeros((4, 4)), np.zeros(()))
    p.adaptive(t, top)
    assert_type(tree.lookup("map", "base", t), NDArray[np.float64])
    with tree.publisher("base", "map") as pub:
        pub.push(t, [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
    tf_tree.push(tree, "base", "map", t, [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])

    # The array overload still wins for an array: `np.int64` in a scalar
    # overload must not swallow `NDArray[np.int64]`, which `SupportsIndex`
    # would have done.
    assert_type(p.at(stamps), NDArray[np.float64])
    assert_type(
        p.at_extrapolating(stamps, "hold"),
        tuple[NDArray[np.float64], NDArray[np.int64]],
    )


def refused(tree: tf_tree.Tree) -> None:
    p = tree.plan("map", "base")
    p.at(1.5)  # pyright: ignore[reportCallIssue, reportArgumentType]
    p.at_into(1.5, np.zeros((4, 4)))  # pyright: ignore[reportCallIssue, reportArgumentType]
    tree.lookup("map", "base", 1.5)  # pyright: ignore[reportArgumentType]
    with tree.publisher("base", "map") as pub:
        pub.push(1.5, [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])  # pyright: ignore[reportArgumentType]
