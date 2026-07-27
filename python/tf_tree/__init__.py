"""tf_tree — a transform tree engine.

A faster, more scalable alternative to ROS ``tf2``.

Two things surprise people, and both are deliberate:

**Stamps are integer nanoseconds.** There is no float-seconds overload. At a
2026 epoch the ULP of ``float64`` seconds is 238 ns, so *every* interval in a
1 kHz stream is wrong after a round trip. :func:`from_sec` exists for callers
who genuinely have float seconds and accept the loss.

**Nothing returns a view into shared memory.** An edge's samples are a ring
being overwritten by another process, and correct reads go through a seqlock;
an array pointing into it would be a data race by construction. "Zero-copy"
here means no *intermediate* allocation — results are written once, into their
final home. Use :meth:`Plan.at_into` to supply that home yourself.
"""

from ._core import (
    BufferError,
    DisconnectedError,
    ExtrapolationError,
    FrameNotDeclaredError,
    NoDataError,
    Plan,
    TfTreeError,
    TopologyChangedError,
    Tree,
    build,
    from_sec,
    has_shared_memory,
    open_arena,
    open_file,
    push,
)

__all__ = [
    "BufferError",
    "DisconnectedError",
    "ExtrapolationError",
    "FrameNotDeclaredError",
    "NoDataError",
    "Plan",
    "TfTreeError",
    "TopologyChangedError",
    "Tree",
    "build",
    "from_sec",
    "has_shared_memory",
    "open",
    "open_arena",
    "open_file",
    "push",
]

# `open` shadows the builtin inside this module only; the public spelling is
# `tf_tree.open()`, which is what §4.1 promises.
open = open_arena  # noqa: A001
