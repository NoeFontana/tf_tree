//! `docs/PHASE2.md` §10's NORMATIVE test, and §15's box 11.
//!
//! §10: *"Replay one recording into a `HeapArena` and a `MappedArena`, run an
//! identical query set against both, and assert **bit-identical `f64`
//! results**, not approximate equality. Lookups are pure functions of `(plan,
//! stamp, buffer contents)`, so any difference at all means the shared-memory
//! path is not the same code, which is the central claim of this phase."*
//!
//! # Why this is the missing test rather than a third one
//!
//! Two bit-identity tests already exist and neither is this pair.
//! `a_frozen_lookup_is_bit_identical_to_the_live_one` compares **heap against
//! frozen**; `another_process_reads_the_same_arena_bit_identically` compares
//! **one mapped segment against itself, from another process**. Heap against
//! mapped — the pair §10 names, and the one that would catch a shared-memory
//! read path that had diverged from the in-process one — was covered by
//! neither.
//!
//! # One variable
//!
//! Both trees are filled by the **same** `replay` function from the **same**
//! `Vec<FixtureMessage>`, so the only difference between them is the backing
//! store. Ingesting into one and hand-pushing into the other would have
//! compared two code paths as well as two backends, and a difference could then
//! be attributed to either.
//!
//! The recording is real: it is written to MCAP and read back, so the messages
//! both trees replay have been through the same serialisation a robot's bag
//! would put them through.

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

/// Every `(parent, child)` edge the recording declares, in first-seen order.
///
/// Order matters and is taken from the recording rather than sorted: edge ids
/// are assigned at declaration time and append-only (D10), so declaring in a
/// different order would give the two arenas different `EdgeId`s for the same
/// edge — a difference that is not the one under test.
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

/// Declare the recording's topology. Every edge is dynamic, including the ones
/// the recording published on `/tf_static`: a static edge holds one inline pose
/// and no ring, so replaying a static edge's samples is not possible and the
/// comparison would be over a different quantity in each arena.
fn builder_for(msgs: &[FixtureMessage]) -> TreeBuilder {
    let mut b = TreeBuilder::new();
    for (parent, child) in edges_of(msgs) {
        b = b.dynamic_edge(&parent, &child, EdgeCfg::new(Capacity::slots(256)));
    }
    b
}

/// Push every transform in `msgs` into `tree`, in recording order.
///
/// This is the "replay" half of §10, and it is deliberately one function used
/// for both arenas.
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
            // A recording can carry two transforms with one stamp on one edge;
            // the ring refuses the second, identically in both arenas.
            let _ = w.push(t.stamp_ns, &iso);
        }
    }
}

/// The query set, run against both arenas.
///
/// Stamps straddle the recording rather than sitting on its samples: a query
/// that lands exactly on a stored stamp returns that sample unchanged and would
/// compare two memcpys, where an interpolated one compares the arithmetic.
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

    // Through a real MCAP file and back, so both arenas replay messages that
    // have been serialised and parsed rather than held in memory.
    let bag = scratch.0.join("run.mcap");
    write_mcap(&bag, &msgs).expect("write the recording");
    assert!(bag.is_file(), "the recording must exist to be replayed");

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

    // Two anti-vacuity guards, because a comparison of two empty vectors, or of
    // two vectors of `None`, is satisfied by any pair of arenas at all.
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
