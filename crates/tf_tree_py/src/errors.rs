//! The exception hierarchy (`docs/PHASE3.md` §4.4).
//!
//! Rust's errors are `Copy` and carry structured fields; Python's carry
//! messages. The gap matters: a user who has to parse a string to find out
//! *which edge* extrapolated cannot program against it, so the fields are
//! attached to the exception rather than only formatted into it.
//!
//! # This module is `docs/API.md` R5's "separate layer", and it has to earn it
//!
//! R5 buys its `Copy`, `String`-free error types by promising the prose lives
//! somewhere else. **Somewhere else is here**, so a message that reads
//! `edge EdgeId(3)` is not a cosmetic defect: it is this layer declining to do
//! the one job that justifies the rule. A Python caller has no `EdgeId` — the
//! surface never hands one out and offers no way to invert one — so an id in a
//! message is strictly less information than no message at all, because it
//! looks like it means something.
//!
//! Every id that reaches a Python message therefore goes through
//! [`edge_label`] / [`frame_label`], which resolve against the arena the caller
//! is holding. That capability was already in the binding —
//! [`crate::offline::named_edge_in`] has resolved edge ids for `Tree.edges` and
//! for `span`'s no-data path since `docs/PHASE5.md` §4.2 — and the routing is
//! what was missing, not the resolution.
//!
//! **Two arms deliberately do not resolve**, and they are the ones where
//! resolution can only ever fail: `FrameOutOfRange` and the out-of-range half of
//! `UnknownEdge` say the id is out of range *once*, rather than pairing a
//! fallback that says "no record at that index" with a sentence that says the
//! same thing again.

use pyo3::prelude::*;
use pyo3::{create_exception, exceptions::PyException};

use tf_tree::unstable::ArenaView;
use tf_tree::{
    BuildError, ClaimApiError, EdgeId, FrameError, FrameId, InterpPolicy, LookupError, PushError,
    Tree,
};
// **Linux-only, like the mapping it describes.** The facade gates
// `open::{open, CreatePolicy, Open, OpenError}` on
// `#[cfg(all(feature = "shm", target_os = "linux"))]`, and this crate always
// turns `shm` on — so it is the *target*, not the feature, that decides whether
// the name exists. Importing it unconditionally is what made `tf_tree_py` fail
// to compile on macOS and Windows.
#[cfg(target_os = "linux")]
use tf_tree::OpenError;

use crate::offline::{named_edge_in, named_frame_in};
use crate::tree::interp_name;

create_exception!(
    _core,
    TfTreeError,
    PyException,
    "Base of every tf_tree error."
);
create_exception!(
    _core,
    ExtrapolationError,
    TfTreeError,
    "The requested stamp lies outside an edge's retained history."
);
create_exception!(
    _core,
    DisconnectedError,
    TfTreeError,
    "No path joins the two frames."
);
create_exception!(
    _core,
    NoDataError,
    TfTreeError,
    "An edge on the path has no samples yet."
);
create_exception!(
    _core,
    TopologyChangedError,
    TfTreeError,
    "The tree was re-parented after this plan was compiled; re-plan."
);
create_exception!(
    _core,
    FrameNotDeclaredError,
    TfTreeError,
    "No such frame in this arena."
);
create_exception!(
    _core,
    BufferError,
    TfTreeError,
    "An output buffer was the wrong shape, dtype, or size."
);
create_exception!(
    _core,
    DerivativesUnavailableError,
    TfTreeError,
    "This edge's interpolator has no exact derivative; layout='quat_twist' \
     cannot be served over it."
);
create_exception!(
    _core,
    NoSegmentError,
    TfTreeError,
    "A pose exists at this stamp but there is no segment to differentiate; \
     layout='quat_twist' needs two samples spanning a non-zero interval."
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
    Ok(())
}

// ---------------------------------------------------------------------------
// Naming things
// ---------------------------------------------------------------------------

/// How this binding spells an edge: `edge "parent" -> "child"`.
///
/// **One spelling, and it is the one that already shipped.** `span`'s no-data
/// path has said `edge "base_link" -> "lidar"` since `docs/PHASE5.md` §4.2 and
/// `tests/python/test_frozen.py` pins that exact substring; this is that
/// spelling lifted to where every message can reach it, not a second one
/// (`docs/PROJECT.md` §6).
///
/// Quoted, because every other name this binding echoes back is quoted — `no
/// frame named "nope"` — and because a frame name may contain a space, which an
/// unquoted `edge map link -> base link` renders unreadable.
pub(crate) fn edge_label_of(parent: &str, child: &str) -> String {
    format!("edge {parent:?} -> {child:?}")
}

