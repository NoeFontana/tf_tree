//! The exception hierarchy (`docs/PHASE3.md` §4.4).
//!
//! Python errors carry a class, a message, and the fields a handler branches on
//! as attributes (`docs/decisions/0058`), set by [`with_attrs`] into the
//! instance `__dict__` with `args` left `(message,)`, so pickling works. Mappers
//! that attach one take `py: Python<'_>` and are called after `py.detach`
//! returns. Every class is declared under the module path `tf_tree`, so it can
//! be pickled out of a `multiprocessing` worker; [`ChildProcessDetachedError`]
//! is `PHASE3.md` §8.1, NORMATIVE.
//!
//! This module is `docs/API.md` R5's "separate layer": a message never carries
//! an `EdgeId`, which a Python caller cannot invert. Ids go through
//! [`edge_label_in`] / [`frame_label`], except `FrameOutOfRange` and the
//! out-of-range half of `UnknownEdge`, where resolution can only fail.

use pyo3::prelude::*;
use pyo3::{
    create_exception,
    exceptions::{PyBaseException, PyException},
    types::PyTuple,
};

use tf_tree::unstable::ArenaView;
use tf_tree::{
    BuildError, ClaimApiError, EdgeId, FrameError, FrameId, InterpPolicy, LookupError, PushError,
    Tree,
};
// Linux-only: the facade gates these on `target_os`, not only on `shm`.
#[cfg(target_os = "linux")]
use tf_tree::{IpcError, OpenError};

use crate::offline::{named_edge_in, named_frame_in};
use crate::tree::interp_name;

create_exception!(
    tf_tree,
    TfTreeError,
    PyException,
    "Base of every tf_tree error."
);
create_exception!(
    tf_tree,
    ExtrapolationError,
    TfTreeError,
    "The requested stamp lies outside an edge's retained history.\n\n\
     Attributes: edge, requested, oldest, newest, domain. They exist only on \
     instances the library raises."
);
create_exception!(
    tf_tree,
    DisconnectedError,
    TfTreeError,
    "No path joins the two frames.\n\n\
     Attributes: target, source, cut_at. They exist only on instances the \
     library raises."
);
create_exception!(
    tf_tree,
    NoDataError,
    TfTreeError,
    "An edge on the path has no samples yet.\n\n\
     Attribute: edge. It exists only on instances the library raises."
);
create_exception!(
    tf_tree,
    TopologyChangedError,
    TfTreeError,
    "The tree was re-parented after this plan was compiled; re-plan.\n\n\
     Attributes: plan_generation, current_generation. They exist only on \
     instances the library raises."
);
create_exception!(
    tf_tree,
    FrameNotDeclaredError,
    TfTreeError,
    "No such frame in this arena.\n\n\
     Attribute: name. It exists only on instances the library raises."
);
create_exception!(
    tf_tree,
    BufferError,
    TfTreeError,
    "An output buffer was the wrong shape, dtype, or size."
);
create_exception!(
    tf_tree,
    DerivativesUnavailableError,
    TfTreeError,
    "This edge's interpolator has no exact derivative; layout='quat_twist' \
     cannot be served over it.\n\n\
     Attribute: edge. It exists only on instances the library raises."
);
create_exception!(
    tf_tree,
    NoSegmentError,
    TfTreeError,
    "A pose exists at this stamp but there is no segment to differentiate; \
     layout='quat_twist' needs two samples spanning a non-zero interval.\n\n\
     Attribute: edge. It exists only on instances the library raises."
);
create_exception!(
    tf_tree,
    TimeDomainMismatchError,
    TfTreeError,
    "A stamp's time domain is not the path's: the plan was compiled, or the \
     query made, in another clock's domain.\n\n\
     Attributes: expected (the path's or plan's tag), got (the caller's). They \
     exist only on instances the library raises."
);
create_exception!(
    tf_tree,
    NonMonotonicStampError,
    TfTreeError,
    "A pushed stamp is older than the newest one already published on its edge.\n\n\
     Attributes: edge, last (the newest published stamp), got (the refused one). \
     They exist only on instances the library raises."
);
create_exception!(
    tf_tree,
    EdgeAlreadyClaimedError,
    TfTreeError,
    "Another publisher holds this edge's claim: one writer per edge.\n\n\
     Attributes: edge, owner_slot (a participant slot, not a pid; None while \
     the claim is still being taken). They exist only on instances the library \
     raises."
);
create_exception!(
    tf_tree,
    ArenaHeldButUnreachableError,
    TfTreeError,
    "An arena's participant bytes are held, but nothing serves it: retry, then \
     find the holders.\n\n\
     Attributes: holder_slots (held participant slots, ascending), \
     ownership_held. They exist only on instances the library raises."
);
create_exception!(
    tf_tree,
    ArenaAbsentError,
    TfTreeError,
    "No arena is serving under this name, and this open was not asked to \
     create one: retry once its owner has started."
);
create_exception!(
    tf_tree,
    ChildProcessDetachedError,
    TfTreeError,
    "This handle was inherited across a fork(); the child has no mapping and \
     the handle cannot be repaired. Open a new tree in the child."
);

