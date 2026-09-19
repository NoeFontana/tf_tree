"""Type stubs for the `tf_tree` extension module.

Hand-written (`docs/PHASE3.md` §9); `tests/python/test_stubs.py` asserts every
public symbol of the built module appears here.
"""

import os
from typing import Literal, overload

import numpy as np
from numpy.typing import NDArray

class TfTreeError(Exception): ...

# Attributes exist only on instances the library raises (`docs/decisions/0058`);
# a caller-constructed instance raises `AttributeError` on a read. An id is never
# an integer: an edge is its stored `(parent, child)` names, a frame its stored
# name, each `None` where the arena holds no usable record.

class ExtrapolationError(TfTreeError):
    """The requested stamp lies outside an edge's retained history.

    `requested`, `oldest` and `newest` are integer nanoseconds on the clock
    `domain` names (the query's time-domain tag).
    """

    edge: tuple[str, str] | None
    requested: int
    oldest: int
    newest: int
    domain: int

class DisconnectedError(TfTreeError):
    """No path joins the two frames.

    Attributes: the target frame, the source frame, and where the chain stopped.
    """

    target: str | None
    source: str | None
    cut_at: str | None

class NoDataError(TfTreeError):
    """An edge on the path has no samples yet."""

    edge: tuple[str, str] | None

class TopologyChangedError(TfTreeError):
    """The tree was re-parented after this plan was compiled; call `plan` again.

    The error a correct program on a shared arena routinely meets.
    """

    plan_generation: int
    current_generation: int

class FrameNotDeclaredError(TfTreeError):
    """No such frame in this arena (`docs/PHASE3.md` §4.4)."""

    name: str | None

class BufferError(TfTreeError): ...

class TimeDomainMismatchError(TfTreeError):
    """A stamp's time domain is not the path's.

    Raised at plan time by `Tree.plan(..., domain=)` and per query by
    `Tree.lookup(..., domain=)` (`docs/PHASE3.md` §4.4).
    """

    expected: int
    got: int

class NonMonotonicStampError(TfTreeError):
    """A pushed stamp is older than the newest one already published on its edge.

    Raised by `Publisher.push`, `Publisher.push_many` and `tf_tree.push`; equal
    stamps are accepted. `last` and `got` are integer nanoseconds. `push_many`
    publishes the samples before the refused one; its message names the index.
    """

    edge: tuple[str, str] | None
    last: int
    got: int

class EdgeAlreadyClaimedError(TfTreeError):
    """Another publisher holds this edge's claim: one writer per edge.

    Raised by `Tree.publisher` and `tf_tree.push`. `owner_slot` is the holder's
    participant **slot**, not a pid (`tf_tree participants` lists slots,
    `tf_tree doctor` names the process); `None` while the claim is being taken
    or was abandoned mid-claim.
    """

    edge: tuple[str, str] | None
    owner_slot: int | None

class ArenaHeldButUnreachableError(TfTreeError):
    """Participants still hold an arena's lock bytes, but nothing serves it.

    Raised by `tf_tree.open` after its open timeout, typically when an owner
    died and no survivor has called `Tree.inherit_ownership`. Retry on
    `except (ArenaAbsentError, ArenaHeldButUnreachableError)`. Only a Linux
    `open` raises it. `holder_slots` is the held slots, ascending;
    `ownership_held` is whether the ownership byte was held at timeout.
    """

    holder_slots: tuple[int, ...]
    ownership_held: bool

class ArenaAbsentError(TfTreeError):
    """No arena is serving under this name, and `open` was not asked to create.

    Raised at once by `tf_tree.open` without `create=`. Carries no attributes.
    Only a Linux `open` raises it.
    """

class ChildProcessDetachedError(TfTreeError):
    """This handle was inherited across a `fork()` and cannot be used.

    The shared mapping is `MADV_DONTFORK`, so every call that reaches the arena
    raises this (`docs/PHASE3.md` §8.1). Not retryable: open a new tree in the
    child, or use the `"spawn"` or `"forkserver"` start method. A retry loop
    around other `TfTreeError`s should catch this first.
    """

