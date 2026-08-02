//! The offline API — `docs/PHASE5.md` §4.
//!
//! # There is no offline API
//!
//! That is the point of §4.1, which is NORMATIVE: a `.tft` opens into the
//! **same** [`PyTree`](crate::PyTree) a live arena does, so `plan`, `at`,
//! `at_into`, `adaptive` and `latest` are the objects that were already there.
//! What this module adds is a way in ([`open_file`]), a way out
//! ([`freeze_impl`], behind `Tree.freeze`), and the one query in §4.2 that
//! cannot be phrased in terms of the online API ([`span_impl`]).
//!
//! # What of §4.2 is here, and what deliberately is not
//!
//! §4.2 lists five helpers. `span` is in the module and so, since §4.4, is the
//! *names* half of `edges`; the omissions are decisions rather than a backlog:
//!
//! * `resample(t0, t1, hz)` is `plan.at(np.arange(t0, t1, 10**9 // hz))` — one
//!   line of NumPy over the vectorised call §4.1 insists is the same one. A
//!   binding for it would be a second spelling of an existing path, which is
//!   exactly what §4.1 forbids.
//! * **`edges()` is two different queries and only one of them ships.** §4.4's
//!   `tree.edges()` is the *identities* of the edges — a list of name pairs,
//!   and [`edges_impl`] below. §4.2's `ds.edges()` promises per-edge rate,
//!   jitter, gaps and count, and that half still needs §3's counting pass: the
//!   ring knows what it *retained*, which is not what the source produced (see
//!   `tf_tree::Tree::manifest`'s amendment), and dividing the one by the other
//!   is the 4-kHz-off-a-1-kHz-edge error §2.3's `samples`/`pushes_total`
//!   amendment already had to correct once. **The names must not acquire the
//!   statistics by adjacency**, which is §4.4's own instruction and the reason
//!   the two are named apart here rather than left to look like one feature
//!   half-built.
//! * `gaps()` needs the same counting pass, and `manifest` needs a CBOR
//!   *reader* where the crate has only a writer.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use pyo3::prelude::*;

use tf_tree::unstable::ArenaView;
use tf_tree::{EdgeId, FrameId, LookupError, Plan, Step, Tree};

use crate::errors::{detached_err, lookup_err, NoDataError, TfTreeError};
use crate::tree::PyTree;

/// Open a frozen `.tft` and read it through the ordinary `Tree` (§4.1).
///
/// Opening is an `mmap`: microseconds, and no parse. Sixteen dataloader workers
/// that each open the same file share one set of clean page-cache pages, which
/// is the whole argument of §2.2 — so **open it inside the worker**, not once in
/// the parent.
///
/// A `Tree` cannot be pickled, and a `DataLoader` with `num_workers > 0` sends
/// the dataset object to its workers by pickle under `spawn` *and* under
/// `forkserver`, which is CPython 3.14's default start method on Linux. So the
/// §4.3 pattern holds a `None` until the first `__getitem__`. Under a plain
/// `fork` an inherited mapping does keep working — it is `MAP_PRIVATE |
/// PROT_READ` and deliberately not fork-poisoned, unlike a shared-memory attach
/// — which is why the rule is about picklability and not, as §4.3 says, about
/// the arena going away.
///
/// **This text is duplicated in `_core.pyi`, on purpose.** The stub is what an
/// IDE shows; this is what `help(tf_tree.open_file)` shows at a REPL, and the
/// reader in front of a dataloader that has just deadlocked is at the REPL.
///
/// `path` is any `os.PathLike`, not only a `str`: this and `Tree.freeze` are the
/// binding's first filesystem-path arguments, so there is no earlier spelling to
/// stay consistent with, and a dataloader is precisely where paths arrive as
/// `pathlib.Path`. PyO3's `PathBuf` extractor also accepts the non-UTF-8 paths a
/// `&str` parameter cannot represent at all.
#[pyfunction]
#[pyo3(signature = (path, /))]
pub fn open_file(path: PathBuf) -> PyResult<PyTree> {
    Ok(PyTree {
        inner: std::sync::Arc::new(open_frozen(&path)?),
    })
}