/// Add every exception type to the module.
pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    m.add("TfTreeError", py.get_type::<TfTreeError>())?;
    m.add("ExtrapolationError", py.get_type::<ExtrapolationError>())?;
    m.add("DisconnectedError", py.get_type::<DisconnectedError>())?;
    m.add("NoDataError", py.get_type::<NoDataError>())?;
    m.add(
        "TopologyChangedError",
        py.get_type::<TopologyChangedError>(),
    )?;
    m.add(
        "FrameNotDeclaredError",
        py.get_type::<FrameNotDeclaredError>(),
    )?;
    m.add("BufferError", py.get_type::<BufferError>())?;
    m.add(
        "DerivativesUnavailableError",
        py.get_type::<DerivativesUnavailableError>(),
    )?;
    m.add("NoSegmentError", py.get_type::<NoSegmentError>())?;
    m.add(
        "ChildProcessDetachedError",
        py.get_type::<ChildProcessDetachedError>(),
    )?;
    m.add(
        "TimeDomainMismatchError",
        py.get_type::<TimeDomainMismatchError>(),
    )?;
    m.add(
        "NonMonotonicStampError",
        py.get_type::<NonMonotonicStampError>(),
    )?;
    m.add(
        "EdgeAlreadyClaimedError",
        py.get_type::<EdgeAlreadyClaimedError>(),
    )?;
    // Registered everywhere so `except` on it is valid on every platform (`0058` §4).
    m.add(
        "ArenaHeldButUnreachableError",
        py.get_type::<ArenaHeldButUnreachableError>(),
    )?;
    m.add("ArenaAbsentError", py.get_type::<ArenaAbsentError>())?;
    Ok(())
}

/// Set attributes on `err`'s instance and hand the exception back
/// (`docs/decisions/0058` §5); a failing `setattr` replaces the error.
fn with_attrs<'py>(
    py: Python<'py>,
    err: PyErr,
    attrs: impl FnOnce(&Bound<'py, PyBaseException>) -> PyResult<()>,
) -> PyErr {
    match attrs(err.value(py)) {
        Ok(()) => err,
        Err(failed) => failed,
    }
}

// ---------------------------------------------------------------------------
// Naming things
// ---------------------------------------------------------------------------

/// The one spelling of an edge, `edge "parent" -> "child"`; `span`'s no-data path
/// pins it in `tests/python/test_frozen.py`.
pub(crate) fn edge_label_of(parent: &str, child: &str) -> String {
    format!("edge {parent:?} -> {child:?}")
}

/// [`edge_label_of`] for an id, resolved against a view the caller holds.
///
/// Unresolvable ids print `#7` with the reason ([`nameless`]), never `EdgeId(7)`.
/// One view serves a whole message; there is no `&Tree` spelling.
pub(crate) fn edge_label_in(tree: &Tree, view: &ArenaView<'_>, edge: EdgeId) -> String {
    resolved_edge(tree, view, edge).0
}

/// An edge resolved once: the label the sentence reads and the stored pair
/// `.edge` carries (`docs/decisions/0058` §1).
pub(crate) fn resolved_edge(
    tree: &Tree,
    view: &ArenaView<'_>,
    edge: EdgeId,
) -> (String, Option<(String, String)>) {
    let named = named_edge_in(view, edge);
    let label = match &named {
        Some((parent, child)) => edge_label_of(parent, child),
        None => format!("edge #{} ({})", edge.get(), nameless(tree)),
    };
    (label, named)
}

/// A frame id as the caller's own quoted name, bare so it reads inside a
/// sentence with its own noun; [`frame_phrase_in`] supplies the noun.
pub(crate) fn frame_label(tree: &Tree, frame: FrameId) -> String {
    frame_label_in(tree, &tree.arena_view(), frame)
}

/// [`frame_label`] against a view the caller already holds.
fn frame_label_in(tree: &Tree, view: &ArenaView<'_>, frame: FrameId) -> String {
    match named_frame_in(view, frame) {
        Some(name) => format!("{name:?}"),
        None => format!("frame #{} ({})", frame.get(), nameless(tree)),
    }
}

/// [`frame_label_in`] with the noun; the fallback already begins `frame #7`, so
/// a caller-side `format!("frame {}")` would double it.
fn frame_phrase_in(tree: &Tree, view: &ArenaView<'_>, frame: FrameId) -> String {
    match named_frame_in(view, frame) {
        Some(name) => format!("frame {name:?}"),
        None => format!("frame #{} ({})", frame.get(), nameless(tree)),
    }
}

/// The one refusal for a name this arena has never interned; `.name` is the name
/// typed (`docs/decisions/0058` §1). The remedy is in the message because a
/// traceback shows it and not the class docstring.
fn frame_not_declared(py: Python<'_>, name: &str) -> PyErr {
    let err = FrameNotDeclaredError::new_err(format!(
        "no frame named {name:?} in this arena; if the name is spelled right, \
         its publisher has not declared it yet — wait for one, or declare it \
         on the builder that creates the arena"
    ));
    with_attrs(py, err, |e| e.setattr("name", name))
}

/// Resolve a frame name **without interning**.
///
/// [`Tree::frame`] interns on a writable tree, so a typo would spend a frame
/// slot permanently (`docs/PROJECT.md` §5 D10) and a loop of them could exhaust
/// the headroom every participant shares. `find_frame` is the read-only probe.
///
/// # Errors
///
/// [`FrameNotDeclaredError`] for an absent name; the base `TfTreeError` for the
/// two other failures ([`unresolvable_name`]).
pub(crate) fn resolve_frame(py: Python<'_>, tree: &Tree, name: &str) -> PyResult<FrameId> {
    if tree.detached() {
        return Err(detached_err());
    }
    match tree.arena_view().find_frame(name) {
        Ok(Some(id)) => Ok(id),
        Ok(None) => Err(frame_not_declared(py, name)),
        Err(e) => Err(unresolvable_name(py, name, e)),
    }
}

