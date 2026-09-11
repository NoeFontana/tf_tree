"""`docs/PHASE3.md` §12.2 criterion 4 and §7.3's missing scaling test.

Criterion 4 is *"thread scaling >= 6x from 1 to 8 threads on `3.14t`"*, and §7.3
requires *"a scaling test: 1/2/4/8 threads calling `plan.at` on a shared `Tree`,
asserting near-linear aggregate throughput"*. Nothing measured either until this
file. `tests/python/test_freethreading.py` runs eight threads and asserts
**correctness**; it would pass on a build that serialised every call, so it is not
this claim.

**The verdict and the number are separated on purpose, and this host is why.**
The criterion names a thread count (8) and the scaling it demands is a statement
about *cores*. The development host has **4 physical cores** with SMT, so a second
thread on a pair contributes a fraction of a core and 6x is unreachable
arithmetically — a run here can refute the criterion and can never satisfy it.
Reporting `FAIL` would therefore charge the host's core count to the code, which
is the confusion `docs/PHASE5.md` §9.3 exists to prevent. So a run whose thread
count exceeds the physical core count reports **INVALID**, prints the curve, and
exits 0 unless `--gate` was passed.

What makes a number here worth having anyway: the *shape* of the curve up to the
core count is a claim about the code. If `plan.at` held a lock, or re-entered the
interpreter per sample, the 1 -> 4 arm would be flat on any host.

**`--serialize` is the falsifier**, and it edits no threshold: it wraps every call
in one `threading.Lock`, which is what a build that failed to release the GIL — or
a `Plan` that was not `Sync` — would behave like. It must read about 1.0x at every
thread count. A harness that cannot produce a flat curve on demand is not
measuring one.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import threading
import time

import numpy as np

import tf_tree

# Depth 3 after constant folding, which is `docs/PHASE1.md` §11.3's shape and the
# one every latency figure in this repository is stated over. A one-edge tree
# would measure the call and not the fold.
EDGES = [("map", "odom"), ("odom", "base"), ("base", "imu_link")]
THREAD_COUNTS = (1, 2, 4, 8)
# The control-loop batch. `docs/API.md` R2 argues the `_into` forms from n = 64,
# so it is the size the project already treats as the interesting one.
DEFAULT_BATCH = 64
# Long enough that thread start-up is not the measurement, short enough that the
# whole sweep is a few seconds.
DEFAULT_SECONDS = 2.0
DEFAULT_WARMUP = 0.5


def physical_cores() -> int | None:
    """Distinct `(physical id, core id)` pairs, or `None` if unpublished.

    The same rule `tf_tree_bench::report::physical_cores` uses, and deliberately
    **not** `os.cpu_count()`: that counts SMT siblings, which would make the core
    budget vacuously satisfied and report a verdict about a host nothing was
    learned from.
    """
    try:
        with open("/proc/cpuinfo", encoding="utf-8") as f:
            text = f.read()
    except OSError:
        return None
    pairs, phys, core = set(), None, None
    for line in text.splitlines():
        if line.startswith("physical id"):
            phys = line.split(":")[-1].strip()
        elif line.startswith("core id"):
            core = line.split(":")[-1].strip()
        if phys is not None and core is not None:
            pairs.add((phys, core))
            phys = core = None
    return len(pairs) or None


def build_tree() -> tf_tree.Tree:
    tree = tf_tree.build(EDGES)
    # Two samples per edge so every step interpolates: a single sample folds to a
    # constant and the fold this is meant to time disappears.
    for parent, child in EDGES:
        for i, stamp in enumerate((1_000, 2_000)):
            tf_tree.push(
                tree,
                child,
                parent,
                stamp,
                [1.0, 0.0, 0.0, 0.0, float(i), 0.0, 0.0],
            )
    return tree


def one_arm(
    plan: tf_tree.Plan,
    stamps: np.ndarray,
    threads: int,
    seconds: float,
    warmup: float,
    lock: threading.Lock | None,
) -> tuple[float, int]:
    """Aggregate samples per second over `threads` threads, and the sample count.

    Every thread runs for the same wall-clock window rather than for a fixed
    iteration count: a fixed count makes the slowest thread the measurement, and
    the quantity the criterion is about is aggregate throughput.
    """
    counts = [0] * threads
    go = threading.Event()
    stop = threading.Event()

    def worker(slot: int) -> None:
        # Warm-up runs in the same thread on the same objects, and its iterations
        # are **not counted** — the timed loop starts from zero once `go` is set.
        # Scaling a mixed count by a time fraction was the first shape here and it
        # is a guess; this is exact, and the per-iteration `is_set` it costs is
        # present in both loops so it cannot bias one thread count against another.
        while not go.is_set():
            if lock is None:
                plan.at(stamps)
            else:
                with lock:
                    plan.at(stamps)
        n = 0
        if lock is None:
            while not stop.is_set():
                plan.at(stamps)
                n += 1
        else:
            while not stop.is_set():
                with lock:
                    plan.at(stamps)
                n += 1
        counts[slot] = n

    pool = [threading.Thread(target=worker, args=(i,)) for i in range(threads)]
    for t in pool:
        t.start()
    time.sleep(warmup)
    go.set()
    started = time.perf_counter()
    time.sleep(seconds)
    stop.set()
    elapsed = time.perf_counter() - started
    for t in pool:
        t.join()
    total = sum(counts)
    return total * len(stamps) / elapsed, total


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--batch", type=int, default=DEFAULT_BATCH)
    ap.add_argument("--seconds", type=float, default=DEFAULT_SECONDS)
    ap.add_argument("--warmup", type=float, default=DEFAULT_WARMUP)
    ap.add_argument(
        "--serialize",
        action="store_true",
        help="the falsifier: one lock around every call; must read ~1.0x",
    )
    ap.add_argument(
        "--gate",
        action="store_true",
        help="exit non-zero on FAIL. Refused where the verdict would be INVALID.",
    )
    args = ap.parse_args()

    gil = getattr(sys, "_is_gil_enabled", lambda: True)()
    cores = physical_cores()
    logical = os.cpu_count() or 1
    widest = max(THREAD_COUNTS)

    print("tf_tree Python thread scaling — PHASE3 §12.2 criterion 4")
    print(f"  interpreter      {sys.version.split()[0]}  GIL enabled: {gil}")
    print(f"  physical cores   {cores if cores is not None else 'unknown'}")
    print(f"  logical CPUs     {logical}")
    print(f"  batch            {args.batch} stamps")
    print(f"  window           {args.seconds:g}s after {args.warmup:g}s warm-up, discarded")
    if args.serialize:
        print("  MODE             --serialize: the flat-curve control, not a measurement")
    print()

    tree = build_tree()
    plan = tree.plan("map", "imu_link")
    stamps = np.linspace(1_000, 2_000, args.batch, dtype=np.int64)
    lock = threading.Lock() if args.serialize else None

    rows: list[tuple[int, float]] = []
    for n in THREAD_COUNTS:
        rate, iters = one_arm(plan, stamps, n, args.seconds, args.warmup, lock)
        rows.append((n, rate))
        print(f"  {n:2d} thread(s)  {rate / 1e6:8.3f} M samples/s   ({iters} calls)")

    base = rows[0][1]
    print()
    for n, rate in rows:
        print(f"  1 -> {n:<2d} scaling   {rate / base:6.3f}x")
    print()

    # The verdict. Three states, and which one applies is a fact about the host
    # before it is anything about the code.
    scaling = rows[-1][1] / base
    if gil:
        print(
            "  INVALID — this is a GIL interpreter. Criterion 4 is about `3.14t`; "
            "run it under `python3.14t`."
        )
        verdict = "INVALID"
    elif cores is None:
        print(
            "  INVALID — this host publishes no physical core count, so the "
            f"{widest}-thread arm cannot be checked against anything ({logical} "
            "logical CPUs counts SMT siblings and answers the wrong question)."
        )
        verdict = "INVALID"
    elif cores < widest:
        print(
            f"  INVALID — {widest} threads over {cores} physical cores. The "
            f"criterion's 6x needs {widest} cores' worth of work to distribute; "
            "above the core count this measures the scheduler. The curve above is "
            "the finding; the 6x verdict is not evaluable here."
        )
        verdict = "INVALID"
    elif scaling >= 6.0:
        print(f"  PASS — {scaling:.3f}x over {widest} threads, criterion 4 wants >= 6x.")
        verdict = "PASS"
    else:
        print(f"  FAIL — {scaling:.3f}x over {widest} threads, criterion 4 wants >= 6x.")
        verdict = "FAIL"

    print()
    print(
        json.dumps(
            {
                "schema": "tf_tree.py-thread-scaling/1",
                "verdict": verdict,
                "gil_enabled": gil,
                "physical_cores": cores,
                "logical_cpus": logical,
                "batch": args.batch,
                "serialize": args.serialize,
                f"scaling_1_to_{widest}": round(scaling, 4),
                "rows": [{"threads": n, "samples_per_s": round(r, 1)} for n, r in rows],
            }
        )
    )

    if args.gate:
        if verdict == "INVALID":
            print(
                "\n  --gate refused: the verdict is INVALID, and exiting non-zero on a "
                "host condition is what makes a red gate unreadable.",
                file=sys.stderr,
            )
            return 2
        if verdict == "FAIL":
            return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
