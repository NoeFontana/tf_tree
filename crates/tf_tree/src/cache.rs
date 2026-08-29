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
//! What a slot holds is the **result** of compiling that key, refusal included
//! (#259); the generation component is what makes that sound, and
//! [`store_refusal`] is what checks it rather than assuming it. See [`Entry`].
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
    /// What [`Tree::plan`] answered for [`Entry::key`] — **a refusal included**.
    ///
    /// # Why an error is exactly as cacheable as a plan
    ///
    /// The cache's whole correctness argument is that `compile`'s output is a
    /// function of `(arena, target, source, generation)`. That argument does
    /// not mention success. `compile` reads two things — the topology through
    /// `read_frame`, and edge metadata through `edge_meta` — and every refusal
    /// it can produce is a verdict on those same reads: `Disconnected` and
    /// `MissingEdge` on the parent chain, `TreeTooDeep` on its length,
    /// `UnknownEdge` and `MixedTimeDomains` on the edge records the walk
    /// reaches. A key that fixes the reads fixes the verdict.
    ///
    /// Three classes of error are *not* functions of the key. The first is out
    /// of reach here rather than filtered out of it — a distinction worth
    /// keeping, because it cannot rot — and [`store_refusal`] declines the
    /// other two, one per condition:
    ///
    /// * `NoData`, `Extrapolation`, `SlotRecycled`, `SlotContended`,
    ///   `TimeDomainMismatch` and `TopologyChanged` come from
    ///   `Plan::at`, i.e. from `f`, and travel in the `R` of [`with_plan`]'s
    ///   return. They never touch an `Entry`, and a change that let them would
    ///   be caching the sample history.
    /// * `ChildDetached` is [`Tree::plan`]'s own guard, raised before it reads
    ///   the arena at all. It is a property of the *process* after a `fork`,
    ///   not of the key. [`store_refusal`] declines it by name.
    /// * A `FrameOutOfRange` from `read_frame` exhausting `TOPO_RETRY_LIMIT`
    ///   rather than from a genuinely out-of-range id — a livelock fallback
    ///   under a mutation storm, which is transient by construction.
    ///   [`store_refusal`] declines that one too, because reaching the retry
    ///   limit takes `TOPO_RETRY_LIMIT` observed changes to the topology word
    ///   and therefore moves the generation.
    ///
    /// # It costs nothing
    ///
    /// `size_of::<Result<Plan, LookupError>>() == size_of::<Plan>() == 2064`:
    /// `Plan`'s `[Step; MAX_DEPTH]` has spare discriminant encodings, so the
    /// `Err` variant occupies a niche and `LookupError`'s 32 bytes land inside
    /// the array. The slot, the table and the 32.6 KiB per thread are all
    /// unchanged. [`tests::a_refusal_is_free_to_cache`] pins it, because a
    /// future `LookupError` variant that outgrew the niche would grow every
    /// slot by an alignment step silently. (2064 and 32.6 KiB were 4160 and
    /// 66.0 before `0042` halved `Step`.)
    result: Result<Plan, LookupError>,
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
/// # A refusal is a result, and is cached like one
///
/// `f` runs only when there is a `Plan` to run it against; a key whose compile
/// was refused answers with the refusal and does not call `f`. That is the
/// whole of issue #259, whose symptom is that a frame pair which cannot be
/// planned used to recompile **on every lookup, forever** — the `?` returned
/// before the store and nothing else on that path reached the cache.
///
/// It was invisible while it was cheap. `MAX_DEPTH` was 16 and was checked
/// *during* the walk, so a 40-edge path was refused after 16 edges at ~50-70 ns.
/// `0034` separated the raw-walk bound from the compiled-plan bound, and a path
/// that is going to be refused is now walked to its full length and — under the
/// fold that gives `0034` its stated error precedence — folded in full as well:
/// ~1.0 µs where the bound stops the walk early, ~1.8 µs where it does not. A
/// consumer polling one mis-configured pair pays that per lookup.
///
/// **Which pairs those are is narrower than it first looks**, and the three
/// examples that stood here — a typo'd frame name, a sensor that never came up,
/// a URDF branch nobody publishes — were all wrong. The first two never reach
/// this function: `Tree::lookup` resolves names through `find`, which is a
/// lookup and not an intern, so an undeclared name is `UnknownFrame` before a
/// compile is attempted. The third compiles perfectly and fails in `Plan::at`
/// with `NoData`, which this deliberately does not cache. The pairs that
/// actually pay are the ones whose *topology* refuses:
///
/// * `Disconnected` — a declared frame whose parent link was never established,
///   or one `reparent`ed out of the queried subtree.
/// * `MissingEdge` — a link carrying the `0` edge sentinel, which `set_parent`
///   accepts for a reparent "where only the parent link matters". Permanent,
///   and on every lookup that crosses it.
/// * `TreeTooDeep` — a path at the ceiling; `0034` surveyed 91 descriptions and
///   found diameters to 30 against a `MAX_DEPTH` of 32.
/// * `UnknownEdge` / `MixedTimeDomains` — a defect anywhere on the path, which
///   the fold resolves every edge to find.
///
/// Measured through `Tree::lookup` on a 60-edge chain that walks inside
/// `MAX_PATH_EDGES` and folds past `MAX_DEPTH` — the arm that pays the *whole*
/// fold before refusing — medians of 5 rounds of 20 000 reps, `taskset -c 2`,
/// builds interleaved: **579.0 ns → 291.5 ns, −49.6%**, ranges [576-585] against
/// [291-297]. It lands on top of the *shallow* refusal's 290.9 ns, which is the
/// point: what is left is resolving the two names, and the compile is gone
/// whatever it would have cost. A third build carrying only #264's change moved
/// this metric −0.9%, so the win is this change's and not that one's.
///
/// The **control** is `lookup_ok_hit_ns` — a successful repeat lookup, the path
/// the copy-count table above exists to protect. It moved +0.1% (504.5 → 505.1),
/// inside its own run-to-run range. Widening [`Entry`] to hold a `Result` cost
/// the hit path nothing, which is what the `&entry.result` match is for. A
/// 4160-byte `memcpy` there would have cost ~40 ns against a 506 ns baseline —
/// that being `Plan`'s size when this was measured; it is 2064 since `0042`, so
/// the same argument now runs on half the bytes and holds by a wider margin —
/// +8%, an order of magnitude outside the run-to-run range — so the control is
/// not merely consistent with no copy, it excludes one.
///
/// The **miss** path did move, and it is written down here because it was
/// measured rather than because it matters: the three `Plan` copies `Tree::lookup`
/// makes on a miss are 4160 bytes each where they were 4120 — figures from when
/// this was measured, 2064 and 2024 since `0042` — since the `Err`
/// variant's niche lives inside `steps` and the leading bytes can no longer be
/// treated as padding. 120 bytes added to a path that already moves twelve
/// kilobytes, on the arm #264 cut by more than half.
///
/// # The slot it occupies is not a cost worth avoiding
///
/// The objection is that 16 direct-mapped slots have no notion of usefulness,
/// so a permanently-broken pair can evict a working plan and the victim is the
/// colliding entry rather than the least useful one. True, and it is the
/// argument *for* treating the two alike rather than against it: a cache
/// entry's worth is its hit rate times the work it avoids, and a refusal avoids
/// **more** work per hit than a plan does — the refused path is the one that
/// walks to the bound. Ranking refusals below plans would be the arbitrary
/// choice here. A separate negative table was the alternative and buys nothing
/// it would not also cost: at 32 bytes a refusal it lands at about the same
/// footprint, in exchange for a second index, a second residency argument, and
/// a working set split across two tables that the [`index`] measurements were
/// never taken over.
///
/// # Why a closure and not `-> Plan`
///
/// `Plan` is `Copy` and **2064 bytes** (`size_of`, measured; `align_of` 8, so
/// the 24-byte `Key` costs 24 rather than hiding in padding, making `Entry`
/// 2088 and the 16-slot table **32.6 KiB per thread**). Those were 4160, 64,
/// 4224 and 66.0 KiB until
/// [`0042`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0042-the-cacheline-the-arena-never-asked-for.md):
/// the key used to disappear into `Plan`'s cacheline padding, which is why the
/// slot shrank by less than `Plan` did. Returning one hands the caller a copy,
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
                    // `&entry.result`, and the `&` is the same load-bearing one
                    // the section above is about: matching through the
                    // reference yields a `&Plan` pointing into the slot, where
                    // matching the value would lift the whole `Plan` — 2064
                    // bytes, and 4160 when this comment was written — out of it
                    // just to decide which variant it is.
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

