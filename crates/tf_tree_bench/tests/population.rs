//! Page population — `docs/PHASE2.md` §7.1, `docs/decisions/0005` step 10.
//!
//! §7.1 is NORMATIVE: `mmap` **without** `MAP_POPULATE`, then populate at
//! declaration granularity. The reason is a page-fault storm — a minor fault
//! costs single-digit microseconds against a 150 ns p50 lookup budget, so the
//! *first* lookup after attach pays two orders of magnitude more than the steady
//! state and never appears in a steady-state benchmark.
//!
//! # The test has to be three-sided, or it proves nothing
//!
//! "Headroom is not charged" is satisfied perfectly by populating *nothing* —
//! which reintroduces exactly the fault storm §7.1 exists to prevent, and would
//! look like a resounding success on a one-sided test. So there were two:
//!
//! * [`declared_headroom_is_not_charged`] — big headroom, small content.
//! * [`declared_content_is_charged`] — no headroom, big content.
//!
//! Each fails against the other's fix. Restoring `MAP_POPULATE` fails the first;
//! making population a no-op fails the second.
//!
//! **That pair has a hole between it, and a third test closes it.** Both are
//! satisfied by populating every declared ring at attach, which is what this
//! crate did until population became per-edge: headroom is not declared content,
//! so the first still passes, and everything declared is charged, so the second
//! passes emphatically. Neither notices a process being charged for two hundred
//! edges it never reads.
//!
//! * [`only_the_edges_this_process_uses_are_charged`] — no headroom, big
//!   content, and only a few edges taken up.
//!
//! It fails against the other two's fix in exactly the same way they fail
//! against each other's: restoring the per-arena ring population passes both of
//! them and fails it. All three mutants have been run rather than reasoned
//! about, and each test's doc names its own with the number it produced.
//!
//! # Why RSS and not `mincore`
//!
//! `mincore` needs the mapping's base pointer, which `tf_tree` does not expose
//! and should not start exposing for a test. `/proc/self/statm` is coarser but
//! answers the only question being asked: whether pages nobody declared are
//! being charged to this process. Measured as a **delta** around the
//! construction, so the process's own baseline drops out.
#![cfg(all(feature = "shm", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use tf_tree::{Capacity, EdgeCfg, InterpPolicy, Tree, TreeBuilder};

const MIB: usize = 1 << 20;

/// Resident set size in bytes, from `/proc/self/statm` field 2 (pages).
fn rss_bytes() -> usize {
    let s = std::fs::read_to_string("/proc/self/statm").unwrap();
    let pages: usize = s.split_whitespace().nth(1).unwrap().parse().unwrap();
    pages * 4096
}

fn charged(build: impl FnOnce() -> Tree) -> (usize, usize) {
    let before = rss_bytes();
    let tree = build();
    let after = rss_bytes();
    let size = tree.arena_size_bytes();
    // The tree stays alive until here, so the mapping cannot have been torn down
    // between the two readings.
    drop(tree);
    (after.saturating_sub(before), size)
}

/// **Headroom must not be charged.**
///
/// One declared dynamic edge with 1024 slots — about 74 KiB of rings — against
/// 200 000 slots each of frame and edge headroom. The headroom is tens of
/// megabytes of tables nobody declared and nothing ever reads.
///
/// Measured before this landed, with `MAP_POPULATE`: **66.3 MiB charged against
/// 66.1 MiB declared, 100%**. After: **3.8 MiB, 6%**.
///
/// **The residue is not `MADV_HUGEPAGE`, and this comment used to say it was.**
/// It claimed that populating any part of a 2 MiB-aligned range under THP faults
/// in the whole huge page, so small live regions round up. Three measurements
/// refute it:
///
/// * **Transparent huge pages are not granted here at all.** The arena is a
///   `memfd` mapped `MAP_SHARED`, which is *shmem* and is governed by
///   `transparent_hugepage/shmem_enabled` — `[never]` on this host — not by
///   `transparent_hugepage/enabled`. `/proc/vmstat` agrees: `thp_file_alloc 0`
///   **and** `thp_file_fallback 0`, so nothing has ever asked and been refused
///   either. `docs/PHASE5.md` §2.3 records the same finding independently, and
///   `TFT016` was fixed to read the right sysfs file because of it.
/// * **The residue scales linearly with headroom**, which a huge-page round-up
///   cannot do. Measured at 50k/100k/200k/400k units of headroom: 1.05, 1.95,
///   3.75, 7.34 MB — doubling the headroom doubles the residue, ~19 B per unit.
///   A 2 MiB round-up would be a step function, not a line.
/// * **About half of it appears on the *heap* path too**, where `populate_hot`
///   does not exist. Same headroom, `build()` against `build_shared()`: 2.04 MB
///   against 3.76 MB at 200k. So at least that half is not population of any
///   kind — it is the builder's own `O(max_frames + max_edges)` temporaries,
///   freed before the second reading but still resident in the process heap,
///   which `charged()` cannot separate because it reads whole-process RSS.
///
/// **The remaining ~10 B/unit that is specific to the shared path is not
/// attributed here.** It is small, it is bounded by the assertion below, and
/// guessing at it is what produced the sentence this replaced. Re-run the
/// experiment by varying `edge_headroom`/`frame_headroom` and comparing `build`
/// with `build_shared`.
///
/// None of this weakens the test: the bound is a fraction of the arena because
/// the residue is dominated by costs that scale with *declared* size, whatever
/// their origin, and the property under test is that the headroom *pages* are
/// not faulted.
///
/// Mutant: restore `MapFlags::POPULATE` in `unsafe_map` ⇒ 100% charged.
#[test]
fn declared_headroom_is_not_charged() {
    let (rss, size) = charged(|| {
        TreeBuilder::new()
            .default_interp(InterpPolicy::LerpSlerp)
            .dynamic_edge("map", "base", EdgeCfg::new(Capacity::slots(1024)))
            .edge_headroom(200_000)
            .frame_headroom(200_000)
            .build_shared("tf_tree_pop_headroom")
            .expect("build_shared")
    });

    // Non-vacuity: there must actually be a large headroom region to leave cold,
    // or this passes on an arena that never had anything to save.
    assert!(
        size > 32 * MIB,
        "the over-provisioned arena is only {size} B — the layout is not what this test assumes"
    );
    assert!(
        rss < size / 8,
        "charged {rss} B of a {size} B arena: headroom is being faulted in"
    );
}

