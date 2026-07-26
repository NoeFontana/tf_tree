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

    @overload
    def at_into(self, stamps: int, out: object, /) -> None:
        """Evaluate one stamp into a caller-provided `(4, 4)` float64 array.

        **The allocation-free scalar path, for a control loop.** A node does one
        lookup per tick and cannot batch, so `at`'s per-call array allocation is
        paid every tick forever. Measured on a depth-3 chain, release build:
        `at` 224 ns against `at_into` **173 ns**, and nothing allocated.

        Allocate `out` once, outside the loop.
        """

    @overload
    def at_into(self, stamps: NDArray[np.int64], out: object, /) -> None:
        """Evaluate into a caller-provided `(N, 4, 4)` float64 array.

        Allocates nothing. `out` must be C-contiguous and exactly the right
        shape; it is validated completely *before* any element is written, so a
        rejected call leaves it untouched.

        Raises `BufferError` on a wrong shape, dtype or stride. Non-contiguous
        input is refused rather than silently copied — a silent copy would
        defeat the point of this method while appearing to work.

        `out` is typed `object` rather than `NDArray` because the device check
        below accepts anything and then refuses it by message. **Only
        `numpy.ndarray` is written to** (subclasses included); a `memoryview`,
        or a pinned torch or CuPy allocation, is refused whatever its layout.
        `PHASE3.md` §5.5 describes those as qualifying and that is **not
        implemented** — `np.asarray(...)` first.

        **Device memory is refused** with a message naming the fix: a CPU store
        to a `cudaMalloc` pointer is undefined, not slow.

        A genuine `numpy.ndarray` skips the device check, because its data
        pointer is host memory by construction; CuPy and torch arrays are not
        numpy subclasses, so they still pay for it. The probe is a Python method
        call — `__dlpack_device__()` — and running it on every numpy call cost
        ~120 ns of a ~173 ns lookup.
        """

    def adaptive(
        self,
        start_ns: int,
        end_ns: int,
        /,
        *,
        lin: float = ...,
        ang: float = ...,
    ) -> tuple[NDArray[np.int64], NDArray[np.float64]]:
        """Knots whose linear interpolation stays within `lin` m / `ang` rad.

        Returns `(stamps, poses)` of shapes `(K,)` and `(K, 4, 4)`, strictly
        increasing. LERP between adjacent knots on whatever device they live
        on; the reconstruction error is bounded by construction.
        """

    def latest(self) -> NDArray[np.float64]:
        """The most recent transform on this path, as `(4, 4)`."""

    def depth(self) -> int:
        """Folded depth of this path, in edges."""

class Publisher:
    """A claimed edge. Use as a context manager; the claim releases on exit."""

    def __enter__(self) -> Publisher: ...
    def __exit__(self, *args: object) -> bool: ...
    def release(self) -> None:
        """Drop the claim now, rather than at an unspecified finalization."""

    def push(self, stamp_ns: int, quat7: list[float], /) -> None:
        """Publish `[qw, qx, qy, qz, tx, ty, tz]` at `stamp_ns`."""

    def push_many(
        self, stamps: NDArray[np.int64], poses: NDArray[np.float64], /
    ) -> None:
        """Publish `(N,)` stamps and `(N, 7)` poses in one crossing."""

class Tree:
    """A transform tree. Obtain with `tf_tree.open()` or `tf_tree.build()`."""

    def plan(self, target: str, source: str, /) -> Plan:
        """Compile a path from `source` to `target`.

        Compile once and reuse: the path walk and per-edge metadata lookup
        happen here, not per sample.
        """

    def publisher(self, child: str, parent: str, /) -> Publisher:
        """Claim `child`'s edge. Argument order is **(child, parent)**."""

    def lookup(self, target: str, source: str, stamp_ns: int, /) -> NDArray[np.float64]:
        """One transform, without compiling a plan first.

        The plan is cached per *thread*. Prefer `tree.plan(...)` in a loop —
        this pays a cache probe per call and a compiled plan pays nothing.
        """

    def instance_uuid(self) -> str:
        """Which arena instance this is, as 32 hex characters.

        All-zero in-process. Two processes that resolved the same *name* can
        still hold different segments; this is what tells them apart.
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
    create: list[tuple[str, str]] | None = ...,
    capacity: int = ...,
) -> Tree:
    """Attach to a running arena. Exported as `tf_tree.open`.

    `mode="ro"` by default and `create=None`: a consumer must be incapable of
    corrupting a robot's tree (the MMU enforces it), and a notebook started
    before the robot must fail loudly rather than create an empty arena the
    real publisher then refuses to join.

    Pass `create=[(parent, child), ...]` — the same edge list `build` takes —
    to create the arena when it is absent. An arena is sized from its declared
    edges, so there is no way to create one without saying what is in it; that
    is why this is an edge list rather than a boolean. **It requires
    `mode="rw"`** and is refused otherwise, so a read-only consumer still
    cannot bring an arena into existence.
    """

def from_sec(seconds: float, /) -> int:
    """Nanoseconds from float seconds. Lossy above ~10^7 s — see `Plan.at`."""

def has_shared_memory() -> bool:
    """Whether this build can share a tree between processes."""
