//! `docs/decisions/0019` §2b's frames wait, and the `Described` context that
//! goes with it — the half of both that needs no shared arena.
//!
//! **Deliberately not in `tests/rendezvous.rs`.** That target carries
//! `required-features = ["shm"]` and runs only under `just shm-rendezvous`;
//! everything here runs under plain `just test`, because
//! `cargo nextest run --workspace` builds default features. The writable-tree
//! refusal in particular is the assertion `0019`'s plan calls the crux — a wait
//! built on `Tree::frame` would return `Ok` with a freshly interned id for a
//! name nobody declared — and it must not be gated behind a feature a
//! contributor may never enable.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use tf_tree::{AwaitError, Capacity, EdgeCfg, Iso3, LookupError, Stamp, SystemDomain, TreeBuilder};

/// A stamp in the default domain. Method-call inference does not apply a type
/// parameter's default, so the binding is annotated once here rather than at
/// every call.
fn stamp() -> Stamp<SystemDomain> {
    Stamp::from_nanos(0)
}

/// **The crux, and it pins the *guard* — not the predicate.** A heap tree is
/// writable, so `Tree::frame` would intern an undeclared name on demand and hand
/// back an id for a frame nobody declared. `await_frames` refuses instead, and
/// refuses *before* any sleep.
///
/// **What this test cannot see.** A previous revision of this note claimed the
/// killer here was "build the predicate on `Tree::frame`". It is not, and the
/// mutation was run: with `match self.frame(name)` substituted for
/// `view.find_frame(name)` and the `is_writable` guard untouched, this target
/// reports *"5 tests run: 5 passed"*. The guard returns `WritableTree` before
/// the predicate is reached, so on the only tree a default build can construct
/// the predicate is unreachable. Killing the predicate mutant needs a *read-only*
/// handle, and that needs a live shared arena:
/// `a_consumer_waits_for_a_frame_interned_after_the_arena_exists` in
/// `tests/rendezvous.rs` is the test that does it (verified — it fails
/// `Frame(ReadOnly)`), and `just shm-rendezvous` is its only gate.
///
/// **Mutant: `if false && self.is_writable()`** ⇒ verified. `find_frame` answers
/// `None` forever for the undeclared name, so the call runs the full budget:
/// *"FAIL [5.007s] … left: Err(Timeout { hash: 13827985020167223838 }), right:
/// Err(WritableTree)"*, with `elapsed 5.000076187s` in the message.
/// `await_frames_of_nothing_is_not_a_special_case` fails alongside it, on
/// *"left: Ok([]), right: Err(WritableTree)"*.
///
/// **Mutant: keep the guard but move it inside the deadline branch**, so the
/// refusal is issued only after the budget is spent ⇒ verified. The equality
/// assertion now *passes* — the answer is still `WritableTree` — and the
/// elapsed bound is what fires: *"the refusal is a property of the handle and
/// must not cost a poll: 5.000065135s"*. That is the assertion the second
/// `assert!` exists for, and the reason it is not redundant with the first.
#[test]
fn await_frames_on_a_writable_tree_is_refused_immediately() {
    let tree = TreeBuilder::new()
        .static_edge("map", "odom", &Iso3::IDENTITY)
        // Headroom, so the last assertion measures the *refusal* being free
        // rather than the frame table being full.
        .frame_headroom(1)
        .build()
        .unwrap();
    assert!(
        tree.is_writable(),
        "a heap tree is writable by construction"
    );

    let started = Instant::now();
    let got = tree.await_frames(["map", "nobody_declared_this"], Duration::from_secs(5));
    let elapsed = started.elapsed();

    assert_eq!(got, Err(AwaitError::WritableTree), "elapsed {elapsed:?}");
    assert!(
        elapsed < Duration::from_millis(10),
        "the refusal is a property of the handle and must not cost a poll: {elapsed:?}"
    );
    // And the alternative the error points at does work, which is why refusing
    // costs a writable caller nothing.
    assert!(tree.frame("nobody_declared_this").is_ok());
}

/// A zero-length request is answered without touching the arena, and without a
/// `FrameId` placeholder leaking out of the conversion helper.
#[test]
fn await_frames_of_nothing_is_not_a_special_case() {
    let tree = TreeBuilder::new()
        .static_edge("map", "odom", &Iso3::IDENTITY)
        .build()
        .unwrap();
    // Still refused: the refusal is about the tree, not about the request.
    assert_eq!(
        tree.await_frames([], Duration::from_secs(5)),
        Err(AwaitError::WritableTree)
    );
}

