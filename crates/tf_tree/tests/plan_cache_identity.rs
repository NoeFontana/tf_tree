//! Issue #196: the per-thread plan cache behind [`tf_tree::Tree::lookup`] must
//! not serve one tree's compiled plan to another.
//!
//! The cache is shared by every `Tree` on the thread, and trees built from the
//! same names in the same order agree on frame ids and generation (a fresh tree's
//! generation is its edge count, pinned by
//! [`a_fresh_trees_generation_is_its_edge_count`]), so its key must carry the
//! arena. Shapes, in increasing severity: static plans (folded numbers are stolen),
//! dynamic plans with identical topology (passes even with the defect; guards a
//! too-coarse fix), differing topology, rebuilt trees at a recycled address,
//! five interleaved trees, and a plan naming an edge the other arena lacks.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::ns;
use tf_tree::{Capacity, EdgeCfg, Iso3, Quat, Tree, TreeBuilder, Vec3};

/// A pure-x translation, so a composed chain's answer is the sum of its parts
/// and a wrong answer names the arena it came from.
fn tx(x: f64) -> Iso3 {
    Iso3::new(Quat::IDENTITY, Vec3::new(x, 0.0, 0.0))
}

/// The x component of `lookup(target, source)` at stamp 0.
fn look(tree: &Tree, target: &str, source: &str) -> f64 {
    tree.lookup(target, source, ns(0)).unwrap().t.x
}

fn cfg() -> EdgeCfg {
    EdgeCfg::new(Capacity::slots(8))
}

/// Two static chains of one shape and names with different transforms. A stolen
/// plan returns A's folded 11 where 22 is correct.
#[test]
fn static_plans_are_not_shared_between_trees() {
    let a = TreeBuilder::new()
        .static_edge("root", "mid", &tx(1.0))
        .static_edge("mid", "tip", &tx(10.0))
        .build()
        .unwrap();
    let b = TreeBuilder::new()
        .static_edge("root", "mid", &tx(2.0))
        .static_edge("mid", "tip", &tx(20.0))
        .build()
        .unwrap();

    assert_eq!(look(&a, "root", "tip"), 11.0, "tree A, uncontested");
    assert_eq!(
        look(&b, "root", "tip"),
        22.0,
        "tree B must not be served tree A's folded statics"
    );
    // Back to A: eviction on every foreign lookup must still answer for A.
    assert_eq!(look(&a, "root", "tip"), 11.0, "tree A after tree B");
}

/// Two trees of identical topology with dynamic edges. Passes with the defect
/// present (edge indices mean the same in both arenas); it guards a fix keyed too
/// coarsely.
#[test]
fn dynamic_plans_with_identical_topology_stay_correct() {
    let build = |near: f64, far: f64| {
        let tree = TreeBuilder::new()
            .dynamic_edge("root", "mid", cfg())
            .dynamic_edge("mid", "tip", cfg())
            .build()
            .unwrap();
        let (root, mid, tip) = (
            tree.frame("root").unwrap(),
            tree.frame("mid").unwrap(),
            tree.frame("tip").unwrap(),
        );
        tree.claim(mid, root).unwrap().push(0, &tx(near)).unwrap();
        tree.claim(tip, mid).unwrap().push(0, &tx(far)).unwrap();
        tree
    };
    let a = build(1.0, 10.0);
    let b = build(2.0, 20.0);

    assert_eq!(look(&a, "root", "tip"), 11.0, "tree A, uncontested");
    assert_eq!(look(&b, "root", "tip"), 22.0, "tree B");
    assert_eq!(look(&a, "root", "tip"), 11.0, "tree A after tree B");
}

/// Same names and order, different shape: A is the chain `root -> mid -> tip`, B
/// the star `root -> mid`, `root -> tip`. A stolen plan gives B 12.0 (`5 + 7`),
/// neither tree's answer.
#[test]
fn dynamic_plans_are_not_shared_across_differing_topology() {
    let chain = TreeBuilder::new()
        .dynamic_edge("root", "mid", cfg())
        .dynamic_edge("mid", "tip", cfg())
        .build()
        .unwrap();
    {
        let (root, mid, tip) = (
            chain.frame("root").unwrap(),
            chain.frame("mid").unwrap(),
            chain.frame("tip").unwrap(),
        );
        chain.claim(mid, root).unwrap().push(0, &tx(1.0)).unwrap();
        chain.claim(tip, mid).unwrap().push(0, &tx(10.0)).unwrap();
    }

    let star = TreeBuilder::new()
        .dynamic_edge("root", "mid", cfg())
        .dynamic_edge("root", "tip", cfg())
        .build()
        .unwrap();
    {
        let (root, mid, tip) = (
            star.frame("root").unwrap(),
            star.frame("mid").unwrap(),
            star.frame("tip").unwrap(),
        );
        star.claim(mid, root).unwrap().push(0, &tx(5.0)).unwrap();
        star.claim(tip, root).unwrap().push(0, &tx(7.0)).unwrap();
    }

    // The collision precondition.
    assert_eq!(
        chain.frame("tip").unwrap().get(),
        star.frame("tip").unwrap().get(),
        "the two trees must agree on frame ids for this to be the collision case"
    );

    assert_eq!(look(&chain, "root", "tip"), 11.0, "chain, uncontested");
    assert_eq!(
        look(&star, "root", "tip"),
        7.0,
        "the star's tip hangs off root directly; 12.0 is the chain's plan walked \
         over the star's edges and is neither tree's answer"
    );
    assert_eq!(look(&chain, "root", "tip"), 11.0, "chain after star");
}