/// The interval over which a plan is answerable, or `None` when it is unbounded.
///
/// The arithmetic is [`tf_tree::Plan::span`] and **this is a forwarder**, which
/// is the whole point: an earlier revision re-derived the window intersection
/// here, in the one crate the workspace's `just test`, `just miri` and
/// `just loom` never build. `tf_tree/src/frozen.rs` makes the same argument
/// about the same arithmetic — the definition of a ring's readable window has
/// already changed once, and a private copy of it does not move when the
/// definition does. Read that method for the three answers `span` distinguishes
/// and why an empty intersection is returned rather than raised.
///
/// Going through `Plan` also buys the two checks a hand-rolled view walk did not
/// have: a plan compiled against an older topology raises
/// `TopologyChangedError`, and a fork-poisoned guard raises rather than reading
/// an arena the child has been detached from.
///
/// # Errors
///
/// Everything `Plan::span` reports, mapped by [`lookup_err`] — except
/// `LookupError::NoData`, which is re-raised **naming the two frames** rather
/// than an `EdgeId`. §4.2's premise is that this query answers "why did my
/// lookup fail at t", and `EdgeId(2)` does not: the Python surface exposes no
/// way to turn an edge id back into the names the caller typed.
pub(crate) fn span_impl(tree: &Tree, target: &str, source: &str) -> PyResult<Option<(i64, i64)>> {
    let t = tree.frame(target).map_err(|_| {
        crate::errors::FrameNotDeclaredError::new_err(format!("no frame named {target:?}"))
    })?;
    let s = tree.frame(source).map_err(|_| {
        crate::errors::FrameNotDeclaredError::new_err(format!("no frame named {source:?}"))
    })?;
    let plan = tree.plan(t, s).map_err(lookup_err)?;
    plan.span(&tree.guard()).map_err(|e| match e {
        LookupError::NoData { edge } => match named_edge(tree, edge) {
            Some((parent, child)) => NoDataError::new_err(format!(
                "edge {parent:?} -> {child:?} on the path from {source:?} to \
                 {target:?} has no samples, so the path is not answerable at any \
                 stamp"
            )),
            None => lookup_err(e),
        },
        other => lookup_err(other),
    })
}

/// `(parent, child)` frame names for an edge, or `None` if either is missing.
///
/// Only ever called on an error path, so the two `String`s it allocates are not
/// a hot-path allocation — the rule they would otherwise violate.
fn named_edge(tree: &Tree, edge: EdgeId) -> Option<(String, String)> {
    named_edge_in(&tree.arena_view(), edge)
}

/// [`named_edge`] against a view the caller already holds.
///
/// The enumerators below call this per edge, and building an `ArenaView` per
/// iteration to throw it away would be the kind of loop that reads as free
/// because each step is cheap.
fn named_edge_in(view: &ArenaView<'_>, edge: EdgeId) -> Option<(String, String)> {
    // One observation of the record: re-reading `view.edge(edge)` for the child
    // could name a parent and a child that never belonged to the same edge.
    let rec = view.edge(edge)?;
    let name = |raw: u32| -> Option<String> {
        let r = view.frame_record(FrameId::new(raw)?)?;
        Some(stored_name(&r.name, r.name_len))
    };
    Some((name(rec.parent)?, name(rec.child)?))
}

/// A frame record's stored — and therefore possibly truncated — name.
///
/// `FrameRecord` keeps 48 bytes and a length; a longer name was cut at intern
/// time and the cut is not recoverable here. `from_utf8_lossy` rather than a
/// refusal because a truncation can land mid-codepoint, and a frame listing
/// that raises on one bad byte tells the caller nothing about the other ninety
/// frames.
fn stored_name(bytes: &[u8], len: u8) -> String {
    let n = (len as usize).min(bytes.len());
    String::from_utf8_lossy(&bytes[..n]).into_owned()
}

