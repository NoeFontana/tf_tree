//! Issue #196: the per-thread plan cache behind [`tf_tree::Tree::lookup`] must
//! not serve one tree's compiled plan to another.
//!
//! The cache is `thread_local!` and shared by every `Tree` on the thread, so its
//! key has to carry *which arena* a plan was compiled against. `FrameId`s are
//! handed out in interning order, and a freshly built tree's generation is its
//! **declared edge count** — one tick per link, measured by
//! [`a_fresh_trees_generation_is_its_edge_count`] — so two trees built from the
//! same names in the same order agree on every other component of the key as a
//! matter of course. They are not adversarial inputs, they are the normal case.
//!
//! The generation being the edge count rather than a constant zero is the one
//! thing that narrows the defect, and the narrowing is worth nothing: it means
//! two trees of *different shape* miss and are safe by accident, while the
//! trees that collide are the ones anybody actually builds twice — a fixture
//! rebuilt per test, two robots of one model, a tree reopened in a loop.
//!
//! Six shapes, in increasing order of how badly a shared key fails:
//!
//! 1. [`static_plans_are_not_shared_between_trees`] — a static edge's transform
//!    is *folded into the plan*, so a stolen plan carries the other tree's
//!    numbers. The wrong answer is the other tree's answer.
//! 2. [`dynamic_plans_with_identical_topology_stay_correct`] — passes even with
//!    the defect present, because a plan over dynamic edges is a list of edge
//!    indices and evaluating it against the second arena happens to read the
//!    second arena's samples. It is here precisely because it passes: it is the
//!    reason the bug looks absent, and it is the regression that a fix keyed too
//!    coarsely (or an eviction bug) would break.
//! 3. [`dynamic_plans_are_not_shared_across_differing_topology`] — the two
//!    trees have the same frame names in the same order but a different shape,
//!    so the stolen plan walks edges that exist in both arenas and means
//!    something different in each. The answer belongs to neither tree.
//! 4. [`a_rebuilt_tree_is_not_the_tree_it_replaced`] — no two trees are alive at
//!    once. Build, look up, **drop**, rebuild: the allocator hands the same
//!    address back, so the second tree inherits the first's entry and every
//!    iteration of a rebuild loop answers with the first one's numbers. This is
//!    the shape a test suite and a reopen-on-reconnect loop have, and it is the
//!    reason the arena id is a counter and not the base pointer.
//! 5. [`five_live_trees_interleaved_each_answer_for_themselves`] — more than two
//!    trees, round-robin, which is what a cache keyed too coarsely collapses to
//!    a single entry.
//! 6. [`a_stolen_plan_can_name_an_edge_the_other_arena_does_not_have`] — the
//!    worst one, and the one that decides whether this defect is a wrong number
//!    or a memory-safety problem. The stolen plan names `EdgeId(3)` against an
//!    arena whose edge slots stop at 2. **It is a bounds-checked miss, not a
//!    bad read** — `ArenaView::edge` goes through `edge_slot_off`, which tests
//!    `id < max_edges` — so the pre-fix failure mode is
//!    `LookupError::UnknownEdge`, measured. That is worth pinning precisely
//!    because it is the case a reader will assume is worse than it is.
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

/// Two static chains of the same shape and names, differing only in their
/// transforms. Both are alive at once and looked up from one thread.
///
/// The plan folds the static transforms, so serving the first tree's plan for
/// the second tree's lookup returns the *first tree's numbers*: 11 where 22 is
/// the only defensible answer.
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
    // Back to A, so a fix that merely evicted on every foreign lookup would
    // still have to answer for A.
    assert_eq!(look(&a, "root", "tip"), 11.0, "tree A after tree B");
}

/// Two trees of identical topology whose transforms live in dynamic edges.
///
/// **This passes with the defect present.** The compiled plan is a list of edge
/// indices, and those indices mean the same thing in both arenas, so evaluating
/// A's plan against B's arena reads B's samples and gets B's answer. That is
/// luck, not correctness — and it is why the defect reads as absent from any
/// test built on dynamic edges alone.
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

/// Same frame names, declared in the same order, but a different *shape*: A is
/// the chain `root -> mid -> tip`, B is the star `root -> mid`, `root -> tip`.
///
/// The frame ids therefore collide, and A's two-step plan is walkable against
/// B's arena — it just means something else there. B answers 12.0 (`5 + 7`,
/// A's chain walked over B's edges), which is neither tree's transform.
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

    // Same ids in both arenas is the precondition for the collision, not an
    // incidental detail: assert it so this test cannot quietly stop testing.
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

/// The generation of a freshly built tree is its **declared edge count**, one
/// tick per link.
///
/// This exists because the first revision of this file, and of `cache.rs`'s
/// module doc, said "a fresh tree starts at generation zero" — which is false,
/// and false in the direction that matters. The three cases above collide only
/// because their two trees declare the *same number of edges*; give them
/// different shapes with different edge counts and the generation alone
/// separates them, which is why [`a_stolen_plan_can_name_an_edge_the_other_arena_does_not_have`]
/// has to spend a `reparent` to line the generations back up.
///
/// Pinned rather than asserted in prose: if the builder ever stops ticking per
/// link, the reasoning in this file's module doc is wrong and this fails.
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

/// **Nothing is alive at the same time as anything else**, and the cache still
/// has to tell the trees apart.
///
/// Build, look up, drop, rebuild — six times, each tree carrying its own
/// numbers. A heap arena is one `calloc` block of a fixed size, so the
/// allocator hands the same address straight back: keying the cache on the
/// arena's base pointer would pass every test above and fail this one, which is
/// why `cache_scope_for` uses a counter that cannot be recycled.
///
/// Measured with the arena component removed from the key: all six iterations
/// answer `11.0`, the first tree's number, because there is only ever one entry
/// and it is never invalidated.
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

/// Five trees alive at once, round-robin, three rounds.
///
/// Two trees can be told apart by a key that merely alternates; five cannot.
/// This also exercises the cache in the direction a fix must *not* break —
/// every one of the fifteen repeats after the first round is a hit, so a key
/// that separated the trees by never matching anything would still fail
/// `cache::tests::two_trees_keep_separate_entries_and_still_hit`.
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

/// **The case that decides whether this is a wrong number or a bad read.**
///
/// `A` is the chain `w -> m -> n -> t`: three edges, so ids `1..=3` and a plan
/// for `w -> t` that names `EdgeId(3)`. `B` interns the same four names in the
/// same order but declares only two edges, so its edge budget stops at
/// `EdgeId(2)` — `EdgeId(3)` is not a stale slot in `B`, it is off the end of
/// the array.
///
/// The `reparent` is not decoration. Without it `B`'s generation is 2 against
/// `A`'s 3 and the stale key misses on the generation, so the interesting read
/// never happens; the reparent ticks `B` to 3 and lines the two keys up
/// exactly. Getting this case to occur at all took that step, which is itself
/// the measurement of how narrow it is.
///
/// **Measured with the arena component removed from the key: `B` returns
/// `Err(UnknownEdge { edge: EdgeId(3) })`.** `ArenaView::edge` resolves through
/// `edge_slot_off`, which tests `id < max_edges` and returns `None`, so the
/// out-of-range plan is refused rather than read. That is the whole basis for
/// calling #196 an incorrectness bug and not a memory-safety one, and it is
/// pinned here so the claim is not re-derived from a reading of the code.
///
/// With the fix, `B` compiles its own one-hop plan and answers `7.0`.
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
    // Hangs `t` off `w` — which both connects `B` and ticks its generation from
    // 2 to 3, so the stale key would match `A`'s exactly.
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
