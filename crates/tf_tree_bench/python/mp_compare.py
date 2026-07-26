"""Multi-process Python: N consumer nodes on one arena, against N `tf2_ros` buffers.

`docs/PHASE2.md` §12.4 and `docs/PHASE3.md` §12.1. The Rust `mp_bench` answers
this for Rust nodes; **Python is where the claim is largest and was unmeasured**,
because a Python `tf2_ros` node carries a full private `Buffer` and a Python
`tf_tree` node carries a `PROT_READ` mapping of one shared arena.

The methodology is `crates/tf_tree_bench/src/mp.rs`'s, and the reasons are the
same — restated here only where Python changes them:

* **Open loop.** Tick `i` is due at `t0 + i/rate` whether or not the consumer
  was ready, and its latency is `finish - intended`. A closed loop cannot
  measure latency: a stall *reduces* the offered load, so every recorded sample
  looks fast. That is coordinated omission.
* **A publisher runs throughout**, so the seqlock retry path is exercised and
  the readers' cache lines are actually invalidated. Measuring a transform
  engine with no publisher is measuring an empty road.
* **Per-consumer tails, not one mean.** `PHASE1.md` §11.2: p99.9 is the number
  that matters.
* **PSS, not summed RSS.** Summing RSS double-counts every shared page, which
  is precisely the quantity under test. PSS is the kernel's own answer — each
  shared page divided by the number of mappers — and needs no knowledge of the
  arena size, so it is equally fair to tf2's private buffers.
* **CPU from `schedstat`**, in nanoseconds. `/proc/<pid>/stat`'s utime/stime are
  in 10 ms ticks, which against a few milliseconds of work over six seconds
  reads as a flat 0.0%.

**`subprocess`, never `os.fork`.** The arena is mapped `MADV_DONTFORK`, so a
forked child inherits a handle to memory it does not have. That is not a
limitation being worked around here — it is the documented contract, and a
benchmark that forked would be measuring something no correct program does.

Usage: `just py-mp-bench` (both engines, in the ROS container).
"""

from __future__ import annotations

import os
import subprocess
import sys
import tempfile
import time

# Consumer counts to sweep. Above the physical core count the rows are
# scheduler noise, and the report says so rather than pretending otherwise.
CONSUMERS = [1, 2, 4, 8]
#: Per-consumer tick rate. 100 Hz is a plausible perception/planning node.
HZ = 100.0
#: Measurement window per point.
SECONDS = 4.0
#: The publisher's rate, and the depth of the chain every consumer resolves.
PUB_HZ = 100.0
#: How far behind the wall clock every query is aimed.
#:
#: Both engines are stamped with `time.time_ns()`, so both consumers can aim at
#: the same instant without knowing anything about the publisher. 100 ms is far
#: enough back that a consumer is never asking for a sample that has not been
#: published yet — an `ExtrapolationError` is an error path, and timing an error
#: path measures the error path.
LAG_NS = 100_000_000
#: History each tf2 consumer must materialise to answer the same queries.
#:
#: Both engines are asked for `now - LAG_NS`, and `now` advances for the whole
#: measurement window. tf_tree's answer comes from a live publisher; tf2's
#: buffer is static once filled, so it has to be filled *past* the end of the
#: window or the last ticks extrapolate into the future. That is not a handicap
#: invented here — it is what having no shared arena costs, and the fill itself
#: is reported as `fill` below.
TF2_HISTORY_S = 30.0
EDGES = [("map", "odom"), ("odom", "base"), ("base", "cam")]


# ---------------------------------------------------------------------------
# Process accounting
# ---------------------------------------------------------------------------


def pss_kib(pid: int) -> int:
    """Proportional set size, from `smaps_rollup`.

    Each shared page counted as `1/n` for `n` mappers. Summing RSS instead
    would count the one shared arena once per consumer, which would flatter
    tf_tree by exactly the amount being claimed — so it is not used.
    """
    try:
        with open(f"/proc/{pid}/smaps_rollup") as f:
            for line in f:
                if line.startswith("Pss:"):
                    return int(line.split()[1])
    except OSError:
        pass
    return 0


