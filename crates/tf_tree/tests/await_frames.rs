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

/// **The crux.** A heap tree is writable, so `Tree::frame` would intern an
/// undeclared name on demand and hand back an id for a frame nobody declared.
/// `await_frames` refuses instead — and refuses *before* any sleep, which the
/// elapsed-time assertion is what pins.
///
/// **Mutant: build the predicate on `Tree::frame`** ⇒ `await_frames` returns
/// `Ok([FrameId(1), FrameId(2)])` for `"nobody_declared_this"`, a confident
/// wrong answer, and this test fails on the `assert_eq!`.
///
/// **Mutant: drop the `is_writable` guard** ⇒ the poll on `find_frame` never
/// resolves and the call takes the full five seconds, failing the elapsed
/// assertion rather than the equality one.
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

/// The listing is **bounded**: `Display` on a wide tree must not allocate one
/// `String` per frame to render one error line.
///
/// **Mutant: drop the `names.truncate(SHOWN)`** ⇒ `frame_11` appears and the
/// last assertion fails.
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
