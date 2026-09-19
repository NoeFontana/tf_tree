//! `Tree::frames` / `Tree::edges` — the stable answer to "what is in this
//! tree" (`docs/API.md` §2.6 row 4, §3.2). The trees have frame headroom, edge
//! headroom, a runtime-interned frame and no reversed edge pairs, so a wrong
//! walk fails.
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

/// The declared frames, in `FrameId` order, and nothing else. In a
/// single-process tree the `count` bound and the `name_hash != 0` filter are
/// mutually redundant, so no test here separates them.
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

/// `(parent, child)` pairs, in `EdgeId` order, with no sentinel and no headroom;
/// no pair is another's reverse, so a field swap fails.
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

/// `frame` and `frames` agree: enumerated ids resolve to `1..=len` (`docs/API.md` §2.6).
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
