//! Per-thread compiled-plan cache behind [`crate::Tree::lookup`].
//!
//! A direct-mapped, 16-entry cache keyed by `(target, source, generation)`. A
//! topology mutation bumps the generation, so a stale plan can never be served:
//! the key simply no longer matches and the entry is recompiled. The cache is
//! `thread_local!`, so it needs `std` and lives in the facade, not the `no_std`
//! core.

use std::cell::RefCell;

use tf_tree_core::{FrameId, LookupError, Plan};

use crate::tree::Tree;

/// Number of direct-mapped slots. A power of two so indexing is a mask.
const SLOTS: usize = 16;

#[derive(Clone, Copy)]
struct Entry {
    key: (u32, u32, u64),
    plan: Plan,
}

thread_local! {
    static CACHE: RefCell<[Option<Entry>; SLOTS]> = const { RefCell::new([None; SLOTS]) };
}

/// Map a key to its direct-mapped slot.
fn index(key: (u32, u32, u64)) -> usize {
    let (a, b, g) = key;
    let mut h = u64::from(a);
    h = h.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ u64::from(b);
    h = h.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ g;
    (h as usize) & (SLOTS - 1)
}

/// Return the cached plan for `(target, source, generation)`, compiling and
/// caching it on a miss. The `bool` is `true` on a cache hit (used by tests).
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
    let key = (target.get(), source.get(), generation);
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
}
