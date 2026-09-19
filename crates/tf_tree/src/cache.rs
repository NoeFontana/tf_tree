//! Per-thread compiled-plan cache behind [`crate::Tree::lookup`].
//!
//! Direct-mapped, 16 entries, keyed by `(arena, target, source, generation)`. A
//! topology mutation bumps the generation, so a stale plan is never served.
//!
//! **The arena component is load-bearing** (#196): one thread's cache is shared
//! by every `Tree` it touches, and trees built from the same names in the same
//! order agree on the other three components. `tests/plan_cache_identity.rs`
//! holds the shapes. A slot holds the **result** of compiling that key,
//! refusal included (#259); see [`Entry`] and [`store_refusal`].
//!
//! `docs/API.md` §1 R1 permits `lookup` to collapse the three tiers through this
//! cache.

use std::cell::RefCell;

use tf_tree_core::{FrameId, LookupError, Plan};

use crate::tree::Tree;

/// Number of direct-mapped slots. A power of two so indexing is a mask.
const SLOTS: usize = 16;

/// The odd multiplier [`index`] folds with. Module scope so
/// [`tests::the_low_bit_mask_wins_on_a_small_tree_and_ties_on_a_large_one`]
/// builds its rejected alternative from the same constant. Its final digit is
/// load-bearing; see [`index`].
const MIX: u64 = 0x9E37_79B9_7F4A_7C15;

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
    /// What [`Tree::plan`] answered for [`Entry::key`] — **a refusal included**.
    ///
    /// `compile`'s output, refusals too, is a function of
    /// `(arena, target, source, generation)`. Not functions of the key, and so
    /// not stored: errors from `Plan::at` (they travel in [`with_plan`]'s `R`),
    /// `ChildDetached` (a property of the process after `fork`), and a
    /// `FrameOutOfRange` from exhausting `TOPO_RETRY_LIMIT` (transient); the
    /// last two are declined by [`store_refusal`].
    ///
    /// Storing one costs no bytes: `size_of::<Result<Plan, LookupError>>()` equals
    /// `size_of::<Plan>()` through a niche, pinned by
    /// [`tests::a_refusal_is_free_to_cache`].
    result: Result<Plan, LookupError>,
}

thread_local! {
    static CACHE: RefCell<[Option<Entry>; SLOTS]> = const { RefCell::new([None; SLOTS]) };
}

/// Map a key to its direct-mapped slot.
///
/// Masks the **low** bits deliberately: `MIX` ends in `5`, and multiplication by
/// 5 modulo 16 is a bijection, so keys differing in those bits get distinct slots
/// where a hash collides at the birthday rate.
/// [`tests::the_low_bit_mask_wins_on_a_small_tree_and_ties_on_a_large_one`]
/// pins the win on a small tree (0.72 against 0.51 steady-state residency) and
/// the tie on a large one. The slot decides only whether a lookup hits; the
/// full [`Key`] comparison decides what it may return.
fn index(key: Key) -> usize {
    let mut h = key.scope;
    h = h.wrapping_mul(MIX) ^ u64::from(key.target);
    h = h.wrapping_mul(MIX) ^ u64::from(key.source);
    h = h.wrapping_mul(MIX) ^ key.generation;
    (h as usize) & (SLOTS - 1)
}

/// Evaluate `f` against the cached plan for
/// `(tree's arena, target, source, generation)`, compiling and caching it first
/// on a miss. The `bool` is `true` on a cache hit (used by tests).
///
/// **A refusal is a result and is cached like one** (#259): a pair whose
/// topology refuses (`Disconnected`, `MissingEdge`, `TreeTooDeep`,
/// `UnknownEdge`, `MixedTimeDomains`) answers with the refusal, without calling
/// `f`, instead of recompiling on every lookup. That is −49.6% on a 60-edge
/// chain (579.0 to 291.5 ns) with the hit path unchanged.
///
/// **Why a closure and not `-> Plan`:** `Plan` is `Copy` and 2064 bytes;
/// returning one copies it. Passing `f` and matching through `&entry.result`
/// makes a hit copy nothing. **Do not "simplify" the `&` away**: matching the
/// value lifts the whole `Plan` out of the slot.
///
/// **`f` cannot re-enter the cache:** on a hit it runs under an immutable
/// `RefCell` borrow, so a re-entrant lookup that missed would panic. Today the
/// crate graph prevents it; re-check when adding a second caller whose closure
/// reaches back into `tf_tree`.
///
/// # Errors
///
/// Any [`LookupError`] from compilation on a miss.
pub(crate) fn with_plan<R>(
    tree: &Tree,
    target: FrameId,
    source: FrameId,
    generation: u64,
    f: impl FnOnce(&Plan) -> R,
) -> (Result<R, LookupError>, bool) {
    let key = Key {
        scope: tree.cache_scope(),
        target: target.get(),
        source: source.get(),
        generation,
    };
    let idx = index(key);
    CACHE.with(|c| {
        {
            let slots = c.borrow();
            if let Some(entry) = &slots[idx] {
                if entry.key == key {
                    // Match through the reference so no `Plan` is copied.
                    return match &entry.result {
                        Ok(plan) => (Ok(f(plan)), true),
                        Err(e) => (Err(*e), true),
                    };
                }
            }
        }
        let plan = match tree.plan(target, source) {
            Ok(plan) => plan,
            Err(e) => {
                if store_refusal(tree, key, e) {
                    c.borrow_mut()[idx] = Some(Entry {
                        key,
                        result: Err(e),
                    });
                }
                return (Err(e), false);
            }
        };
        c.borrow_mut()[idx] = Some(Entry {
            key,
            result: Ok(plan),
        });
        (Ok(f(&plan)), false)
    })
}

