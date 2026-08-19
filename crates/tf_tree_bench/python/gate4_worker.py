"""One gate-4 worker, in Python: map the `.tft`, read all of it, report Pss.

`docs/PHASE5.md` §12 gate 4 asks whether 16 workers sharing one `.tft` cost
more than 1.2x what one worker costs. `frozen_workers.rs` answers it with a
*Rust* worker, and the criterion `(S + 16p)/(S + p) <= 1.2` rearranges to
`S >= 74p` — so the verdict is arithmetic about `p`, the private bytes one
worker costs, and `p` is a property of the worker's language and start method
rather than of tf_tree. A spawned CPython worker's `p` is large enough that the
gate's own 338 MiB file fails the gate's own criterion — §12 gate 4's amendment
carries the figures, and this script's whole job is to regenerate them, so it
deliberately does not restate them here.

This script is that second arm, and it exists because the Python reading had no
recipe while the Rust one did — the shape `docs/benchmarks/EVIDENCE.md` was
created to stop. It is driven by `frozen_workers --python`, speaks the same three-line
protocol its Rust sibling speaks, and is **spawned, never forked**, because
`spawn`/`forkserver` is what a `DataLoader` does on CPython 3.14 and Linux
(§4.3's amendment, and `open_file`'s docstring). The forked arm in the
amendment's table is a third measurement and is not what the wedge's audience
runs.

**The sweep is the Rust one's, deliberately** — same stamp grid, passed in by
the driver rather than restated here, same edge order, same batch entry point.
A sweep of its own would make the ratio a comparison of two query sets.

**Every import is charged to `p`**, which is the quantity under measurement, so
the argument parsing below is by hand and the imports are the two the work
needs. numpy is one of them: `at_into` writes into a caller-supplied array, and
numpy is what the amendment measured.

Not run by hand — `frozen_workers` starts it and holds the barrier.
"""

from __future__ import annotations

import sys

import numpy as np
import tf_tree


def pss_kib() -> int:
    """Proportional set size of this process, in KiB, from `smaps_rollup`.

    Each shared page divided by the number of processes currently mapping it,
    so summing it over live workers gives total unique resident bytes — the
    only accounting under which "16 workers share one arena" means anything.

    Loud when it cannot be read, where `mp::self_pss_kib` returns 0: the driver
    sums these, and a zero would enter that sum as a measurement of a worker
    that costs nothing. §9.3's memory axis refuses a row on an unreadable
    `smaps_rollup` for the same reason.
    """
    with open("/proc/self/smaps_rollup") as rollup:
        for line in rollup:
            if line.startswith("Pss:"):
                return int(line.split()[1])
    raise SystemExit("no Pss line in /proc/self/smaps_rollup")


def sweep(tree: tf_tree.Tree, stamps: int, window_ns: int) -> int:
    """Read across the whole tree, and return how many lookups answered.

    `open_file` is an `mmap`, so a worker that maps and never reads holds
    almost none of the file resident and the ratio collapses to `16p/p`. The
    count is printed by the driver so a run that answered nothing cannot be
    mistaken for a run that shared everything.
    """
    step = max(window_ns // max(stamps, 1), 1)
    grid = np.arange(stamps, dtype=np.int64) * step
    batch = np.zeros((stamps, 4, 4), dtype=np.float64)
    single = np.zeros((4, 4), dtype=np.float64)
    ok = 0
    for parent, child in tree.edges():
        try:
            plan = tree.plan(child, parent)
        except tf_tree.TfTreeError:
            continue
        try:
            plan.at_into(grid, batch)
        except tf_tree.TfTreeError:
            # A batch is all-or-nothing and the Rust sweep counts per stamp, so
            # an edge whose window does not cover the whole grid falls back to
            # the scalar path. Without it the two arms report different lookup
            # counts for the same fixture, and the column exists to show that
            # they read the same thing.
            for stamp in grid.tolist():
                try:
                    plan.at_into(stamp, single)
                except tf_tree.TfTreeError:
                    continue
                ok += 1
        else:
            ok += stamps
    return ok


def _value(rest: list[str], flag: str) -> str:
    if not rest:
        raise SystemExit(f"{flag} wants a value")
    return rest.pop(0)


def main() -> None:
    args = sys.argv[1:]
    if not args:
        raise SystemExit(
            "usage: gate4_worker.py <path.tft> "
            "[--stamps N] [--window-ns N] [--no-touch]"
        )
    path, rest = args[0], args[1:]
    stamps, window_ns, touch = 64, 40_000_000_000, True
    while rest:
        flag = rest.pop(0)
        if flag == "--stamps":
            stamps = int(_value(rest, flag))
        elif flag == "--window-ns":
            window_ns = int(_value(rest, flag))
        elif flag == "--no-touch":
            touch = False
        else:
            raise SystemExit(f"unknown argument {flag!r}")

    tree = tf_tree.open_file(path)
    read = sweep(tree, stamps, window_ns) if touch else 0

    # Two phases, and the barrier between them is not optional: Pss divides a
    # shared page by the number of processes *currently* mapping it, so a
    # worker that reports as soon as it has swept is divided by three rather
    # than by sixteen. `frozen_workers.rs`'s `worker` carries the measurement
    # that found it.
    print(f"ready {read}", flush=True)
    if not sys.stdin.readline():
        raise SystemExit("the driver closed stdin before releasing the barrier")
    print(f"pss {pss_kib()}", flush=True)

    # Hold the mapping open until the driver has heard from every peer.
    sys.stdin.read()
    del tree


if __name__ == "__main__":
    main()
