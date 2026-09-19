//! Page population — `docs/PHASE2.md` §7.1, `docs/decisions/0005` step 10.
//!
//! §7.1 is NORMATIVE: `mmap` without `MAP_POPULATE`, then populate per edge, so
//! the first lookup after attach takes no page fault. Three tests bound
//! residency from three sides, each failing against the others' fix:
//! [`declared_headroom_is_not_charged`], [`declared_content_is_charged`] and
//! [`only_the_edges_this_process_uses_are_charged`]. Residency is the RSS delta
//! from `/proc/self/statm`; `mincore` would need the mapping's base pointer.
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
    // The tree stays alive until here, so the mapping is not torn down early.
    drop(tree);
    (after.saturating_sub(before), size)
}

/// Headroom must not be charged: one 1024-slot dynamic edge against 200 000
/// slots each of frame and edge headroom (tens of MB nobody declared).
///
/// The residue is not huge pages (`shmem_enabled` is `never`; `docs/PHASE5.md`
/// §2.3). It scales linearly with headroom, and about half also appears on the
/// heap path, from the builder's own temporaries. The bound is a fraction of the
/// arena.
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

    // Non-vacuity: a large headroom region must exist to leave cold.
    assert!(
        size > 32 * MIB,
        "the over-provisioned arena is only {size} B — the layout is not what this test assumes"
    );
    assert!(
        rss < size / 8,
        "charged {rss} B of a {size} B arena: headroom is being faulted in"
    );
}

/// 64 dynamic edges of 8192 slots (~36 MiB of rings), no headroom, so every page
/// is live; `used` of them are claimed. The writers drop before return:
/// releasing a claim un-owns the edge but does not un-populate its pages.
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

/// Declared content must be charged, which stops "populate nothing" from
/// passing. It claims all 64 edges because population is per edge, at
/// `Tree::claim` (writer) or plan compilation (reader), per
/// `docs/PHASE2.md` §7.1.
///
/// Mutant: drop the `populate_edge_rings` call from `Tree::claim` ⇒ 1% charged,
/// while the other two residency tests keep passing.
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

/// A process is charged for the edges it uses, not the edges that exist: the
/// per-arena scheme passes the other two residency tests and fails this. The
/// bound is a quarter of the arena, since the tables are charged either way.
///
/// Mutant: restore the two `populate(h.stamp_arena_off …)` / `pose_arena_off`
/// lines in `populate_hot` ⇒ ~101% charged.
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

/// Minor faults taken by this process, from `/proc/self/stat` field 10 — a
/// count, not a duration, so load cannot flip it.
fn minor_faults() -> u64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    // Field 2 (`comm`) may contain spaces and parentheses, so scan after the last
    // `)` (`docs/PHASE2.md` §5.1).
    let after_comm = &stat[stat.rfind(')').unwrap() + 1..];
    after_comm
        .split_whitespace()
        .nth(7)
        .unwrap()
        .parse()
        .unwrap()
}

/// The first lookup after attach must not fault (§7.1): one minor fault is two
/// orders of magnitude over the 150 ns p50 budget. A second mapping of the
/// segment has cold page tables, which makes this measurable in-process.
/// `joiner.plan` is not scaffolding: it warms this edge's rings.
///
/// Mutants: drop `populate_hot()` from `attach_shared_inner` ⇒ the lookup faults
/// on the tables; drop the `populate_edge_rings` loop from `Tree::plan` ⇒ 1
/// fault on the rings.
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

    // From here on, any fault is one the lookup itself took.
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
