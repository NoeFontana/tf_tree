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
default profile the same script on the same host reads roughly a sixth of the
release throughput and — the part that matters — the *scaling curve* moves with it,
by enough to cross the criterion's floor. A debug build is not a slower release
build, it is a different program, which is the sentence
`tf_tree_bench::report::Fitness::probe` carries for exactly this reason.

**The detector for that is printed rather than invented.** Every arm prints
ns/sample, and the single-thread arm prints it beside `tree.rs`'s own documented
**328 ns/elem** for a release, pinned, depth-3 `at` — a figure this repository
already maintains. A release run lands near it; a `develop` run reads about six
times it, which is unmissable. It is a comparison and not a floor, because the
reference is a differently shaped measurement.

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

**The figures are in `docs/benchmarks/EVIDENCE.md`'s probe row and deliberately not
restated here.** They were written into four places at once — this docstring, the
recipe comment, `PHASE3.md` §12.2 and that row — and within one revision the four
copies disagreed in the third digit, which is the drift this repository keeps
finding in exactly this shape. The register holds the numbers; what follows is what
they mean.

**Criterion 4's free-threaded half straddles the floor on the development host, and
"passes" would be too strong.** Over five runs the 1->8 reading sits either side of
6x, most of them above it. Under the one-sided argument each clearing run is a
conservative pass, so the criterion is *met at least conservatively* on a host with
half the physical cores its "8 threads" implies — but the margin is inside this
instrument's own spread, so **this host cannot settle the criterion either way**.
One with eight physical cores could. That is a more useful statement than a verdict.

**The GIL half reads consistently below the floor, the host is not the explanation,
and since 2026-09-11 the arm that says so has been run.** Its 1->4 arm is as high
as the free-threaded one or higher; the gap opens only at 8 threads, where SMT
siblings contend on GIL-held work. The code-side suspect was `Plan::at` allocating
its `(N,4,4)` output before `fill` detaches, and `--call at_into` prices it: the
same sweep, a caller-owned buffer, nothing allocated per call. It reads **higher at
1->8 in six of six interleaved pairs** and **still short of the floor**. So roughly
half the gap is a GIL-held allocation and the rest is unattributed — a narrower and
more useful statement than either "the host blocks it" or "the allocation is the
cause", both of which this file has published and neither of which survived
measurement. `--gate --call at_into` is refused: §7.3 names `plan.at`, and a
criterion re-pointed at the faster call stops meaning anything.

**The control falls and keeps falling.** `--serialize` reads below 1.0x at every
thread count and lower at each step, the way lock contention does. Against an
almost-4x 4-thread arm there is no reading of the noise that confuses the two.
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
# **The wrong-profile detector, and it is a maintained figure rather than one
# invented here.** `crates/tf_tree_py/src/tree.rs` documents `at`'s per-element
# cost as **328 ns/elem** for `layout="quat"` at depth 3 in a *release* build,
# pinned, best of five, and `docs/PHASE3.md` §6.1 repeats it. This sweep's
# single-thread arm is the same shape unpinned over a window, so it should land
# near that; a `develop`-profile build reads roughly six times worse. Printed as a
# comparison rather than enforced as a floor: the reference is a differently
# shaped measurement, so it is a sanity check a reader applies, not a gate.
DOCUMENTED_NS_PER_ELEM = 328.0


def usable_cpus() -> set[int]:
    """The CPUs this process may run on, as ids.

    `os.sched_getaffinity` and **not** `os.cpu_count()`: the latter ignores CPU
    affinity, so under `taskset -c 0,1` it reports the machine and any verdict
    would be about a host this process cannot reach.

    **It does not see a cgroup CPU *bandwidth* quota, and this used to claim it
    did.** `docker --cpus=2` writes `cpu.max`, not `cpuset.cpus`, so the affinity
    mask is unchanged by it and nothing below can tell. `cpu.max` is read
    separately by [`quota_cores`]; where a quota is in force it is the binding
    limit and the affinity mask is not.
    """
    try:
        return set(os.sched_getaffinity(0))
    except AttributeError:  # pragma: no cover - not Linux
        return set(range(os.cpu_count() or 1))