/// Sixty-four dynamic edges of 8192 slots each — about 36 MiB of rings — with
/// **no headroom at all**, so every page of the arena is live. `used` names how
/// many of the 64 the returned tree actually takes up, by claiming them.
///
/// The writers are dropped before the closure returns, which is deliberate and
/// is the thing that makes this measurable: releasing a claim un-*owns* the
/// edge, it does not un-populate its pages. So the residency this reads back is
/// attributable to population and to nothing the writer is still holding.
fn build_claiming(name: &str, used: usize) -> Tree {
    let mut b = TreeBuilder::new().default_interp(InterpPolicy::LerpSlerp);
    for i in 0..64 {
        let child: &'static str = Box::leak(format!("link{i}").into_boxed_str());
        b = b.dynamic_edge("map", child, EdgeCfg::new(Capacity::slots(8192)));
    }
    let tree = b.build_shared(name).expect("build_shared");
    let parent = tree.frame("map").unwrap();
    for i in 0..used {
        let child = tree.frame(&format!("link{i}")).unwrap();
        drop(tree.claim(child, parent).unwrap());
    }
    tree
}

/// **Declared content must be charged.** The other half, and the one that stops
/// "populate nothing" from passing.
///
/// # Why this claims all 64 edges, and why that is not a relaxation
///
/// It used to build the arena and measure, because population was per-*arena*:
/// `populate_hot` warmed the whole stamp and pose arenas at build, so declaring
/// an edge was enough to be charged for it. Population is now per-*edge*, which
/// is what `docs/PHASE2.md` §7.1 says it should always have been, and the moment
/// an edge is taken up is `Tree::claim` (writer) or plan compilation (reader).
/// So the test has to take the edges up to be asking the same question.
///
/// **Weakening the assertion instead would have been the wrong repair**, and it
/// is the one available: this file's header explains that a one-sided test is
/// satisfied perfectly by populating nothing, so an `rss > size / 2` softened to
/// accommodate the new scheme would have quietly become the no-op-passes test
/// the header exists to forbid. The property is unchanged — pages an edge's user
/// will read are warm before they read them — and only the *moment* it is
/// established has moved. Measured: 38 584 320 B charged of a 37 797 888 B
/// arena, 102%.
///
/// Mutant, run: drop the `populate_edge_rings` call from `Tree::claim` ⇒ 438 272
/// B charged of the same arena (1%) and this fails, while
/// [`declared_headroom_is_not_charged`] and
/// [`the_first_lookup_after_attach_does_not_fault`] go right on passing.
#[test]
fn declared_content_is_charged() {
    let (rss, size) = charged(|| build_claiming("tf_tree_pop_declared", 64));

    assert!(
        size > 32 * MIB,
        "the declared arena is only {size} B — the layout is not what this test assumes"
    );
    assert!(
        rss > size / 2,
        "charged only {rss} B of a fully-declared {size} B arena: population is not happening, \
         so the first lookup on each edge pays a page fault"
    );
}

