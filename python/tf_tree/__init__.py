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

**A query carries a time domain, and it defaults to zero.** Edges are stamped
with the clock that produced them (``SYSTEM_DOMAIN``, ``SENSOR_DOMAIN``,
``SIM_DOMAIN``, ``STEADY_DOMAIN``, or an integer a driver declared for itself),
and a stamp from one clock cannot address an edge sampled on another. So a tree
under ``use_sim_time`` is read with ``tree.plan(target, source,
domain=tf_tree.SIM_DOMAIN)``; the default is ``SYSTEM_DOMAIN``, which is right
for a wall-clock arena and refused — loudly, at ``plan()`` — for any other. It
is *not* ``tf_tree.open(domain=...)``, which selects which arena to attach to.

**Identifying a build.** A benchmark number or a bug report has to say which
build produced it, and three values do that::

    tf_tree.__version__                     # the extension build, as a str
    tf_tree.arena_format_version()          # the header's set of fields
    f"0x{tf_tree.arena_layout_hash():08X}"  # the geometry, as tft prints it

They are three because they fail independently: the right version can still
refuse to attach, because the arena it was pointed at was written by a
different geometry. The last two are what every participant compares on attach.

``__version__`` is compiled in from ``crates/tf_tree_py/Cargo.toml``, and it is
*not* the canonical answer for the wheel: ``importlib.metadata.version`` is,
and it reads ``pyproject.toml``. ``tests/python/test_version.py`` asserts the
two agree, so a disagreement is a stale wheel or a half-applied bump rather
than two right answers — which is the attribution this whole trio exists to
get right.
"""

from ._core import (
    SENSOR_DOMAIN,
    SIM_DOMAIN,
    STEADY_DOMAIN,
    SYSTEM_DOMAIN,
    BufferError,
    DerivativesUnavailableError,
    DisconnectedError,
    ExtrapolationError,
    FrameNotDeclaredError,
    NoDataError,
    NoSegmentError,
    Plan,
    Publisher,
    TfTreeError,
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

# Its own statement, and the redundant alias is not a typo. `__version__` is
# deliberately absent from `__all__` (the note below it says why), so the alias
# is what marks it as re-exported — without it ruff reports F401 and a type
# checker treats the name as private to this module. ruff's isort keeps aliased
# imports in a separate statement from plain ones, which is why it is down here
# rather than inside the block above.
from ._core import (
    __version__ as __version__,
)

__all__ = [
    "SENSOR_DOMAIN",
    "SIM_DOMAIN",
    "STEADY_DOMAIN",
    "SYSTEM_DOMAIN",
    "BufferError",
    "DerivativesUnavailableError",
    "DisconnectedError",
    "ExtrapolationError",
    "FrameNotDeclaredError",
    "NoDataError",
    "NoSegmentError",
    "Plan",
    "Publisher",
    "TfTreeError",
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
    "open",
    "open_arena",
    "open_file",
    "push",
]

# `__version__` is deliberately absent from `__all__` above. It is not an
# oversight and not a style call: `tests/python/test_stubs.py` asserts `__all__`
# equals the package's public namespace, and it computes that namespace by
# skipping underscore-prefixed names — so listing the dunder here would make
# those two sets differ by exactly this name. `from tf_tree import *` binding a
# `__version__` is not something anyone wants either.

# `open` shadows the builtin inside this module only; the public spelling is
# `tf_tree.open()`, which is what §4.1 promises.
open = open_arena  # noqa: A001