/// [`edge_label_of`] for an id, resolved against the arena the caller holds.
///
/// # The fallback says *why*, and that is the whole point of having one
///
/// `None` from [`named_edge_in`] means the arena has no usable record at that
/// index — the id is past the edge table, or an endpoint names a zeroed slot.
/// Printing `EdgeId(7)` there would be the defect this function exists to
/// remove, and printing nothing would lose the one fact still known. So the id
/// appears as `#7`, marked as an index rather than dressed up as a value, next
/// to the reason it could not be named. A fork-detached tree gets its own
/// reason because it is the one case where the *arena* is fine and the *handle*
/// is not: `Tree::view` substitutes a zeroed poison arena in the child, so every
/// name would read absent and "no record" would be a lie.
///
/// # `_in`, and why every caller here should prefer it
///
/// [`ArenaView`] is what actually resolves a name, and `Tree::view` rebuilds one
/// per call — `detached()`, `as_participant`, `with_liveness`, `is_writable`.
/// One message can name three frames ([`LookupError::Disconnected`]), so
/// [`lookup_err`] takes a single view for the whole message and these two
/// convenience wrappers exist for the callers that hold a `&Tree` and nothing
/// else.
pub(crate) fn edge_label(tree: &Tree, edge: EdgeId) -> String {
    edge_label_in(tree, &tree.arena_view(), edge)
}

/// [`edge_label`] against a view the caller already holds.
fn edge_label_in(tree: &Tree, view: &ArenaView<'_>, edge: EdgeId) -> String {
    match named_edge_in(view, edge) {
        Some((parent, child)) => edge_label_of(&parent, &child),
        None => format!("edge #{} ({})", edge.get(), nameless(tree)),
    }
}

/// A frame id as the caller's own name, quoted, with the same fallback.
///
/// **Bare, so it reads inside a sentence that supplies its own noun** — `no path
/// from "world_a" to "sensor_c"`. A message that wants the noun spelled out uses
/// [`frame_phrase_in`], which is not the same string with a prefix: the fallback
/// already begins `frame #7`, and prefixing that produced `frame frame #7 (name
/// unavailable: ...)`.
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

/// [`frame_label_in`] with the noun, for a sentence whose subject is the frame.
///
/// One function rather than a caller-side `format!("frame {}", ...)`, because
/// the two branches need *different* prefixes and only one of them is `frame `:
/// the resolved branch is a bare quoted name and the fallback already names
/// itself. That is the whole defect this exists to prevent, and it shipped.
fn frame_phrase_in(tree: &Tree, view: &ArenaView<'_>, frame: FrameId) -> String {
    match named_frame_in(view, frame) {
        Some(name) => format!("frame {name:?}"),
        None => format!("frame #{} ({})", frame.get(), nameless(tree)),
    }
}

/// The refusal for a name this arena has never interned.
///
/// **One spelling of a sentence that had five.** `Tree.plan`, `Tree.publisher`,
/// `Tree.span` and the module-level `push` each wrote
/// `format!("no frame named {name:?}")` inline, and `Tree.lookup` wrote a
/// sixth, different one about a hash. `docs/PROJECT.md` §6 is about paths, and
/// this is the same failure in prose: five copies of a remedy drift, and the
/// drift is invisible because no test reads two of them at once.
///
/// It has no callers outside this module now, and that is the point rather than
/// an accident: those five sites all resolve their names through
/// [`resolve_frame`], and `Tree.lookup`'s hash arm through
/// [`unknown_frame_err`], so the sentence is reached rather than repeated.
///
/// The remedy is here rather than in the `docs/PHASE3.md` §4.4 class docstring
/// because a Python traceback shows the message and not the class docstring.
fn frame_not_declared(name: &str) -> PyErr {
    FrameNotDeclaredError::new_err(format!(
        "no frame named {name:?} in this arena; if the name is spelled right, \
         its publisher has not declared it yet — wait for one, or declare it \
         on the builder that creates the arena"
    ))
}

/// Resolve a frame name for an entry point that holds one, **without interning**.
///
/// # `find_frame`, not `Tree::frame`, and the difference is a write
///
/// [`Tree::frame`] is not a read. Its last line on a writable tree is
/// `self.view().intern(name)`, which publishes a new `FrameRecord` into the
/// shared arena with a `compare_exchange` — so resolving a *typo* through it
/// spends a frame slot, permanently: ids are append-only and never recycled
/// (`docs/PROJECT.md` §5 D10). On an arena with headroom (the ROS ingest bridge
/// reserves eight, `tf_tree_bridge::config`) a loop of misspelled reads from one
/// `rw` participant exhausts it for *every* participant, and the bridge's own
/// legitimate `Tree::frame` then fails `CapacityExceeded`. `find_frame` is the
/// read-only half of the same probe and is what a query path wants.
///
/// It goes unnoticed on a tree with no headroom — `frame_headroom=0`, which is
/// the default and was the only setting the Python surface had — because the
/// intern then fails `intern_core`'s capacity pre-check before it touches
/// anything. So the write is invisible on the trees a test builds and permanent
/// on the arenas Python is pointed at.
///
/// # Errors
///
/// [`FrameNotDeclaredError`] for a name this arena has never interned, and the
/// base `TfTreeError` for the two failures that are *not* an absent name —
/// see [`unknown_frame_err`], which spells them.
pub(crate) fn resolve_frame(tree: &Tree, name: &str) -> PyResult<FrameId> {
    if tree.detached() {
        return Err(detached_err());
    }
    match tree.arena_view().find_frame(name) {
        Ok(Some(id)) => Ok(id),
        Ok(None) => Err(frame_not_declared(name)),
        Err(e) => Err(unresolvable_name(name, e)),
    }
}