/// Attribute a `LookupError::UnknownFrame` to one of the names the caller typed.
///
/// `UnknownFrame` carries a hash that does not invert, so `Tree.lookup` probes
/// each name. The engine collapsed three outcomes that want different remedies:
/// never interned (wait or declare), a hash collision (rename; waiting is wrong)
/// and a mid-intern publisher (retry). Both names resolving means a peer interned
/// one in between; [`lookup_err_untagged`] then reports the hash.
pub(crate) fn unknown_frame_err(
    py: Python<'_>,
    tree: &Tree,
    names: [&str; 2],
    e: LookupError,
) -> PyErr {
    let view = tree.arena_view();
    for name in names {
        match view.find_frame(name) {
            Ok(Some(_)) => continue,
            Ok(None) => return frame_not_declared(py, name),
            Err(err) => return unresolvable_name(py, name, err),
        }
    }
    lookup_err_untagged(py, tree, e)
}

/// The two ways a name fails to resolve that are not "never declared": they
/// raise the base `TfTreeError` (`docs/API.md` R5: the type is the contract).
fn unresolvable_name(py: Python<'_>, name: &str, e: FrameError) -> PyErr {
    match e {
        FrameError::FrameHashCollision { hash } => TfTreeError::new_err(format!(
            "{name:?} cannot be resolved in this arena: another frame name \
             already occupies its 64-bit hash slot ({hash:#x}). This is not a \
             race and waiting will not clear it — rename either frame. \
             (tf_tree_core puts the odds at ~3e-12 for ten thousand frames, \
             and detects it rather than aliasing the two names.)"
        )),
        FrameError::InternContended => TfTreeError::new_err(format!(
            "{name:?} is being interned right now by a participant this arena \
             cannot identify, so no id can be read for it yet and no reader can \
             judge whether that publisher is still alive. Retry"
        )),
        // `find_frame` never inserts; these two both mean the name cannot be declared here.
        FrameError::ReadOnly | FrameError::CapacityExceeded => frame_not_declared(py, name),
        FrameError::ChildDetached => detached_err(),
        other => TfTreeError::new_err(format!(
            "{name:?} could not be resolved, and this binding has no message \
             for the reason. That is a bug in tf_tree_py's error layer, not in \
             your program; please report it with this line: {other:?}"
        )),
    }
}

// ---------------------------------------------------------------------------
// Creating an arena
// ---------------------------------------------------------------------------

/// Map a failed `tf_tree.build(...)` onto Python, in the caller's own names.
///
/// No arena exists to resolve ids against, so duplicates and cycles are found
/// in the caller's edge list rather than read from the error (a hash does not
/// invert; the builder's id order is internal). `Layout` and `Participant` carry
/// this binding's remedies; `Shm` forwards `ShmError`'s `Display`, whose text
/// is the search key of `docs/RUNBOOK.md` (`docs/decisions/0059`).
pub(crate) fn build_err(edges: &[(String, String)], capacity: u32, e: BuildError) -> PyErr {
    TfTreeError::new_err(match e {
        // Named from the list; both parents are shown so the colliding pair can be found.
        BuildError::DuplicateEdge { child } => match duplicate_child(edges) {
            // Both parents, not just the child: the caller has to find *which
            // two pairs* collide, and in a list of forty edges the child's name
            // alone appears in both of them.
            Some((name, first, second)) => format!(
                "two edges declare {name:?} as their child — {} and {}. A frame \
                 has exactly one parent, which is what makes this a tree, so \
                 one of the two pairs has to go",
                edge_label_of(first, name),
                edge_label_of(second, name),
            ),
            None => format!(
                "two edges declare the same child (its name hashes to \
                 {child:#018x}), and a frame has exactly one parent"
            ),
        },
        BuildError::TooManyFrames | BuildError::TooManyEdges => format!(
            "this edge list is too large for the u32 id space: {} pairs",
            edges.len()
        ),
        // `WouldCreateCycle` is the only provocable variant; the caller's own list answers which.
        BuildError::Topology(_) => match cycle_through(edges) {
            Some(chain) => format!(
                "this edge list is not a tree: {chain}. Every frame has exactly \
                 one parent and no frame may be its own ancestor"
            ),
            None => "the topology could not be wired from this edge list, and \
                     no cycle is visible in it. That is a bug in tf_tree rather \
                     than in your call — every name here is interned before any \
                     edge is wired"
                .to_owned(),
        },
        // Reachable only as a genuine collision: every name is interned into a table sized from the list.
        BuildError::Frame(FrameError::FrameHashCollision { hash }) => format!(
            "two of the frame names in this edge list collide on the same \
             64-bit hash ({hash:#x}), so they cannot both be interned. Rename \
             either — tf_tree_core puts the odds at ~3e-12 for ten thousand \
             frames, and detects the collision rather than aliasing the two \
             names onto one frame"
        ),
        BuildError::Frame(_) => "a frame name in this edge list could not be interned. Every \
             name here goes into a table sized from this same list, so a \
             collision is the only cause a caller can produce; anything else is \
             a bug in tf_tree"
            .to_owned(),
        // Size only; the other `LayoutError` members are unreachable.
        BuildError::Layout(_) => format!(
            "{} edges at capacity={capacity} do not form a valid arena layout. \
             Every region offset in an arena header is a u32, so the whole \
             arena has to fit in 4 GiB — lower capacity=, or declare fewer \
             edges",
            edges.len()
        ),
        BuildError::Participant(_) => "this process could not take a slot in the arena's \
             participant table: every slot is occupied and the count is fixed \
             when the arena is created. A peer has to exit, or be reaped, first"
            .to_owned(),
        #[cfg(target_os = "linux")]
        BuildError::Shm(inner) => format!(
            "the shared-memory segment for this arena could not be created, \
             sized, mapped or sealed: {inner}"
        ),
        other => format!(
            "tf_tree could not build this tree, and this binding has no message \
             for the reason. That is a bug in tf_tree_py's error layer, not in \
             your program; please report it with this line: {other:?}"
        ),
    })
}