/// **A process is charged for the edges it uses, not the edges that exist.**
///
/// This is the third side of the test, and without it the pair above is
/// satisfied by the per-arena scheme this replaced — that scheme passes
/// [`declared_content_is_charged`] (it populates everything) and passes
/// [`declared_headroom_is_not_charged`] (headroom is not declared content). The
/// gap between them is exactly the case this covers: capacity that *is*
/// declared, by somebody, and that this process never touches.
///
/// It is also the realistic one. A robot's arena is declared once, for the whole
/// vehicle; a node attaches to it and reads a handful of chains. Under the old
/// scheme every such node paid for every edge on the vehicle, permanently, and
/// the rings are 99.8% of a large arena.
///
/// Four of sixty-four edges, measured: **7 368 704 B charged of 37 797 888 B,
/// 19.5%**, against 102% for the same arena fully taken up — a 5.2× reduction in
/// what an operator sees against this arena in `top`. The bound below is a
/// quarter rather than that 19.5%, because the tables are charged in full either
/// way and the point is the order of magnitude, not the constant.
///
/// Mutant, run: restore the two `self.populate(h.stamp_arena_off …)` /
/// `pose_arena_off` lines in `populate_hot` ⇒ 38 248 448 B charged, 101%, and
/// this fails while both tests above keep passing. That is the mutation this
/// test exists to catch and the reason it is not redundant with them.
#[test]
fn only_the_edges_this_process_uses_are_charged() {
    let (rss, size) = charged(|| build_claiming("tf_tree_pop_subset", 4));

    assert!(
        size > 32 * MIB,
        "the declared arena is only {size} B — the layout is not what this test assumes"
    );
    assert!(
        rss < size / 4,
        "charged {rss} B of a {size} B arena while using 4 of its 64 edges: rings this process \
         never claims or plans are being faulted in"
    );
}

/// Minor faults taken by this process, from `/proc/self/stat` field 10.
///
/// A count, not a duration. §7.1's claim is about *page faults*, and measuring
/// it as a duration would mean measuring scheduler noise and cache state at the
/// same time — a test that fails on a loaded machine and passes on an idle one
/// proves nothing about the code.
fn minor_faults() -> u64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    // Field 2 is `comm`, parenthesised and free to contain spaces and
    // parentheses, so the scan starts after the **last** `)` — the same parsing
    // trap `docs/PHASE2.md` §5.1 calls out for `start_time`.
    let after_comm = &stat[stat.rfind(')').unwrap() + 1..];
    after_comm
        .split_whitespace()
        .nth(7)
        .unwrap()
        .parse()
        .unwrap()
}

/// **The first lookup after attach must not fault** — which is the entire reason
/// §7.1 exists.
///
/// A minor fault costs single-digit microseconds against a 150 ns p50 budget, so
/// one fault in the lookup path is two orders of magnitude over. It happens
/// exactly once per page, on first touch, which is why it never appears in a
/// steady-state benchmark and why an attaching consumer is the process that eats
/// it: inside the first iteration of a control loop.
///
/// A **second mapping of the same segment** in this process is what makes this
/// measurable without a second process — the new mapping has its own page
/// tables, so its pages are cold no matter how warm the creator's are.
///
/// # This test now covers two call sites, and it is the only one that covers the
/// second
///
/// `joiner.plan(s, t)` above is not scaffolding. Since population became
/// per-edge it is what warms this edge's rings, so the fault this measures would
/// be taken *inside `plan.at`* without it — which is the one place §7.1 says a
/// fault may never be. The other two tests in this file weigh residency and
/// would not notice.
///
/// Mutants, both run: drop the `populate_hot()` call from `attach_shared_inner`
/// ⇒ the first lookup faults on the tables. Drop the `populate_edge_rings` loop
/// from `Tree::plan` ⇒ **1 minor fault**, on the rings. Either is a failure, and
/// the other two tests stay green through the second of them.
#[test]
fn the_first_lookup_after_attach_does_not_fault() {
    use tf_tree::{AttachMode, Stamp, SystemDomain};

    let creator = TreeBuilder::new()
        .default_interp(InterpPolicy::LerpSlerp)
        .dynamic_edge("map", "base", EdgeCfg::new(Capacity::slots(65_536)))
        .build_shared("tf_tree_pop_firstlookup")
        .expect("build_shared");
    let child = creator.frame("base").unwrap();
    let parent = creator.frame("map").unwrap();
    let w = creator.claim(child, parent).unwrap();
    for i in 0..4096 {
        w.push(
            1_000 + i * 1_000,
            &tf_tree_math::exp_se3([0.0, 0.0, 0.001 * i as f64, i as f64, 0.0, 0.0]),
        )
        .unwrap();
    }

    let fd = creator.shared_fd().unwrap().try_clone_to_owned().unwrap();
    let joiner = tf_tree::Tree::attach_shared(fd, AttachMode::ReadOnly).expect("attach");
    let t = joiner.frame("base").unwrap();
    let s = joiner.frame("map").unwrap();
    let plan = joiner.plan(s, t).unwrap();

    // Everything above has already touched whatever it is going to touch. From
    // here on, any fault is one the lookup itself took.
    let before = minor_faults();
    let g = joiner.guard();
    plan.at(&g, Stamp::<SystemDomain>::from_nanos(2_000_000))
        .expect("lookup");
    let faults = minor_faults() - before;

    assert_eq!(
        faults, 0,
        "the first lookup after attach took {faults} minor faults; §7.1 exists to make that zero"
    );
}