class DerivativesUnavailableError(TfTreeError):
    """This edge's interpolator has no exact derivative.

    Raised only by `layout="quat_twist"` on a `LerpSlerp` edge. Declare the edge
    `ScLerp` (the default) or ask for a pose layout. A property of the edge, so
    it fires at element 0 of a batch.
    """

    edge: tuple[str, str] | None

class NoSegmentError(TfTreeError):
    """A pose exists at this stamp, but no segment to differentiate.

    Raised by `layout="quat_twist"` and **transient**: the edge retains one
    sample, or the samples bracketing the stamp carry equal stamps. Unlike
    `NoDataError`, the transform is defined. A property of the stamp, so it can
    fire partway through a batch.
    """

    edge: tuple[str, str] | None

F32Layout = Literal["affine32"]
"""The one layout that writes `float32`."""

F64Layout = Literal["mat4", "quat", "quat_twist"]
"""The layouts that write `float64`."""

Layout = Literal["mat4", "quat", "affine32", "quat_twist"]
"""How a transform is written into memory. Stated, never inferred.

`"mat4"`: `(4, 4)` / `(N, 4, 4)` float64. `"quat"`: `(7,)` / `(N, 7)` float64
`[qw qx qy qz tx ty tz]`. `"affine32"`: `(12,)` / `(N, 12)` **float32**, row-major
3x4. `"quat_twist"`: `(13,)` / `(N, 13)` float64, `"quat"` plus body twist
`[wx wy wz vx vy vz]`.
"""

ExtrapPolicy = Literal["error", "hold", "constant_twist"]
"""What `Plan.at_extrapolating` does past the newest published sample.

`"error"` refuses (what `at` does); `"hold"` returns the newest pose;
`"constant_twist"` extends the screw the two newest samples imply.
"""

