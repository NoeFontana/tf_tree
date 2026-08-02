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

import os
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

    def edges(self) -> list[tuple[str, str]]:
        """The **dynamic** edges this plan samples, as `(parent, child)` pairs.

        In fold order — the order the compositions happen, which is the order
        the plan is.

        Shorter than `depth()` when the path crosses a static edge. A static
        edge (or a whole run of them) is folded into one constant transform at
        compile time and its identity does not survive the fold, so a plan
        cannot list what it no longer knows. Use `Tree.edges()` for the
        topology; this is what *this path samples at evaluation time*.
        """

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

    def freeze(
        self, path: str | os.PathLike[str], /, *, source: str | None = ...
    ) -> None:
        """Write this tree to `path` as a frozen `.tft` (`PHASE5.md` §2.3).

        The file *is* the arena: `open_file` maps it back with no parse and no
        fixups, and the lookups it answers are bit-identical to this tree's.

        Replacing `path` is atomic — the bytes land in a sibling temporary and
        are renamed over it — so an interrupted freeze leaves the previous index
        intact instead of a half-written one under the name somebody will open
        next week.

        `source` is the recording these poses came from; it is recorded in the
        manifest as `null` when there is none. Linux only.

        The GIL is released for the copy, so a background freeze does not stall
        the threads servicing your progress bar or socket.
        """

    def span(self, target: str, source: str, /) -> tuple[int, int] | None:
        """The interval, in nanoseconds, over which `plan(target, source)` answers.

        `LatestCommon` generalised to a range: the *intersection* of every
        dynamic edge's retained window, so the lower end is a `max` and the
        upper end a `min`. It is the query to reach for when a lookup fails at
        a stamp, because the answer is nearly always "one edge on the path had
        not started yet".

        Three distinct answers:

        * `(t0, t1)` with `t0 <= t1` — answerable there, nowhere else.
        * `(t0, t1)` with `t0 > t1` — the windows do not overlap. That is a real
          answer, not an error: `t0 <= t <= t1` is correctly false everywhere.
        * `None` — every step on the path is static (or the path is empty), so
          the plan answers at *any* stamp and there is no finite interval.

        Raises `NoDataError`, naming the edge's two **frames**, when an edge on
        the path has no samples at all — which is a different situation from a
        non-overlapping window and calls for a different fix. Raises
        `TopologyChangedError` if the tree was re-parented under the call.

        On a live tree the answer is a snapshot that ages immediately, exactly
        as `Plan.latest` does.
        """

    def frames(self) -> list[str]:
        """The frame names on this tree, in declaration order.

        The cheap way to see what is in an arena without shelling out to
        `tf_tree doctor`. Frame identity is append-only, so a name that appears
        here will never be removed or renumbered — but on a *live* shared arena
        a peer process can add one under you, so treat the list as a snapshot,
        exactly as `Plan.latest` and `Tree.span` already are.

        A name longer than 48 bytes was truncated when it was interned; the
        stored form is what comes back.
        """

    def edges(self) -> list[tuple[str, str]]:
        """The edges on this tree, as `(parent, child)` name pairs.

        `(parent, child)` is the order `tf_tree.build` and `tf_tree.open(
        create=...)` take, so `tf_tree.build(tree.edges())` reconstructs this
        topology. It is deliberately *not* `Tree.publisher`'s `(child, parent)`
        order: an edge list silently reversed builds a tree that is upside down
        and still perfectly valid.

        **Names only** — no rate, no jitter, no gaps, no sample count. Those are
        `PHASE5.md` §4.2's `ds.edges()` and are held back until the counting
        pass that can answer them honestly exists: a ring knows what it
        *retained*, which is not what the publisher produced, and a rate derived
        from the one and reported as the other is worse than no rate at all.
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

def open_file(path: str | os.PathLike[str], /) -> Tree:
    """Open a frozen `.tft` and read it as an ordinary `Tree` (`PHASE5.md` §4.1).

    Opening is an `mmap`, so it costs microseconds and no parse — and it hands
    back the **same** `Tree` a live arena does. `plan`, `at`, `at_into`,
    `adaptive`, `latest` and `span` are the objects that were already there,
    with the same semantics and bit-identical results. There is no offline API
    to learn.

    The tree is permanently read-only: `is_writable()` is `False` and
    `publisher()` refuses, because the mapping is `PROT_READ` and a store
    through it would be a fault rather than an error.

    Raises `FileNotFoundError` (and its `OSError` siblings) for a path problem,
    and `TfTreeError` for a file that is not a readable `.tft` — a layout or
    format mismatch names both values and says to re-freeze, since a `.tft` is a
    cache and not an archive.

    **Dataloader pattern (§4.3).** Documented, not shipped: a
    `torch.utils.data.Dataset` subclass would bind this package to a framework
    version for no benefit, and the pattern is four lines::

        class Frames(Dataset):
            def __init__(self, path):
                self.path, self.ds = path, None

            def __getitem__(self, i):
                if self.ds is None:                    # per worker
                    self.ds = tf_tree.open_file(self.path)
                ...

    Open it **in the worker, not in the parent**. A `Tree` cannot be pickled,
    and a `DataLoader` with `num_workers > 0` sends the dataset object to its
    workers — by pickle under `spawn` and `forkserver`, which is CPython 3.14's
    default start method on Linux. The lazy `None` is what keeps the object
    picklable.

    Under a plain `fork` an inherited `.tft` mapping does keep working: it is
    `MAP_PRIVATE | PROT_READ` and is deliberately *not* poisoned at fork, unlike
    a shared-memory attach. So the rule is about picklability, not about the
    arena going away — §4.3 gives the fork-poisoning reason and that reason does
    not apply to a frozen file.

    Sixteen workers that each open the same file share one set of clean
    page-cache pages, so the marginal cost per worker is about zero. That is the
    entire argument for `.tft` (§2.2), and opening once in the parent and
    passing poses down instead gives it up.
    """

def from_sec(seconds: float, /) -> int:
    """Nanoseconds from float seconds. Lossy above ~10^7 s — see `Plan.at`."""

def has_shared_memory() -> bool:
    """Whether this build can share a tree between processes."""
