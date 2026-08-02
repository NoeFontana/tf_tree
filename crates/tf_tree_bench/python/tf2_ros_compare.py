"""tf_tree's Python API against `tf2_ros`'s, on the same tree and stamps.

`docs/PHASE3.md` §12.1 asks for this row, and §13 says it is "the most dramatic
number this project will produce". It is also the one most easily made
dishonest, so:

* **Same topology, same samples, same query stamps.** One dynamic edge, 2000
  samples at 1 ms, queried inside the retained window so neither engine is
  extrapolating.
* **tf2 is given every advantage available in-process.** ``tf2_ros.Buffer``
  wraps the same ``BufferCore`` a real node uses, fed directly — no DDS, no
  serialisation, no ``TransformListener``. A deployed consumer pays more than
  this; nothing here pays less.
* **The batch row is reported per sample**, and tf2 has no batch API, so its
  per-sample figure is its scalar figure. That is the honest comparison: the
  absence of a vectorised path *is* the cost, and hiding it by only comparing
  scalars would understate what a user actually experiences.
"""

from __future__ import annotations

import time

import numpy as np
import tf_tree
from geometry_msgs.msg import TransformStamped
from tf2_ros import Buffer

N_SAMPLES = 2000
DT_NS = 1_000_000
LO, HI = 1_000_000_000, 1_998_000_000


def bench(fn, iters: int, reps: int = 9) -> float:
    """Minimum of `reps` runs, in ns per iteration.

    The minimum rather than the mean: this is a latency measurement on a quiet
    machine, so the fastest run is the one least contaminated by scheduling.
    """
    for _ in range(3):
        fn()
    times = []
    for _ in range(reps):
        a = time.perf_counter_ns()
        fn()
        times.append(time.perf_counter_ns() - a)
    return min(times) / iters


def build_tf_tree():
    # `interp="lerpslerp"` explicitly, because this is a comparison *against*
    # tf2 and LERP+SLERP is tf2's own interpolator — the two must be doing the
    # same arithmetic for the ratio to mean anything. It used to be the Python
    # binding's default and is no longer (`API.md` §3), so relying on the
    # default here would have silently switched this row to ScLerp.
    t = tf_tree.build([("map", "base")], interp="lerpslerp")
    for k in range(N_SAMPLES):
        tf_tree.push(
            t, "base", "map", k * DT_NS, [1.0, 0.0, 0.0, 0.0, float(k), 0.0, 0.0]
        )
    return t, t.plan("map", "base")


def build_tf2() -> Buffer:
    buf = Buffer(cache_time=None)
    for k in range(N_SAMPLES):
        ts = TransformStamped()
        ts.header.frame_id = "map"
        ts.child_frame_id = "base"
        ns = k * DT_NS
        ts.header.stamp.sec = ns // 1_000_000_000
        ts.header.stamp.nanosec = ns % 1_000_000_000
        ts.transform.translation.x = float(k)
        ts.transform.rotation.w = 1.0
        buf.set_transform(ts, "bench")
    return buf


def main() -> None:
    _tree, plan = build_tf_tree()
    buf = build_tf2()

    from builtin_interfaces.msg import Time as MsgTime

    mid = (LO + HI) // 2

    def t2(stamp_ns: int) -> None:
        buf.lookup_transform(
            "map",
            "base",
            MsgTime(sec=stamp_ns // 1_000_000_000, nanosec=stamp_ns % 1_000_000_000),
        )

    print("=== scalar lookup ===")
    ours = bench(lambda: [plan.at(mid) for _ in range(5000)], 5000)
    theirs = bench(lambda: [t2(mid) for _ in range(5000)], 5000)
    print(f"tf_tree  plan.at(t)          {ours:10.1f} ns")
    print(f"tf2_ros  lookup_transform    {theirs:10.1f} ns")
    print(f"ratio                        {theirs / ours:10.1f}x")

    print()
    print("=== batch (per sample) ===")
    for n in (64, 4096):
        stamps = np.linspace(LO, HI, n).astype(np.int64)
        out = np.empty((n, 4, 4))
        ours_b = bench(lambda st=stamps, o=out: plan.at_into(st, o), n)
        # tf2 has no batch API; a loop is what a user writes.
        theirs_b = bench(lambda st=stamps: [t2(int(s)) for s in st], n)
        print(
            f"n={n:5d}  tf_tree {ours_b:8.2f} ns/sample   "
            f"tf2_ros {theirs_b:9.2f} ns/sample   ratio {theirs_b / ours_b:7.1f}x"
        )


if __name__ == "__main__":
    main()
