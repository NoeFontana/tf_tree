"""Multi-process Python: N consumer nodes on one arena, against N `tf2_ros` buffers.

`docs/PHASE2.md` §12.4 and `docs/PHASE3.md` §12.1. Methodology is
`crates/tf_tree_bench/src/mp.rs`'s: open loop (latency is `finish - intended`), a
publisher throughout, per-consumer tails, PSS not summed RSS, CPU from `schedstat`.

**`subprocess`, never `os.fork`**: the arena is mapped `MADV_DONTFORK`.

Usage: `just py-mp-bench` (both engines, in the ROS container).
"""

from __future__ import annotations

import os
import subprocess
import sys
import tempfile
import time

# Consumer counts to sweep; above the physical core count rows are scheduler noise.
CONSUMERS = [1, 2, 4, 8]
HZ = 100.0
SECONDS = 4.0
PUB_HZ = 100.0
#: How far behind the wall clock every query is aimed.
LAG_NS = 100_000_000
#: History each tf2 consumer must materialise (its buffer is static once filled).
TF2_HISTORY_S = 30.0
EDGES = [("map", "odom"), ("odom", "base"), ("base", "cam")]


def pss_kib(pid: int) -> int:
    """Proportional set size from `smaps_rollup` (RSS double-counts shared pages)."""
    try:
        with open(f"/proc/{pid}/smaps_rollup") as f:
            for line in f:
                if line.startswith("Pss:"):
                    return int(line.split()[1])
    except OSError:
        pass
    return 0


def cpu_ns(pid: int) -> int:
    """CPU time in nanoseconds, from `schedstat` field 1 (`stat` ticks are 10 ms)."""
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


def run_consumer(engine: str) -> None:
    """One node's worth of work, reporting its latency distribution on stdout."""
    ticks = int(HZ * SECONDS)

    if engine == "tf_tree":
        import numpy as np
        import tf_tree

        fill_t0 = time.perf_counter()
        tree = tf_tree.open(mode="ro")
        plan = tree.plan("map", "cam")

        # `at_into` a buffer allocated once: a node cannot batch (`at` allocates).
        out = np.empty((4, 4))

        def lookup(stamp_ns: int) -> None:
            plan.at_into(stamp_ns, out)

    else:
        import rclpy.time
        from geometry_msgs.msg import TransformStamped
        from rclpy.duration import Duration
        from tf2_ros import Buffer

        def rclpy_duration(seconds: float) -> Duration:
            return Duration(seconds=seconds)

        # The structural difference: no shared arena, so each holds its own copy.
        buf = Buffer(cache_time=rclpy_duration(TF2_HISTORY_S + 5.0))
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
    # CPU is measured by the consumer over its own loop.
    cpu_t0 = cpu_ns(os.getpid())
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
        service.append((done - start) * 1e9)
        cycle.append((done - due) * 1e9)

    busy = (cpu_ns(os.getpid()) - cpu_t0) / ((time.perf_counter() - t0) * 1e9)
    service.sort()
    cycle.sort()
    print(
        f"RESULT {os.getpid()} "
        f"{pct(service, 0.5):.0f} {pct(service, 0.99):.0f} {pct(service, 0.999):.0f} "
        f"{pct(cycle, 0.5):.0f} {pct(cycle, 0.99):.0f} {pct(cycle, 0.999):.0f} "
        f"{fill_ms:.1f} {busy:.6f}",
        flush=True,
    )


def run_publisher() -> None:
    """Publish into the shared arena until killed."""
    import tf_tree

    tree = tf_tree.open(mode="rw", create=EDGES, capacity=4096, interp="lerpslerp")
    writers = [tree.publisher(child, parent) for parent, child in EDGES]
    print("READY", flush=True)
    period = 1.0 / PUB_HZ
    t0 = time.perf_counter()
    while True:
        # The tick index comes from elapsed time and is not also incremented (that ran
        # ~156 Hz, not `PUB_HZ`).
        now = time.perf_counter()
        i = int((now - t0) / period) + 1
        due = t0 + i * period
        if now < due:
            time.sleep(due - now)
        stamp = time.time_ns()
        for w in writers:
            w.push(stamp, [1.0, 0.0, 0.0, 0.0, 0.001 * i, 0.0, 0.0])


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
    time.sleep(1.5 if engine == "tf_tree" else 3.0)
    pss = sum(pss_kib(k.pid) for k in kids)

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

    return {
        "svc_p50": max(r[0] for r in rows),
        "svc_p999": max(r[2] for r in rows),
        "cyc_p999": max(r[5] for r in rows),
        "fill_ms": max(r[6] for r in rows),
        "pss_mib": pss / 1024.0,
        "cpu_pct": 100.0 * sum(r[7] for r in rows) / len(rows),
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
                # Reading READY synchronises: the arena exists and is published into.
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

        if len(sweep) >= 2:
            lo, hi = sweep[0], sweep[-1]
            dn = CONSUMERS[-1] - CONSUMERS[0]
            d_pss = (hi["pss_mib"] - lo["pss_mib"]) / dn
            d_cpu = (hi["cpu_pct"] * CONSUMERS[-1] - lo["cpu_pct"] * CONSUMERS[0]) / dn
            print(f"  marginal: {d_pss:.1f} MiB/node, {d_cpu:.2f}% cpu/node")
        print()


if __name__ == "__main__":
    main()