/// The frame names on this tree, in `FrameId` order, behind `Tree.frames`.
///
/// # Why this walks the arena when [`span_impl`] refuses to
///
/// `span` is a forwarder because the thing it forwards to is *arithmetic* — the
/// retained-window intersection — whose definition has already changed once,
/// and a private copy of it in the one crate `just test`, `just miri` and
/// `just loom` never build does not move when the definition does
/// (`docs/PHASE5.md` §4.2's amendment). There is no arithmetic here: the frame
/// table is append-only and the enumeration is `1..=frame_count`, three lines
/// `tf_tree doctor`'s `Snapshot::capture` and `tf_tree_c`'s unstable
/// enumerators already state independently — though not identically, which is
/// the next paragraph.
///
/// **That is a reason it is tolerable here, not a reason it is right here.**
/// The facade has no public `Tree::frames`, and the amendment's argument says a
/// third copy should have become the first shared one. Adding it is a change to
/// `crates/tf_tree/src/tree.rs`, which is out of this change's scope; when the
/// facade grows the method, this becomes a forwarder like `span_impl`.
///
/// **The four copies do not agree, which is the argument, not a footnote.**
/// `tf_tree_c::unstable` (`tft_tree_frame_name`) checks `FrameId::new`,
/// `id <= frame_count` and `name_hash != 0`, and loads the count `Acquire`;
/// `tf_tree_cli`'s `Snapshot::capture` checks only `FrameId::new` and loads
/// `Relaxed`, so `tf_tree doctor` can print a zeroed headroom slot as a frame
/// with an empty name; this one applies all three and loads `Relaxed` for the
/// reason stated at the load. Two of those three crates belong to other
/// branches, so this copy is made the correct one and the consolidation onto
/// `Tree` is filed rather than smuggled in here.
///
/// # The snapshot is a snapshot
///
/// On a live shared arena another process may intern a frame while this loop
/// runs, so the list is what was true at some instant inside the call — exactly
/// as `Plan::latest` and `Tree::span` already are. Frames are append-only, so
/// what the list *does* promise is that nothing in it will ever be removed or
/// renumbered.
///
/// **It does *not* promise that a name appears once, and append-only is not
/// what rules that out.** When a rescuer judges a stalled interner dead and
/// publishes the same name first, the loser's id is abandoned:
/// `tf_tree_core::frame`'s `finish` states that the record "stays written but
/// unreferenced, and `frame_count` over-counts by one" — the deliberate trade,
/// because giving the id back could alias two frames onto one record. That
/// abandoned record was written by `FrameRecord::for_name`, so it carries the
/// real name and a non-zero `name_hash`; it passes all three checks below and
/// lands in this list at a second id. So `len()` of the result is an upper
/// bound on the tree's frames and `dict(zip(frames, ...))` can silently drop an
/// entry. It takes the A8 liveness-rescue path — a claimant that stalled long
/// enough to be judged dead and then published anyway — so it is rare, not
/// impossible, and it is stated here because the section above would otherwise
/// read as exhaustive.
///
/// A tree inherited across a `fork()` has no snapshot to take: its mapping is
/// gone (`MADV_DONTFORK`) and [`Tree::view`](tf_tree::Tree) substitutes a
/// one-frame poison arena, which would make this answer `[]`. See the guard.
///
/// # Errors
///
/// [`detached_err`] on a tree inherited across a `fork()`.
pub(crate) fn frames_impl(tree: &Tree) -> PyResult<Vec<String>> {
    // **Refuse a fork-detached tree rather than describing the poison arena.**
    // `Tree::view` swaps in a one-frame, zero-edge heap arena for a detached
    // tree so that no accessor reads the vanished mapping; every count below
    // then reads 0 and this would hand a `multiprocessing` worker `[]` — which
    // reads as an empty or corrupt arena, not as the fork it is. `span_impl`
    // gets this for free by going through a `Guard`; a walk of the view has to
    // say so itself. `docs/PHASE5.md` §4.3 makes `fork` the *expected* way in.
    if tree.detached() {
        return Err(detached_err());
    }
    let view = tree.arena_view();
    // Usable frame ids are `1..=frame_count`; slot 0 is the root sentinel.
    //
    // **`Relaxed`, and that is the justified ordering, not the cheap one.**
    // `tf_tree_core::frame`'s `finish` does `frame_count.fetch_add`, *then*
    // `write_record`, then the Release publish into the intern table. An
    // `Acquire` load here would therefore synchronize with everything the
    // interner did *before* it took its id and with nothing it did after —
    // which is precisely the record we are about to read. Acquire would buy
    // ordering that reads like a guarantee and is not one; the `name_hash`
    // filter below is the actual guard. (The other three copies disagree about
    // this; see the doc comment's second section.)
    let count = view.header().frame_count.load(Ordering::Relaxed);
    let mut out = Vec::with_capacity(count as usize);
    for raw in 1..=count {
        // Three checks, the strictest set any of the four copies of this loop
        // applies (`tf_tree_c::unstable::tft_tree_frame_name` states them as
        // one chain; `tf_tree_cli`'s `Snapshot::capture` applies only the
        // first):
        //
        //  1. `FrameId::new` rejects 0, the root sentinel.
        //  2. `id <= frame_count` — *this loop's bound*, and load-bearing:
        //     `frame_record` bounds against `max_frames`, which is
        //     `frame_count + 1 + frame_headroom`, so an unbounded walk hands
        //     back zeroed headroom slots as if they were frames.
        //  3. `name_hash != 0`, below.
        let Some(id) = FrameId::new(raw) else {
            continue;
        };
        let Some(rec) = view.frame_record(id) else {
            continue;
        };
        // **`frame_count` is bumped *before* the record is written**, so a
        // concurrent interner in another process can be counted here one
        // instant before its name exists, and the slot still reads as zeros. A
        // written record's `name_hash` is BLAKE3 of the name — non-zero for
        // every name including `""`, which hashes to `0xa6a1f9f5b94913af`; a
        // zeroed one is zero always. Skipping it reports that frame one call
        // later, where taking it would report it as `""` — a name no caller can
        // act on and one that looks like our bug rather than like a race they
        // lost by a microsecond.
        //
        // This is a filter, not a synchronization edge: the arena's model is
        // that a record is written before its id is ever *published* and a
        // shared read of a published record races nothing
        // (`ArenaView::frame_record`'s SAFETY note). Enumerating by index steps
        // outside that model — the id came from a counter, not from a publish —
        // and no ordering available here puts it back inside. That is an
        // argument for the enumeration living on `Tree`, where `just loom` and
        // `just miri` can see it, which is the filed follow-up.
        if rec.name_hash == 0 {
            continue;
        }
        out.push(stored_name(&rec.name, rec.name_len));
    }
    Ok(out)
}