def cpu_ns(pid: int) -> int:
    """CPU time in nanoseconds, from `schedstat` field 1.

    Not `stat`'s utime+stime: those are in 10 ms clock ticks, and a consumer
    doing a few milliseconds of work across a six-second window reads as
    exactly 0.0% on every row.
    """
    try:
        with open(f"/proc/{pid}/schedstat") as f:
            return int(f.read().split()[0])
    except OSError:
        return 0


def pct(xs: list[float], q: float) -> float:
    """Nearest-rank percentile of an already-sorted list."""
    if not xs:
        return float("nan")
    i = min(len(xs) - 1, max(0, int(round(q * (len(xs) - 1)))))
    return xs[i]


# ---------------------------------------------------------------------------
# The consumer, which is also this file run as `-m` with a role argument
# ---------------------------------------------------------------------------


def run_consumer(engine: str) -> None:
    """One node's worth of work, reporting its latency distribution on stdout."""
    ticks = int(HZ * SECONDS)

    if engine == "tf_tree":
        import tf_tree

        # **Time to first usable lookup.** tf_tree joins an arena somebody else
        # is already publishing into, so this is a handshake and a mapping.
        fill_t0 = time.perf_counter()
        tree = tf_tree.open(mode="ro")
        plan = tree.plan("map", "cam")

        def lookup(stamp_ns: int) -> None:
            plan.at(stamp_ns)

    else:
        import rclpy.time
        from geometry_msgs.msg import TransformStamped
        from rclpy.duration import Duration
        from tf2_ros import Buffer

        def rclpy_duration(seconds: float) -> Duration:
            return Duration(seconds=seconds)

        # **The structural difference, and the point of the whole benchmark.**
        # There is no shared arena, so this consumer must hold its own copy of
        # the entire history. Every node pays for it again, in memory and in
        # the CPU that filled it.
        buf = Buffer(cache_time=rclpy_duration(TF2_HISTORY_S + 5.0))
        # Anchored on this consumer's own start, because the queries below are
        # aimed at the wall clock and there is no shared arena to read a stamp
        # from. Every consumer materialises all of it, separately — that is the
        # cost being measured.
        fill_t0 = time.perf_counter()
        t_start = time.time_ns()
        n_ticks = int(PUB_HZ * TF2_HISTORY_S)
        for i in range(n_ticks):
            stamp = t_start - 1_000_000_000 + int(i * 1e9 / PUB_HZ)
            for parent, child in EDGES:
                t = TransformStamped()
                t.header.stamp.sec = stamp // 1_000_000_000
                t.header.stamp.nanosec = stamp % 1_000_000_000
                t.header.frame_id = parent
                t.child_frame_id = child
                t.transform.translation.x = 0.001 * i
                t.transform.rotation.w = 1.0
                buf.set_transform(t, "bench")

        def lookup(stamp_ns: int) -> None:
            buf.lookup_transform("map", "cam", rclpy.time.Time(nanoseconds=stamp_ns))

    fill_ms = (time.perf_counter() - fill_t0) * 1e3

    # **Wait for history before measuring.** A consumer that starts before the
    # publisher has filled its window gets `ExtrapolationError` — an error path,
    # and timing an error path measures the error path. Polling until the first
    # query succeeds is startup, not measurement, and it keeps the harness
    # honest about start ordering rather than depending on a sleep being long
    # enough on whatever machine runs it.
    deadline = time.perf_counter() + 20.0
    while True:
        try:
            lookup(time.time_ns() - LAG_NS)
            break
        except Exception as e:  # noqa: BLE001 - any engine's "no data yet"
            if time.perf_counter() > deadline:
                raise RuntimeError(f"no usable history after 20 s: {e}") from e
            time.sleep(0.02)

    period = 1.0 / HZ
    t0 = time.perf_counter()
    service: list[float] = []
    cycle: list[float] = []

    for i in range(ticks):
        due = t0 + i * period
        now = time.perf_counter()
        if now < due:
            time.sleep(due - now)
        start = time.perf_counter()
        lookup(time.time_ns() - LAG_NS)
        done = time.perf_counter()
        # `service` is what the engine cost; `cycle` is what the node
        # experienced, measured from the tick's *intended* time, so falling
        # behind shows up as latency rather than as fewer samples.
        service.append((done - start) * 1e9)
        cycle.append((done - due) * 1e9)

    service.sort()
    cycle.sort()
    print(
        f"RESULT {os.getpid()} "
        f"{pct(service, 0.5):.0f} {pct(service, 0.99):.0f} {pct(service, 0.999):.0f} "
        f"{pct(cycle, 0.5):.0f} {pct(cycle, 0.99):.0f} {pct(cycle, 0.999):.0f} "
        f"{fill_ms:.1f}",
        flush=True,
    )


