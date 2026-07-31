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

use tf_tree_bridge::{Action, Ingest, Publisher, Sample, SteadyNanos, Topic, TopologyConfig};

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

/// A receipt-clock origin. Never 0: `SteadyNanos(0)` is the "no receipt clock"
/// sentinel, and a fixture that used it would leave the per-publisher offset
/// table dormant and measure a path this test does not mean to measure.
const T0: i64 = 5_000_000_000_000;
/// The stamp a healthy publisher emits at `T0`.
const STAMP0: i64 = 10_000_000_000;
/// One millisecond.
const MS: i64 = 1_000_000;

/// Allocating calls per `offer`, averaged over `ITERS` steady-state messages.
///
/// The `Sample` and the `Publisher` are built once and reused, so what is
/// measured is `offer`'s own cost and not the caller's message construction.
///
/// **The stamp and the receipt clock advance together**, which is not cosmetic.
/// An earlier version advanced `stamp_nanos` by 1 ms per iteration and left
/// `received` frozen, so the publisher's `stamp - received` offset drifted by
/// 1 ms per message — and once that drift crossed the 100 ms reset threshold the
/// offset table would have started recording spurious steps in the middle of the
/// measurement. It could not have halted (one publisher never promotes), but it
/// would have made an allocation budget flaky rather than failing cleanly, which
/// is the worst failure mode a steady-state test can have.
fn allocs_per_offer(sample: &Sample, publisher: &Publisher) -> usize {
    let config = TopologyConfig::parse(TOPO).unwrap();
    let mut ingest = Ingest::new(&config);
    let mut s = sample.clone();

    // Warm-up. The first message for an edge legitimately allocates: it interns
    // the frame names, for an undeclared edge it creates the counter entry, and
    // for the first sight of a publisher it creates the offset baseline.
    for k in 0..8i64 {
        s.stamp_nanos = STAMP0 + k * MS;
        s.received = SteadyNanos(T0 + k * MS);
        ingest.offer(Topic::Tf, &s, publisher);
    }

    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for k in 0..ITERS {
        s.stamp_nanos = STAMP0 + (8 + k as i64) * MS;
        s.received = SteadyNanos(T0 + (8 + k as i64) * MS);
        ingest.offer(Topic::Tf, &s, publisher);
    }
    let after = ALLOCATIONS.load(Ordering::Relaxed);
    (after - before) / ITERS
}

/// Allocating calls per `offer` for a publisher **stuck below its own high-water
/// mark** — the path a broken node occupies at message rate, indefinitely.
///
/// Every iteration takes the clock rules' refusal path: the per-edge guard
/// reports a past-threshold regression and the sample is dropped, counted and
/// diagnosed. The offset stays constant (stamp and receipt advance in lockstep
/// five seconds apart), so the initial step is recorded once during warm-up and
/// the measured window is the *sustained* regression, which is what a stuck
/// publisher actually produces.
fn allocs_per_regressing_offer(publisher: &Publisher) -> usize {
    let config = TopologyConfig::parse(TOPO).unwrap();
    let mut ingest = Ingest::new(&config);
    let mut s = Sample::identity("odom", "base", STAMP0).received_at(SteadyNanos(T0));

    // One good message establishes the high-water mark, the frame names and the
    // publisher's offset baseline…
    assert!(matches!(
        ingest.offer(Topic::Tf, &s, publisher),
        Action::Publish { .. }
    ));
    // …and then the publisher restarts and replays from five seconds ago. The
    // first of these is the one step this fixture contains.
    for k in 0..8i64 {
        s.stamp_nanos = STAMP0 - 5_000 * MS + k * MS;
        s.received = SteadyNanos(T0 + (1 + k) * MS);
        ingest.offer(Topic::Tf, &s, publisher);
    }

    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for k in 0..ITERS {
        let k = 8 + k as i64;
        s.stamp_nanos = STAMP0 - 5_000 * MS + k * MS;
        s.received = SteadyNanos(T0 + (1 + k) * MS);
        assert!(matches!(
            ingest.offer(Topic::Tf, &s, publisher),
            Action::Drop { .. }
        ));
    }
    let after = ALLOCATIONS.load(Ordering::Relaxed);
    assert_eq!(
        ingest.stats().dropped_non_monotonic,
        (ITERS + 8) as u64,
        "the fixture must really be on the regression path"
    );
    (after - before) / ITERS
}