class Plan:
    """A compiled lookup path. Build with `Tree.plan`."""

    @overload
    def at(self, stamps: int | np.int64, /) -> NDArray[np.float64]:
        """One stamp in, one `(4, 4)` float64 transform out."""

    @overload
    def at(self, stamps: NDArray[np.int64], /) -> NDArray[np.float64]:
        """`(N,)` stamps in, `(N, 4, 4)` float64 out: the path to prefer."""

    @overload
    def at(
        self, stamps: int | np.int64 | NDArray[np.int64], /, *, layout: F32Layout
    ) -> NDArray[np.float32]:
        """`layout="affine32"`: `(12,)` or `(N, 12)` **float32**, row-major 3x4."""

    @overload
    def at(
        self,
        stamps: int | np.int64 | NDArray[np.int64],
        /,
        *,
        layout: F64Layout | None = ...,
    ) -> NDArray[np.float64]:
        """`layout=` selects what is written per stamp (see `Layout`).

        `layout="quat_twist"` appends the body twist in the plan's **source**
        frame, angular part first; it alone can raise
        `DerivativesUnavailableError` or `NoSegmentError`.
        """

    @overload
    def at(
        self,
        stamps: int | np.int64 | NDArray[np.int64],
        /,
        *,
        layout: Layout | None = ...,
    ) -> NDArray[np.float64] | NDArray[np.float32]:
        """Fallback for a `layout` not statically known; returns a union."""

    @overload
    def at_into(
        self, stamps: int | np.int64, out: object, /, *, layout: Layout | None = ...
    ) -> None:
        """Evaluate one stamp into a caller-provided `(4, 4)` float64 array.

        The allocation-free scalar path for a control loop; allocate `out` once.
        """

    @overload
    def at_into(
        self, stamps: NDArray[np.int64], out: object, /, *, layout: Layout | None = ...
    ) -> None:
        """Evaluate into a caller-provided `(N, 4, 4)` float64 array.

        With `layout=`, `out` is `(N, layout_elems)` (`(layout_elems,)` for a
        scalar stamp), `float32` for `"affine32"`, else `float64`. Allocates
        nothing. `out` must be C-contiguous and exactly the right shape; it is
        validated before any element is written.

        Raises `BufferError` on a wrong shape, dtype or stride; non-contiguous
        input is refused, not copied. A bad `stamps` raises numpy's or PyO3's own
        conversion `TypeError`, as `at` does.

        `out` is typed `object` because device memory is accepted then refused
        by message (a CPU store to a `cudaMalloc` pointer is undefined). Only
        `numpy.ndarray` (subclasses included) is written to; a `memoryview` or
        pinned torch/CuPy allocation is refused (`PHASE3.md` §5.5 is not
        implemented) — call `np.asarray(...)` first.
        """

    def adaptive(
        self,
        start_ns: int | np.int64,
        end_ns: int | np.int64,
        /,
        *,
        lin: float = ...,
        ang: float = ...,
    ) -> tuple[NDArray[np.int64], NDArray[np.float64]]:
        """Knots whose linear interpolation stays within `lin` m / `ang` rad.

        Returns `(stamps, poses)` of shapes `(K,)` and `(K, 4, 4)`, strictly
        increasing.
        """

    @overload
    def at_extrapolating(
        self,
        stamps: int | np.int64,
        policy: ExtrapPolicy,
        /,
        *,
        layout: Layout | None = ...,
    ) -> tuple[NDArray[np.float64], int]:
        """One stamp in; `((4, 4)` float64, `by_ns` as an `int)` out."""

    @overload
    def at_extrapolating(
        self,
        stamps: NDArray[np.int64],
        policy: ExtrapPolicy,
        /,
        *,
        layout: Layout | None = ...,
    ) -> tuple[NDArray[np.float64], NDArray[np.int64]]:
        """`(N,)` stamps in; `((N, 4, 4)` float64, `(N,)` int64) out.

        `by_ns` is an array: `max(0, stamp - newest_common)` per stamp, so one
        batch can mix interpolated (`0`) and extrapolated elements.
        """

    @overload
    def at_extrapolating(
        self,
        stamps: int | np.int64 | NDArray[np.int64],
        policy: ExtrapPolicy,
        /,
        *,
        layout: Layout | None = ...,
    ) -> tuple[NDArray[np.float64], int | NDArray[np.int64]]:
        """Evaluate past the newest sample, and learn how far past that was.

        Returns `(poses, by_ns)`; there is no spelling that returns the pose
        alone. `policy` is required: `"error"` is `at` with the distance
        attached on success (raises `ExtrapolationError` past the newest sample),
        `"hold"` the newest pose, `"constant_twist"` the screw the two newest
        samples imply. `mat4` by default, `quat` with `layout=`; f32 and twist
        layouts are refused. The edge that ran out of data is not carried; see
        `Plan.edges()` and `tf_tree doctor`. A batch loops the scalar form.
        """

    def at_extrapolating_into(
        self,
        stamps: int | np.int64 | NDArray[np.int64],
        policy: ExtrapPolicy,
        poses: object,
        by_ns: object,
        /,
        *,
        layout: Layout | None = ...,
    ) -> None:
        """`at_extrapolating` writing into two caller-provided arrays.

        `poses` takes the shape the allocating form returns; `by_ns` is
        `()`-shaped or `(N,)` int64. Allocate both once. A failure part-way
        leaves the buffers part-written (as `at_into` does).
        """

    def latest(self) -> NDArray[np.float64]:
        """The most recent transform on this path, as `(4, 4)`."""

    def depth(self) -> int:
        """Folded depth of this path, in edges."""

    def edges(self) -> list[tuple[str, str]]:
        """The **dynamic** edges this plan samples, as `(parent, child)` pairs.

        In fold order. Shorter than `depth()` when the path crosses a static
        edge: static runs fold into one constant at compile time. Use
        `Tree.edges()` for the topology.

        Raises `ChildProcessDetachedError` on a tree inherited across a `fork()`.
        """

class Publisher:
    """A claimed edge. Use as a context manager; the claim releases on exit."""

    def __enter__(self) -> Publisher: ...
    def __exit__(self, *args: object) -> bool: ...
    def release(self) -> None:
        """Drop the claim now, rather than at an unspecified finalization."""

    def push(self, stamp_ns: int | np.int64, quat7: list[float], /) -> None:
        """Publish `[qw, qx, qy, qz, tx, ty, tz]` at `stamp_ns`."""

    def push_many(
        self, stamps: NDArray[np.int64], poses: NDArray[np.float64], /
    ) -> None:
        """Publish `(N,)` stamps and `(N, 7)` poses in one crossing."""