/// Map a failed `tf_tree.open(...)` onto Python — [`build_err`]'s other half.
///
/// Arms that are already sentences forward their `Display`. `ArenaAbsent` raises
/// the attribute-free [`ArenaAbsentError`]; `ArenaHeldButUnreachable` raises
/// [`ArenaHeldButUnreachableError`] with `.holder_slots` and `.ownership_held`
/// (`docs/decisions/0058` §4), and no pid, which is namespace-local (`0033`).
#[cfg(target_os = "linux")]
pub(crate) fn open_err(
    py: Python<'_>,
    edges: &[(String, String)],
    capacity: u32,
    e: OpenError,
) -> PyErr {
    match e {
        OpenError::Build(inner) => build_err(edges, capacity, inner),
        // Stage in this binding's words, then `ShmError`'s `Display`.
        OpenError::Map(inner) => TfTreeError::new_err(format!(
            "the arena's shared-memory segment was handed over but could not be \
             mapped: {inner}"
        )),
        OpenError::Rendezvous(
            inner @ IpcError::ArenaHeldButUnreachable {
                holder_slots,
                ownership_held,
                ..
            },
        ) => {
            let err = ArenaHeldButUnreachableError::new_err(format!("{inner}"));
            with_attrs(py, err, |e| {
                let slots: Vec<u32> = (0..u64::BITS)
                    .filter(|slot| holder_slots >> slot & 1 == 1)
                    .collect();
                e.setattr("holder_slots", PyTuple::new(py, slots)?)?;
                e.setattr("ownership_held", ownership_held)
            })
        }
        // A unit variant: a leaf with no attributes (`0058` §4).
        OpenError::Rendezvous(inner @ IpcError::ArenaAbsent) => {
            ArenaAbsentError::new_err(format!("{inner}"))
        }
        // Already prose; `IpcError`'s `Display` owns it.
        other => TfTreeError::new_err(format!("{other}")),
    }
}

/// The first child two edges declare, as `(child, first parent, second parent)`,
/// in `TreeBuilder::build`'s order.
fn duplicate_child(edges: &[(String, String)]) -> Option<(&str, &str, &str)> {
    let mut first: std::collections::HashMap<&str, &str> =
        std::collections::HashMap::with_capacity(edges.len());
    for (parent, child) in edges {
        match first.insert(child.as_str(), parent.as_str()) {
            Some(earlier) => return Some((child.as_str(), earlier, parent.as_str())),
            None => continue,
        }
    }
    None
}

/// A cycle in the caller's edge list, spelled as a chain (`"b" is under "a",
/// which is under "b"`). Assumes `build` has already rejected duplicate children.
fn cycle_through(edges: &[(String, String)]) -> Option<String> {
    let parent: std::collections::HashMap<&str, &str> = edges
        .iter()
        .map(|(p, c)| (c.as_str(), p.as_str()))
        .collect();
    for (_, start) in edges {
        let mut chain: Vec<String> = Vec::new();
        let mut at = start.as_str();
        for _ in 0..parent.len() {
            let Some(&up) = parent.get(at) else { break };
            chain.push(format!("{up:?}"));
            if up == start.as_str() {
                return Some(format!(
                    "{start:?} is under {}",
                    chain.join(", which is under ")
                ));
            }
            at = up;
        }
    }
    None
}

/// Why an id could not be turned into a name.
fn nameless(tree: &Tree) -> &'static str {
    if tree.detached() {
        "name unavailable: this tree was inherited across a fork(), so the \
         child has no mapping left to read the name from"
    } else {
        "name unavailable: this arena holds no record at that index"
    }
}

// ---------------------------------------------------------------------------
// The errors themselves
// ---------------------------------------------------------------------------

/// A failed `inherit_ownership`. Every error path restores the attachment and
/// gives back the ownership byte, and the message says so, so a caller does not
/// restart a healthy fleet
/// ([`0044`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0044-recovery-the-languages-a-robot-is-written-in-cannot-reach.md)).
#[cfg(target_os = "linux")]
pub(crate) fn inherit_err(e: &tf_tree::OpenError) -> PyErr {
    TfTreeError::new_err(format!(
        "could not inherit the owner role: {e}. This process kept its participant \
         slot, its lock byte and its mapping, and gave back the ownership byte if \
         it had taken one — so the arena still has no owner rather than an owner \
         that is not serving, and another survivor (or this one, next pass) can \
         still take it. Reads are unaffected throughout"
    ))
}

