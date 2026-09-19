"""tf_tree — a transform tree engine, a faster and more scalable alternative to ROS ``tf2``.

**Stamps are integer nanoseconds.** There is no float-seconds overload (``float64``
seconds cannot represent 1 kHz intervals at a 2026 epoch). :func:`from_sec` is
the lossy escape hatch.

**Nothing returns a view into shared memory.** An edge's samples are a ring
overwritten by another process; results are copied once into their final home.
Use :meth:`Plan.at_into` to supply that home yourself.

**A query carries a time domain, default ``SYSTEM_DOMAIN``.** A stamp from one
clock cannot address an edge sampled on another, and a mismatch is refused at
``plan()``. Read a ``use_sim_time`` tree with
``tree.plan(target, source, domain=tf_tree.SIM_DOMAIN)``. This is not
``tf_tree.open(domain=...)``, which selects the arena.

**Identifying a build.** ``tf_tree.__version__`` (extension build),
``tf_tree.arena_format_version()`` (header fields) and
``tf_tree.arena_layout_hash()`` (geometry) fail independently.
``importlib.metadata.version`` is the canonical wheel version;
``tests/python/test_version.py`` asserts the two agree.
"""

from ._core import (
    SENSOR_DOMAIN,
    SIM_DOMAIN,
    STEADY_DOMAIN,
    SYSTEM_DOMAIN,
    ArenaAbsentError,
    ArenaHeldButUnreachableError,
    ChildProcessDetachedError,
    DerivativesUnavailableError,
    DisconnectedError,
    EdgeAlreadyClaimedError,
    ExtrapolationError,
    FrameNotDeclaredError,
    NoDataError,
    NonMonotonicStampError,
    NoSegmentError,
    Plan,
    Publisher,
    TfTreeError,
    TimeDomainMismatchError,
    TopologyChangedError,
    Tree,
    arena_format_version,
    arena_layout_hash,
    build,
    from_parts,
    from_ros,
    from_sec,
    has_shared_memory,
    ingest_bag,
    open_arena,
    open_file,
    push,
)

# `BufferError` is public but kept out of `__all__` so a star-import does not
# shadow the builtin. The redundant alias marks a re-export (F401,
# `reportPrivateImportUsage`).
from ._core import (
    BufferError as BufferError,
)

# Separate statement: ruff isort keeps aliased imports apart.
from ._core import (
    __version__ as __version__,
)

__all__ = [
    "SENSOR_DOMAIN",
    "SIM_DOMAIN",
    "STEADY_DOMAIN",
    "SYSTEM_DOMAIN",
    "ArenaAbsentError",
    "ArenaHeldButUnreachableError",
    "ChildProcessDetachedError",
    "DerivativesUnavailableError",
    "DisconnectedError",
    "EdgeAlreadyClaimedError",
    "ExtrapolationError",
    "FrameNotDeclaredError",
    "NoDataError",
    "NonMonotonicStampError",
    "NoSegmentError",
    "Plan",
    "Publisher",
    "TfTreeError",
    "TimeDomainMismatchError",
    "TopologyChangedError",
    "Tree",
    "arena_format_version",
    "arena_layout_hash",
    "build",
    "from_parts",
    "from_ros",
    "from_sec",
    "has_shared_memory",
    "ingest_bag",
    "open_arena",
    "open_file",
    "push",
]

# `__version__` is absent from `__all__`: `tests/python/test_stubs.py` compares
# `__all__` to the public namespace, which skips underscore names.

# `open` is `open_arena` under the spelling §4.1 promises. It is absent from
# `__all__` so a star-import does not rebind the builtin; it shadows it only in
# this module.
open = open_arena  # noqa: A001
