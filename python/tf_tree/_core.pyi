"""Type stubs for the `tf_tree` extension module.

Hand-written, not generated (`docs/PHASE3.md` §9). Generated stubs cannot
express the scalar-vs-array return overloads on `Plan.at`, and those overloads
are the most important thing a user needs to see: they are what makes the
vectorised path the obvious one.

The standing hazard with hand-written stubs is not that they are wrong on day
one — it is that a method added in Rust never reaches them. `tests/python/
test_stubs.py` closes that: it asserts every public symbol of the built module
appears here. Signatures are ours; *existence* is checkable, and that is the
half that rots.
"""

from typing import Literal, overload

import numpy as np
from numpy.typing import NDArray

class TfTreeError(Exception): ...
class ExtrapolationError(TfTreeError): ...
class DisconnectedError(TfTreeError): ...
class NoDataError(TfTreeError): ...
class TopologyChangedError(TfTreeError): ...
class FrameNotDeclaredError(TfTreeError): ...
class BufferError(TfTreeError): ...

class Plan:
    """A compiled lookup path. Build with `Tree.plan`."""

    @overload
    def at(self, stamps: int, /) -> NDArray[np.float64]:
        """One stamp in, a `(4, 4)` float64 matrix out."""

    @overload
    def at(self, stamps: NDArray[np.int64], /) -> NDArray[np.float64]:
        """`(N,)` stamps in, `(N, 4, 4)` out — the path to prefer.

        A Python loop over the scalar form costs ~200 ns per iteration; this
        amortises to near-native.
        """

    def at_into(self, stamps: NDArray[np.int64], out: NDArray[np.float64], /) -> None:
        """Evaluate into a caller-provided `(N, 4, 4)` float64 array.

        Allocates nothing. `out` must be C-contiguous and exactly the right
        shape; it is validated completely *before* any element is written, so a
        rejected call leaves it untouched.

        Raises `BufferError` on a wrong shape, dtype or stride. Non-contiguous
        input is refused rather than silently copied — a silent copy would
        defeat the point of this method while appearing to work.
        """

    def latest(self) -> NDArray[np.float64]:
        """The most recent transform on this path, as `(4, 4)`."""

    def depth(self) -> int:
        """Folded depth of this path, in edges."""

class Tree:
    """A transform tree. Obtain with `tf_tree.open()` or `tf_tree.build()`."""

    def plan(self, target: str, source: str, /) -> Plan:
        """Compile a path from `source` to `target`.

        Compile once and reuse: the path walk and per-edge metadata lookup
        happen here, not per sample.
        """

    def is_shared(self) -> bool:
        """Whether this tree's arena is shared with other processes."""

    def is_writable(self) -> bool:
        """Whether this process may publish into this tree."""

def build(edges: list[tuple[str, str]], *, capacity: int = ...) -> Tree:
    """An in-process tree from `(parent, child)` edges.

    Topology is builder-time (decision `0004`), so there is no `declare_*` on a
    live tree: the layout is a property of the arena, fixed when it is created.
    """

def push(
    tree: Tree, child: str, parent: str, stamp_ns: int, quat7: list[float], /
) -> None:
    """Publish `[qw, qx, qy, qz, tx, ty, tz]` onto an edge at `stamp_ns`.

    Takes the engine's own representation rather than a 4x4: a *nearly* rigid
    matrix — which is what arrives after any floating-point round trip — has no
    exact conversion back, only a projection.
    """

def open_arena(
    *,
    name: str | None = ...,
    domain: int | None = ...,
    mode: Literal["ro", "rw"] = ...,
) -> Tree:
    """Attach to a running arena. Exported as `tf_tree.open`.

    `mode="ro"` by default, and creation is refused: a consumer must be
    incapable of corrupting a robot's tree (the MMU enforces it), and a
    notebook started before the robot must fail loudly rather than create an
    empty arena the real publisher then refuses to join.
    """

def from_sec(seconds: float, /) -> int:
    """Nanoseconds from float seconds. Lossy above ~10^7 s — see `Plan.at`."""

def has_shared_memory() -> bool:
    """Whether this build can share a tree between processes."""