/// Whether `refusal` — which [`Tree::plan`] just returned for `key` — is a
/// function of `key`, and may therefore be stored under it.
///
/// **The check, not an argument that one is unnecessary.** There is a sound
/// argument that the generation half is unnecessary: `compile` re-reads the
/// generation itself and the counter only ever increases, so a refusal computed
/// under a generation other than `key.generation` is filed under a key no later
/// lookup can construct, and is simply dead. That argument is correct and it is
/// also three facts deep, one of which lives in another crate. This is one
/// relaxed load on a path that has just spent a microsecond compiling, and it
/// converts the argument into an invariant the code enforces: **nothing is
/// stored whose value the key does not determine.**
///
/// # Why `ChildDetached` is matched by name
///
/// It is the one refusal [`Tree::plan`] produces without reading the arena, so
/// it describes the *process* after a `fork` and not the key. This condition
/// used to be spelled `!tree.detached()`, which is equivalent — `Tree::plan`
/// returns `ChildDetached` if and only if it is detached, that being its first
/// statement — and equivalent in the way that cannot be tested: reaching it
/// needs a `fork` landing between `Tree::lookup`'s own detached check and
/// `Tree::plan`'s, and the workspace's only `fork()` lives in another crate's
/// test binary. Matching the value says the same thing about the thing it is
/// actually about, and [`tests::store_refusal_declines_what_the_key_does_not_determine`]
/// can then pin both arms with no fork at all.
///
/// Reading the generation is safe either way and that is not what changed:
/// [`Tree::view`] answers from a *poison arena* once detached — a real, mapped,
/// in-process arena — so the load would have been meaningless rather than
/// unsound.
///
/// The success path needs none of this: a `Plan` carries the generation it was
/// compiled under and `Plan::at` refuses a mismatch with `TopologyChanged`, so
/// it is self-checking at *evaluation* time. A refusal has no evaluation, and
/// this is where it gets the equivalent.
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
    /// that it did, and returns the hit flag.
    ///
    /// The `.unwrap()` these call sites used to carry did this for free, when
    /// the return was `Result<(R, bool), LookupError>`. Lifting the compile
    /// result out of the tuple — so a *refusal* can report a hit too (#259) —
    /// deleted the assertion silently, and `let (_, hit) = …` is an explicit
    /// discard, so nothing warned. Because refusals now land in the cache, every
    /// hit-rate test below would then have kept passing against a build where
    /// nothing compiles at all: measured, with `tree.plan(..).and(Err(..))`
    /// forcing every compile to fail,
    /// [`the_cache_hits_exactly_where_its_index_predicts`],
    /// [`two_trees_keep_separate_entries_and_still_hit`] and
    /// `two_handles_on_one_shared_arena_share_their_plans` were all green — they
    /// were measuring that *something* was cached, not that a plan was. With the
    /// assertion back, all three fail against that build, in both feature
    /// configurations. [`the_low_bit_mask_wins_on_a_small_tree_and_ties_on_a_large_one`]
    /// is **not** on the list and stays green either way: it models [`super::index`]
    /// directly and never reaches the cache at all.
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

    /// Two frames in **separate components**, plus the reparent that joins them.
    ///
    /// `y` is the frame moved rather than `x`, and that is not arbitrary: it is
    /// the one carrying an `edge_of_child`, and a path through a link whose edge
    /// is the `0` sentinel is [`LookupError::MissingEdge`] — which would turn
    /// the "now it works" half of
    /// [`a_cached_refusal_does_not_survive_the_topology_that_caused_it`] into a
    /// second refusal that the test would have accepted as one.
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
    /// pinned rather than asserted.
    ///
    /// `Plan` is `[Step; MAX_DEPTH]` and `Step` is an enum with spare
    /// discriminant encodings, so `Result<Plan, LookupError>` puts its `Err`
    /// variant in a niche inside that array and `LookupError`'s 32 bytes fit
    /// there. Nothing guarantees that survives a new `LookupError` variant with
    /// a wider payload, and the failure mode if it does not is silent: every
    /// slot grows by an alignment step and the per-thread table with it.
    ///
    /// The equality is the assertion; the absolute numbers are recorded in the
    /// message so a failure says which way it moved. They are `MAX_DEPTH = 32`
    /// figures and a retune of that constant is expected to change them.
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
        // **Written against `Key`'s size, not against `Plan`'s alignment.** This
        // read `size_of::<Plan>() + align_of::<Plan>()`, which was right only
        // while `Plan` was `align(64)` and `Key` was 24 bytes — the key hid
        // entirely inside the alignment padding, so the padding *was* the key's
        // cost. `0042` dropped `Iso3`'s cacheline alignment and `Plan` became
        // `align(8)`, at which point the two stopped coinciding and the formula
        // was measuring nothing. This is the invariant either way: an `Entry` is
        // a `Key`, rounded up to `Plan`'s alignment, and then the `Plan`.
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
    ///
    /// The defect: `with_plan` propagated a failed compile with `?`, which
    /// returned before the store, so nothing on that path ever reached the
    /// cache. The second probe below missed forever.
    ///
    /// **Mutant: restore the `?`** — i.e. delete the `store_refusal` arm and let
    /// the `Err` return without storing ⇒ `hit2` is `false` and this fails on
    /// its first assertion.
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

    /// **[`super::store_refusal`] declines what the key does not determine** —
    /// both arms, directly, because neither had a test and the whole point of
    /// the predicate is to be the check rather than the argument.
    ///
    /// Measured before this existed: replacing the body with `true` left
    /// `cargo nextest run -p tf_tree` at 101/101 passing, with or without
    /// `--features shm`. An equivalent mutant is worse than no guard, because it
    /// reads as one.
    ///
    /// **Mutant: body → `true`** ⇒ both `assert!(!…)` lines fail.
    /// **Mutant: drop the `!` on the `matches!`** ⇒ four tests fail, this one
    /// among them: the control and the `ChildDetached` line here, plus
    /// [`a_refused_pair_is_compiled_once_and_then_answered_from_the_cache`] and
    /// [`a_cached_refusal_does_not_survive_the_topology_that_caused_it`], since
    /// nothing but a `ChildDetached` is stored any more.
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

        // Control: an ordinary compile refusal at the live generation is exactly
        // what the cache is *for*, and must be stored.
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
    ///
    /// The other half of the pair above: that test pins the predicate, this one
    /// pins that the store is gated on it, and neither implies the other. The
    /// key carries a generation the arena does not have, so `compile` — which
    /// reads the live generation itself — produces a refusal that is not this
    /// key's, and it must not be filed under it.
    ///
    /// **Mutant: `store_refusal` → `true`** ⇒ the stale probe hits and the final
    /// assertion fails, while the control above it still passes.
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

    /// **A cached refusal does not survive the topology that produced it.**
    ///
    /// The property the whole of #259 rests on, and the one a negative cache
    /// gets wrong if it gets anything wrong: the key carries the generation, a
    /// topology mutation bumps it, and a pair that could not be planned before
    /// the mutation must be planned again after it. Without that, connecting two
    /// components would leave the pair permanently unresolvable in every thread
    /// that had already asked.
    ///
    /// **Mutant: build the key with `generation: 0`** — the whole mutation, since
    /// that blinds both [`super::index`] and `Key`'s derived `PartialEq` at once
    /// ⇒ the third probe *hits*, and this fails on `!hit3` one assertion before
    /// it would have reached `r3.is_ok()`. (Executed; it takes
    /// `cache_hits_and_invalidates_on_generation` down with it, which is the
    /// same property from the success side.)
    ///
    /// **Mutant: never store a refusal** ⇒ this fails on its *precondition*
    /// rather than its subject, which is the assertion doing its job.
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

    /// **An evaluation error is not a compile refusal, and is not cached as
    /// one.**
    ///
    /// `NoData`, `Extrapolation`, `SlotRecycled` and `TopologyChanged` come from
    /// `Plan::at` — from `f` — and describe the sample history, which the key
    /// says nothing about. Caching one would pin "this edge has no data" for the
    /// life of a generation, which is the one class of staleness this cache has
    /// never been able to produce.
    ///
    /// It is structural rather than filtered: an error from `f` travels inside
    /// `with_plan`'s `R`, which is opaque to this module, and has no path to an
    /// `Entry`. The test spends [`LookupError::WrongElementType`] to say so,
    /// precisely because `compile` cannot produce it — so a copy of it coming
    /// back out of the cache could only have been stored from `f`.
    ///
    /// **No mutant, and that is the honest report.** Because the property is
    /// carried by the *type* rather than by a line, no single-line edit to
    /// [`super::with_plan`] expresses "cache `f`'s error too"; it takes
    /// collapsing the two `Result`s into one, which is a redesign. This test is
    /// a guard on that redesign, not a discriminator between two builds that
    /// exist. What *was* executed is the control — running `f` before the store
    /// instead of after leaves all eight cache tests green — which says the test
    /// is not accidentally sensitive to the order of those two statements.
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
        assert!(!compiled_hit(&owner, b, a, g), "cold cache");
        assert!(
            compiled_hit(&peer, b, a, g),
            "the peer's FIRST lookup must reuse the owner's plan, not recompile"
        );
    }
}