/// **No path allocates for its table lookups — including the path a *broken*
/// publisher occupies.** All three budgets are exact upper bounds on the
/// *current* code, chosen to sit strictly below what the flat-tuple maps cost,
/// so the regression they exist to catch cannot slip back in under them.
///
/// The undeclared case is the one `first_time` was supposed to have solved:
/// it silenced the log for a 1 kHz undeclared edge and left the allocator
/// running at 1 kHz anyway, which is the more expensive half.
///
/// **The regressing case is the one whose absence let a real defect through.**
/// Every scenario here used to feed strictly increasing stamps, so the gate
/// could only ever see the happy path — and the clock machinery it replaced
/// allocated *twice per sample* on the regression path (a `BTreeMap` built to
/// count distinct owners, a `BTreeSet` built to count distinct publishers),
/// which a stuck publisher occupies at message rate for as long as it is stuck,
/// because a past-threshold regression deliberately does not advance the
/// high-water mark. Two heap allocations per message at 1 kHz, indefinitely,
/// with every test green.
///
/// Fixture note: `base -> lidar` is genuinely absent from `TOPO`, so the
/// undeclared path is really taken — a fixture whose "undeclared" edge was
/// declared would measure the declared path twice and pass regardless. The
/// regressing fixture asserts its own `Action::Drop`s and its
/// `dropped_non_monotonic` total for the same reason.
///
/// Mutant: change `StaticStore`'s `kinds` back to
/// `BTreeMap<(String, String), StaticKind>`, probed with
/// `(parent.to_string(), child.to_string())` ⇒ both budgets are exceeded by 2.
/// Mutant: in `Ingest::offer`, replace the `lookup_mut` fast path with
/// `self.undeclared.entry((parent.clone(), child.clone())).or_insert(0)` over a
/// flat map ⇒ the undeclared budget is exceeded by 2.
/// Mutant: give `Authority::owners` a `(String, String)` key and build one at
/// the top of `admit` ⇒ the declared budget is exceeded by 2.
/// Mutant: restore `NameNormalizer::normalize`'s
/// `let first_sight = self.seen.insert(raw.to_string());` ⇒ both budgets are
/// exceeded by 2, one owned key per frame allocated and dropped again on every
/// message of a frame already seen.
/// Mutant: build the offset row with
/// `self.rows.entry(owner.to_string()).or_insert(..)` in `OffsetTable::observe`
/// instead of probing with `get_mut(&str)` first — applied, and this failed at
/// `declared path: 3 allocations per offer, budget 2`, because `entry` needs an
/// owned key whether or not it inserts.
/// Mutant: take an owned publisher identity on the refusal path in
/// `Ingest::offer` (`let _owner = owner_key(publisher).to_string();` at the top
/// of the `Jitter | Reset` arm — which is what the deleted quorum did, once per
/// regressing sample) — applied, and this failed at `regressing path: 3
/// allocations per offer, budget 2` **with the other two budgets still met**.
/// That asymmetry is the point of this scenario: no other fixture here can see
/// it, and the code this replaced allocated twice on exactly that path.
#[test]
fn offer_does_not_allocate_for_its_table_lookups() {
    let publisher = Publisher::Node("/ekf".to_string());

    let declared = allocs_per_offer(&Sample::identity("odom", "base", 0), &publisher);
    let undeclared = allocs_per_offer(&Sample::identity("base", "lidar", 0), &publisher);
    let regressing = allocs_per_regressing_offer(&publisher);

    // Two, one per frame: the owned normalized name `NameNormalizer::normalize`
    // returns, which the caller keeps. Nothing else on either path allocates in
    // steady state.
    //
    // It was four. The other two were `self.seen.insert(raw.to_string())`,
    // driving §5.6's "warn once per distinct frame": `BTreeSet::insert` needs an
    // owned key whether or not it stores one, so every message of an
    // already-known frame built a `String` purely to discover it was already
    // there. Probing with `contains` first borrows and allocates nothing.
    //
    // The budgets are equalities in spirit; `<=` so that removing an allocation
    // is never a test failure.
    // The regressing path gets the *same* budget as the happy one, and that is
    // the whole claim: a broken publisher must not cost the bridge more than a
    // working one. The two normalized names are the only allocation, and the
    // per-publisher offset row is paid once, on that publisher's first sample.
    const DECLARED_BUDGET: usize = 2;
    const UNDECLARED_BUDGET: usize = 2;
    const REGRESSING_BUDGET: usize = 2;

    assert!(
        declared <= DECLARED_BUDGET,
        "declared path: {declared} allocations per offer, budget {DECLARED_BUDGET}"
    );
    assert!(
        undeclared <= UNDECLARED_BUDGET,
        "undeclared path: {undeclared} allocations per offer, budget {UNDECLARED_BUDGET}"
    );
    assert!(
        regressing <= REGRESSING_BUDGET,
        "regressing path: {regressing} allocations per offer, budget {REGRESSING_BUDGET}"
    );
}
