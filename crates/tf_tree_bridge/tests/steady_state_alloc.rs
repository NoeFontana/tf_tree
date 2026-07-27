//! `Ingest::offer` must not allocate more as it runs longer.
//!
//! The bridge sits in front of every `/tf` message a robot publishes. §5.8 put
//! two new per-message table lookups on that path — "is this edge declared?"
//! and, for the ones that are not, "have I complained about it yet?" — and both
//! were first written against `BTreeMap<(String, String), _>`. `Borrow` does not
//! reach inside a tuple, so such a map **cannot be probed by reference at all**:
//! every lookup had to build two owned `String`s and drop them again, to answer
//! a question about memory the map already held. At twenty edges and 1 kHz that
//! is forty thousand allocations a second bought for nothing.
//!
//! This is the gate on that. It is a *steady-state* bound, not a zero-allocation
//! one: `NameNormalizer` legitimately owns the normalized names it returns, and
//! `docs/PHASE1.md` §10.4's zero-allocation invariant is about the **engine**,
//! which this is not.
//!
//! The `CountingAllocator` is copied from `crates/tf_tree_bench/tests/zero_alloc.rs`,
//! which established the pattern; the `unsafe` is confined to this test target
//! and the library crate stays `#![forbid(unsafe_code)]`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use tf_tree_bridge::{Ingest, Publisher, Sample, Topic, TopologyConfig};

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

/// A `System`-backed allocator that counts allocating calls.
struct CountingAllocator;

// SAFETY: `CountingAllocator` forwards every call unchanged to `System`, which is
// a sound `GlobalAlloc`. The only added work is a `Relaxed` atomic increment on
// the allocating paths, which cannot affect the returned pointers or their
// validity. This impl therefore upholds every `GlobalAlloc` invariant `System`
// upholds. (Test-only binary; `tf_tree_bridge` proper is `#![forbid(unsafe_code)]`.)
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: forwarding an unmodified `layout` to the system allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr`/`layout` came from `System.alloc` (this allocator only
        // ever forwards to `System`), so returning them to `System.dealloc` is
        // sound. Deallocation is intentionally not counted.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: `ptr`/`layout` originate from `System` and `new_size` is
        // passed through unchanged, satisfying `System::realloc`'s contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

const ITERS: usize = 2_000;

/// One dynamic edge, plus a second the config deliberately does *not* declare.
const TOPO: &str = r#"
[[edge]]
parent = "odom"
child = "base"
kind = "dynamic"
capacity = 256
"#;

/// Allocating calls per `offer`, averaged over `ITERS` steady-state messages.
///
/// The `Sample` and the `Publisher` are built once and reused, so what is
/// measured is `offer`'s own cost and not the caller's message construction.
fn allocs_per_offer(sample: &Sample, publisher: &Publisher) -> usize {
    let config = TopologyConfig::parse(TOPO).unwrap();
    let mut ingest = Ingest::new(&config);
    let mut s = sample.clone();

    // Warm-up. The first message for an edge legitimately allocates: it interns
    // the frame names, and for an undeclared edge it creates the counter entry.
    for k in 0..8 {
        s.stamp_nanos = k * 1_000_000;
        ingest.offer(Topic::Tf, &s, publisher);
    }

    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for k in 0..ITERS {
        s.stamp_nanos = (8 + k as i64) * 1_000_000;
        ingest.offer(Topic::Tf, &s, publisher);
    }
    let after = ALLOCATIONS.load(Ordering::Relaxed);
    (after - before) / ITERS
}

/// **Neither the declared nor the undeclared path allocates for its table
/// lookups.** Both budgets are exact upper bounds on the *current* code, chosen
/// to sit strictly below what the flat-tuple maps cost, so the regression they
/// exist to catch cannot slip back in under them.
///
/// The undeclared case is the one `first_time` was supposed to have solved:
/// it silenced the log for a 1 kHz undeclared edge and left the allocator
/// running at 1 kHz anyway, which is the more expensive half.
///
/// Fixture note: `base -> lidar` is genuinely absent from `TOPO`, so the
/// undeclared path is really taken — a fixture whose "undeclared" edge was
/// declared would measure the declared path twice and pass regardless.
///
/// Mutant: change `StaticStore`'s `kinds` back to
/// `BTreeMap<(String, String), StaticKind>`, probed with
/// `(parent.to_string(), child.to_string())` ⇒ both budgets are exceeded by 2.
/// Mutant: in `Ingest::offer`, replace the `lookup_mut` fast path with
/// `self.undeclared.entry((parent.clone(), child.clone())).or_insert(0)` over a
/// flat map ⇒ the undeclared budget is exceeded by 2.
/// Mutant: give `Authority::owners` a `(String, String)` key and build one at
/// the top of `admit` ⇒ the declared budget is exceeded by 2.
#[test]
fn offer_does_not_allocate_for_its_table_lookups() {
    let publisher = Publisher::Node("/ekf".to_string());

    let declared = allocs_per_offer(&Sample::identity("odom", "base", 0), &publisher);
    let undeclared = allocs_per_offer(&Sample::identity("base", "lidar", 0), &publisher);

    // Four: `NameNormalizer::normalize` is called once per frame and allocates
    // twice — the owned normalized name it returns, and the raw spelling it
    // interns to drive §5.6's "warn once per distinct frame". Nothing else on
    // either path allocates in steady state. The budgets are equalities in
    // spirit; `<=` so that removing an allocation is never a test failure.
    const DECLARED_BUDGET: usize = 4;
    const UNDECLARED_BUDGET: usize = 4;

    assert!(
        declared <= DECLARED_BUDGET,
        "declared path: {declared} allocations per offer, budget {DECLARED_BUDGET}"
    );
    assert!(
        undeclared <= UNDECLARED_BUDGET,
        "undeclared path: {undeclared} allocations per offer, budget {UNDECLARED_BUDGET}"
    );
}
