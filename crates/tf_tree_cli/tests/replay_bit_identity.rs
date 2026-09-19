//! `docs/PHASE2.md` §10's NORMATIVE test, and §15's box 11: replay one
//! recording into a `HeapArena` and a `MappedArena`, run an identical query set,
//! assert bit-identical `f64` results.
//!
//! Both trees are filled by the same `replay` from the same in-memory
//! `Vec<FixtureMessage>`; the file written to `run.mcap` is not read back, so
//! serialisation is outside the assertion (§15's box 11 has the argument).
//!
//! `shm`-gated: `just shm-check` runs it.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(all(feature = "shm", target_os = "linux"))]

use std::collections::BTreeSet;

use tf_tree::{Capacity, EdgeCfg, Iso3, Stamp, SystemDomain, Tree, TreeBuilder};
use tf_tree_ingest::fixture::{small_recording, write_mcap, FixtureMessage};

/// A scratch directory that cleans up even when an assertion fails.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let p = std::env::temp_dir().join(format!("tf_tree_replay-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Scratch(p)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Every `(parent, child)` edge the recording declares, in first-seen order
/// (edge ids are append-only, D10, so order must match in both arenas).
fn edges_of(msgs: &[FixtureMessage]) -> Vec<(String, String)> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for m in msgs {
        for t in &m.transforms {
            let key = (t.frame_id.clone(), t.child_frame_id.clone());
            if seen.insert(key.clone()) {
                out.push(key);
            }
        }
    }
    out
}

/// Declare the recording's topology. Every edge is dynamic, including the
/// `/tf_static` ones, since a static edge holds no ring to replay into.
fn builder_for(msgs: &[FixtureMessage]) -> TreeBuilder {
    let mut b = TreeBuilder::new();
    for (parent, child) in edges_of(msgs) {
        b = b.dynamic_edge(&parent, &child, EdgeCfg::new(Capacity::slots(256)));
    }
    b
}

/// Push every transform in `msgs` into `tree`, in recording order.
fn replay(tree: &Tree, msgs: &[FixtureMessage]) {
    let mut writers = std::collections::BTreeMap::new();
    for m in msgs {
        for t in &m.transforms {
            let child = tree.frame(&t.child_frame_id).expect("declared child");
            let parent = tree.frame(&t.frame_id).expect("declared parent");
            let w = writers
                .entry((t.child_frame_id.clone(), t.frame_id.clone()))
                .or_insert_with(|| tree.claim(child, parent).expect("unclaimed edge"));
            let iso = Iso3::from_bits(&{
                let mut bits = [0u64; 7];
                for (i, v) in t.pose.iter().enumerate() {
                    bits[i] = v.to_bits();
                }
                bits
            });
            // A repeated stamp on one edge is refused identically in both arenas.
            let _ = w.push(t.stamp_ns, &iso);
        }
    }
}

/// The query set, run against both arenas. Stamps straddle the recording so
/// answers are interpolated, not stored samples.
fn probe_stamps(msgs: &[FixtureMessage]) -> Vec<i64> {
    let mut stamps: Vec<i64> = msgs
        .iter()
        .flat_map(|m| m.transforms.iter().map(|t| t.stamp_ns))
        .collect();
    stamps.sort_unstable();
    stamps.dedup();
    let mut out = Vec::new();
    for w in stamps.windows(2) {
        out.push(w[0]);
        out.push(w[0] + (w[1] - w[0]) / 2); // between two samples
        out.push(w[0] + (w[1] - w[0]) / 3);
    }
    out
}

/// Every lookup's answer as raw bits, so a comparison cannot round.
fn answers(tree: &Tree, pairs: &[(String, String)], stamps: &[i64]) -> Vec<Option<[u64; 7]>> {
    let g = tree.guard();
    let mut out = Vec::new();
    for (parent, child) in pairs {
        let a = tree.frame(parent).expect("frame");
        let b = tree.frame(child).expect("frame");
        let Ok(plan) = tree.plan(a, b) else {
            out.push(None);
            continue;
        };
        for s in stamps {
            out.push(
                plan.at(&g, Stamp::<SystemDomain>::from_nanos(*s))
                    .ok()
                    .map(|iso| iso.to_bits()),
            );
        }
    }
    out
}