/// The error every entry point raises on a tree inherited across a `fork()`:
/// [`ChildProcessDetachedError`] (`docs/PHASE3.md` §8.1, NORMATIVE), so a retry
/// loop catching `TfTreeError` can stop on it by class. It subclasses
/// `TfTreeError`. The message names the remedy because `multiprocessing` forked,
/// not the caller.
pub(crate) fn detached_err() -> PyErr {
    ChildProcessDetachedError::new_err(DETACHED)
}

/// [`detached_err`]'s sentence, shared with [`push_msg`].
const DETACHED: &str = "this tree was inherited across a fork(); the child's mapping is gone \
     and the handle cannot be repaired. Open a new tree in the child \
     (tf_tree.open(...)), or use multiprocessing's 'spawn' or 'forkserver' \
     start method";

/// Map a `LookupError` to its Python exception: a class, a sentence, and the
/// attributes `docs/decisions/0058` §1 gives that class.
///
/// `domain` is the **query's** tag for `ExtrapolationError.domain` (`0058` §3);
/// call sites that hold none use [`lookup_err_untagged`]. One [`ArenaView`]
/// serves the whole message. Variants are enumerated so each failure has one
/// place to look; the wildcard is `#[non_exhaustive]`'s.
///
/// `#[cold]` + `#[inline(never)]` were measured on scalar `plan.at` (`0058`
/// step 2); the other mappers show no regression and carry neither.
#[cold]
#[inline(never)]
pub(crate) fn lookup_err(py: Python<'_>, tree: &Tree, domain: u8, e: LookupError) -> PyErr {
    let view = tree.arena_view();
    match e {
        LookupError::Extrapolation {
            edge,
            requested,
            oldest,
            newest,
        } => {
            let (label, named) = resolved_edge(tree, &view, edge);
            let msg = format!(
                "{label}: stamp {requested} ns is outside the retained history \
                 [{oldest}, {newest}] ns"
            );
            with_attrs(py, ExtrapolationError::new_err(msg), |e| {
                e.setattr("edge", named)?;
                e.setattr("requested", requested)?;
                e.setattr("oldest", oldest)?;
                e.setattr("newest", newest)?;
                e.setattr("domain", domain)
            })
        }
        LookupError::Disconnected {
            target,
            source,
            cut_at,
        } => {
            let err = DisconnectedError::new_err(format!(
                "no path from {} to {}; the chain stops at {}",
                frame_label_in(tree, &view, source),
                frame_label_in(tree, &view, target),
                frame_label_in(tree, &view, cut_at),
            ));
            with_attrs(py, err, |e| {
                e.setattr("target", named_frame_in(&view, target))?;
                e.setattr("source", named_frame_in(&view, source))?;
                e.setattr("cut_at", named_frame_in(&view, cut_at))
            })
        }
        LookupError::NoData { edge } => {
            let (label, named) = resolved_edge(tree, &view, edge);
            no_data_err(py, named, format!("{label} has no samples yet"))
        }
        LookupError::TopologyChanged { plan, current } => {
            let err = TopologyChangedError::new_err(format!(
                "this plan was compiled at topology generation {plan}, the tree \
                 is now at {current}; call tree.plan(...) again"
            ));
            with_attrs(py, err, |e| {
                e.setattr("plan_generation", plan)?;
                e.setattr("current_generation", current)
            })
        }
        // Last resort: the hash does not invert; `PyTree::lookup` attributes
        // the name itself. `.name` is `None` only here.
        LookupError::UnknownFrame { hash } => {
            let err = FrameNotDeclaredError::new_err(format!(
                "no frame with hash {hash:#x} in this arena; if the name is spelled right, its publisher has not declared it yet — wait for one, or declare it on the builder that creates the arena"
            ));
            with_attrs(py, err, |e| e.setattr("name", py.None()))
        }
        LookupError::BufferTooSmall { need, got } => BufferError::new_err(format!(
            "output buffer holds {got} elements; this batch needs {need}"
        )),
        // `DerivativesUnavailable` is a property of an edge (`docs/PHASE5.md` §4.4
        // item 1) and permanent; `NoSegment` is of a stamp and transient. Distinct
        // types because R5 forbids telling them apart by text; a batch fails at
        // element 0 for the first, possibly later for the second.
        LookupError::DerivativesUnavailable { edge, interp } => {
            let (label, named) = resolved_edge(tree, &view, edge);
            let err = DerivativesUnavailableError::new_err(format!(
                "{label} declares {}, which has no exact derivative; use \
                 layout='quat' or declare the edge interp='sclerp'",
                stored_interp(interp),
            ));
            with_attrs(py, err, |e| e.setattr("edge", named))
        }
        // `NoSegment` is a property of a **stamp**, and is transient: the ring
        // retains one sample, or the two bracketing `t` carry equal stamps —
        // which invariant 6 permits — so there is a pose but no interval to
        // differentiate over. The fix is to publish another sample or ask again
        // later, which is the opposite response to the arm above — and telling
        // the two apart by message text is what R5 forbids. `Plan::at_many_into`
        // documents the consequence for a batch: the arm above always fires at
        // element 0 and leaves `out` untouched, this one can fire after `k` rows
        // are written.
        LookupError::NoSegment { edge } => {
            let (label, named) = resolved_edge(tree, &view, edge);
            let err = NoSegmentError::new_err(format!(
                "{label} has a pose at this stamp but no segment to \
                 differentiate: it retains one sample, or the two bracketing \
                 samples carry equal stamps. Publish another sample, or use \
                 layout='quat'"
            ));
            with_attrs(py, err, |e| e.setattr("edge", named))
        }
        LookupError::ChildDetached => detached_err(),

        // Below: base `TfTreeError` by preservation (R5: the type is the
        // contract); only `TimeDomainMismatch` has a leaf (`0058`).

        // Above `MAX_PATH_EDGES` the depth is a floor, so the message says
        // "longer than"; at or below it is the exact folded step count. No remedy
        // naming static edges: `tf_tree.build` declares every edge dynamic.
        LookupError::TreeTooDeep { depth } => {
            TfTreeError::new_err(if usize::from(depth) > tf_tree::MAX_PATH_EDGES {
                format!(
                    "the path between these frames is longer than the {} edges a \
                     lookup walks — re-parent so the two frames share a nearer \
                     ancestor",
                    tf_tree::MAX_PATH_EDGES
                )
            } else {
                format!(
                    "this path compiles to {depth} steps and a plan holds {} — \
                     re-parent so the two frames share a nearer ancestor",
                    tf_tree::MAX_DEPTH
                )
            })
        }
        // Recycled (history gone; a retry reads a newer window) and Contended
        // (writer mid-update; a retry reads the same stamps) get different advice.
        LookupError::SlotRecycled { edge } => TfTreeError::new_err(format!(
            "the ring on {} lapped this reader mid-read: the samples being \
             interpolated were overwritten before the read finished. Retry, or \
             give the edge more capacity when the arena is built",
            edge_label_in(tree, &view, edge)
        )),
        LookupError::SlotContended { edge } => TfTreeError::new_err(format!(
            "a slot on {} stayed mid-write for the whole retry budget, so no \
             consistent sample could be read. Retry",
            edge_label_in(tree, &view, edge)
        )),
        // Time-domain refusals (D9). The message quotes the integer tag and names
        // the `domain=` keyword as the remedy (`0038` §3).
        LookupError::TimeDomainMismatch { expected, got } => {
            let err = TimeDomainMismatchError::new_err(format!(
                "this plan was compiled for time domain {expected}; the query \
                 supplied a stamp in domain {got}. A stamp from one clock cannot \
                 address an edge sampled on another — compile the plan in the \
                 arena's domain with tree.plan(target, source, domain={expected})"
            ));
            domain_mismatch_attrs(py, err, expected, got)
        }
        LookupError::MixedTimeDomains {
            edge,
            expected,
            got,
        } => TfTreeError::new_err(format!(
            "{} is in time domain {got} and the rest of the path is in domain \
             {expected}; no single stamp addresses both, so the path is \
             refused rather than sampled with the wrong clock",
            edge_label_in(tree, &view, edge)
        )),
        // Not via [`edge_label_in`]: its fallback would say the same fact three times.
        LookupError::UnknownEdge { edge } => {
            TfTreeError::new_err(match named_edge_in(&view, edge) {
                // A named edge here is a dynamic step over a static or tombstoned record.
                Some((parent, child)) => format!(
                    "{} carries no sample ring, so the step that names it \
                     cannot be evaluated: this arena records the edge as static \
                     or tombstoned. Re-compile with tree.plan(...)",
                    edge_label_of(&parent, &child)
                ),
                None if tree.detached() => return detached_err(),
                None => format!(
                    "edge id {} is past the end of this arena's edge table, so \
                     the plan naming it was compiled against a different arena",
                    edge.get()
                ),
            })
        }
        // Not via `frame_label`: resolving an out-of-range id can only yield the fallback.
        LookupError::FrameOutOfRange { frame } => TfTreeError::new_err(format!(
            "frame id {} is out of range for this arena's frame table",
            frame.get()
        )),
        // [`frame_phrase_in`], not `format!("frame {}", ..)`, which doubles the noun.
        LookupError::MissingEdge { child } => TfTreeError::new_err(format!(
            "{} has a parent in the topology but no edge records the link, so \
             the path through it cannot be evaluated",
            frame_phrase_in(tree, &view, child)
        )),
        // Unreachable from Python: the binding picks the f32/f64 entry point itself.
        LookupError::WrongElementType => TfTreeError::new_err(
            "an f32 layout reached the f64 entry point, or the reverse; the \
             binding chooses that pairing itself, so this is a bug in \
             tf_tree_py rather than in your call",
        ),
        other => TfTreeError::new_err(format!(
            "tf_tree reported a lookup failure this binding has no message \
             for. That is a bug in tf_tree_py's error layer, not in your \
             program; please report it with this line: {other:?}"
        )),
    }
}

