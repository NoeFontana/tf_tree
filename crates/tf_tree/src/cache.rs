//! Per-thread compiled-plan cache behind [`crate::Tree::lookup`].
//!
//! A direct-mapped, 16-entry cache keyed by
//! `(arena, target, source, generation)`. A topology mutation bumps the
//! generation, so a stale plan can never be served: the key simply no longer
//! matches and the entry is recompiled. The cache is `thread_local!`, so it
//! needs `std` and lives in the facade, not the `no_std` core.
//!
//! **The arena component is load-bearing** (issue #196). One thread's cache is
//! shared by every `Tree` it touches, and the other three components collide
//! across trees as a matter of course: `FrameId`s are handed out in interning
//! order, and a freshly built tree's generation is **its declared edge count**
//! — one tick per link, measured, not zero — so two trees built from the same
//! names in the same order agree on all three. A plan is edge indices plus the
//! static transforms folded along the way, so serving one across trees returns
//! the other tree's numbers where the topologies match and a number belonging
//! to neither where they do not.
//!
//! That the generation is the edge count rather than a constant is why the
//! defect is *narrower* than "any two trees": two trees with different edge
//! counts miss on the generation and never reach the wrong plan at all. It is
//! also why the narrowing is worth nothing as a defence — the sibling cases
//! that matter (a tree rebuilt in a loop, two robots of the same model, a
//! test suite's fixtures) are exactly the ones whose edge counts agree.
//! `tests/plan_cache_identity.rs` is the three shapes of that.
//!
//! `docs/API.md` §1 R1 permits `lookup` to collapse the three tiers on the
//! condition that it goes through this cache; R2's "never allocates, never
//! locks" governs the *hot* tier — `Plan::at` — and this probe is neither an
//! allocation nor a lock in any case.

use std::cell::RefCell;

use tf_tree_core::{FrameId, LookupError, Plan};

use crate::tree::Tree;

/// Number of direct-mapped slots. A power of two so indexing is a mask.
const SLOTS: usize = 16;

/// What a cached plan was compiled from and for.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Key {
    /// Which arena the plan is meaningful against (`Tree::cache_scope`).
    scope: u64,
    target: u32,
    source: u32,
    generation: u64,
}

#[derive(Clone, Copy)]
struct Entry {
    key: Key,
    plan: Plan,
}

thread_local! {
    static CACHE: RefCell<[Option<Entry>; SLOTS]> = const { RefCell::new([None; SLOTS]) };
}

/// Map a key to its direct-mapped slot.
///
/// The fold seeds on the arena id and ends by masking the **low** bits, which
/// is what the pre-#196 version did and is kept deliberately. Masking the low
/// bits of a multiply looks like the weak choice — the low bits of a product
/// depend only on the low bits of its operands, so the slot is a function of
/// the low four bits of each component alone — and taking the high bits of a
/// final multiply instead was written and measured first. **The measurement
/// refuted it.** `0x9E37_79B9_7F4A_7C15` ends in `5`, and multiplication by 5
/// modulo 16 is a bijection, so the mask is a *permutation* of the low bits of
/// the key rather than a hash of them: keys that differ in those bits are
/// guaranteed distinct slots, where a hash scatters them at random and collides
/// at the birthday rate.
///
/// Steady-state residency in this 16-entry cache (fraction of a working set
/// whose slot no other member shares), 2000 random working sets each:
///
/// | working set | this mask | high bits of a final multiply |
/// |---|---|---|
/// | 1 tree, 6 pairs, 8 frames | **0.719** | 0.513 |
/// | 1 tree, 6 pairs, 40 frames | 0.717 | 0.726 |
/// | 1 tree, 16 pairs, 40 frames | 0.384 | 0.378 |
/// | 4 trees, 3 pairs, 8 frames | 0.493 | 0.505 |
/// | 2 trees, 8 pairs, 40 frames | 0.384 | 0.381 |
///
/// [`tests::the_low_bit_mask_beats_a_hash_on_a_small_tree`] re-measures the
/// first row in-crate, so the choice is not defended by a number in a comment
/// alone; it draws its own working sets and reads 0.722 against 0.510.
///
/// The mask wins where a tree is small enough for its ids to fit in the mask
/// and ties elsewhere; one pair looked up across 16 consecutive arena ids lands
/// in 16 distinct slots under the mask and 12 under the hash. The hash's only
/// win is a population whose ids are all congruent modulo 16 — twelve frames at
/// 3, 19, 35, … collapse onto one slot here — which a dense, interning-order id
/// counter does not produce.
///
/// None of this is a correctness argument: the slot decides only whether a
/// lookup hits, and the full [`Key`] comparison decides what it is allowed to
/// return.
fn index(key: Key) -> usize {
    const MIX: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut h = key.scope;
    h = h.wrapping_mul(MIX) ^ u64::from(key.target);
    h = h.wrapping_mul(MIX) ^ u64::from(key.source);
    h = h.wrapping_mul(MIX) ^ key.generation;
    (h as usize) & (SLOTS - 1)
}

