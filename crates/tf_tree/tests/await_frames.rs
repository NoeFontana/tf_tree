//! `docs/decisions/0019` §2b's frames wait, and the `Described` context that
//! goes with it. Runs under plain `just test`, unlike `tests/rendezvous.rs`
//! (`shm` only).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use tf_tree::{AwaitError, Capacity, EdgeCfg, Iso3, LookupError, Stamp, SystemDomain, TreeBuilder};

/// A stamp in the default domain.
fn stamp() -> Stamp<SystemDomain> {
    Stamp::from_nanos(0)
}

/// Pins the writable-tree *guard*, not the predicate: a heap tree is writable,
/// so `await_frames` refuses with `WritableTree` before any sleep. The predicate
/// is covered by `a_consumer_waits_for_a_frame_interned_after_the_arena_exists`
/// in `tests/rendezvous.rs`.
#[test]
fn await_frames_on_a_writable_tree_is_refused_immediately() {
    let tree = TreeBuilder::new()
        .static_edge("map", "odom", &Iso3::IDENTITY)
        // Headroom, so the last assertion measures the refusal, not a full table.
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
    // The alternative the error points at works.
    assert!(tree.frame("nobody_declared_this").is_ok());
}

/// A zero-length request is refused too, without touching the arena.
#[test]
fn await_frames_of_nothing_is_not_a_special_case() {
    let tree = TreeBuilder::new()
        .static_edge("map", "odom", &Iso3::IDENTITY)
        .build()
        .unwrap();
    assert_eq!(
        tree.await_frames([], Duration::from_secs(5)),
        Err(AwaitError::WritableTree)
    );
}

/// `Described` names the frames that exist (`docs/API.md` R5: presence of
/// context and a remedy, never the wording).
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

/// The rendered text is bounded at eight names and sorted.
#[test]
fn the_described_frame_listing_is_bounded_and_sorted() {
    let mut b = TreeBuilder::new();
    // Twelve frames; the four cut are lexically last.
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

/// An empty tree says "no frames yet" rather than listing nothing.
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