/// The tag [`lookup_err_untagged`] hands on; nothing reads it.
const UNTAGGED_TAG: u8 = 0;

/// [`lookup_err`] for the three call sites holding no time-domain tag
/// (`span_impl`'s two and [`unknown_frame_err`]'s).
///
/// Their `Extrapolation` arm is unreachable and reports a bug, so a reachable
/// raise can never silently get the base class instead of
/// [`ExtrapolationError`] (R5).
pub(crate) fn lookup_err_untagged(py: Python<'_>, tree: &Tree, e: LookupError) -> PyErr {
    match e {
        LookupError::Extrapolation {
            edge,
            requested,
            oldest,
            newest,
        } => {
            let view = tree.arena_view();
            TfTreeError::new_err(format!(
                "{}: stamp {requested} ns is outside the retained history \
                 [{oldest}, {newest}] ns. This call site holds no time-domain \
                 tag to attach, which is a bug in tf_tree_py's error layer, not \
                 in your program; please report it with this line",
                edge_label_in(tree, &view, edge)
            ))
        }
        other => lookup_err(py, tree, UNTAGGED_TAG, other),
    }
}

/// A [`NoDataError`] with its `.edge` around the caller's sentence; both raise
/// sites (`lookup_err`, `span`) must carry it (`docs/decisions/0058` §1). Takes
/// the [`resolved_edge`] pair.
pub(crate) fn no_data_err(py: Python<'_>, edge: Option<(String, String)>, msg: String) -> PyErr {
    with_attrs(py, NoDataError::new_err(msg), |e| e.setattr("edge", edge))
}