class Tree:
    """A transform tree. Obtain with `tf_tree.open()` or `tf_tree.build()`."""

    def plan(self, target: str, source: str, /, *, domain: int = ...) -> Plan:
        """Compile a path from `source` to `target`; compile once and reuse.

        `domain` is the time domain every query on the plan carries:
        `SYSTEM_DOMAIN` (default, `0`), `SENSOR_DOMAIN`, `SIM_DOMAIN`,
        `STEADY_DOMAIN`, or an integer from `4` up a driver declared. A
        mismatch with the path's own domain raises `TimeDomainMismatchError`
        here, naming both frames. Not `open(domain=...)`, which selects the
        arena.
        """

    def publisher(self, child: str, parent: str, /) -> Publisher:
        """Claim `child`'s edge. Argument order is **(child, parent)**."""

    def lookup(
        self,
        target: str,
        source: str,
        stamp_ns: int | np.int64,
        /,
        *,
        domain: int = ...,
    ) -> NDArray[np.float64]:
        """One transform, without compiling a plan first.

        The plan is cached per thread; prefer `tree.plan(...)` in a loop.
        `domain` is `plan`'s, checked per call.
        """

    def freeze(
        self, path: str | os.PathLike[str], /, *, source: str | None = ...
    ) -> None:
        """Write this tree to `path` as a frozen `.tft` (`PHASE5.md` §2.3).

        `open_file` maps it back with no parse, bit-identical. Replacing `path`
        is atomic. `source` is the recording these poses came from, recorded in
        the manifest (`null` if none). Linux only. Releases the GIL.
        """

    @property
    def source(self) -> dict[str, object] | None:
        """The recording this tree was ingested from, or `None`.

        `None` for a tree built in Python or opened with `open_file`, and again
        once `publisher()` has been called on it. Keys: `path`, `digest` (BLAKE3
        hex), `transforms`, `edges_without_samples`, `recording_start_ns`,
        `recording_end_ns`. These bound the recording, not what the tree can
        answer; use `span` to plan queries.
        """

    def span(self, target: str, source: str, /) -> tuple[int, int] | None:
        """The interval, in nanoseconds, over which `plan(target, source)` answers.

        The intersection of every dynamic edge's retained window:

        * `(t0, t1)` with `t0 <= t1` — answerable there, nowhere else.
        * `(t0, t1)` with `t0 > t1` — the windows do not overlap.
        * `None` — every step is static (or the path is empty); any stamp works.

        Raises `NoDataError` when an edge has no samples at all, and
        `TopologyChangedError` if the tree was re-parented under the call. A
        snapshot on a live tree.
        """

    def frames(self) -> list[str]:
        """The frame names on this tree, in declaration order.

        Append-only, but a snapshot on a live arena. Names over 48 bytes are
        returned truncated. May contain duplicates (a rescued intern leaves the
        same name at two ids), so `len()` is an upper bound.

        Raises `ChildProcessDetachedError` on a tree inherited across a `fork()`.
        """

    def edges(self) -> list[tuple[str, str]]:
        """The edges on this tree, as `(parent, child)` name pairs.

        This is the order `tf_tree.build` and `open(create=...)` take, not
        `Tree.publisher`'s `(child, parent)`. The list omits static-vs-dynamic
        and `build` cannot declare a static edge, so a round trip turns each
        static edge into a dynamic one with no samples. Names only: rate and
        counts are `PHASE5.md` §4.2's.

        Raises `ChildProcessDetachedError` on a tree inherited across a `fork()`.
        """

    def instance_uuid(self) -> str:
        """Which arena instance this is, as 32 hex characters (all-zero in-process).

        Raises `ChildProcessDetachedError` on a tree inherited across a
        `fork()`; `repr()` prints `detached-by-fork` instead.
        """

    def is_shared(self) -> bool:
        """Whether this tree's arena is shared with other processes."""

    def is_writable(self) -> bool:
        """Whether this process may publish into this tree."""

    def owner_lost(self) -> bool:
        """Has the process that owns this arena gone away (`PHASE2` §3.5)?

        Answers "the arena has no owner", not "my socket is dead": one
        non-blocking `poll` of the attach socket, plus one `F_OFD_GETLK` on the
        ownership byte once it reports a hangup. `False` for anything not a
        joined shared attachment, and always off Linux. **Nothing calls it for
        you**; an arena whose survivors never ask stays ownerless. A dying owner
        is seen at the end of its exit (`docs/decisions/0057`).

            if tree.owner_lost():
                tree.inherit_ownership()
        """

    def inherit_ownership(self) -> str:
        """Inherit the owner role from a departed owner and begin serving.

        Returns `"Inherited"`, `"OwnerAlive"`, `"Contended"`, `"ReadOnly"` or
        `"NotApplicable"`. Anything but `"Inherited"` means this process is not
        the owner; lookups are unaffected either way. `"OwnerAlive"` and
        `"Contended"` are not final while `owner_lost()` is `True`: call again.
        `"ReadOnly"`: a read-only mapping cannot write the participant table;
        open with `mode="rw"` to be able to inherit.

        Raises `TfTreeError` if the `fcntl` fails or the rendezvous socket cannot
        be bound; the arena is then left ownerless and another survivor can try.
        """

    def reap_dead(self) -> int:
        """Collect what dead participants left behind; how many were freed.

        Sums two sweeps: claim leases no live process holds, and participant
        records whose lock bytes the kernel released. Usually `0`: the owner's
        hangup callback already handles a killed publisher. Its real work is
        after a dead owner. **Dangerous** where a Rust component served an arena
        with `build_shared` and published by hand (out of contract, `0031`): such
        a participant holds no lock byte and this frees a *running* process's
        records. Safe from Python alone (`docs/RUNBOOK.md`, *ParticipantTableFull*).
        `0` for a read-only, in-process or rendezvous-less tree.
        """