/// Whether `refusal`, which [`Tree::plan`] just returned for `key`, is a
/// function of `key` and may be stored under it.
///
/// Declines `ChildDetached` (matched by name so
/// [`tests::store_refusal_declines_what_the_key_does_not_determine`] can pin it
/// without a `fork`) and any refusal computed at a generation other than
/// `key.generation`: one relaxed load that makes "nothing is stored whose value
/// the key does not determine" an invariant the code enforces.
fn store_refusal(tree: &Tree, key: Key, refusal: LookupError) -> bool {
    !matches!(refusal, LookupError::ChildDetached)
        && tree.view().topology().stable_generation() == key.generation
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use tf_tree_core::LookupError;

    use crate::{Iso3, TreeBuilder};

    /// [`super::with_plan`] for a pair that is **expected to compile**: asserts
    /// that it did, and returns the hit flag. Without the assertion the hit-rate
    /// tests stay green against a build where nothing compiles, since refusals
    /// are cached too.
    fn compiled_hit(
        tree: &crate::Tree,
        target: tf_tree_core::FrameId,
        source: tf_tree_core::FrameId,
        generation: u64,
    ) -> bool {
        let (compiled, hit) = super::with_plan(tree, target, source, generation, |_| ());
        compiled.expect("expected a compiled plan");
        hit
    }

    /// Two frames in **separate components**. `y` is the frame reparented to
    /// join them because it carries an `edge_of_child`; a link with the `0`
    /// sentinel would refuse `MissingEdge` instead.
    fn two_components() -> (crate::Tree, tf_tree_core::FrameId, tf_tree_core::FrameId) {
        let tree = TreeBuilder::new()
            .static_edge("a", "b", &Iso3::IDENTITY)
            .static_edge("x", "y", &Iso3::IDENTITY)
            .build()
            .unwrap();
        let b = tree.frame("b").unwrap();
        let y = tree.frame("y").unwrap();
        (tree, b, y)
    }

    /// **Caching a refusal costs no bytes** — the claim [`super::Entry`] makes,
    /// pinned because a wider `LookupError` variant would outgrow the niche.
    #[test]
    fn a_refusal_is_free_to_cache() {
        use std::mem::size_of;
        assert_eq!(
            size_of::<Result<tf_tree_core::Plan, LookupError>>(),
            size_of::<tf_tree_core::Plan>(),
            "Result<Plan, LookupError> ({}) outgrew Plan ({}): the Err variant \
             stopped fitting a niche, and every cache slot just grew",
            size_of::<Result<tf_tree_core::Plan, LookupError>>(),
            size_of::<tf_tree_core::Plan>(),
        );
        // An `Entry` is a `Key` rounded up to `Plan`'s alignment, then the `Plan`.
        let a = align_of::<tf_tree_core::Plan>();
        let key_padded = size_of::<super::Key>().div_ceil(a) * a;
        assert_eq!(
            size_of::<super::Entry>(),
            key_padded + size_of::<tf_tree_core::Plan>(),
            "an Entry is a Key padded to Plan's alignment ({}) plus the Plan \
             itself ({}); Entry is {}",
            key_padded,
            size_of::<tf_tree_core::Plan>(),
            size_of::<super::Entry>(),
        );
    }

    /// **A refused pair compiles once, not once per lookup** (#259).
    /// Mutant: delete the `store_refusal` arm ⇒ `hit2` is `false`.
    #[test]
    fn a_refused_pair_is_compiled_once_and_then_answered_from_the_cache() {
        let (tree, b, y) = two_components();
        let g = tree.guard().generation();

        let (r1, hit1) = super::with_plan(&tree, b, y, g, |_| ());
        assert!(!hit1, "the first compile must be a miss");
        let e1 = r1.unwrap_err();
        assert!(
            matches!(e1, LookupError::Disconnected { .. }),
            "expected Disconnected, got {e1:?}"
        );

        let (r2, hit2) = super::with_plan(&tree, b, y, g, |_| ());
        assert!(hit2, "the refusal was recompiled instead of being cached");
        assert_eq!(
            r2.unwrap_err(),
            e1,
            "the cached refusal is not the same one"
        );
    }

    /// **[`super::store_refusal`] declines what the key does not determine**,
    /// both arms. Mutants: body → `true` ⇒ both `assert!(!…)` fail; drop the `!`
    /// on the `matches!` ⇒ four tests fail.
    #[test]
    fn store_refusal_declines_what_the_key_does_not_determine() {
        let (tree, b, y) = two_components();
        let live = tree.guard().generation();
        let key = |generation| super::Key {
            scope: tree.cache_scope(),
            target: b.get(),
            source: y.get(),
            generation,
        };
        let disconnected = LookupError::Disconnected {
            target: b,
            source: y,
            cut_at: b,
        };

        // Control: an ordinary refusal at the live generation must be stored.
        assert!(
            super::store_refusal(&tree, key(live), disconnected),
            "control: a refusal at the live generation must be storable"
        );
        assert!(
            !super::store_refusal(&tree, key(live.wrapping_add(1)), disconnected),
            "a refusal computed under a generation the arena does not have is not \
             about this key"
        );
        assert!(
            !super::store_refusal(&tree, key(live), LookupError::ChildDetached),
            "ChildDetached is about the process, not the key"
        );
    }

    /// **[`super::with_plan`] actually consults [`super::store_refusal`].**
    /// The key carries a generation the arena does not have, so the refusal must
    /// not be filed under it. Mutant: `store_refusal` → `true` ⇒ the final
    /// assertion fails.
    #[test]
    fn a_refusal_is_not_stored_under_a_generation_the_arena_does_not_have() {
        let (tree, b, y) = two_components();
        let live = tree.guard().generation();

        // Control, at the live generation: stored, so the repeat hits.
        assert!(super::with_plan(&tree, b, y, live, |_| ()).0.is_err());
        assert!(
            super::with_plan(&tree, b, y, live, |_| ()).1,
            "control: a refusal at the live generation is cached"
        );

        let stale = live.wrapping_add(1);
        assert!(super::with_plan(&tree, b, y, stale, |_| ()).0.is_err());
        assert!(
            !super::with_plan(&tree, b, y, stale, |_| ()).1,
            "a refusal was filed under a generation it was not computed at"
        );
    }

    /// **A cached refusal does not survive the topology that produced it**: the
    /// key carries the generation, and a mutation bumps it. Mutant: key with
    /// `generation: 0` ⇒ `!hit3` fails.
    #[test]
    fn a_cached_refusal_does_not_survive_the_topology_that_caused_it() {
        let (tree, b, y) = two_components();
        let a = tree.frame("a").unwrap();

        let g1 = tree.guard().generation();
        assert!(super::with_plan(&tree, b, y, g1, |_| ()).0.is_err());
        assert!(
            super::with_plan(&tree, b, y, g1, |_| ()).1,
            "precondition: the refusal is in the cache"
        );

        // Join the two components. `y` keeps its edge record, so the path
        // b -> a <- y is two real edges and compiles.
        tree.reparent(y, a).unwrap();
        let g2 = tree.guard().generation();
        assert_ne!(g1, g2, "re-parent must change the generation");

        let (r3, hit3) = super::with_plan(&tree, b, y, g2, |_| ());
        assert!(!hit3, "a new generation must not hit the old refusal");
        assert!(
            r3.is_ok(),
            "the pair is connected now and still refuses: {:?}",
            r3.unwrap_err()
        );
    }

    /// **An evaluation error is not a compile refusal and is not cached as
    /// one.** `WrongElementType` is used because `compile` cannot produce it.
    /// No mutant: the property is carried by the types, so this guards a
    /// redesign.
    #[test]
    fn an_error_from_the_evaluation_closure_is_not_cached() {
        let tree = TreeBuilder::new()
            .static_edge("a", "b", &Iso3::IDENTITY)
            .build()
            .unwrap();
        let a = tree.frame("a").unwrap();
        let b = tree.frame("b").unwrap();
        let g = tree.guard().generation();

        let (r1, hit1) = super::with_plan(&tree, b, a, g, |_| {
            Err::<(), LookupError>(LookupError::WrongElementType)
        });
        assert!(!hit1, "the first compile must be a miss");
        assert!(r1.unwrap().is_err(), "precondition: the closure did fail");

        let (r2, hit2) = super::with_plan(&tree, b, a, g, |_| Ok::<(), LookupError>(()));
        assert!(hit2, "the plan itself must still have been cached");
        assert!(
            r2.unwrap().is_ok(),
            "the closure's error was stored as if the compile had refused it"
        );
    }

    /// The [`super::index`] mask keeps more of a small tree's working set
    /// resident than the high bits of a final multiply: the measurement behind
    /// that function's doc. Residency is the fraction of a working set whose slot
    /// no other member shares, over 2000 seeded sets. Both assertions are
    /// relative, so a retune of [`super::MIX`] or [`super::SLOTS`] that
    /// regressed nothing does not fail the build.
    #[test]
    fn the_low_bit_mask_wins_on_a_small_tree_and_ties_on_a_large_one() {
        // The rejected alternative: one more multiply, top four bits.
        fn hashed(key: super::Key) -> usize {
            let mut h = key.scope;
            h = h.wrapping_mul(super::MIX) ^ u64::from(key.target);
            h = h.wrapping_mul(super::MIX) ^ u64::from(key.source);
            h = h.wrapping_mul(super::MIX) ^ key.generation;
            (h.wrapping_mul(super::MIX) >> (u64::BITS - super::SLOTS.trailing_zeros())) as usize
        }

        // Mean residency of a `pairs`-pair working set, as (mask, alternative);
        // xorshift64 keeps the sets identical on every run.
        let residency = |frames: u32, pairs: usize| {
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
                for _ in 0..pairs {
                    let key = super::Key {
                        scope: 1,
                        target: (next() % u64::from(frames)) as u32,
                        source: (next() % u64::from(frames)) as u32,
                        generation: 0,
                    };
                    mask_slots.push(super::index(key));
                    hash_slots.push(hashed(key));
                }
                mask_total += resident(&mask_slots);
                hash_total += resident(&hash_slots);
            }
            (
                mask_total / f64::from(trials),
                hash_total / f64::from(trials),
            )
        };

        // Eight frames: every id fits the mask, so it is a permutation.
        let (mask, hash) = residency(8, 6);
        assert!(
            mask > hash + 0.15,
            "on an 8-frame tree the mask {mask} should beat the alternative {hash} \
             by the margin the choice was made on"
        );
        // Forty frames: the ids no longer fit, and the two tie.
        let (mask, hash) = residency(40, 6);
        assert!(
            (mask - hash).abs() < 0.05,
            "on a 40-frame tree the mask {mask} and the alternative {hash} tie; \
             a gap either way means the index changed shape, not tuning"
        );
    }

    /// Two trees on one thread do not evict each other: each still hits on a
    /// repeat and they hold distinct arena ids (#196; and the cache must still
    /// cache, `docs/API.md` §1 R1).
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
        // The #196 precondition: everything but the arena id agrees.
        assert_eq!((a1.get(), b1.get(), g1), (a2.get(), b2.get(), g2));

        let probe = |t: &crate::Tree, target, source, g| compiled_hit(t, target, source, g);
        assert!(!probe(&first, b1, a1, g1));
        assert!(probe(&first, b1, a1, g1), "the first tree's repeat hits");
        assert!(
            !probe(&second, b2, a2, g2),
            "the second tree must not be served the first tree's plan"
        );
        assert!(probe(&second, b2, a2, g2), "the second tree's repeat hits");
    }

    /// A repeated lookup hits the cache; a topology change (new generation)
    /// produces a freshly-compiled plan stamped with the new generation.
    #[test]
    fn cache_hits_and_invalidates_on_generation() {
        // `c` has an edge so it can be re-parented to bump the generation.
        let tree = TreeBuilder::new()
            .static_edge("a", "b", &Iso3::IDENTITY)
            .static_edge("b", "c", &Iso3::IDENTITY)
            .build()
            .unwrap();
        let a = tree.frame("a").unwrap();
        let b = tree.frame("b").unwrap();
        let c = tree.frame("c").unwrap();

        let gen1 = tree.guard().generation();
        let (g1_stamped, hit1) =
            super::with_plan(&tree, b, a, gen1, tf_tree_core::Plan::generation);
        assert!(!hit1, "first compile is a miss");
        assert_eq!(g1_stamped.unwrap(), gen1);

        // An immediate repeat with the same key hits the per-thread cache.
        let hit2 = compiled_hit(&tree, b, a, gen1);
        assert!(hit2, "repeat lookup hits the cache");

        // A runtime re-parent bumps the generation; the recompiled plan carries it.
        tree.reparent(c, a).unwrap();
        let gen2 = tree.guard().generation();
        assert_ne!(gen1, gen2, "re-parent must change the generation");

        let (g3_stamped, _hit3) =
            super::with_plan(&tree, b, a, gen2, tf_tree_core::Plan::generation);
        assert_eq!(
            g3_stamped.unwrap(),
            gen2,
            "post-change plan is stamped with the new generation"
        );
    }

    /// The cache hits *exactly* where its index predicts, on **live trees**: N
    /// trees, round-robin, three rounds, counting rounds after the first.
    ///
    /// It asserts no fixed hit rate: `next_local_scope`'s counter is
    /// process-global, so under `just miri`'s multi-threaded harness ids have
    /// gaps and two of sixteen can collide. The expectation is derived from the
    /// minted ids through [`super::index`]. This catches the #196 defect, an
    /// eviction or install bug, and an `index` that stopped being a bijection on
    /// the low bits of `scope`. N = 17 stays because seventeen trees cannot all
    /// be resident in sixteen slots.
    #[test]
    fn the_cache_hits_exactly_where_its_index_predicts() {
        let build = || {
            TreeBuilder::new()
                .static_edge("a", "b", &Iso3::IDENTITY)
                .static_edge("b", "c", &Iso3::IDENTITY)
                .build()
                .unwrap()
        };
        // How many members of `slots` no other member collides with.
        let resident = |slots: &[usize]| {
            slots
                .iter()
                .filter(|&&s| slots.iter().filter(|&&o| o == s).count() == 1)
                .count()
        };

        const ROUNDS: usize = 3;
        for n in [2usize, 16, 17] {
            let trees: Vec<crate::Tree> = (0..n).map(|_| build()).collect();

            // The #196 precondition, asserted: only the arena id differs.
            let a = trees[0].frame("a").unwrap();
            let c = trees[0].frame("c").unwrap();
            let g = trees[0].guard().generation();
            for t in &trees {
                assert_eq!(
                    (
                        t.frame("a").unwrap().get(),
                        t.frame("c").unwrap().get(),
                        t.guard().generation()
                    ),
                    (a.get(), c.get(), g),
                    "the trees must agree on everything but their arena id"
                );
            }

            let slots: Vec<usize> = trees
                .iter()
                .map(|t| {
                    super::index(super::Key {
                        scope: t.cache_scope(),
                        target: a.get(),
                        source: c.get(),
                        generation: g,
                    })
                })
                .collect();
            let residues: Vec<usize> = trees
                .iter()
                .map(|t| (t.cache_scope() as usize) & (super::SLOTS - 1))
                .collect();

            let (mut hits, mut total) = (0usize, 0usize);
            for round in 0..ROUNDS {
                for tree in &trees {
                    let hit = compiled_hit(tree, a, c, g);
                    if round > 0 {
                        total += 1;
                        hits += usize::from(hit);
                    }
                }
            }

            assert_eq!(
                hits,
                resident(&slots) * (ROUNDS - 1),
                "{n} trees: the cache hit {hits} times in {total} steady-state \
                 lookups, but its own index puts {} of them in a slot no other \
                 tree shares",
                resident(&slots)
            );
            assert_eq!(
                resident(&slots),
                resident(&residues),
                "{n} trees: `index` must separate arena ids exactly as their low \
                 {} bits do — it is a permutation of them, and that is what \
                 keeps consecutive ids from thrashing",
                super::SLOTS.trailing_zeros()
            );
            if n > super::SLOTS {
                assert!(
                    hits < total,
                    "{n} trees cannot all be resident in {} slots — {hits} of \
                     {total} means the arena component stopped separating them",
                    super::SLOTS
                );
            }
        }
    }

    /// Two handles onto **one shared segment** share one arena identity and so
    /// one set of cached plans: the peer's *first* lookup must hit the owner's
    /// entry. Runs only under `just shm-check`. No `TF_TREE_RUNTIME_DIR` scratch
    /// directory: `build_shared` is `memfd_create` plus `mmap` and touches no
    /// lock file or socket.
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
        assert!(!compiled_hit(&owner, b, a, g), "cold cache");
        assert!(
            compiled_hit(&peer, b, a, g),
            "the peer's FIRST lookup must reuse the owner's plan, not recompile"
        );
    }
}