/// The plan-time domain refusal (`docs/decisions/0038` §2): the same
/// [`TimeDomainMismatchError`] type as the per-query arm (`0058` §4), with prose
/// naming the route, since both frame names are still strings here.
pub(crate) fn plan_domain_err(
    py: Python<'_>,
    target: &str,
    source: &str,
    expected: u8,
    got: u8,
) -> PyErr {
    let err = TimeDomainMismatchError::new_err(format!(
        "the path {source:?} -> {target:?} is sampled in time domain {expected}, \
         and this plan was asked for domain {got}. A stamp from one clock \
         cannot address an edge sampled on another; pass \
         domain={expected} to tree.plan(). The four built-in tags are \
         tf_tree.SYSTEM_DOMAIN, SENSOR_DOMAIN, SIM_DOMAIN and STEADY_DOMAIN, \
         and a domain declared beyond them is the integer its declarer chose"
    ));
    domain_mismatch_attrs(py, err, expected, got)
}

/// `TimeDomainMismatchError`'s `expected` (path's or plan's tag) and `got`
/// (caller's) attributes.
fn domain_mismatch_attrs(py: Python<'_>, err: PyErr, expected: u8, got: u8) -> PyErr {
    with_attrs(py, err, |e| {
        e.setattr("expected", expected)?;
        e.setattr("got", got)
    })
}

/// Name a stored [`InterpPolicy`] discriminant as `interp=` spells it.
///
/// `InterpPolicy::from_u8` collapses unknown values onto the default, which
/// would make a message from a future arena self-contradictory; an unknown
/// number is echoed unresolved.
fn stored_interp(interp: u8) -> String {
    let policy = InterpPolicy::from_u8(interp);
    if policy.as_u8() == interp {
        format!("interp='{}'", interp_name(policy))
    } else {
        format!(
            "interpolation policy {interp}, which this build of tf_tree does \
             not know"
        )
    }
}

/// Map a failed `push` onto Python. The message names the edge as the caller
/// typed it ([`edge_label_of`]); the `.edge` attribute is the arena's stored pair
/// (`docs/decisions/0058` §2). `ChildDetached` raises
/// [`ChildProcessDetachedError`], `NonMonotonicStamp` raises
/// [`NonMonotonicStampError`] (`0058` §4), the rest `TfTreeError`. `tree` is
/// `None` only where the publisher's tree is gone; `.edge` is then `None`.
pub(crate) fn push_err(py: Python<'_>, tree: Option<&Tree>, edge: &str, e: PushError) -> PyErr {
    push_class(py, tree, e)(push_msg(edge, e))
}

/// The class and attributes of a failed push, apart from its sentence, so
/// `push_many` can prefix the sample index without re-wording or re-typing it.
pub(crate) fn push_class<'a>(
    py: Python<'a>,
    tree: Option<&'a Tree>,
    e: PushError,
) -> impl FnOnce(String) -> PyErr + 'a {
    move |msg| match e {
        PushError::ChildDetached => ChildProcessDetachedError::new_err(msg),
        PushError::NonMonotonicStamp { edge, last, got } => {
            with_attrs(py, NonMonotonicStampError::new_err(msg), |x| {
                x.setattr(
                    "edge",
                    tree.and_then(|t| named_edge_in(&t.arena_view(), edge)),
                )?;
                x.setattr("last", last)?;
                x.setattr("got", got)
            })
        }
        _ => TfTreeError::new_err(msg),
    }
}

/// [`push_err`]'s sentence; the wildcard is `#[non_exhaustive]`'s.
pub(crate) fn push_msg(edge: &str, e: PushError) -> String {
    match e {
        // Equal stamps are accepted (invariant 6), hence "older than". The
        // variant name stays as the search key of `docs/RUNBOOK.md`.
        PushError::NonMonotonicStamp { last, got, .. } => format!(
            "{edge}: stamp {got} ns is older than the newest published stamp \
             {last} ns. Stamps are non-decreasing per edge; equal stamps are \
             accepted and the newer value wins. A burst of these on a \
             wall-clock edge is usually a clock step rather than a publisher \
             fault — `tf_tree doctor`'s TFT018/TFT019 make that call, and \
             docs/RUNBOOK.md's NonMonotonicStamp section is the procedure"
        ),
        PushError::ClaimRevoked { .. } => format!(
            "{edge}: this writer's claim was revoked — a reaper judged the \
             process dead while it was stopped or stalled, and the edge is \
             free or owned by someone else now. Stop publishing and claim it \
             again"
        ),
        PushError::ChildDetached => DETACHED.to_owned(),
        other => format!(
            "{edge}: tf_tree reported a push failure this binding has no \
             message for. That is a bug in tf_tree_py's error layer, not in \
             your program; please report it with this line: {other:?}"
        ),
    }
}