/// Attribute a `LookupError::UnknownFrame` to one of the names the caller typed.
///
/// # Why the entry point does this and [`lookup_err`] cannot
///
/// `UnknownFrame` carries a BLAKE3 prefix and BLAKE3 does not invert, so the
/// error itself has nothing a caller can search their source for. `Tree.lookup`
/// takes the two names as `&str`, so it can ask the arena about each one.
///
/// # The three answers, and why they are not one
///
/// `Tree::lookup`'s `find` maps `Ok(None) | Err(_)` onto `UnknownFrame`
/// (`crates/tf_tree/src/tree.rs`), so this probe sees the same three outcomes
/// the engine collapsed — and they want opposite remedies:
///
/// * `Ok(None)` — never interned. Wait for the publisher, or declare it.
/// * `FrameHashCollision` — **a different name already occupies this name's
///   64-bit hash slot**, so this name can never be interned in this arena and
///   waiting is exactly the wrong advice. Renaming either frame is the fix.
/// * `InternContended` — a publisher is mid-intern behind an anonymous view;
///   the name is arriving. Retry.
///
/// An earlier revision sent all three through the "has not been declared yet;
/// wait for one" sentence, and its comment claimed a collision fell through to
/// the hash message instead — the routing and the comment were each wrong about
/// the other.
///
/// `None` back from the walk means both names resolve *now*, which is the race
/// the caller lost by a microsecond: a peer interned the name between the engine's
/// probe and this one. There is nothing to blame, so [`lookup_err`] reports the
/// hash it was given.
pub(crate) fn unknown_frame_err(tree: &Tree, names: [&str; 2], e: LookupError) -> PyErr {
    let view = tree.arena_view();
    for name in names {
        match view.find_frame(name) {
            Ok(Some(_)) => continue,
            Ok(None) => return frame_not_declared(name),
            Err(err) => return unresolvable_name(name, err),
        }
    }
    lookup_err(tree, e)
}