/// The edges on this tree as `(parent, child)` name pairs, behind `Tree.edges`.
///
/// # `(parent, child)`, in that order, because `build` takes that order
///
/// `tf_tree.build([...])` and `tf_tree.open(create=[...])` both take
/// `(parent, child)` pairs, so a caller can hand this list straight back to
/// either. Choosing `(child, parent)` — `Tree.publisher`'s order — would have
/// made that hand-back build a tree that is upside down and still valid, which
/// is the quaternion-order trap in the topology axis.
///
/// # It is the parent/child graph, and **not** a round trip
///
/// An earlier revision of this doc said `tf_tree.build(tree.edges())`
/// "reconstructs the topology". It reconstructs the *graph*, and only for an
/// all-dynamic tree it reconstructs anything usable: `tf_tree.build` has no way
/// to declare a static edge and this list does not report an edge's kind, so on
/// the surfaces this call is actually aimed at — a `.tft` from bag ingest, or a
/// shared arena a Rust or C peer built with `TreeBuilder::static_edge` — every
/// static edge comes back as a dynamic edge with an empty ring, and every lookup
/// crossing one raises `NoData` instead of returning the constant it had.
///
/// Reporting the kind is surface `docs/PHASE5.md` §4.4 does not authorise, and
/// declaring a static edge from Python is surface that does not exist at all, so
/// the promise is withdrawn rather than half-kept. A documented limit beats a
/// round trip that holds only on the case a test can reach.
///
/// # The pair is the edge's *declared* endpoints
///
/// `Tree::reparent` moves a child under a new parent by rewriting the topology
/// block; `EdgeRecord::parent`, which is what this reads, keeps the frame the
/// edge was declared under. The two agree on every tree that was never
/// reparented, which is every tree Python can build — the binding exposes no
/// `reparent` — and they can disagree on a shared arena a peer process has
/// reparented. This reads the record because [`named_edge`], `tf_tree doctor`'s
/// `Snapshot` and the CLI's edge listing all already do: one wrong-after-reparent
/// answer beats two answers that disagree with each other.
///
/// # Names only — see the module docs
///
/// No rate, no jitter, no gap count, no sample count. That is §4.2's `ds.edges()`
/// and it stays held back until §3's counting pass exists.
///
/// # Errors
///
/// [`detached_err`] on a tree inherited across a `fork()`.
pub(crate) fn edges_impl(tree: &Tree) -> PyResult<Vec<(String, String)>> {
    // See [`frames_impl`]: the poison arena a detached tree reads has zero
    // edges, so without this the answer is a silent `[]`.
    if tree.detached() {
        return Err(detached_err());
    }
    let view = tree.arena_view();
    // `edge_count` is stored as (declared edges + 1 sentinel), so the real ids
    // are `1..edge_count` — `tf_tree_core::EdgeId`'s own doc comment, and the
    // off-by-one that cost `tf_tree_c::unstable` a test.
    //
    // `Relaxed` needs no argument beyond `frames_impl`'s: unlike `frame_count`
    // there is no window at all here. The edge table is sized and filled by
    // `TreeBuilder`, and `edge_count` is stored exactly once
    // (`tf_tree/src/tree.rs`) before the arena is ever shared; nothing declares
    // an edge at runtime.
    let count = view.header().edge_count.load(Ordering::Relaxed);
    let mut out = Vec::with_capacity(count.saturating_sub(1) as usize);
    for raw in 1..count {
        // `None` here means either an id past the edge table — which
        // `edge_count <= max_edges` makes unreachable — or a slot whose record
        // is still zeros, because a zeroed record names frame 0 and
        // `FrameId::new(0)` declines. **That second case is what keeps the
        // sentinel and any headroom slot out of this list**, not the loop
        // bound, so the tempting "never drop an entry" refactor into
        // `Tree::edge_name`'s `<root>` fallback would put `('', '')`-shaped
        // noise in a notebook. `tests/python/test_api.py` pins it.
        if let Some(pair) = named_edge_in(&view, EdgeId(raw)) {
            out.push(pair);
        }
    }
    Ok(out)
}