/// The generation of a freshly built tree is its declared edge count, one tick
/// per link; the module reasoning depends on it.
#[test]
fn a_fresh_trees_generation_is_its_edge_count() {
    for edges in 0..4usize {
        let mut b = TreeBuilder::new().frame("root");
        let names = ["a", "b", "c"];
        let mut parent = "root";
        for name in names.iter().take(edges) {
            b = b.static_edge(parent, name, &tx(1.0));
            parent = name;
        }
        let tree = b.build().unwrap();
        assert_eq!(
            tree.guard().generation(),
            edges as u64,
            "{edges} declared edges"
        );
    }
}

/// Build, look up, drop, rebuild six times: the allocator recycles the arena
/// address, so a key on the base pointer would answer `11.0` throughout. Hence
/// `cache_scope_for` uses a counter.
#[test]
fn a_rebuilt_tree_is_not_the_tree_it_replaced() {
    let mut answers = Vec::new();
    for i in 1..=6u32 {
        let v = f64::from(i);
        let tree = TreeBuilder::new()
            .static_edge("root", "mid", &tx(v))
            .static_edge("mid", "tip", &tx(v * 10.0))
            .build()
            .unwrap();
        answers.push(look(&tree, "root", "tip"));
        drop(tree);
    }
    assert_eq!(
        answers,
        vec![11.0, 22.0, 33.0, 44.0, 55.0, 66.0],
        "each rebuild must answer for itself, not for the tree at the same address"
    );
}

/// Five live trees, round-robin, three rounds; also guards a key that separates
/// trees by never hitting (`cache::tests::two_trees_keep_separate_entries_and_still_hit`).
#[test]
fn five_live_trees_interleaved_each_answer_for_themselves() {
    let trees: Vec<Tree> = (1..=5u32)
        .map(|i| {
            let v = f64::from(i);
            TreeBuilder::new()
                .static_edge("root", "mid", &tx(v))
                .static_edge("mid", "tip", &tx(v * 100.0))
                .build()
                .unwrap()
        })
        .collect();
    for round in 0..3 {
        for (i, tree) in trees.iter().enumerate() {
            let want = f64::from(u32::try_from(i).unwrap() + 1) * 101.0;
            assert_eq!(look(tree, "root", "tip"), want, "round {round}, tree {i}");
        }
    }
}

/// A's plan for `w -> t` names `EdgeId(3)`; B declares two edges, so that id is
/// off the end of its array. Without the arena in the key B returns
/// `Err(UnknownEdge { edge: EdgeId(3) })`: `ArenaView::edge` bounds-checks via
/// `edge_slot_off`, so #196 is a wrong answer, not a bad read. With the fix B
/// answers `7.0`.
#[test]
fn a_stolen_plan_can_name_an_edge_the_other_arena_does_not_have() {
    let a = TreeBuilder::new()
        .dynamic_edge("w", "m", cfg())
        .dynamic_edge("m", "n", cfg())
        .dynamic_edge("n", "t", cfg())
        .build()
        .unwrap();
    {
        let f = |n: &str| a.frame(n).unwrap();
        a.claim(f("m"), f("w")).unwrap().push(0, &tx(1.0)).unwrap();
        a.claim(f("n"), f("m")).unwrap().push(0, &tx(10.0)).unwrap();
        a.claim(f("t"), f("n"))
            .unwrap()
            .push(0, &tx(100.0))
            .unwrap();
    }

    let b = TreeBuilder::new()
        .dynamic_edge("w", "m", cfg())
        .dynamic_edge("n", "t", cfg())
        .build()
        .unwrap();
    let f = |n: &str| b.frame(n).unwrap();
    b.claim(f("m"), f("w")).unwrap().push(0, &tx(5.0)).unwrap();
    // Ticks B's generation from 2 to 3 so the stale key matches A's.
    b.reparent(f("t"), f("w")).unwrap();
    b.claim(f("t"), f("w")).unwrap().push(0, &tx(7.0)).unwrap();

    assert_eq!(
        (
            a.frame("w").unwrap().get(),
            a.frame("t").unwrap().get(),
            a.guard().generation()
        ),
        (
            b.frame("w").unwrap().get(),
            b.frame("t").unwrap().get(),
            b.guard().generation()
        ),
        "ids and generation must agree, or the stale key misses and this tests nothing"
    );

    assert_eq!(look(&a, "w", "t"), 111.0, "A, uncontested");
    assert_eq!(
        look(&b, "w", "t"),
        7.0,
        "B must compile its own plan; A's names an EdgeId B's arena does not have"
    );
}
