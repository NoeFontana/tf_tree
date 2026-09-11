"""`docs/PHASE3.md` §12.2 criterion 4 and §7.3's missing scaling test.

Criterion 4 has two halves: *"thread scaling >= 6x from 1 to 8 threads on
`3.14t`, and >= 6x on the GIL build for batches above the release threshold"*.
§7.3 requires *"a scaling test: 1/2/4/8 threads calling `plan.at` on a shared
`Tree`, asserting near-linear aggregate throughput"*. Nothing measured either
until this file. `tests/python/test_freethreading.py` runs eight threads and
asserts **correctness**; it would pass on a build that serialised every call, so
it is not this claim.

**Which half you get is decided by the interpreter that runs this**, so both are
reachable and neither is declared out of scope: under `python3.14t` this is the
free-threaded half, under `python3.14` the GIL half. §6.1's release threshold is
what makes the GIL half meaningful, and the default batch clears it by an order of
magnitude at depth 3.

**It must be run against a `--release` extension and `just py-thread-scaling`
builds one.** This is not a formality and it was got wrong first: at `develop`'s
default profile the same script on the same host reads **0.52 M samples/s** on one
thread against **3.33 M** at release, and — the part that matters — the *scaling
curve* changes too, 4.4-5.1x against 6.17-6.23x. A debug build is not a slower
release build, it is a different program, which is the sentence
`tf_tree_bench::report::Fitness::probe` carries for exactly this reason. Every arm
prints ns/sample so a run against the wrong profile is visible rather than
plausible.

**The verdict is one-sided and that is what makes a PASS here real.** This is a
scaling *floor*, and every way a host can be unfair to it — fewer physical cores
than threads, SMT siblings sharing execution resources, another tenant — can only
push aggregate scaling **down**. So a reading at or above the floor is a
conservative claim and a reading below it is not attributable to the code. That is
`docs/PHASE5.md` §9.3's one-sided-budget argument, mirrored: a budget admits a
PASS from above, a floor admits one from below. A run that misses on a host with fewer
physical cores than threads reports its margin and `INVALID`.

**There is still a `FAIL`, and there has to be.** The one-sided argument excuses a
shortfall only where the host can explain it; on a host with a core per thread it
cannot, so a miss there is the code's and `--gate` exits 1 on it. A verdict with no
failing state is a gate that cannot fail, which is this repository's cardinal
defect class.

**`--serialize` is the falsifier**, and it edits no threshold: it wraps every call
in one `threading.Lock`, which is how a build that failed to release the GIL - or
a `Plan` that was not `Sync` - would behave. It must read a flat or falling curve.
A harness that cannot produce one on demand is not measuring one. `--gate` is
refused beside it rather than allowed to report the control as a regression.

Measured on the development host - 4 physical cores, 8 logical, `--release`, this
file's default window. Four runs on `3.14t` and three on the GIL build:

* **`3.14t`: 1->8 reads 6.17-6.23x, above the criterion's 6x in every run.**
  Criterion 4's free-threaded half **passes**, and it passes on a host with half
  the physical cores the "8 threads" in that criterion implies - which under the
  one-sided argument above makes it a stronger result rather than a weaker one.
* **GIL build: 1->8 reads 5.79-5.94x**, a miss of 1-4%. Reported with its margin
  and `INVALID`: the shortfall is smaller than what this host's own core count
  contributes, so it is not attributable to the code.
* **1->4 reads 3.92-3.93x (`3.14t`) and 3.97-4.03x (GIL)** - linear to the
  physical core count on both.
* **Control**: `--serialize` falls below 1.0x at every thread count and keeps
  falling as threads are added, the way lock contention does.

**No interval is quoted as this instrument's spread**, per
`docs/benchmarks/EVIDENCE.md`'s rule: the ranges above are the runs that were
taken, and re-running walks outside them without anything having changed. The
recipe prints each run's own numbers.
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
# so it is the size the project already treats as the interesting one, and at
# depth 3 it is an order of magnitude above §6.1's GIL-release threshold - which
# is what makes the GIL half of criterion 4 meaningful at this size.
DEFAULT_BATCH = 64
# Long enough that thread start-up is not the measurement, short enough that the
# whole sweep is a few seconds.
DEFAULT_SECONDS = 2.0
DEFAULT_WARMUP = 0.5
# Criterion 4's floor, for both halves.
FLOOR = 6.0


def usable_cpus() -> int:
    """CPUs this process may actually run on.

    `os.sched_getaffinity` and **not** `os.cpu_count()`: the latter ignores CPU
    affinity and cgroup quota, so under `taskset -c 0,1` or `docker --cpus=2` it
    reports the machine and the verdict would be about a host this process cannot
    reach. The Rust side uses `available_parallelism()` for the same reason.
    """
    try:
        return len(os.sched_getaffinity(0))
    except AttributeError:  # pragma: no cover - not Linux
        return os.cpu_count() or 1


def physical_cores() -> int | None:
    """Distinct `(physical id, core id)` pairs, capped by this process's affinity.

    The pair-counting rule is `tf_tree_bench::report::physical_cores`'s, and
    deliberately **not** `os.cpu_count()`: that counts SMT siblings, which would
    make the core budget vacuously satisfied and report a verdict about a host
    nothing was learned from.

    **Capped by affinity, because `/proc/cpuinfo` reports the machine's topology
    and not this container's.** Inside `docker --cpus=2`, or under `taskset`, the
    file still lists every core on the box, so an uncapped reading would let an
    8-thread arm be *evaluated* on two usable CPUs and print a FAIL that is
    entirely the cgroup's - the exact collapse of `INVALID` into `FAIL` this
    file's verdict exists to prevent.
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
    if not pairs:
        return None
    return min(len(pairs), usable_cpus())


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
    """Aggregate samples per second over `threads` threads, and the call count.

    Every thread runs for the same wall-clock window rather than for a fixed
    iteration count: a fixed count makes the slowest thread the measurement, and
    the quantity the criterion is about is aggregate throughput.

    Raises if any worker raised. A thread that dies is otherwise invisible here:
    `threading` prints its traceback to stderr and this function would return a
    rate computed from the threads that survived, which under `--gate` becomes a
    PASS or FAIL about an arm that never ran at its declared width.
    """
    counts = [0] * threads
    errors: list[BaseException] = []
    go = threading.Event()
    stop = threading.Event()

    def worker(slot: int) -> None:
        try:
            # Warm-up runs in the same thread on the same objects, and its
            # iterations are **not counted** - the timed loop starts from zero
            # once `go` is set. Scaling a mixed count by a time fraction was the
            # first shape here and it is a guess; this is exact, and the
            # per-iteration `is_set` it costs is present in both loops so it
            # cannot bias one thread count against another.
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
        except BaseException as e:  # noqa: BLE001 - re-raised by the caller
            errors.append(e)
            go.set()
            stop.set()

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
    if errors:
        raise RuntimeError(
            f"{len(errors)} of {threads} worker(s) raised; the arm measured "
            f"nothing it can be trusted for: {errors[0]!r}"
        )
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
        help="the falsifier: one lock around every call; must read flat or falling",
    )
    ap.add_argument(
        "--gate",
        action="store_true",
        help="exit non-zero on a miss. Refused where the verdict would be INVALID.",
    )
    args = ap.parse_args()

    # Rejected at parse time rather than after paying the sweep: `--batch 0`
    # measures an empty array and divides by zero at the very end, and
    # `--seconds 0` divides by a near-zero elapsed.
    if args.batch < 1:
        ap.error("--batch must be at least 1; 0 stamps measures nothing")
    if args.seconds <= 0 or args.warmup < 0:
        ap.error("--seconds must be positive and --warmup non-negative")
    # The control is not a measurement, so it may not produce a gated verdict.
    # `frozen_workers` refuses `--gate --python` for the same reason: a deferred
    # decision must not arrive as a flag pair.
    if args.gate and args.serialize:
        ap.error(
            "--gate --serialize is refused: --serialize is the deliberate "
            "flat-curve control, and gating it would report the control as a "
            "regression"
        )

    gil = getattr(sys, "_is_gil_enabled", lambda: True)()
    cores = physical_cores()
    usable = usable_cpus()
    widest = max(THREAD_COUNTS)
    half = "the GIL build" if gil else "`3.14t`"

    print("tf_tree Python thread scaling — PHASE3 §12.2 criterion 4")
    print(f"  interpreter      {sys.version.split()[0]}  GIL enabled: {gil}")
    print(f"  criterion half   {half}")
    print(f"  physical cores   {cores if cores is not None else 'unknown'}")
    print(f"  usable CPUs      {usable} (affinity- and cgroup-aware)")
    print(f"  batch            {args.batch} stamps")
    print(
        f"  window           {args.seconds:g}s after {args.warmup:g}s warm-up,"
        " discarded"
    )
    if args.serialize:
        print(
            "  MODE             --serialize: the flat-curve control, not a measurement"
        )
    print()

    tree = build_tree()
    plan = tree.plan("map", "imu_link")
    stamps = np.linspace(1_000, 2_000, args.batch, dtype=np.int64)
    lock = threading.Lock() if args.serialize else None

    rows: list[tuple[int, float]] = []
    for n in THREAD_COUNTS:
        rate, calls = one_arm(plan, stamps, n, args.seconds, args.warmup, lock)
        rows.append((n, rate))
        # ns/sample is printed because it is what makes a wrong build profile
        # visible: this host reads ~300 ns/sample at `--release` on one thread
        # and ~1900 ns under `develop`'s default profile.
        per = 1e9 / rate if rate > 0 else float("inf")
        print(
            f"  {n:2d} thread(s)  {rate / 1e6:8.3f} M samples/s"
            f"   {per:7.1f} ns/sample aggregate   ({calls} calls)"
        )

    base = rows[0][1]
    print()
    for n, rate in rows:
        print(f"  1 -> {n:<2d} scaling   {rate / base:6.3f}x")
    print()

    # The verdict. One-sided: every way this host can be unfair to a scaling
    # floor pushes the reading down, so at-or-above is a conservative claim and
    # below is not attributable. A miss is INVALID with its margin, never FAIL.
    scaling = rows[-1][1] / base
    if cores is None:
        print(
            "  INVALID — this host publishes no physical core count, so a "
            f"{widest}-thread reading cannot be placed against anything "
            f"({usable} usable CPUs counts SMT siblings and answers the wrong "
            "question)."
        )
        verdict = "INVALID"
    elif args.serialize:
        print(
            f"  CONTROL — {scaling:.3f}x over {widest} threads. This is the "
            "serialised arm; a flat or falling curve is the expected reading and "
            "no verdict is taken from it."
        )
        verdict = "CONTROL"
    elif scaling >= FLOOR:
        margin = (scaling / FLOOR - 1.0) * 100.0
        print(
            f"  PASS — {scaling:.3f}x over {widest} threads against a floor of "
            f"{FLOOR:g}x, {margin:+.1f}%. Criterion 4's {half} half is met."
        )
        if cores < widest:
            print(
                f"  …and met on {cores} physical cores for {widest} threads, "
                "which the one-sided argument makes a conservative pass rather "
                "than a lucky one: every deficiency here pushes scaling down."
            )
        verdict = "PASS"
    elif cores < widest:
        margin = (1.0 - scaling / FLOOR) * 100.0
        print(
            f"  INVALID — {scaling:.3f}x over {widest} threads against a floor "
            f"of {FLOOR:g}x, short by {margin:.1f}%."
        )
        print(
            f"  Not FAIL: this host has {cores} physical cores for {widest} "
            "threads, so the shortfall is not attributable to the code. The "
            f"number is the finding; re-take it on >= {widest} physical cores to "
            "decide the criterion."
        )
        verdict = "INVALID"
    else:
        # **The one place a miss is the code's, and it has to exist.** The
        # one-sided argument excuses a shortfall only where the host can explain
        # it; on a host with a core per thread it cannot, so this is a FAIL and
        # `--gate` exits 1 on it. Without this arm the verdict would have no
        # failing state at all, which is a gate that cannot fail.
        margin = (1.0 - scaling / FLOOR) * 100.0
        print(
            f"  FAIL — {scaling:.3f}x over {widest} threads against a floor of "
            f"{FLOOR:g}x, short by {margin:.1f}%, on {cores} physical cores. "
            "The host has a core per thread, so the shortfall is the code's."
        )
        verdict = "FAIL"

    print()
    print(
        json.dumps(
            {
                "schema": "tf_tree.py-thread-scaling/1",
                "verdict": verdict,
                "criterion_half": "gil" if gil else "freethreaded",
                "gil_enabled": gil,
                "physical_cores": cores,
                "usable_cpus": usable,
                "batch": args.batch,
                "serialize": args.serialize,
                "floor": FLOOR,
                f"scaling_1_to_{widest}": round(scaling, 4),
                "rows": [{"threads": n, "samples_per_s": round(r, 1)} for n, r in rows],
            }
        )
    )

    if args.gate and verdict == "INVALID":
        print(
            "\n  --gate refused: the verdict is INVALID, and exiting non-zero "
            "on a host condition is what makes a red gate unreadable.",
            file=sys.stderr,
        )
        return 2
    if args.gate and verdict == "FAIL":
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