/// The **dynamic** edges a compiled plan samples, behind `Plan.edges`.
///
/// # A plan does not remember its static edges, and cannot
///
/// `Step::Static` is "a folded static edge *or a run of them*", pre-inverted and
/// composed at compile time (`tf_tree_core::plan::Step`). By the time a plan
/// exists, the identities of the static edges that went into it are gone — not
/// hidden, *gone*, which is the whole point of folding them. So this enumerates
/// the `Step::Dyn` steps and the doc string says so; inventing ids for the
/// folded ones would be fabricating topology, and returning nothing at all would
/// be less useful than the answer the plan can actually give.
///
/// Fold order, not the order the frames appear in the path: a plan is a sequence
/// of compositions and that is the sequence.
///
/// The direction each step composes in (`Step::Dyn { inverted }`) is not
/// reported. The pair is the edge's identity — the same identity
/// [`edges_impl`] hands out — and a plan from `base` to `map` names the same
/// edge as one from `map` to `base`.
///
/// # Errors
///
/// [`detached_err`] on a tree inherited across a `fork()`. The plan's own
/// [`Guard`](tf_tree::Guard) would refuse too, but a plan is not evaluated here:
/// nothing but this guard stands between a detached tree and a silent `[]`.
pub(crate) fn plan_edges_impl(tree: &Tree, plan: &Plan) -> PyResult<Vec<(String, String)>> {
    if tree.detached() {
        return Err(detached_err());
    }
    let view = tree.arena_view();
    let mut out = Vec::with_capacity(plan.len());
    for step in plan.steps() {
        let Step::Dyn { edge, .. } = step else {
            continue;
        };
        if let Some(pair) = named_edge_in(&view, *edge) {
            out.push(pair);
        }
    }
    Ok(out)
}

/// Write this tree's arena to `path` as a `.tft` (§2.3), behind `Tree.freeze`.
///
/// This is the Python entry §3.3 asks for — the way in for a user whose poses
/// were never in a bag — and it is also what makes §4.1's claim testable from
/// Python at all: freeze a tree, reopen the file, and demand the *same* numbers
/// out of the *same* calls.
///
/// `source_digest` is all-zero. §2.3 defines it as BLAKE3 of the source
/// recording, and a tree assembled in Python has none; inventing a digest of the
/// arena bytes instead would put a value in a field that means something else,
/// which is worse than the documented "there was no recording" zero.
///
/// # The GIL is released for the copy
///
/// A freeze is one `write` of the *whole* arena — hundreds of milliseconds and
/// hundreds of megabytes for a tree big enough to be worth freezing, against
/// CPython's 5 ms switch interval. Holding the GIL across it stops every other
/// thread in the process dead, which is the rule `PyPlan::at_many_into` already
/// follows from 1 µs of estimated work upward
/// ([`GIL_RELEASE_THRESHOLD_NS`](crate::tree::GIL_RELEASE_THRESHOLD_NS)). There
/// is no size threshold here because there is no cheap case: the smallest useful
/// arena is still a file write.
///
/// Nothing inside the `detach` touches a Python object — `path` and `source` are
/// already owned Rust values, which is what makes the release sound.
#[cfg(target_os = "linux")]
pub(crate) fn freeze_impl(
    py: Python<'_>,
    tree: &Tree,
    path: &Path,
    source: Option<&str>,
) -> PyResult<()> {
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_nanos()).ok())
        .unwrap_or(0);
    py.detach(|| tree.freeze_to(path, source, [0u8; 32], created))
        .map(|_| ())
        .map_err(|e| frozen_err(path, e))
}