/// Map a failed claim onto Python, naming the edge the caller asked for.
///
/// `ClaimApiError`'s `Display` spells edges `EdgeId(3)` and frames by raw index,
/// unreachable from Python, so arms are re-spelled around the two names passed
/// to `tree.publisher(child, parent)`. `ChildDetached` raises
/// [`ChildProcessDetachedError`]; `AlreadyClaimed` raises
/// [`EdgeAlreadyClaimedError`] (`docs/decisions/0058` §4); the rest `TfTreeError`.
pub(crate) fn claim_err(
    py: Python<'_>,
    tree: &Tree,
    parent: &str,
    child: &str,
    e: ClaimApiError,
) -> PyErr {
    let edge = edge_label_of(parent, child);
    match e {
        ClaimApiError::ChildDetached => detached_err(),
        // The three races of `docs/decisions/0005` §5 are transient; kept apart
        // because `LeaseUnavailable` is a lock-file problem retrying cannot fix.
        ClaimApiError::LeaseContended { .. } => TfTreeError::new_err(format!(
            "{edge}: the claim record was free but its lease is still held; \
             retry"
        )),
        ClaimApiError::LeaseUnavailable { .. } => TfTreeError::new_err(format!(
            "{edge}: the claim lease could not be taken — the arena's lock \
             file could not be asked about the edge"
        )),
        ClaimApiError::ReapedDuringClaim { .. } => TfTreeError::new_err(format!(
            "{edge}: a reaper cleared this claim while it was being taken; \
             retry"
        )),
        // Pre-empted by `Tree.publisher`'s name resolution; reachable only if the frame table changed between calls.
        ClaimApiError::UnknownFrame { .. } => {
            TfTreeError::new_err(format!("{child:?} is not a frame of this tree"))
        }
        // A reversed pair lands here whenever the child is a root, so it says which argument is which.
        ClaimApiError::NoEdge { .. } => TfTreeError::new_err(format!(
            "no edge attaches {child:?} to a parent, so there is nothing to \
             publish on. The call is publisher(child, parent) and {parent:?} \
             was given as the parent — if those are the wrong way round, swap \
             them. Otherwise: topology is builder-time (decision 0004), so \
             declare the edge on the call that creates the arena"
        )),
        ClaimApiError::NotDynamic { .. } => TfTreeError::new_err(format!(
            "the edge attaching {child:?} is static or tombstoned — it carries \
             no sample ring, so there is nothing to publish to"
        )),
        // Reports what the arena says instead of what was asked; `actual` is resolved to a name.
        ClaimApiError::ParentMismatch { actual, .. } => TfTreeError::new_err(format!(
            "{child:?} is not attached to {parent:?} but to {}; an edge names \
             the frame it moves, and reversing the pair is the usual cause \
             (the call is publisher(child, parent))",
            // Index 0 is the "no parent" sentinel.
            match FrameId::new(actual) {
                Some(f) => frame_label(tree, f),
                None => "the root (no parent)".to_owned(),
            }
        )),
        // `.edge` from the variant's `EdgeId`, `.owner_slot` from its cause
        // (`0058` §4); a later `ClaimError` cause falls to the bug-report arm.
        ClaimApiError::AlreadyClaimed {
            edge: id,
            cause: tf_tree::ClaimError::EdgeAlreadyClaimed { owner_slot },
        } => {
            let owner_slot = claimed_by(owner_slot);
            let holder = match owner_slot {
                Some(slot) => format!("participant slot {slot}"),
                None => "a claim that is still being taken, which has no slot \
                         recorded yet"
                    .to_owned(),
            };
            let err = EdgeAlreadyClaimedError::new_err(format!(
                "{edge}: already claimed by {holder}. One writer per edge \
                 (invariant 4): the other publisher must release it, or be \
                 reaped, first"
            ));
            with_attrs(py, err, |e| {
                e.setattr("edge", named_edge_in(&tree.arena_view(), id))?;
                e.setattr("owner_slot", owner_slot)
            })
        }
        ClaimApiError::ReadOnly => TfTreeError::new_err(format!(
            "{edge}: this arena is mapped read-only, so no edge can be claimed \
             for writing. tf_tree.open(...) defaults to mode='ro' (D18); pass \
             mode='rw' if this process really is a publisher"
        )),
        other => TfTreeError::new_err(format!(
            "{edge}: tf_tree reported a claim failure this binding has no \
             message for. That is a bug in tf_tree_py's error layer, not in \
             your program; please report it with this line: {other:?}"
        )),
    }
}

/// The participant slot holding a claim (not a pid) for [`claim_err`] and
/// `EdgeAlreadyClaimedError.owner_slot`.
///
/// `None` exactly for `u32::MAX`, `edge::slot_of`'s value for a claim word in
/// `CLAIMING`. No Python test reaches this arm: the window has no §11.3 crash
/// site (`docs/decisions/0058` §4).
fn claimed_by(owner_slot: u32) -> Option<u32> {
    (owner_slot != u32::MAX).then_some(owner_slot)
}
