//! `Tree::frames` / `Tree::edges` — the **stable** answer to "what is in this
//! tree" (`docs/API.md` §2.6 row 4, §3.2's Python mirror).
//!
//! These live in their own binary rather than in `construction.rs` because they
//! are the one part of the facade's read surface that walks the arena's tables
//! by index. Every assertion below is written so that it fails when the walk is
//! wrong, not merely when it is absent: the tree each test builds has frame
//! headroom, edge headroom, a runtime-interned frame and a chain in which no
//! edge is another's reverse.
//!
//! **Each test's doc comment lists the mutants that were actually run against
//! it and what they printed — including two that survived.** A single-process
//! tree cannot separate the frame walk's three filters from each other, because
//! each of them alone already excludes what the other two exclude; that is
//! recorded at `frames_lists_exactly_the_declared_frames_in_id_order` rather
//! than papered over.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use tf_tree::{Capacity, EdgeCfg, Iso3, Tree, TreeBuilder};

/// A tree with headroom in both tables, so a walk that used the *table* bound
/// instead of the *count* bound reports slots that are not frames or edges.
fn tree() -> Tree {
    TreeBuilder::new()
        .static_edge("map", "odom", &Iso3::IDENTITY)
        .dynamic_edge("odom", "base", EdgeCfg::new(Capacity::slots(8)))
        .dynamic_edge("base", "lidar", EdgeCfg::new(Capacity::slots(8)))
        .frame_headroom(4)
        .edge_headroom(3)
        .build()
        .unwrap()
}

/// The declared frames, in `FrameId` order, and **nothing else**.
///
/// # What was mutated, and what each mutant did
///
/// Run against this file, reverted after each:
///
/// * `1..=count` → `1..count` ⇒ FAIL, `left: ["map", "odom", "base"]`. The upper
///   bound is checked in the direction that drops a frame.
/// * `stored_name`'s `&bytes[..n]` → `bytes` ⇒ FAIL,
///   `left: ["map\0\0…", …]`. A name is `name_len` bytes, not the 48-byte
///   record field.
/// * `1..=count` → `1..=count + 4`, walking into the headroom ⇒ **PASS, all
///   five tests.** Recorded because it is the interesting one: the
///   `name_hash != 0` filter below already drops a zeroed headroom slot, so on
///   a quiescent single-process tree the bound and the filter are *mutually
///   redundant* and no test here can separate them. Deleting the filter **as
///   well** ⇒ FAIL, `left: ["map", "odom", "base", "lidar", "", "", "", ""]`.
///   The pair is defence in depth against a concurrent interner, which is a
///   condition this binary does not create; `just shm-check`'s multiprocess
///   targets are where that would have to be exercised.
/// * `0..=count` ⇒ **PASS.** `FrameId::new(0)` declines the root sentinel, so
///   the lower bound is redundant with it in the same way. Stated rather than
///   dressed up as a caught mutant.
#[test]
fn frames_lists_exactly_the_declared_frames_in_id_order() {
    let t = tree();
    assert_eq!(
        t.frames().unwrap(),
        vec![
            "map".to_owned(),
            "odom".to_owned(),
            "base".to_owned(),
            "lidar".to_owned()
        ],
        "declaration order is FrameId order, the sentinel is not a frame, and \
         headroom slots are not frames"
    );
}

/// A frame interned after `build()` appears, at the end.
///
/// This is the half of the contract a build-time-only walk satisfies by
/// accident: `frame_count` is bumped at intern time, so a walk that cached a
/// count or read `max_frames` would answer the same list before and after.
#[test]
fn frames_includes_a_frame_interned_after_build() {
    let t = tree();
    let before = t.frames().unwrap();
    let _ = t.frame("camera").unwrap();
    let after = t.frames().unwrap();

    assert_eq!(
        after.len(),
        before.len() + 1,
        "interning one name adds exactly one entry: {before:?} -> {after:?}"
    );
    assert_eq!(
        after.last().map(String::as_str),
        Some("camera"),
        "an interned frame takes the next id, so it lands last"
    );
    assert_eq!(&after[..before.len()], &before[..], "and moves nothing");
}

/// `(parent, child)` pairs, in `EdgeId` order, with no sentinel and no headroom.
///
/// The pair order is asserted against a topology where every edge's parent and
/// child differ *and no pair is the reverse of another*, so swapping the two
/// fields of the tuple is a failure rather than a permutation of the same list.
///
/// # What was mutated
///
/// * `out.push((parent, child))` → `(child, parent)` ⇒ FAIL,
///   `left: [("odom", "map"), ("base", "odom"), ("lidar", "base")]`.
/// * the `let … else { continue }` that drops an edge whose endpoint does not
///   resolve → `name(…).unwrap_or_default()` ⇒ FAIL,
///   `left: [… , ("", "")]`, and `no_enumeration_reports_an_empty_name` fails
///   with it. That is the `edge_headroom(3)` slot arriving as a pair of empty
///   names, which is what the drop exists to prevent.
#[test]
fn edges_lists_parent_child_pairs_in_id_order() {
    let t = tree();
    assert_eq!(
        t.edges().unwrap(),
        vec![
            ("map".to_owned(), "odom".to_owned()),
            ("odom".to_owned(), "base".to_owned()),
            ("base".to_owned(), "lidar".to_owned()),
        ],
        "parent first, declaration order, no sentinel and no headroom slot"
    );
}

/// Neither list ever contains an empty name.
///
/// Stated separately from the two `assert_eq!`s above because it is the
/// property that survives a topology change: whatever the tree is, a zeroed slot
/// read as a frame shows up as `""`, and that is the shape of every failure mode
/// this walk has.
#[test]
fn no_enumeration_reports_an_empty_name() {
    let t = tree();
    let _ = t.frame("late").unwrap();

    assert!(
        t.frames().unwrap().iter().all(|n| !n.is_empty()),
        "frames: {:?}",
        t.frames().unwrap()
    );
    assert!(
        t.edges()
            .unwrap()
            .iter()
            .all(|(p, c)| !p.is_empty() && !c.is_empty()),
        "edges: {:?}",
        t.edges().unwrap()
    );
}

/// The stable tier answers `frame` and `frames` consistently.
///
/// `Tree::frame(name)` is the singular and this is the plural; a caller that
/// enumerates and then resolves must get ids `1..=len`. That is what makes the
/// list usable without `Tree::arena_view`, which is the whole reason these two
/// methods are on the stable surface (`docs/API.md` §2.6).
#[test]
fn every_enumerated_name_resolves_to_its_position() {
    let t = tree();
    for (i, name) in t.frames().unwrap().iter().enumerate() {
        let id = t.frame(name).unwrap();
        assert_eq!(
            id.get(),
            i as u32 + 1,
            "{name} is at index {i}, so it must be FrameId({})",
            i + 1
        );
    }
}