/// See [`freeze_impl`]. `.tft` support is Linux-only, like the mapping it is.
#[cfg(not(target_os = "linux"))]
pub(crate) fn freeze_impl(
    _py: Python<'_>,
    _tree: &Tree,
    _path: &Path,
    _source: Option<&str>,
) -> PyResult<()> {
    Err(not_on_this_platform())
}

#[cfg(target_os = "linux")]
fn open_frozen(path: &Path) -> PyResult<Tree> {
    Tree::open_frozen(path).map_err(|e| frozen_err(path, e))
}

/// See [`open_file`]. `.tft` support is Linux-only, like the mapping it is.
#[cfg(not(target_os = "linux"))]
fn open_frozen(_path: &Path) -> PyResult<Tree> {
    Err(not_on_this_platform())
}

/// The whole frozen path is `#[cfg(all(feature = "shm", target_os = "linux"))]`
/// in the facade, so on any other platform the *method* still exists and
/// refuses. A missing attribute would make a portable script fail with
/// `AttributeError` at a line that has nothing to do with the reason.
#[cfg(not(target_os = "linux"))]
fn not_on_this_platform() -> PyErr {
    TfTreeError::new_err(
        "frozen .tft files need the mmap-backed arena, which is Linux-only in \
         this build",
    )
}

/// Map a `.tft` failure onto Python, keeping the path and the remedy.
///
/// **The errno path becomes a real `OSError` subclass**, because that is what a
/// Python caller already handles: a missing index raises `FileNotFoundError`,
/// and a `try: ... except FileNotFoundError:` around the open works without
/// anyone learning our exception hierarchy. Passing `(errno, strerror,
/// filename)` is what makes CPython pick the subclass — a bare
/// `OSError(message)` would not.
///
/// The container failures stay `TfTreeError` and carry §2.4's remedy: a
/// `layout_hash` mismatch names **both** values and says to re-freeze, because
/// a `.tft` is a cache and not an archive.
#[cfg(target_os = "linux")]
fn frozen_err(path: &Path, e: tf_tree::FrozenFileError) -> PyErr {
    use tf_tree::{FrozenError, FrozenFileError};
    // `Path` has no `Display`; `display()` is lossy for a non-UTF-8 path, which
    // is right for a *message*. The `filename` attribute below keeps the real
    // bytes, because that is the one a caller may reopen with.
    let shown = path.display();
    match e {
        FrozenFileError::Path { raw_os_error } if raw_os_error != 0 => {
            let io = std::io::Error::from_raw_os_error(raw_os_error);
            PyErr::new::<pyo3::exceptions::PyOSError, _>((
                raw_os_error,
                io.to_string(),
                // `OsString`, deliberately, not `PathBuf`. PyO3 converts a
                // `PathBuf` into a `pathlib.PurePath`, which would make
                // `e.filename` a `PosixPath` even when the caller passed a
                // `str` — CPython's own `OSError.filename` is a `str` there.
                // `OsString` converts with `os.fsdecode` semantics, so it is
                // the string form *and* survives a non-UTF-8 path.
                path.as_os_str().to_owned(),
            ))
        }
        FrozenFileError::Path { .. } => {
            TfTreeError::new_err(format!("{shown}: could not be opened"))
        }
        FrozenFileError::Frozen(f) => {
            let detail = match f {
                FrozenError::BadMagic => {
                    "does not begin with the .tft magic, so it is not a frozen arena".to_owned()
                }
                FrozenError::LayoutMismatch { found, expected } => format!(
                    "was written with arena layout hash {found:#010x}; this build computes \
                     {expected:#010x}. Re-freeze the source recording — a .tft is a cache, \
                     not an archive (`tf_tree doctor --explain-version`)"
                ),
                FrozenError::VersionMismatch { found, expected } => format!(
                    "was written by arena FORMAT_VERSION {found}; this build speaks \
                     {expected}. Re-freeze the source recording — a .tft is a cache, not \
                     an archive (`tf_tree doctor --explain-version`)"
                ),
                other => format!("could not be opened as a .tft: {other:?}"),
            };
            TfTreeError::new_err(format!("{shown}: {detail}"))
        }
        // `FrozenFileError` is `#[non_exhaustive]`, so a variant added later
        // reaches Python as a base `TfTreeError` rather than failing to
        // compile here — deliberate: this crate is outside the workspace and a
        // compile error in it is found late.
        other => TfTreeError::new_err(format!("{shown}: {other:?}")),
    }
}