def build(
    edges: list[tuple[str, str]] | str,
    *,
    capacity: int | None = ...,
    interp: Literal["sclerp", "lerpslerp"] | None = ...,
    frame_headroom: int = ...,
) -> Tree:
    """An in-process tree from `(parent, child)` edges, or from a topology config.

    `edges` is a list of `(parent, child)` pairs (all dynamic, sharing
    `capacity`) or a `str` of topology-config text, the only form that can
    declare a static edge, per-edge size, rate or domain. `capacity=` and
    `interp=` are refused beside a config. Topology is builder-time (`0004`).

    `interp` defaults to `"sclerp"`, the only policy with an exact derivative
    (`layout="quat_twist"`). `"lerpslerp"` is `tf2`-bit-compatible, not
    right-invariant, and `quat_twist` over it raises `DerivativesUnavailableError`.
    """

def push(
    tree: Tree,
    child: str,
    parent: str,
    stamp_ns: int | np.int64,
    quat7: list[float],
    /,
) -> None:
    """Publish `[qw, qx, qy, qz, tx, ty, tz]` onto an edge at `stamp_ns`.

    Takes the engine's representation, not a 4x4: a nearly rigid matrix has no
    exact conversion back.
    """

def open_arena(
    *,
    name: str | None = ...,
    domain: int | None = ...,
    mode: Literal["ro", "rw"] = ...,
    create: list[tuple[str, str]] | str | None = ...,
    capacity: int | None = ...,
    interp: Literal["sclerp", "lerpslerp"] | None = ...,
    frame_headroom: int = ...,
) -> Tree:
    """Attach to a running arena. Exported as `tf_tree.open`.

    `mode="ro"` and `create=None` by default, so a consumer cannot corrupt a
    robot's tree or create an empty arena the real publisher refuses to join.
    `create=` takes `build`'s `edges` (pairs or config text), **requires
    `mode="rw"`**, and creates the arena when absent. `capacity` and `interp`
    are `build`'s, refused beside a config; `interp` is validated even without
    `create`.

    `domain` is the **rendezvous** domain (which arena, like `$ROS_DOMAIN_ID`),
    not `Tree.plan`'s time-domain tag.
    """