def run_publisher() -> None:
    """Publish into the shared arena until killed."""
    import tf_tree

    tree = tf_tree.open(mode="rw", create=EDGES, capacity=4096)
    writers = [tree.publisher(child, parent) for parent, child in EDGES]
    print("READY", flush=True)
    period = 1.0 / PUB_HZ
    t0 = time.perf_counter()
    i = 0
    while True:
        # **The tick index is derived from elapsed time, not incremented.**
        # A publisher that increments and then sleeps `due - now` does not
        # sleep at all once it is behind, so it bursts at full speed until it
        # catches up — and a burst laps the ring, leaving a retained window of
        # milliseconds instead of the 40 s the capacity implies. That is how
        # this first failed: every consumer got `ExtrapolationError` because the
        # publisher had run away from its own history.
        now = time.perf_counter()
        i = max(i + 1, int((now - t0) / period) + 1)
        due = t0 + i * period
        if now < due:
            time.sleep(due - now)
        # Wall-clock stamps, as a real publisher uses, so a consumer can aim at
        # an instant without knowing when this process started.
        stamp = time.time_ns()
        for w in writers:
            w.push(stamp, [1.0, 0.0, 0.0, 0.0, 0.001 * i, 0.0, 0.0])
        i += 1


# ---------------------------------------------------------------------------
# Coordinator
# ---------------------------------------------------------------------------


def measure(engine: str, n: int, env: dict[str, str]) -> dict[str, float]:
    kids = [
        subprocess.Popen(
            [sys.executable, __file__, "consumer", engine],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=env,
        )
        for _ in range(n)
    ]
    # Let every consumer reach its loop before sampling: a process still
    # filling a tf2 buffer has neither its steady-state memory nor its
    # steady-state CPU, and sampling then would understate tf2's cost.
    time.sleep(1.5 if engine == "tf_tree" else 3.0)
    pss = sum(pss_kib(k.pid) for k in kids)
    cpu0 = [cpu_ns(k.pid) for k in kids]
    t0 = time.perf_counter()
    time.sleep(min(2.0, SECONDS / 2))
    cpu1 = [cpu_ns(k.pid) for k in kids]
    wall = time.perf_counter() - t0

    rows = []
    errs = []
    for k in kids:
        out, err = k.communicate(timeout=180)
        if err:
            errs.append(err.strip())
        for line in out.splitlines():
            if line.startswith("RESULT "):
                rows.append([float(x) for x in line.split()[2:]])
    if not rows:
        raise RuntimeError(
            f"{engine}: no consumer reported a result\n" + "\n".join(errs[:1])
        )

    busy = [(c1 - c0) / (wall * 1e9) for c0, c1 in zip(cpu0, cpu1, strict=True)]
    return {
        "svc_p50": max(r[0] for r in rows),
        "svc_p999": max(r[2] for r in rows),
        "cyc_p999": max(r[5] for r in rows),
        "fill_ms": max(r[6] for r in rows),
        "pss_mib": pss / 1024.0,
        "cpu_pct": 100.0 * sum(busy) / max(1, len(busy)),
        "n_reported": len(rows),
    }


