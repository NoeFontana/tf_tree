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
//! # Slope, not intercept
//!
//! Each figure below is the second of **two consecutive equal windows**, not one
//! window's total, and the difference is what makes this test survive CI.
//!
//! The property this file names in its own first line is a *rate*: `offer` must
//! not allocate **more as it runs longer**. An exact total over a single window
//! pins that rate and an intercept together — and the intercept is not a
//! property of `offer`, it is whatever one-off cost happens to land after the
//! warm-up. Three CI runs failed here with `4004` against `4000`: **the same
//! four allocations every time**, so not noise, and never reproducible on a
//! developer machine across filtered, whole-package and two-package
//! invocations, thirty consecutive runs, and both rustc 1.95 and the 1.97 CI
//! resolves — an eight-iteration warm-up is simply not always long enough to
//! contain a one-off growth.
//!
//! Two windows separate the two. A constant cost lands in the first and cancels;
//! a genuine per-offer regression changes both and still fails exactly. Verified
//! by mutation in both directions: one extra allocation *per offer* inside the
//! measured loop fails, and a one-off allocation before the windows passes.
//!
//! The exact equality is kept, because it is still an exact claim — it is now a
//! claim about the slope alone.
//!
//! The `CountingAllocator` is copied from `crates/tf_tree_bench/tests/zero_alloc.rs`,
//! which established the pattern; the `unsafe` is confined to this test target
//! and the library crate stays `#![forbid(unsafe_code)]`.
//!
//! **A copied instrument does not inherit its original's later fixes**, and this
//! file is the evidence. The copy was taken on 2026-07-27; six days later
//! `zero_alloc.rs` was found to be counting other threads' allocations and made
//! thread-local, and nothing carried that across. See the counter's own comment
//! for what that does and does not explain about the failures above.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use tf_tree_bridge::{Action, Ingest, Publisher, Sample, SteadyNanos, Topic, TopologyConfig};

thread_local! {
    /// Allocating calls (`alloc` + `realloc`) made **by this thread**.
    ///
    /// Deliberately character-for-character the counter in
    /// `crates/tf_tree_bench/tests/zero_alloc.rs`, so the two can be diffed. That
    /// file carries the argument; this is the short version. A process-global
    /// counter charges the measured window for whatever else the process does
    /// while it is open, and none of that is a property of `offer`.
    ///
    /// **This file had the defect that file already fixed.** The counter here was
    /// copied from `zero_alloc.rs` on 2026-07-27 (`3f39f9b`); `zero_alloc.rs`
    /// became thread-local on 2026-08-02 (`f53198c`, "the zero-alloc gate counted
    /// other threads' allocations"), six days later, and the fix was never
    /// carried across.
    ///
    /// What that does and does not settle for #178, because the two are easy to
    /// run together. It does NOT explain the `4004`-against-`4000` CI failures:
    /// `zero_alloc.rs` was off by ~4000 because a *sibling test* on another
    /// thread allocated thousands of times, and this target has one test, so its
    /// only other thread is libtest's own — which is a plausible source of four
    /// allocations but has not been shown to be the source of these four. What it
    /// does settle is which of #178's two hypotheses can still be standing. They
    /// have opposite fixes; narrowing the instrument first is correct under
    /// either, so it is done before the search rather than after it.
    ///
    /// `const { Cell::new(0) }` is required, not stylistic: a lazily-initialised
    /// thread-local can allocate on first access, and doing that *inside* the
    /// global allocator would recurse.
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

/// This thread's allocating-call count.
///
/// `try_with` rather than `with`: during thread teardown the local may already
/// be destroyed, and a panic from inside the allocator is not recoverable.
fn allocations() -> usize {
    ALLOCATIONS.try_with(Cell::get).unwrap_or(0)
}

/// Record one allocating call on this thread, if the local is still live.
fn note_allocation() {
    let _ = ALLOCATIONS.try_with(|c| c.set(c.get().wrapping_add(1)));
}

/// A `System`-backed allocator that counts allocating calls.
struct CountingAllocator;

// SAFETY: `CountingAllocator` forwards every call unchanged to `System`, which is
// a sound `GlobalAlloc`. The only added work is a non-allocating, non-panicking
// increment of a thread-local `Cell` on the allocating paths, which cannot affect
// the returned pointers or their validity. This impl therefore upholds every
// `GlobalAlloc` invariant `System` upholds. (Test-only binary; `tf_tree_bridge`
// proper is `#![forbid(unsafe_code)]`.)
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note_allocation();
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
        note_allocation();
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

    // **Two windows, and the second is the answer.** See `SLOPE_NOT_INTERCEPT`.
    let mut window = |from: i64| {
        let before = allocations();
        for k in 0..ITERS {
            let k = from + k as i64;
            s.stamp_nanos = STAMP0 + k * MS;
            s.received = SteadyNanos(T0 + k * MS);
            ingest.offer(Topic::Tf, &s, publisher);
        }
        allocations() - before
    };
    let _first = window(8);
    window(8 + ITERS as i64)
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

    // Two windows, as above.
    let mut window = |from: i64| {
        let before = allocations();
        for k in 0..ITERS {
            let k = from + k as i64;
            s.stamp_nanos = STAMP0 - 5_000 * MS + k * MS;
            s.received = SteadyNanos(T0 + (1 + k) * MS);
            assert!(matches!(
                ingest.offer(Topic::Tf, &s, publisher),
                Action::Drop { .. }
            ));
        }
        allocations() - before
    };
    let _first = window(8);
    let second = window(8 + ITERS as i64);
    assert_eq!(
        ingest.stats().dropped_non_monotonic,
        (2 * ITERS + 8) as u64,
        "the fixture must really be on the regression path"
    );
    second
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
    let publisher = Publisher::named(&tf_tree_bridge::gid_for_name("/ekf"), "/ekf");

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
    // **Exact equalities on the TOTAL, not `<=` on an average, and both halves
    // of that changed for a reason.**
    //
    // `<=` was right while the counts were a cost being bounded. They are now a
    // *property* being pinned: the regressing path allocates nothing, and a
    // budget of "at most 2" would go on passing if all two came back. An
    // improvement that no test can lose is not a gate.
    //
    // And the totals, because the old `(after - before) / ITERS` was integer
    // division by 2000: one allocation every 2001 messages rounded to zero.
    // That is precisely the shape of an amortized table growth — a `Vec`
    // doubling, an index rehash, a capped table filling — which is exactly what
    // this file now has to watch, since `Ingest` gained two hash tables.
    //
    // The per-offer figure is still what a reader wants, so the failure message
    // prints it as a fraction: `0.0005 per offer` is how one allocation per two
    // thousand messages announces itself.
    const DECLARED_PER_OFFER: usize = 2;
    const UNDECLARED_PER_OFFER: usize = 2;
    const REGRESSING_PER_OFFER: usize = 0;

    let check = |what: &str, got: usize, want_each: usize| {
        let want = want_each * ITERS;
        assert_eq!(
            got,
            want,
            "{what} path: {got} allocations across {ITERS} offers ({:.4} per offer), \
             expected exactly {want} ({want_each} per offer)",
            got as f64 / ITERS as f64
        );
    };
    check("declared", declared, DECLARED_PER_OFFER);
    check("undeclared", undeclared, UNDECLARED_PER_OFFER);
    check("regressing", regressing, REGRESSING_PER_OFFER);
}