/// **§10's NORMATIVE test.** One recording, two backends, bit-identical `f64`.
#[test]
fn a_replay_into_heap_and_mapped_arenas_is_bit_identical() {
    let scratch = Scratch::new("bitident");
    let msgs = small_recording();

    // The recording is written but not read back; both arenas replay `msgs`.
    let bag = scratch.0.join("run.mcap");
    write_mcap(&bag, &msgs).expect("write the recording");
    assert!(
        bag.is_file(),
        "the recording must be writable to be a recording"
    );

    let heap = builder_for(&msgs).build().expect("heap arena");
    let mapped = builder_for(&msgs)
        .build_shared("replay-bitident")
        .expect("mapped arena");
    assert!(
        mapped.is_shared(),
        "the second arena must actually be mapped, or this compares heap to heap"
    );

    replay(&heap, &msgs);
    replay(&mapped, &msgs);

    let pairs = edges_of(&msgs);
    let stamps = probe_stamps(&msgs);
    let a = answers(&heap, &pairs, &stamps);
    let b = answers(&mapped, &pairs, &stamps);

    // Anti-vacuity: empty or all-`None` vectors match any pair of arenas.
    assert!(!a.is_empty(), "the query set must not be empty");
    let hits = a.iter().filter(|x| x.is_some()).count();
    assert!(
        hits >= stamps.len(),
        "too few lookups succeeded ({hits}) for this to prove anything"
    );

    assert_eq!(
        a, b,
        "a heap arena and a mapped arena must answer bit-identically; \
         any difference means the shared-memory read path is not the same code"
    );
}

/// `docs/PHASE5.md` §11's three-way bit-identity, §12 criterion 1: one recording
/// replayed into `HeapArena`, `MappedArena` and `FrozenArena`, identical query
/// set, bit-identical `f64`. The frozen arena is frozen from the heap arena
/// after the replay, so only the backing store differs (§2.1: the frozen read
/// path is the identical `Plan::at`).
#[test]
fn a_replay_into_heap_mapped_and_frozen_arenas_is_bit_identical() {
    let scratch = Scratch::new("bitident3");
    let msgs = small_recording();

    let heap = builder_for(&msgs).build().expect("heap arena");
    let mapped = builder_for(&msgs)
        .build_shared("replay-bitident3")
        .expect("mapped arena");
    assert!(
        mapped.is_shared(),
        "the second arena must actually be mapped, or this compares heap to heap"
    );

    replay(&heap, &msgs);
    replay(&mapped, &msgs);

    // Frozen after the replay, written out and mapped back read-only.
    let tft = scratch.0.join("three-way.tft");
    heap.freeze_to(
        &tft,
        Some("replay-bitident3"),
        [0; 32],
        1_700_000_000_000_000_000,
    )
    .expect("freeze the heap arena");
    let frozen = Tree::open_frozen(&tft).expect("open the frozen arena");

    let pairs = edges_of(&msgs);
    let stamps = probe_stamps(&msgs);
    let a = answers(&heap, &pairs, &stamps);
    let b = answers(&mapped, &pairs, &stamps);
    let c = answers(&frozen, &pairs, &stamps);

    // The same anti-vacuity guards as the pair test.
    assert!(!a.is_empty(), "the query set must not be empty");
    let hits = a.iter().filter(|x| x.is_some()).count();
    assert!(
        hits >= stamps.len(),
        "too few lookups succeeded ({hits}) for this to prove anything"
    );

    assert_eq!(
        a, b,
        "heap and mapped must answer bit-identically over the three-way query set"
    );
    assert_eq!(
        a, c,
        "heap and frozen must answer bit-identically; any difference means the \
         frozen read path is not the identical `Plan::at` code (§2.1)"
    );
}
