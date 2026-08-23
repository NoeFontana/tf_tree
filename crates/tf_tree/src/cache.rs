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

/// The odd multiplier [`index`] folds with.
///
/// At module scope rather than inside [`index`] because
/// [`tests::the_low_bit_mask_wins_on_a_small_tree_and_ties_on_a_large_one`] builds the
/// *rejected* alternative out of it: a private copy there would keep comparing
/// the retuned shipped constant against a baseline built from the old one, and
/// the comparison would stay green while measuring two different functions.
///
/// Its final digit is load-bearing — see [`index`].
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
/// [`tests::the_low_bit_mask_wins_on_a_small_tree_and_ties_on_a_large_one`]
/// re-measures the first two rows in-crate, so the choice is not defended by a
/// number in a comment alone; it draws its own working sets and reads 0.722
/// against 0.510, then 0.717 against 0.726. It asserts only the *relative*
/// claim in each — the win, and the tie — because the absolute figures are
/// tuning, and a test that pins tuning fails on a retune that regressed
/// nothing.
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
/// The arena identity is read from `tree` rather than passed in, so no caller
/// can supply one that does not belong to the tree it is about to compile from.
///
/// # Why a closure and not `-> Plan`
///
/// `Plan` is `Copy` and **4160 bytes** (`size_of`, measured; `align_of` 64,
/// which is what pads the 4184-byte `Entry` out to a 4224-byte slot, and the
/// 16-slot table to 66.0 KiB per thread). Returning one hands the caller a copy,
/// and the caller is `Tree::lookup`, which only wants to call `Plan::at` on it.
///
/// **Every byte count in the table below was measured at `MAX_DEPTH = 16`**,
/// where `Plan` was 2112 and the slot 2176, and `0034` has since moved the
/// constant to 32. The counts are not re-taken — the `LD_PRELOAD` interposer
/// session is not reproducible from here — and they do not need to be: what the
/// table establishes is *how many* copies each binding makes, which is a
/// property of the code shape and not of the array's length. The doubling makes
/// the argument stronger, since every copy the shipped form avoids is now twice
/// the size.
///
/// **Counted, not read off a disassembly.** A `memcpy` interposer
/// (`LD_PRELOAD`, versioned `memcpy@GLIBC_2.14`) over a hot-cache harness, with
/// the totals differenced between 1000 and 2000 lookups so process start-up
/// cancels exactly. Three builds of one harness, because **two things had to
/// change together and either one alone buys nothing**:
///
/// | probe binding | returns | copies per cache hit | ns/lookup | vs. shipped, same session |
/// |---|---|---|---|---|
/// | `&slots[idx]` | `Plan` | 3 x 2072 B | 606.9-616.2 | 575.3-580.1 |
/// | `slots[idx]` | closure | 1 x 2176 B | 616.4-623.4 | 580.5-589.6 |
/// | `&slots[idx]` | closure | **none** | — | — |
///
/// Row 3 is what ships, so it has no row of its own: it is the right-hand
/// column, re-measured against each rejected arm in that arm's own session. The
/// 2072-byte copies in row 1 are the returned `Plan` travelling slot -> stack
/// temporary -> caller; the single 2176-byte copy in row 2 is the whole `Entry`
/// lifted out of the slot *before* the key is compared, to decide 24 bytes'
/// worth of question. Only the shipped form has neither. What survives on the
/// hit path is two 184-byte copies of the `Result` return, which were there
/// before and are not the plan.
///
/// Timings are medians of 25 rounds of 200 000 calls, the two builds under
/// comparison run alternately — 6 runs each for row 1, 5 for row 2 — and
/// non-overlapping in both pairings. The shipped arm reads 575-580 in one
/// session and 580-590 in the other, which is why every comparison is paired
/// and alternating and why no cell here may be read against a cell from the
/// other row: the paired difference is the claim, the absolute number is not
/// (`PHASE5.md` §9.3).
///
/// # The `&` is load-bearing, and this comment used to say it was not
///
/// An earlier revision recorded that `slots[idx]` and `&slots[idx]` compile to
/// byte-identical machine code because LLVM sinks the copy past the key
/// comparison, and concluded the reference was cosmetic. That was measured on
/// the `-> Plan` shape, where it is beside the point: a function returning
/// `Plan` by value copies the entry regardless, so removing one copy changes
/// nothing. Once the return became a closure the copy had nowhere else to go,
/// and row 2 is what it costs — a full slot memcpy per hit and the entire win
/// gone. Do not "simplify" the `&` away.
///
/// # `f` cannot re-enter the cache
///
/// On a hit it runs while the `RefCell` is immutably borrowed, so a re-entrant
/// lookup that *missed* would panic in `borrow_mut`. Today that is not a
/// convention to be careful about but a property of the crate graph: the one
/// caller's closure does `Tree::guard` then `Plan::at`, and `Guard` and `Plan`
/// live in `tf_tree_core`, which `tf_tree` depends on and which therefore cannot
/// name this module. A `pub(crate)` with one caller is the other half — the
/// thing to re-check is a **second** caller whose closure reaches back into
/// `tf_tree`, because that one would compile.
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
) -> Result<(R, bool), LookupError> {
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
                    return Ok((f(&entry.plan), true));
                }
            }
        }
        let plan = tree.plan(target, source)?;
        c.borrow_mut()[idx] = Some(Entry { key, plan });
        Ok((f(&plan), false))
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
    ///
    /// **Two rows, and the second one is the honest one.** The mask wins where
    /// a tree's ids fit inside the four bits it keeps and *ties* once they do
    /// not, so a test carrying only the winning row would read as a claim that
    /// the mask is better everywhere. Neither assertion is absolute: pinning
    /// "the mask scores 0.72" would fail the build on a retune of [`super::MIX`]
    /// or [`super::SLOTS`] that regressed nothing, and pinning "the alternative
    /// scores under 0.55" would make the *rejected* arm part of the contract.
    /// The claim is relative, per row, which is the claim the choice rests on.
    ///
    /// The alternative arm reads [`super::MIX`] rather than declaring its own
    /// copy. With a copy, changing the shipped constant compared the new mask
    /// against a baseline built from the old one — two different functions,
    /// green either way.
    #[test]
    fn the_low_bit_mask_wins_on_a_small_tree_and_ties_on_a_large_one() {
        // The alternative arm, not the shipped one: `super::index`'s fold with
        // one more multiply, taking the top four bits instead of the bottom.
        fn hashed(key: super::Key) -> usize {
            let mut h = key.scope;
            h = h.wrapping_mul(super::MIX) ^ u64::from(key.target);
            h = h.wrapping_mul(super::MIX) ^ u64::from(key.source);
            h = h.wrapping_mul(super::MIX) ^ key.generation;
            (h.wrapping_mul(super::MIX) >> (u64::BITS - super::SLOTS.trailing_zeros())) as usize
        }

        // Mean residency of a `pairs`-pair working set over one tree of
        // `frames` frames, as (mask, alternative).
        let residency = |frames: u32, pairs: usize| {
            // xorshift64, so the working sets are the same on every host and
            // every run — a flaky instrument would be worse than no
            // measurement.
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

        // Eight frames: every id fits in the mask's four bits, so the mask is a
        // permutation where the alternative is a hash colliding at the birthday
        // rate. Measured 0.722 against 0.510.
        let (mask, hash) = residency(8, 6);
        assert!(
            mask > hash + 0.15,
            "on an 8-frame tree the mask {mask} should beat the alternative {hash} \
             by the margin the choice was made on"
        );
        // Forty frames: the ids no longer fit, and the two are the same
        // instrument. Measured 0.717 against 0.726 — the alternative is ahead,
        // by less than the third digit of either.
        let (mask, hash) = residency(40, 6);
        assert!(
            (mask - hash).abs() < 0.05,
            "on a 40-frame tree the mask {mask} and the alternative {hash} tie; \
             a gap either way means the index changed shape, not tuning"
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

        let probe = |t: &crate::Tree, target, source, g| {
            super::with_plan(t, target, source, g, |_| ()).unwrap().1
        };
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
        let (g1_stamped, hit1) =
            super::with_plan(&tree, b, a, gen1, tf_tree_core::Plan::generation).unwrap();
        assert!(!hit1, "first compile is a miss");
        assert_eq!(g1_stamped, gen1);

        // An immediate repeat with the same key hits the per-thread cache.
        let (_, hit2) = super::with_plan(&tree, b, a, gen1, |_| ()).unwrap();
        assert!(hit2, "repeat lookup hits the cache");

        // A runtime re-parent bumps the generation; the recompiled plan carries it.
        tree.reparent(c, a).unwrap();
        let gen2 = tree.guard().generation();
        assert_ne!(gen1, gen2, "re-parent must change the generation");

        let (g3_stamped, _hit3) =
            super::with_plan(&tree, b, a, gen2, tf_tree_core::Plan::generation).unwrap();
        assert_eq!(
            g3_stamped, gen2,
            "post-change plan is stamped with the new generation"
        );
    }

    /// The cache still caches, measured on **live trees** rather than on a model
    /// of the index function — and measured against what the index *predicts*
    /// for the ids those trees actually got, not against a fixed number.
    ///
    /// [`the_low_bit_mask_wins_on_a_small_tree_and_ties_on_a_large_one`] argues
    /// the *choice* of index from synthetic keys; this measures the thing that
    /// choice is for. N trees, round-robin, three rounds, counting only the
    /// rounds after the first — the steady state a real reader is in, which one
    /// warm round is enough to reach.
    ///
    /// # Why it does not assert a hit rate
    ///
    /// The obvious form — "N <= 16 live trees must hit 100% of the time" — is
    /// true only while `next_local_scope`'s counter hands *these* trees
    /// consecutive ids, and that counter is process-global. `just miri` runs
    /// `-p tf_tree --lib` on libtest's default multi-threaded harness, so a
    /// sibling test building a tree between two of these builds leaves a gap;
    /// two of sixteen ids then agree modulo 16, evict each other, and the rate
    /// is 0.875 through no fault of the cache. `nextest` gives every test its
    /// own process and would never show it, so the miri job would be the only
    /// one to flap — the worst place for a flake to live.
    ///
    /// So the expectation is **derived from the ids that were minted**: the
    /// slots are recomputed through [`super::index`] and a tree is expected to
    /// hit iff no other tree in the set shares its slot. Whatever else is
    /// running, the assertion is exact rather than tolerant, and it is a
    /// stronger statement than the rate ever was: the cache hits *exactly* when
    /// its index says it should.
    ///
    /// # What that catches
    ///
    /// * The #196 defect itself, at every N including 2: a key that stopped
    ///   separating trees makes every tree share one entry, so the measured
    ///   hits go to `total` while the prediction goes to zero.
    /// * An eviction or install bug, from the other side: no hits at all
    ///   against a prediction of N.
    /// * An index that stopped being a bijection on the low bits of `scope` —
    ///   the property that keeps two heap trees from thrashing, and the
    ///   performance regression this fix could plausibly have introduced. That
    ///   is the second assertion: slot-uniqueness must agree with
    ///   `scope`-modulo-[`super::SLOTS`] uniqueness, tree for tree.
    ///
    /// N = 17 is kept because the pigeonhole is the one thing no key can talk
    /// its way out of: seventeen trees cannot all be resident in sixteen slots,
    /// so a run where they are is a run where the arena component is not in the
    /// key.
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

            // Every tree interned the same names in the same order and declared
            // the same edges, so the key's other three components agree across
            // the set and the slot is a function of `scope` alone. That is the
            // #196 precondition, and it is asserted rather than assumed.
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
                    let (_, hit) = super::with_plan(tree, a, c, g, |_| ()).unwrap();
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
    /// peer's first [`super::with_plan`] returns `hit = true`.
    ///
    /// Reachable only through `just shm-check`'s
    /// `cargo nextest run -p tf_tree --features shm --lib` line, which was added
    /// with this test — `just test` builds default features, so without that
    /// line this would be compiled by clippy and executed by nothing.
    ///
    /// **No `TF_TREE_RUNTIME_DIR` scratch directory here, deliberately**, unlike
    /// `tests/rendezvous.rs`, `tests/owned_writer.rs` and
    /// `tf_tree_cli/tests/attach.rs`. Those go through `tf_tree::Open`, whose
    /// rendezvous *is* a lock file and a socket under that directory, so two
    /// concurrent runs collide and a killed run leaves a lock file behind.
    /// `build_shared` reaches none of it: it is `memfd_create` plus `mmap`
    /// (`tf_tree_arena::MappedArena::create`), the fd is the only capability,
    /// and the name is a debug label that shows up in `/proc/<pid>/fd` — memfd
    /// names are not unique and not a namespace. Measured: this test passes with
    /// `TF_TREE_RUNTIME_DIR` pointed at a path that does not exist, and eight
    /// copies of it run concurrently all pass.
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
            !super::with_plan(&owner, b, a, g, |_| ()).unwrap().1,
            "cold cache"
        );
        assert!(
            super::with_plan(&peer, b, a, g, |_| ()).unwrap().1,
            "the peer's FIRST lookup must reuse the owner's plan, not recompile"
        );
    }
}