def quota_cores() -> float | None:
    """Cores this process's cgroup bandwidth quota allows, or `None` if unlimited.

    cgroup v2's `cpu.max` is `"<quota> <period>"` in microseconds, or `"max
    <period>"` when unlimited. `docker --cpus=2` writes `200000 100000`. This is
    the limit `os.sched_getaffinity` cannot see, and without it a container on a
    large host evaluates an 8-thread arm it has two cores' worth of budget for.
    """
    for path in ("/sys/fs/cgroup/cpu.max", "/sys/fs/cgroup/cpu/cpu.cfs_quota_us"):
        try:
            with open(path, encoding="utf-8") as f:
                text = f.read().split()
        except OSError:
            continue
        if not text or text[0] in ("max", "-1"):
            return None
        try:
            quota = float(text[0])
            period = float(text[1]) if len(text) > 1 else 100_000.0
        except ValueError:
            return None
        if quota > 0 and period > 0:
            return quota / period
    return None


def physical_cores() -> int | None:
    """Physical cores this process can actually use, or `None` if undecidable.

    Distinct `(physical id, core id)` pairs — the rule
    `tf_tree_bench::report::physical_cores` uses, and deliberately **not**
    `os.cpu_count()`, which counts SMT siblings and would make the core budget
    vacuously satisfied.

    **Counted only over the CPUs in this process's affinity mask, per processor.**
    An earlier revision took every pair in `/proc/cpuinfo` and then capped the
    *count* by the number of usable **logical** CPUs, which overstates by up to
    the SMT width: on this host `taskset -c 0,1` selects two siblings of one
    physical core, and that arithmetic returned **2**. The failure it admits is
    the one this function exists to prevent — 16 sibling-paired logical CPUs are
    8 real cores, `min(16, 16)` is 16, and an 8-thread shortfall would take the
    `FAIL` branch and be charged to the code.

    **A cgroup bandwidth quota also binds**, and the affinity mask cannot see it,
    so [`quota_cores`] is applied as a floor on the answer.
    """
    try:
        with open("/proc/cpuinfo", encoding="utf-8") as f:
            text = f.read()
    except OSError:
        return None
    mask = usable_cpus()
    pairs, cpu, phys, core = set(), None, None, None
    for line in text.splitlines():
        if line.startswith("processor"):
            # A new block: whatever the previous one had is already recorded.
            cpu, phys, core = line.split(":")[-1].strip(), None, None
        elif line.startswith("physical id"):
            phys = line.split(":")[-1].strip()
        elif line.startswith("core id"):
            core = line.split(":")[-1].strip()
        if cpu is not None and phys is not None and core is not None:
            try:
                if int(cpu) in mask:
                    pairs.add((phys, core))
            except ValueError:
                return None
            phys = core = None
    if not pairs:
        return None
    n = len(pairs)
    quota = quota_cores()
    if quota is not None:
        # Floor, not round: two cores' budget cannot run a third thread's work.
        n = min(n, max(1, int(quota)))
    return n


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
    into: bool = False,
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
            # **The `at_into` arm's buffer is per thread and allocated here**,
            # outside both loops, which is the entire point of the arm: `at`
            # allocates its (N,4,4) result on every call while the GIL is held,
            # and `at_into` writes into a buffer the caller already owns. One
            # shared buffer would make the threads race on the output and
            # measure something else entirely.
            #
            # Shaped by `np.empty_like(plan.at(stamps))` rather than from a
            # hard-coded (N,4,4): the shape and dtype then come from the binding
            # that is being measured, so a layout change cannot leave this arm
            # silently allocating the wrong buffer and falling back.
            out = np.empty_like(plan.at(stamps)) if into else None

            # Warm-up runs in the same thread on the same objects, and its
            # iterations are **not counted** - the timed loop starts from zero
            # once `go` is set. Scaling a mixed count by a time fraction was the
            # first shape here and it is a guess; this is exact, and the
            # per-iteration `is_set` it costs is present in both loops so it
            # cannot bias one thread count against another.
            #
            # **Both arms call the binding directly**, rather than through one
            # selected callable. A shared `call()` indirection would be fair
            # between the two — the same Python frame on each side — but it adds
            # a frame to the `at` numbers already published in
            # `docs/benchmarks/EVIDENCE.md`, and the whole subject here is a
            # per-call cost. Four loops repeated is the cheaper honesty.
            if into:
                while not go.is_set():
                    if lock is None:
                        plan.at_into(stamps, out)
                    else:
                        with lock:
                            plan.at_into(stamps, out)
                n = 0
                if lock is None:
                    while not stop.is_set():
                        plan.at_into(stamps, out)
                        n += 1
                else:
                    while not stop.is_set():
                        with lock:
                            plan.at_into(stamps, out)
                        n += 1
            else:
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
    # **`started` before `go.set()`, and the order is the whole point.** Taken
    # after, the main thread can be descheduled between the two statements — at 8
    # threads there are 9 runnable threads on 4 cores and no GIL to serialise them
    # — and the work done in that gap is counted in `counts` but not in `elapsed`,
    # which inflates the rate. The bias grows with thread count while the
    # 1-thread denominator has spare cores and sees almost none of it, so it pushes
    # the ratio *up*, the unsafe direction for a floor with a single-digit margin.
    # This order errs low instead.
    started = time.perf_counter()
    go.set()
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
        "--call",
        choices=("at", "at_into"),
        default="at",
        help=(
            "which binding the threads call. `at` is the criterion's own wording; "
            "`at_into` is the no-per-call-allocation comparison the GIL half owes."
        ),
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
    # **`--gate --call at_into` is refused for `--serialize`'s reason**, one
    # level up: §7.3's criterion says *"1/2/4/8 threads calling `plan.at`"*, so a
    # verdict taken on `at_into` would be a pass/fail about a call the criterion
    # does not name. The arm exists to explain a shortfall, not to replace the
    # measurement that found one — and a criterion quietly re-pointed at the
    # faster call is how a gate stops meaning anything.
    if args.gate and args.call == "at_into":
        ap.error(
            "--gate --call at_into is refused: PHASE3 §7.3's criterion names "
            "`plan.at`, so gating `at_into` would settle the criterion with a "
            "call it does not describe. Run it ungated as the comparison arm."
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
    quota = quota_cores()
    print(
        f"  usable CPUs      {len(usable)} by affinity"
        + (
            f", {quota:g} cores by cgroup quota"
            if quota is not None
            else ", no cgroup quota"
        )
    )
    print(f"  batch            {args.batch} stamps")
    print(
        f"  call             plan.{args.call}"
        + (
            ""
            if args.call == "at"
            else "  (no per-call allocation — the comparison arm, never the gate)"
        )
    )
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
        rate, calls = one_arm(
            plan, stamps, n, args.seconds, args.warmup, lock, args.call == "at_into"
        )
        rows.append((n, rate))
        # ns/sample is printed because it is what makes a wrong build profile
        # visible: this host reads ~300 ns/sample at `--release` on one thread
        # and ~1900 ns under `develop`'s default profile.
        per = 1e9 / rate if rate > 0 else float("inf")
        note = ""
        if n == 1:
            note = (
                f"   [release reference {DOCUMENTED_NS_PER_ELEM:.0f} ns/elem;"
                f" this is {per / DOCUMENTED_NS_PER_ELEM:.1f}x it]"
            )
        print(
            f"  {n:2d} thread(s)  {rate / 1e6:8.3f} M samples/s"
            f"   {per:7.1f} ns/sample aggregate   ({calls} calls){note}"
        )

    # The verdict distinguishes the two arms, so it needs to know which one ran.
    into = args.call == "at_into"

    base = rows[0][1]
    if base <= 0.0:
        # **The division `--batch 0` and `--seconds 0` are rejected to protect,
        # reached by a route they do not cover.** A 1-thread arm can complete zero
        # calls on legal arguments — `--batch 4000000 --seconds 0.001` does it —
        # and `rate / base` then raises `ZeroDivisionError`, which exits 1: the
        # same code this file reserves for "the shortfall is the code's", so a
        # starved run would be indistinguishable from a real FAIL under `--gate`.
        print(
            "  INVALID — the 1-thread arm completed no calls, so there is no "
            "denominator. Lower --batch or raise --seconds; a window shorter than "
            "one call measures nothing.",
            file=sys.stderr,
        )
        return 2
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
            f"({len(usable)} usable CPUs counts SMT siblings and answers the "
            "wrong question)."
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
        # **"this run clears it", not "the criterion is met".** On a host whose
        # reading straddles the floor, one clearing run is one conservative
        # observation and not a settled answer; the register says which.
        print(
            f"  PASS — {scaling:.3f}x over {widest} threads against a floor of "
            f"{FLOOR:g}x, {margin:+.1f}%. This run clears criterion 4's {half} "
            "half."
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
            "threads, so a shortfall here is not on its own attributable to the "
            "code."
        )
        if gil:
            # **The host is not the whole story for the GIL arm, and the arm that
            # says so has now been run.** The free-threaded half clears the floor
            # on this same host, window and batch, so these cores demonstrably
            # can deliver it. The suspect was code-side GIL-held work — `Plan::at`
            # allocates its (N,4,4) output before `fill` detaches — and
            # `--call at_into`, which writes into a caller-owned buffer and
            # allocates nothing per call, is the comparison that prices it.
            #
            # Measured 2026-09-11, six interleaved pairs on a host `quiet_check`
            # passed at 5.9-7.1%: `at_into` read higher at 1->8 in **6 of 6**
            # pairs, by +0.22 to +1.69, and its spread was 0.36 against `at`'s
            # 1.27. On one thread the two are within ~2%, which is the shape of a
            # cost that is cheap alone and serialises under contention.
            #
            # **And it still does not reach the floor** — `at_into` topped out at
            # 5.64 against 6.0. So neither "the host blocks it" nor "the
            # allocation is the cause" survives: the allocation is a real,
            # measured contributor worth roughly half the gap, and the remainder
            # is unattributed. The figures live in `docs/benchmarks/EVIDENCE.md`,
            # not here.
            if not into:
                print(
                    "  And it is not the whole story: the free-threaded arm "
                    "clears the floor on this same host, so these cores can "
                    "deliver it. `--call at_into` — no per-call allocation, so "
                    "no GIL-held allocation — reads higher at this width in "
                    "every paired run taken so far, and still short of the "
                    "floor. Roughly half the gap is the allocation; the rest is "
                    "unattributed. Re-run with `--call at_into` to see the pair."
                )
            else:
                print(
                    "  This is the no-allocation arm and it is still short, so "
                    "the GIL-held allocation in `Plan::at` does not account for "
                    "the whole gap. What remains is unattributed — not the host, "
                    "which the free-threaded half clears, and not the allocation, "
                    "which this arm removed."
                )
        else:
            print(
                f"  The number is the finding; re-take it on >= {widest} "
                "physical cores to decide the criterion."
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
                "usable_cpus": len(usable),
                "quota_cores": quota,
                "batch": args.batch,
                # **Which call this row measured.** A row that cannot say is a
                # row that gets compared against one taken the other way, which
                # is the four-way drift `EVIDENCE.md`'s register exists to stop.
                "call": args.call,
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