def ingest_bag(
    path: str | os.PathLike[str],
    /,
    *,
    static_topics: list[str] | None = ...,
    tf_topics: list[str] | None = ...,
    tf_prefix: str | None = ...,
    max_memory_mb: int | None = ...,
    max_record_bytes: int | None = ...,
) -> Tree:
    """Read an MCAP recording into an in-memory `Tree` (`PHASE5.md` §3).

    Returns the same `Tree` `open_file` returns. It carries the recording in
    `source`, so `ingest_bag(p).freeze(out)` is the whole bag-to-index path.

    `static_topics` and `tf_topics` override the `/tf_static` and `/tf` defaults;
    `tf_prefix` prepends to every frame name; `max_memory_mb` bounds the second
    pass's sort buffers, not the arena; `max_record_bytes` raises the 256 MiB
    per-record ceiling. Also hashes the recording, one extra sequential pass.

    Raises `FileNotFoundError` (and `OSError` siblings) for a path problem, and
    `TfTreeError` for a file that is not a readable MCAP, including a `.db3`
    rosbag2 bag (the message names `ros2 bag convert`).
    """

def open_file(path: str | os.PathLike[str], /) -> Tree:
    """Open a frozen `.tft` and read it as an ordinary `Tree` (`PHASE5.md` §4.1).

    An `mmap`: microseconds, no parse, bit-identical results. Permanently
    read-only: `is_writable()` is `False` and `publisher()` refuses.

    Raises `FileNotFoundError` (and `OSError` siblings) for a path problem, and
    `TfTreeError` for an unreadable `.tft`; a layout or format mismatch names
    both values and says to re-freeze.

    **Dataloader pattern (§4.3).** Open in the worker, not the parent: a `Tree`
    cannot be pickled, and `spawn`/`forkserver` pickle the dataset::

        class Frames(Dataset):
            def __init__(self, path):
                self.path, self.ds = path, None

            def __getitem__(self, i):
                if self.ds is None:                    # per worker
                    self.ds = tf_tree.open_file(self.path)
                ...

    Workers opening the same file share clean page-cache pages. A `.tft`
    mapping is not poisoned by `fork`.
    """

def from_sec(seconds: float, /) -> int:
    """Nanoseconds from float seconds. Lossy above ~10^7 s.

    Prefer the exact converters: `from_parts` for a `(sec, nanosec)` pair and
    `from_ros` for a `builtin_interfaces/Time`. Neither takes a float.
    """

def from_parts(sec: int, nanosec: int, /) -> int:
    """Exact nanoseconds from a `(sec, nanosec)` pair.

    Raises `ValueError` for a `nanosec` outside `[0, 1e9)` (refused, not
    normalised) or a sum outside `int64` (refused, not wrapped).
    """

def from_ros(stamp: object, /) -> int:
    """Exact nanoseconds from a ROS 2 `builtin_interfaces/Time`.

    Duck-typed on `.sec` and `.nanosec`; `rclpy` is not a dependency. Refusals
    are `from_parts`'s.
    """

def has_shared_memory() -> bool:
    """Whether this build can share a tree between processes."""

SYSTEM_DOMAIN: int
"""Wall clock — `CLOCK_REALTIME`, ROS `/clock` off. Tag `0`, and the default.

Plain `int`s, not an enum: tags from `4` up belong to whoever declares them.
"""

SENSOR_DOMAIN: int
"""A sensor's own oscillator, undisciplined against the host. Tag `1`."""

SIM_DOMAIN: int
"""Simulated time — ROS `use_sim_time`, `/clock`. Tag `2`.

Reading such a tree needs `plan(..., domain=tf_tree.SIM_DOMAIN)`.
"""

STEADY_DOMAIN: int
"""A monotonic clock — `CLOCK_MONOTONIC`, boot-relative, never stepped. Tag `3`."""

__version__: str
"""This extension's version, compiled in from the crate manifest.

`importlib.metadata.version("transform_tree")` is canonical (reads `pyproject.toml`);
`tests/python/test_version.py` asserts the two agree.
"""

def arena_format_version() -> int:
    """This build's arena format version — the *set of fields* in the header.

    3 as of `PHASE5.md` §1. A different one is never compatible.
    """

def arena_layout_hash() -> int:
    """This build's arena layout hash — the *geometry*.

    Checked on attach beside `arena_format_version()`; a mismatch on either is
    refused. For a report write `f"0x{tf_tree.arena_layout_hash():08X}"`, which
    matches `tft doctor --explain-version`.
    """