/// The two ways a name fails to resolve that are *not* "it was never declared".
///
/// Both raise the base `TfTreeError` rather than [`FrameNotDeclaredError`],
/// which is `docs/API.md` R5 read strictly: the exception *type* is the
/// contract, and a caller catching `FrameNotDeclaredError` around a lookup is
/// catching "this name is absent". Neither of these is that — one says the name
/// cannot exist here, the other says it is on its way.
fn unresolvable_name(name: &str, e: FrameError) -> PyErr {
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
        // `find_frame` returns only the two above — it never inserts, so it has
        // no capacity to exceed and no fork guard of its own. The arms below
        // are here because `FrameError` is `#[non_exhaustive]` and because the
        // two remaining variants have a *correct* answer rather than a
        // catch-all one: `ReadOnly` means the same thing as `Ok(None)` (the
        // name is absent and this participant may not declare it — its own doc
        // comment insists on that reading), and `CapacityExceeded` means it can
        // never be declared here either.
        FrameError::ReadOnly | FrameError::CapacityExceeded => frame_not_declared(name),
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
/// # The two entry points every program calls first shipped five Debug dumps
///
/// `BuildError`'s `thiserror` attributes are `#[error("topology error: {0:?}")]`
/// and four more like it, forwarded here as `format!("{e}")` — so
/// `tf_tree.build([("a","b"),("b","a")])`, which is a *typo-grade* mistake,
/// raised `topology error: WouldCreateCycle { child: FrameId(1) }`. That is both
/// things this module exists to keep out of a Python message at once, on the
/// call that runs before anything else in the program.
///
/// # Where the names come from, since the arena does not exist
///
/// There is no tree to resolve ids against — that is the whole difference from
/// [`lookup_err`] — but the caller passed the topology in *as strings*, so the
/// binding has every name it needs and reads them back out of that list. It
/// deliberately does **not** reconstruct the builder's id assignment (first-seen
/// order over `frames` then each edge's endpoints) to turn a `FrameId` into a
/// name: that order is an internal detail whose own comment says it "only
/// affects id assignment, not correctness", and a message that names the wrong
/// frame is worse than one that names none.
///
/// So the cycle is found in the caller's list rather than read out of the error,
/// which is also the only way to *show* it — the error carries one frame and a
/// cycle is a chain.
///
/// # Errors it cannot name
///
/// Three of `BuildError`'s variants describe the arena rather than the edge
/// list, and they say so in words instead of dumping the struct that carries
/// the detail. The exception is `Shm`, which follows [`crate::offline`]'s
/// `FrozenError::Arena` precedent for the identical reason: `ShmError` has no
/// `Display`, sixteen variants and several struct-shaped ones, so enumerating it
/// here would re-spell `tf_tree_arena`'s reasons in a file that cannot see them
/// change. Labelling the discriminant as raw is the honest option.
pub(crate) fn build_err(edges: &[(String, String)], capacity: u32, e: BuildError) -> PyErr {
    TfTreeError::new_err(match e {
        // Named from the list, not from the hash the error carries: the hash is
        // BLAKE3 of the child name and does not invert, while `build`'s own
        // duplicate check walks the edges in this order and stops at the same
        // pair this scan does.
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
        // `WouldCreateCycle` is the only member of `TopologyError` a caller can
        // provoke from here — the other two are index failures against an arena
        // this call just sized from these names — so the discriminator is
        // whether the caller's own list contains a cycle, and that also answers
        // *which* one. No `tf_tree_core` import buys anything the list does not
        // already say.
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
        // Reachable only as a genuine hash collision: `build` interns every name
        // in the list before wiring anything, into a table it sized from that
        // same list, so there is no capacity to exceed and no peer to contend
        // with. Neither name is recoverable — the error carries the hash and
        // BLAKE3 does not invert — so this says which *pair of names* is at
        // fault by describing them, and does not pretend to pick one.
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
        // Size, and only size. `LayoutError`'s other two members are a capacity
        // that is not a power of two — unreachable, because `Capacity::slots`
        // rounds *up* before the layout ever sees it — and a capacity-count
        // mismatch, which is `TreeBuilder`'s own bookkeeping.
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
             sized, mapped or sealed. The engine's reason, raw: {inner:?}"
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
/// Every arm of `OpenError` except two is already a sentence, so this forwards
/// their `Display`; the two are the ones that wrap a *different* error type.
/// `Build` is the whole of [`build_err`] — `open(create=[...])` is the second
/// entry point a program calls first, and it reaches every one of those five
/// Debug dumps through one `From` impl.
#[cfg(target_os = "linux")]
pub(crate) fn open_err(edges: &[(String, String)], capacity: u32, e: OpenError) -> PyErr {
    match e {
        OpenError::Build(inner) => build_err(edges, capacity, inner),
        // `#[error("{0:?}")]`, and the same argument as `BuildError::Shm`.
        OpenError::Map(inner) => TfTreeError::new_err(format!(
            "the arena's shared-memory segment was handed over but could not be \
             mapped. The engine's reason, raw: {inner:?}"
        )),
        // `Rendezvous`, `NoLayoutToCreate`, `ReadOnlyCannotCreate`,
        // `ArenaAlreadyLive`, `TakeoverUnsupported` — prose already, and the
        // last of them cannot arrive here at all besides: it needs a
        // `#[cfg(test)]` field of `tf_tree::Open`, which only that crate's own
        // test target can set. `IpcError`'s `Display` is the one place that
        // knows what a runtime directory or a refused handshake means.
        // Re-spelling it here would be a second copy that stops agreeing with
        // the first.
        other => TfTreeError::new_err(format!("{other}")),
    }
}

/// The first child two edges declare, as `(child, first parent, second parent)`.
///
/// Declaration order, which is `TreeBuilder::build`'s: it walks the same list in
/// the same direction and stops at the same pair, so the name this reports is
/// the one the error was raised about even though the error carries only a hash.
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

/// A cycle in the caller's edge list, spelled the way the list reads.
///
/// `"base" is under "sensor", which is under "base"` — the chain, because one
/// frame id is not a cycle and the caller has to see which pairs close it.
///
/// The walk is `child -> parent` and it stops at the frame it started from, so
/// what comes back is the cycle itself rather than the tail that leads into one.
/// A list with a duplicate child cannot get here: `build` rejects that first,
/// which is what makes a single `parent` per child well defined.
fn cycle_through(edges: &[(String, String)]) -> Option<String> {
    let parent: std::collections::HashMap<&str, &str> = edges
        .iter()
        .map(|(p, c)| (c.as_str(), p.as_str()))
        .collect();
    for (_, start) in edges {
        let mut chain: Vec<String> = Vec::new();
        let mut at = start.as_str();
        // Bounded by the number of distinct children: a walk longer than that
        // has revisited something, and the `== start` test below is what says
        // the revisited node is *this* one.
        for _ in 0..parent.len() {
            let Some(&up) = parent.get(at) else { break };
            chain.push(format!("{up:?}"));
            if up == start.as_str() {
                // `"b" is under "a", which is under "b"` — the first link needs
                // the verb and the rest need the comma, so this is not one join.
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

/// The error every entry point raises on a tree inherited across a `fork()`.
///
/// # Why a message rather than a class of its own
///
/// `TfTreeError` is the base, and this is deliberately not a new leaf: a
/// detached tree is not a condition a program branches on — it is not
/// retryable, not repairable, and the only response is to open a new tree in the
/// child. `docs/PHASE3.md` §4.4's hierarchy exists so a caller can *program*
/// against a distinction (which edge extrapolated, which frame is missing), and
/// there is nothing to program against here.
///
/// The message says what to do because the caller almost never typed `fork` —
/// `multiprocessing` did, and its default start method on Linux is what put
/// them here.
pub(crate) fn detached_err() -> PyErr {
    TfTreeError::new_err(DETACHED)
}

/// [`detached_err`]'s sentence, so [`push_msg`] can embed it rather than
/// re-word it. One spelling, three entry points.
const DETACHED: &str = "this tree was inherited across a fork(); the child's mapping is gone \
     and the handle cannot be repaired. Open a new tree in the child \
     (tf_tree.open(...)), or use multiprocessing's 'spawn' or 'forkserver' \
     start method";

/// Map a `LookupError` to its Python exception, keeping the structured detail.
///
/// `TopologyChanged` is the one a *correct* program routinely hits — a peer
/// re-parented the tree — so its message says what to do rather than only what
/// happened.
///
/// # Why it takes the tree
///
/// Because [`edge_label`] and [`frame_label`] do: nine of these arms carry an
/// `EdgeId` or a `FrameId`, and there is no other object in the process that
/// can turn one into the name the caller typed. Every call site in this crate
/// already had a `&Tree` in scope — `PyPlan` holds a `Py<PyTree>` for exactly
/// the lifetime reason that makes this sound, and the offline entry points take
/// one — so this parameter cost no plumbing, which is a fair summary of why the
/// ids were being printed raw: nothing was in the way.
///
/// **One [`ArenaView`] serves the whole message.** `Disconnected` names three
/// frames, and a per-label view made `Tree::view` re-run `detached()`,
/// `as_participant`, `with_liveness` and `is_writable` three times (four, with
/// the fallback's) to answer one question about one arena. It is an error path,
/// so this is tidiness rather than a measured win — but the `_in` split existed
/// already, for the enumerators, and not using it here was an oversight rather
/// than a trade.
///
/// # The variants are enumerated, and the wildcard below is the compiler's
///
/// Every arm is spelled out even where two share a sentence, so that the
/// question "what does Python say for *this* failure" has exactly one place to
/// look.
///
/// What survives at the bottom is `LookupError`'s `#[non_exhaustive]`, and it
/// is the compiler's arm and not a choice. Deleting it gives:
///
/// ```text
/// error[E0004]: non-exhaustive patterns: `_` not covered
///   --> src/errors.rs:236:11
///    = note: `LookupError` is marked as non-exhaustive, so a wildcard `_` is
///            necessary to match exhaustively
/// ```
///
/// — which is also the check that the enumeration above is *complete*: rustc
/// named no missing variant, only the wildcard. So a new `LookupError` variant
/// still cannot be made a compile error **here**; that check has to live where
/// the enum does. `tf_tree_core::plan::InterpPolicy` shows the repository has
/// already weighed exactly this trade and gone the other way — its doc comment
/// drops `#[non_exhaustive]` *precisely* so every consumer breaks at compile
/// time, and judges that worth a major version bump. Applying the same
/// reasoning to `LookupError` is a decision record, not a binding change.
///
/// What the wildcard no longer does is *print a Rust struct literal at a Python
/// user*. `format!("{other:?}")` shipped `NoSegment { edge: EdgeId(3) }` as if
/// it were a sentence; the Debug is still there because it is the only
/// information a build in this state has, but it is now labelled as the
/// binding's bug rather than presented as the answer.
pub(crate) fn lookup_err(tree: &Tree, e: LookupError) -> PyErr {
    let view = tree.arena_view();
    match e {
        LookupError::Extrapolation {
            edge,
            requested,
            oldest,
            newest,
        } => ExtrapolationError::new_err(format!(
            "{}: stamp {requested} ns is outside the retained history \
             [{oldest}, {newest}] ns",
            edge_label_in(tree, &view, edge)
        )),
        LookupError::Disconnected {
            target,
            source,
            cut_at,
        } => DisconnectedError::new_err(format!(
            "no path from {} to {}; the chain stops at {}",
            frame_label_in(tree, &view, source),
            frame_label_in(tree, &view, target),
            frame_label_in(tree, &view, cut_at),
        )),
        LookupError::NoData { edge } => NoDataError::new_err(format!(
            "{} has no samples yet",
            edge_label_in(tree, &view, edge)
        )),
        LookupError::TopologyChanged { plan, current } => TopologyChangedError::new_err(format!(
            "this plan was compiled at topology generation {plan}, the tree is \
             now at {current}; call tree.plan(...) again"
        )),
        // **The last resort, and the entry points that can do better do.**
        // `UnknownFrame` carries a BLAKE3 prefix and BLAKE3 does not invert, so
        // there is no name to recover *here*; `PyTree::lookup` — the one caller
        // that still has both names as strings — attributes the failure itself
        // and reaches this only when neither name is the missing one, which
        // means a peer interned it between the two reads.
        LookupError::UnknownFrame { hash } => {
            FrameNotDeclaredError::new_err(format!(
                "no frame with hash {hash:#x} in this arena; if the name is spelled right, its publisher has not declared it yet — wait for one, or declare it on the builder that creates the arena"
            ))
        }
        LookupError::BufferTooSmall { need, got } => BufferError::new_err(format!(
            "output buffer holds {got} elements; this batch needs {need}"
        )),
        // **The two refusals `layout="quat_twist"` adds over the pose layouts,
        // and both get a type rather than a message** — `docs/API.md` R5 makes
        // the exception *type* the contract, and each of these is a distinct
        // decision a caller makes.
        //
        // `DerivativesUnavailable` is a property of an **edge**: `LerpSlerp` is
        // `tf2`'s interpolator and has no exact body twist, so an edge that
        // declares it is refused rather than finite-differenced
        // (`docs/PHASE5.md` §4.4 item 1). It fires at element 0 of any batch and
        // the fix is a re-declaration or a pose layout — permanent for the life
        // of the arena.
        LookupError::DerivativesUnavailable { edge, interp } => {
            DerivativesUnavailableError::new_err(format!(
                "{} declares {}, which has no exact derivative; use \
                 layout='quat' or declare the edge interp='sclerp'",
                edge_label_in(tree, &view, edge),
                stored_interp(interp),
            ))
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
        LookupError::NoSegment { edge } => NoSegmentError::new_err(format!(
            "{} has a pose at this stamp but no segment to differentiate: it \
             retains one sample, or the two bracketing samples carry equal \
             stamps. Publish another sample, or use layout='quat'",
            edge_label_in(tree, &view, edge)
        )),
        // Routed through the shared spelling so a fork victim gets the same
        // sentence whether it arrived through `lookup` or through `frames`.
        LookupError::ChildDetached => detached_err(),

        // --- Below here: the arms the deleted `other =>` used to swallow. ---
        //
        // **They all raise the base `TfTreeError`, and that is a preservation
        // rather than a judgement.** `docs/API.md` R5 makes the exception
        // *type* the contract and the prose explicitly not; every one of these
        // reached Python as a bare `TfTreeError` for the whole of Phases 3–5,
        // so giving one a leaf class here would be an API change smuggled into
        // a message fix. Two of them have an obvious home —
        // `FrameOutOfRange`/`MissingEdge` are arguably `FrameNotDeclaredError`,
        // and `MixedTimeDomains` is arguably a `Disconnected`-shaped refusal —
        // and both are noted as decision-record material rather than taken here.

        // **"depth {depth} exceeds the maximum of {MAX_DEPTH}" would be false**,
        // and it is what the obvious phrasing produces. `plan::compile` reports
        // `nt + ns` — the steps it had *already collected* when the fixed array
        // filled — so at the refusal the number equals the bound rather than
        // exceeding it (`crates/tf_tree_core/src/tests.rs` asserts exactly
        // `TreeTooDeep { depth: MAX_DEPTH }`). What is true is that the walk
        // needed more, and that is what this says.
        LookupError::TreeTooDeep { depth } => TfTreeError::new_err(format!(
            "this path needs more than the {} steps a compiled plan holds; the \
             walk had collected {depth} when it stopped. Real trees are 4–8 \
             deep — re-parent so the two frames share a nearer ancestor",
            tf_tree::MAX_DEPTH
        )),
        // Recycled and Contended are both "the ring beat the reader", and both
        // are retryable, but they are *not* the same advice: a lap means the
        // history the reader wanted is gone and a retry re-reads a newer
        // window, while a contended slot means a writer held it mid-update and
        // a retry reads the same stamps. Naming the wrong one sends a caller to
        // resize a ring that is not the problem.
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
        // The two time-domain refusals (D9). Domains are stamped as small
        // integers in the arena and the Python surface has no name for them
        // yet, so the number is reported as a domain *tag* rather than
        // pretending to be a clock name.
        LookupError::TimeDomainMismatch { expected, got } => TfTreeError::new_err(format!(
            "this plan was compiled for time domain {expected}; the query \
             supplied a stamp in domain {got}. A stamp from one clock cannot \
             address an edge sampled on another"
        )),
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
        // **Not routed through [`edge_label_in`], for the reason the
        // `FrameOutOfRange` arm below gives**: its fallback would produce
        // `edge #7 (name unavailable: this arena holds no record at that index)
        // names no usable edge in this arena`, which is one fact said three
        // times. The two causes want different sentences anyway, and only one
        // of them can be named.
        LookupError::UnknownEdge { edge } => TfTreeError::new_err(
            match named_edge_in(&view, edge) {
                // `Plan::sampler` answers `None` for an edge whose capacity is
                // zero as well as for one past the table, so a *named* edge
                // here is a dynamic step over a static or tombstoned record —
                // the arena changed under a compiled plan.
                Some((parent, child)) => format!(
                    "{} carries no sample ring, so the step that names it \
                     cannot be evaluated: this arena records the edge as static \
                     or tombstoned. Re-compile with tree.plan(...)",
                    edge_label_of(&parent, &child)
                ),
                // Nothing to name, and the id being out of range *is* the whole
                // answer, so it is stated once.
                None if tree.detached() => return detached_err(),
                None => format!(
                    "edge id {} is past the end of this arena's edge table, so \
                     the plan naming it was compiled against a different arena",
                    edge.get()
                ),
            },
        ),
        // The one id deliberately *not* sent through `frame_label`: this error
        // means the id is out of range for the frame table, so resolving it can
        // only ever produce the fallback, and `frame #99 (name unavailable:
        // this arena holds no record at that index) is out of range` says the
        // same thing twice.
        LookupError::FrameOutOfRange { frame } => TfTreeError::new_err(format!(
            "frame id {} is out of range for this arena's frame table",
            frame.get()
        )),
        // [`frame_phrase_in`] and not `format!("frame {}", frame_label(..))`:
        // the fallback branch already begins `frame #7`, and prefixing it gave
        // `frame frame #7 (name unavailable: ...)`.
        LookupError::MissingEdge { child } => TfTreeError::new_err(format!(
            "{} has a parent in the topology but no edge records the link, so \
             the path through it cannot be evaluated",
            frame_phrase_in(tree, &view, child)
        )),
        // Not reachable from Python today — the binding picks the entry point
        // from `layout=` itself and never crosses the f32/f64 pair — which is
        // exactly why it says so instead of inventing advice for the caller.
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

/// Name a stored [`InterpPolicy`] discriminant the way `interp=` spells it.
///
/// # Why the round trip is checked
///
/// [`InterpPolicy::from_u8`] collapses an unknown discriminant onto the default
/// — deliberately, so an older binary can read a newer arena (its doc comment
/// makes the argument). That is right for the *fold* and wrong for a *message*:
/// an arena written by a future build could make this arm say "declares
/// interp='sclerp', which has no exact derivative", which is self-contradictory
/// — ScLerp is the policy that *does* have one. So the number is echoed
/// unresolved unless it survives the round trip.
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

/// Map a failed `push` onto Python, naming the edge the caller claimed.
///
/// `edge` is a label from [`edge_label_of`], not an id: a [`PushError`] is
/// raised through a `Publisher`, and a publisher was created from the two frame
/// *names* the caller typed. Resolving an id back into those names through the
/// arena would be a slower way to reach a worse answer — the caller's own
/// spelling is what they will search their source for.
///
/// Every arm raises the base `TfTreeError` — which is what the `format!
/// ("{e:?}")` this replaces raised — so the message is split out as
/// [`push_msg`] for `push_many`, which prefixes the sample index and must not
/// re-word the rest.
pub(crate) fn push_err(edge: &str, e: PushError) -> PyErr {
    TfTreeError::new_err(push_msg(edge, e))
}

/// [`push_err`]'s sentence.
///
/// The wildcard is `PushError`'s `#[non_exhaustive]`, on the same terms as
/// [`lookup_err`]'s.
pub(crate) fn push_msg(edge: &str, e: PushError) -> String {
    match e {
        // The common one by a wide margin, and the one whose Debug spelling
        // (`NonMonotonicStamp { last: 1000, got: 500 }`) was the worst of the
        // set: it reads like a struct a caller could catch and inspect, and
        // there is no such object on the Python side.
        //
        // Equal stamps are *accepted* (invariant 6), so the message says
        // "older than", not "not newer than" — a caller who reads the stricter
        // sentence goes looking for a de-duplication bug that is not there.
        //
        // **The variant name is back, and it is a search key rather than a
        // dump.** `docs/RUNBOOK.md`'s section on this failure is *headed* with
        // it, and that section is the one that says a burst on a wall-clock
        // edge is usually a `CLOCK_REALTIME` step and not the publisher's
        // fault. Scrubbing the word left an operator holding a sentence with no
        // token in it that finds the page explaining it — which is the same
        // mistake as printing `EdgeId(3)`, in the other direction: an id nobody
        // can use, replaced by prose nobody can look up. The braces stay off,
        // because it is the heading that has to be findable, not the struct.
        PushError::NonMonotonicStamp { last, got } => format!(
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
/// # Why this does not simply forward `ClaimApiError`'s `Display`
///
/// It nearly could: `ClaimApiError` is `thiserror`-derived and every arm is
/// already a sentence. Three of them spell the edge `{edge:?}`, which is
/// `EdgeId(3)` — the defect this module exists to keep out of Python — and
/// four more identify a frame by its raw index. Both are reasonable in Rust,
/// where a caller holds `EdgeId`s and can look them up; neither is reachable
/// from Python, where the caller holds the two strings they passed to
/// `tree.publisher(child, parent)` and nothing else.
///
/// So the arms are re-spelled around those two names rather than around ids.
/// Everything raises the base `TfTreeError`, which is what the previous
/// `format!("{e}")` raised for all of them — `docs/API.md` R5 again: the type
/// is the contract, the prose is not.
pub(crate) fn claim_err(tree: &Tree, parent: &str, child: &str, e: ClaimApiError) -> PyErr {
    let edge = edge_label_of(parent, child);
    match e {
        ClaimApiError::ChildDetached => detached_err(),
        // The three CAS-versus-lease races of `docs/decisions/0005` §5. All
        // three are transient and all three back their state out before
        // returning, so all three say "retry" — but they are kept apart
        // because a caller who sees the middle one has a *lock file* problem
        // (a full filesystem, a `fcntl` refusal) and no amount of retrying
        // fixes it.
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
        // Pre-empted in practice: `Tree.publisher` resolves both names through
        // `Tree::frame` first and raises `FrameNotDeclaredError` there, so a
        // Python caller reaches this only if the frame table changed between
        // the two calls. The message still names the frame, because a caller
        // who does hit it is looking at a race and needs to know which side.
        ClaimApiError::UnknownFrame { .. } => {
            TfTreeError::new_err(format!("{child:?} is not a frame of this tree"))
        }
        // **Where a reversed pair lands whenever the child is a root**, which
        // is most of the time — so this arm, not just `ParentMismatch`, has to
        // say which argument is which. `publisher(map, base)` on a `map ->
        // base` tree reaches here, and "no edge attaches map to a parent" is
        // true, unhelpful, and does not mention the other name the caller
        // typed.
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
        // The only arm that reports what the arena says *instead of* what was
        // asked for, and it has to: the whole failure is that the two differ,
        // and repeating the requested parent would not show it. So `actual` is
        // resolved to a name — it is the single most useful fact in the error
        // and the only one the caller did not already type.
        ClaimApiError::ParentMismatch { actual, .. } => TfTreeError::new_err(format!(
            "{child:?} is not attached to {parent:?} but to {}; an edge names \
             the frame it moves, and reversing the pair is the usual cause \
             (the call is publisher(child, parent))",
            // `FrameId::new(0)` is `None` and index 0 is the "no parent"
            // sentinel, so a root reads as a root instead of as `frame #0`.
            match FrameId::new(actual) {
                Some(f) => frame_label(tree, f),
                None => "the root (no parent)".to_owned(),
            }
        )),
        ClaimApiError::AlreadyClaimed(inner) => TfTreeError::new_err(format!(
            "{edge}: already claimed by participant slot {}. One writer per \
             edge (invariant 4): the other publisher must release it, or be \
             reaped, first",
            claimed_by(inner)
        )),
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

/// The participant slot holding a claim, for [`claim_err`]'s message.
///
/// A slot, **not a pid**: amendment A3 made the claim word an indirection into
/// the participant table, and the number is only useful next to `tf_tree
/// doctor`, which prints both. Saying "pid" here would send an operator to
/// `kill` an unrelated process.
///
/// `ClaimError` is `#[non_exhaustive]` and today has one variant; a later one
/// that carries no slot gets the word rather than a number, because `0` is a
/// real participant slot and printing it as a stand-in would name an innocent
/// process.
fn claimed_by(e: tf_tree::ClaimError) -> String {
    match e {
        tf_tree::ClaimError::EdgeAlreadyClaimed { owner_slot } => owner_slot.to_string(),
        _ => "(unknown)".to_owned(),
    }
}