/// `Described` cannot name the frame that was asked for — the error carries a
/// BLAKE3 prefix and BLAKE3 does not invert — but it holds the `&Tree`, so it
/// can name the frames that *do* exist.
///
/// **`docs/API.md` R5 makes message text not a contract**, so this pins the
/// presence of resolved context and of a remedy, never the wording.
///
/// **Mutant: revert the arm to `write!(f, "unknown frame (name hash {:#018x})")`**
/// ⇒ the `msg.contains("odom")` assertion fails.
#[test]
fn an_undeclared_frame_describes_the_tree_it_is_not_in() {
    let tree = TreeBuilder::new()
        .static_edge("map", "odom", &Iso3::IDENTITY)
        .static_edge("odom", "base_link", &Iso3::IDENTITY)
        .build()
        .unwrap();

    let err = tree.lookup("map", "base_lnik", stamp());
    let err = err.unwrap_err();
    assert!(matches!(err, LookupError::UnknownFrame { .. }), "{err:?}");

    let msg = tree.describe(err).to_string();
    for name in ["map", "odom", "base_link"] {
        assert!(
            msg.contains(name),
            "the description names no frame the tree actually has: {msg}"
        );
    }
    assert!(
        msg.contains("await_frames"),
        "the description offers no remedy: {msg}"
    );
    assert!(
        !msg.contains("tf_treed"),
        "the remedy names a program that does not exist: {msg}"
    );
}

/// The rendered **text** is bounded at eight names and sorted.
///
/// **It is the text, not the allocation.** `Described` reaches the frame list
/// through `Tree::frames`, which allocates one `String` per frame and sorts all
/// of them before `truncate` runs — so the truncation saves rendering, not
/// memory, and this test asserts only what is true. See the arm's comment in
/// `tree.rs` for why that allocation is an accepted cost on an error path.
///
/// **Mutant: drop the `names.truncate(SHOWN)`** ⇒ verified. *"the listing is
/// unbounded: … this tree has frame_00, … frame_11, hub, … (13 total)"*, on the
/// `!msg.contains("frame_11")` assertion.
///
/// **Mutant: drop the `names.sort_unstable()`** ⇒ verified, and it is the
/// **first** assertion that catches it, not the `frame_11` one. Intern order
/// puts `hub` first (it is the parent of every edge, so it is interned before
/// `frame_00`), which shifts the window down by one: *"the first eight sorted
/// names should be listed: … this tree has hub, frame_00, … frame_06, … (13
/// total)"*. `frame_11` is still absent, so the bound assertion alone would have
/// passed the mutant — which is why the `frame_07` half of the first assertion
/// is load-bearing and not decoration.
#[test]
fn the_described_frame_listing_is_bounded_and_sorted() {
    let mut b = TreeBuilder::new();
    // Twelve frames, named so that sorting is observable and so that the four
    // that must be cut are lexically last.
    for i in 0..12 {
        b = b.dynamic_edge(
            "hub",
            &format!("frame_{i:02}"),
            EdgeCfg::new(Capacity::slots(4)),
        );
    }
    let tree = b.build().unwrap();

    let err = tree.lookup("hub", "not_here", stamp()).unwrap_err();
    let msg = tree.describe(err).to_string();

    assert!(
        msg.contains("frame_00") && msg.contains("frame_07"),
        "the first eight sorted names should be listed: {msg}"
    );
    assert!(!msg.contains("frame_11"), "the listing is unbounded: {msg}");
    assert!(
        msg.contains("13 total"),
        "a truncated listing must say how many there were: {msg}"
    );
}

/// An empty tree is a different situation and says so: "known frames: (none)"
/// reads as a broken lookup, when what happened is that nothing has been
/// declared yet — the case the wait exists for.
#[test]
fn an_empty_tree_says_it_is_empty_rather_than_listing_nothing() {
    let tree = TreeBuilder::new().build().unwrap();
    assert!(tree.frames().unwrap().is_empty());

    let err = tree.lookup("map", "odom", stamp()).unwrap_err();
    let msg = tree.describe(err).to_string();
    assert!(
        msg.contains("no frames yet"),
        "an empty tree should say so: {msg}"
    );
    assert!(
        msg.contains("await_frames"),
        "the description offers no remedy: {msg}"
    );
}