def main() -> None:
    if len(sys.argv) > 1 and sys.argv[1] == "consumer":
        run_consumer(sys.argv[2])
        return
    if len(sys.argv) > 1 and sys.argv[1] == "publisher":
        run_publisher()
        return

    engines = sys.argv[1:] or ["tf_tree", "tf2"]
    cores = os.cpu_count() or 0
    print("Python multi-process node evaluation")
    print("=" * 76)
    print(
        f"{HZ:.0f} Hz per consumer, {SECONDS:.0f} s window, chain depth {len(EDGES)}."
    )
    print(f"cpus: {cores}. Rows above the physical core count are scheduler noise.")
    print("Latency is measured from each tick's INTENDED time, so a consumer that")
    print("falls behind reports the backlog instead of silently taking fewer samples.")
    print()
    print("  tf_tree consumers share ONE arena, read-only, and a live publisher runs.")
    print("  tf2 consumers each hold a private Buffer filled with the same stream —")
    print("  a FLOOR for tf2: it shows the duplication that having no shared arena")
    print("  forces, with no DDS, no serialisation and no TransformListener.")
    print()
    print("  PSS, not summed RSS: summing would count the shared arena once per")
    print("  consumer, flattering tf_tree by exactly the amount being claimed.")
    print()
    print("  fill = time to the first usable lookup. tf_tree joins an arena that is")
    print("  already being published into; tf2 must materialise the history itself,")
    print("  once per node, before it can answer anything.")
    print()

    for engine in engines:
        print(f"--- {engine} " + "-" * (64 - len(engine)))
        print(
            f"{'nodes':>6}  {'svc p50':>9}  {'svc p99.9':>10}  "
            f"{'cycle p99.9':>12}  {'PSS':>9}  {'CPU/node':>9}  {'fill':>8}"
        )
        with tempfile.TemporaryDirectory(prefix="tf_tree_mp_") as d:
            env = {**os.environ, "TF_TREE_RUNTIME_DIR": d}
            pub = None
            sweep: list[dict[str, float]] = []
            if engine == "tf_tree":
                pub = subprocess.Popen(
                    [sys.executable, __file__, "publisher"],
                    stdout=subprocess.PIPE,
                    text=True,
                    env=env,
                )
                assert pub.stdout is not None
                # Reading READY is the synchronisation: the arena exists and is
                # being published into by the time it returns. No sleeps.
                assert pub.stdout.readline().strip() == "READY", (
                    "publisher did not start"
                )
            try:
                sweep = []
                for n in CONSUMERS:
                    r = measure(engine, n, env)
                    sweep.append(r)
                    assert r["n_reported"] == n, f"{r['n_reported']}/{n} reported"
                    print(
                        f"{n:>6}  {r['svc_p50']:>8.0f}n  {r['svc_p999']:>9.0f}n  "
                        f"{r['cyc_p999'] / 1000:>10.0f}us  "
                        f"{r['pss_mib']:>7.1f}M  {r['cpu_pct']:>8.1f}%  "
                        f"{r['fill_ms']:>6.0f}ms"
                    )
            finally:
                if pub is not None:
                    pub.kill()
                    pub.wait()

        # **Marginal cost is the claim.** The totals are dominated by the Python
        # interpreter and numpy, which both engines pay identically; what the
        # shared arena changes is the *slope* — what each additional node costs.
        if len(sweep) >= 2:
            lo, hi = sweep[0], sweep[-1]
            dn = CONSUMERS[-1] - CONSUMERS[0]
            d_pss = (hi["pss_mib"] - lo["pss_mib"]) / dn
            # `cpu_pct` is the mean over that row's consumers, so it is scaled
            # back to a fleet total before differencing.
            d_cpu = (hi["cpu_pct"] * CONSUMERS[-1] - lo["cpu_pct"] * CONSUMERS[0]) / dn
            print(f"  marginal: {d_pss:.1f} MiB/node, {d_cpu:.2f}% cpu/node")
        print()


if __name__ == "__main__":
    main()