/// Return the cached plan for `(tree's arena, target, source, generation)`,
/// compiling and caching it on a miss. The `bool` is `true` on a cache hit
/// (used by tests).
///
/// The arena identity is read from `tree` rather than passed in, so no caller
/// can supply one that does not belong to the tree it is about to compile from.
///
/// # Errors
///
/// Any [`LookupError`] from compilation on a miss.
pub(crate) fn get_or_compile(
    tree: &Tree,
    target: FrameId,
    source: FrameId,
    generation: u64,
) -> Result<(Plan, bool), LookupError> {
    let key = Key {
        scope: tree.cache_scope(),
        target: target.get(),
        source: source.get(),
        generation,
    };
    let idx = index(key);
    CACHE.with(|c| {
        // Fast path: a matching entry. Copy it out before releasing the borrow.
        if let Some(entry) = c.borrow()[idx] {
            if entry.key == key {
                return Ok((entry.plan, true));
            }
        }
        // Miss: compile (does not touch the cache) then install.
        let plan = tree.plan(target, source)?;
        c.borrow_mut()[idx] = Some(Entry { key, plan });
        Ok((plan, false))
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use crate::{Iso3, TreeBuilder};

    /// The [`super::index`] mask keeps more of a small tree's working set
    /// resident than the obvious alternative, taking the high bits of a final
    /// multiply. This is the measurement behind that function's doc comment,
    /// and the reason the pre-#196 low-bit mask was kept when the key grew.
    ///
    /// A key hits in steady state iff no other key in the working set maps to
    /// its slot, so the metric is the fraction of a working set whose slot is
    /// its own, averaged over 2000 synthetic sets from a fixed seed.
    #[test]
    fn the_low_bit_mask_beats_a_hash_on_a_small_tree() {
        // The alternative arm, not the shipped one: `super::index`'s fold with
        // one more multiply, taking the top four bits instead of the bottom.
        fn hashed(key: super::Key) -> usize {
            const MIX: u64 = 0x9E37_79B9_7F4A_7C15;
            let mut h = key.scope;
            h = h.wrapping_mul(MIX) ^ u64::from(key.target);
            h = h.wrapping_mul(MIX) ^ u64::from(key.source);
            h = h.wrapping_mul(MIX) ^ key.generation;
            (h.wrapping_mul(MIX) >> (u64::BITS - super::SLOTS.trailing_zeros())) as usize
        }

        // xorshift64, so the working sets are the same on every host and every
        // run — a flaky instrument would be worse than no measurement.
        let mut state = 0x1234_5678_9ABC_DEF1u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let resident = |slots: &[usize]| {
            let mut counts = [0usize; super::SLOTS];
            for &s in slots {
                counts[s] += 1;
            }
            slots.iter().filter(|&&s| counts[s] == 1).count() as f64 / slots.len() as f64
        };

        let (mut mask_total, mut hash_total) = (0.0, 0.0);
        let trials = 2000;
        for _ in 0..trials {
            let (mut mask_slots, mut hash_slots) = (Vec::new(), Vec::new());
            for _ in 0..6 {
                let key = super::Key {
                    scope: 1,
                    target: (next() % 8) as u32,
                    source: (next() % 8) as u32,
                    generation: 0,
                };
                mask_slots.push(super::index(key));
                hash_slots.push(hashed(key));
            }
            mask_total += resident(&mask_slots);
            hash_total += resident(&hash_slots);
        }
        let (mask, hash) = (
            mask_total / f64::from(trials),
            hash_total / f64::from(trials),
        );
        // Measured 0.722 against 0.510; the bounds leave room for a different
        // `f64` summation order without leaving room for the two to swap.
        assert!(mask > 0.70, "mask residency {mask}");
        assert!(hash < 0.55, "hash residency {hash}");
        assert!(
            mask > hash + 0.15,
            "mask {mask} should beat hash {hash} on an 8-frame tree"
        );
    }

    /// Two trees on one thread do not evict each other's entries: each still
    /// hits on a repeat, and they hold distinct arena ids.
    ///
    /// The fix for #196 is a fix only if the cache still caches. Widening the
    /// key so far that nothing ever matches — or dropping the cache — would
    /// answer every one of `tests/plan_cache_identity.rs`'s cases correctly and
    /// be worth nothing, and would also breach `docs/API.md` §1 R1, which
    /// permits `lookup` to collapse the three tiers *on the condition* that it
    /// goes through this cache rather than re-resolving topology per call.
    #[test]
    fn two_trees_keep_separate_entries_and_still_hit() {
        let build = || {
            TreeBuilder::new()
                .static_edge("a", "b", &Iso3::IDENTITY)
                .build()
                .unwrap()
        };
        let first = build();
        let second = build();
        assert_ne!(
            first.cache_scope(),
            second.cache_scope(),
            "two heap trees are two arenas"
        );

        let key_of = |t: &crate::Tree| {
            let a = t.frame("a").unwrap();
            let b = t.frame("b").unwrap();
            (a, b, t.guard().generation())
        };
        let (a1, b1, g1) = key_of(&first);
        let (a2, b2, g2) = key_of(&second);
        // The precondition for the whole bug: everything except the arena id
        // agrees between the two trees.
        assert_eq!((a1.get(), b1.get(), g1), (a2.get(), b2.get(), g2));

        assert!(!super::get_or_compile(&first, b1, a1, g1).unwrap().1);
        assert!(
            super::get_or_compile(&first, b1, a1, g1).unwrap().1,
            "the first tree's repeat hits"
        );
        assert!(
            !super::get_or_compile(&second, b2, a2, g2).unwrap().1,
            "the second tree must not be served the first tree's plan"
        );
        assert!(
            super::get_or_compile(&second, b2, a2, g2).unwrap().1,
            "the second tree's repeat hits"
        );
    }

    /// A repeated lookup hits the cache; a topology change (new generation)
    /// produces a freshly-compiled plan stamped with the new generation.
    #[test]
    fn cache_hits_and_invalidates_on_generation() {
        // `c` is declared with an edge (under `b`) so it can be re-parented at
        // runtime to bump the topology generation.
        let tree = TreeBuilder::new()
            .static_edge("a", "b", &Iso3::IDENTITY)
            .static_edge("b", "c", &Iso3::IDENTITY)
            .build()
            .unwrap();
        let a = tree.frame("a").unwrap();
        let b = tree.frame("b").unwrap();
        let c = tree.frame("c").unwrap();

        let gen1 = tree.guard().generation();
        let (p1, hit1) = super::get_or_compile(&tree, b, a, gen1).unwrap();
        assert!(!hit1, "first compile is a miss");
        assert_eq!(p1.generation(), gen1);

        // An immediate repeat with the same key hits the per-thread cache.
        let (_p2, hit2) = super::get_or_compile(&tree, b, a, gen1).unwrap();
        assert!(hit2, "repeat lookup hits the cache");

        // A runtime re-parent bumps the generation; the recompiled plan carries it.
        tree.reparent(c, a).unwrap();
        let gen2 = tree.guard().generation();
        assert_ne!(gen1, gen2, "re-parent must change the generation");

        let (p3, _hit3) = super::get_or_compile(&tree, b, a, gen2).unwrap();
        assert_eq!(
            p3.generation(),
            gen2,
            "post-change plan is stamped with the new generation"
        );
    }

    /// The cache still caches, measured on **live trees** rather than on a model
    /// of the index function.
    ///
    /// [`the_low_bit_mask_beats_a_hash_on_a_small_tree`] argues the *choice* of
    /// index from synthetic keys; this measures the thing that choice is for.
    /// N trees, round-robin, twenty rounds, counting only the rounds after the
    /// first — the steady state a real reader is in.
    ///
    /// Measured on this host: **1.000 at N = 2, 3, 5 and 8**, falling to 0.882
    /// at N = 17. The perfect run up to 16 is not luck and not a tolerance: for
    /// a fixed `(target, source, generation)` the map from `scope` to slot is a
    /// *bijection on the low four bits* — `index` multiplies by a constant
    /// ending in 5 and masks, and multiplication by 5 modulo 16 is a
    /// permutation — so the counter's consecutive arena ids are guaranteed
    /// distinct slots until they wrap past the 16 the cache has. Two heap trees
    /// interleaving their lookups therefore cannot thrash, which is the
    /// performance regression this fix could plausibly have introduced and did
    /// not. Shared arenas draw their scope from a uuid rather than the counter,
    /// so they collide at the 1-in-16 rate instead of never.
    ///
    /// **Instrument check** (the reason the N = 17 row is here at all): with the
    /// arena component removed from the key every row reads 1.000, including
    /// N = 17 — because all seventeen trees share one entry, which is the
    /// defect. A hit-rate test that only ever measured N <= 16 would report
    /// 1.000 either way and prove nothing.
    #[test]
    fn the_cache_still_hits_with_many_live_trees() {
        let build = || {
            TreeBuilder::new()
                .static_edge("a", "b", &Iso3::IDENTITY)
                .static_edge("b", "c", &Iso3::IDENTITY)
                .build()
                .unwrap()
        };
        let rate = |n: usize| {
            let trees: Vec<crate::Tree> = (0..n).map(|_| build()).collect();
            let (mut hits, mut total) = (0u32, 0u32);
            for round in 0..20 {
                for tree in &trees {
                    let a = tree.frame("a").unwrap();
                    let c = tree.frame("c").unwrap();
                    let g = tree.guard().generation();
                    let (_, hit) = super::get_or_compile(tree, a, c, g).unwrap();
                    if round > 0 {
                        total += 1;
                        hits += u32::from(hit);
                    }
                }
            }
            f64::from(hits) / f64::from(total)
        };

        for n in [2usize, 3, 5, 8, 16] {
            let r = rate(n);
            assert!(
                (r - 1.0).abs() < f64::EPSILON,
                "{n} live trees should all stay resident, got {r}"
            );
        }
        // Past the slot count the cache degrades rather than collapsing. The
        // bound is loose on purpose: the exact figure is a property of which
        // ids these particular trees interned, and pinning it would make an
        // unrelated change to interning order look like a cache regression.
        let over = rate(17);
        assert!(over > 0.5, "17 live trees collapsed to {over}");
        assert!(
            over < 1.0,
            "17 trees cannot all be resident in {} slots — {over} means the \
             arena component stopped separating them",
            super::SLOTS
        );
    }

    /// Two handles onto **one shared segment** share one arena identity, and
    /// therefore one set of cached plans.
    ///
    /// This is the other half of the fix and the half a test could easily get
    /// backwards. Keying on the *handle* would answer every case in
    /// `tests/plan_cache_identity.rs` correctly and still be wrong here: two
    /// `Tree`s mapping one segment see one topology and one set of static
    /// transforms, so a plan compiled through either is correct through the
    /// other, and giving them separate identities would cost every second
    /// handle a recompile for no safety. The peer's *first* lookup hitting the
    /// owner's entry is the assertion that pins that.
    ///
    /// Measured here: owner and peer both report the same tagged scope, and the
    /// peer's first `get_or_compile` returns `hit = true`.
    ///
    /// Reachable only through `just shm-check`'s
    /// `cargo nextest run -p tf_tree --features shm --lib` line, which was added
    /// with this test — `just test` builds default features, so without that
    /// line this would be compiled by clippy and executed by nothing.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    #[test]
    fn two_handles_on_one_shared_arena_share_their_plans() {
        let owner = TreeBuilder::new()
            .static_edge("a", "b", &Iso3::IDENTITY)
            .build_shared("tf_tree-cache-identity-test")
            .unwrap();
        let fd = owner
            .shared_fd()
            .expect("a build_shared tree has a segment fd")
            .try_clone_to_owned()
            .unwrap();
        let peer = crate::Tree::attach_shared(fd, crate::AttachMode::ReadOnly).unwrap();

        assert_eq!(
            owner.cache_scope(),
            peer.cache_scope(),
            "one segment is one arena; both handles must key the same"
        );
        assert_eq!(
            owner.cache_scope() >> 63,
            1,
            "a shared scope carries the tag bit that keeps it out of the counter's space"
        );

        let a = owner.frame("a").unwrap();
        let b = owner.frame("b").unwrap();
        let g = owner.guard().generation();
        assert!(
            !super::get_or_compile(&owner, b, a, g).unwrap().1,
            "cold cache"
        );
        assert!(
            super::get_or_compile(&peer, b, a, g).unwrap().1,
            "the peer's FIRST lookup must reuse the owner's plan, not recompile"
        );
    }
}
