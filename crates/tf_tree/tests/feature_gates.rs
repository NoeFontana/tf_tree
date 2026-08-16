//! **The guard on this suite's own `cfg` gates.** It contains no `#[test]`, and
//! that is the point.
//!
//! # What it is guarding
//!
//! Four targets in this crate gate code on `feature = "unstable"` — `counters`
//! whole, `behavior`, `construction` and `frozen` one test each — because
//! `Tree::arena_view` exists only there and `cargo test` on the published
//! tarball has to compile (see the `[dev-dependencies]` comment in
//! `crates/tf_tree/Cargo.toml` for why the self-dev-dependency that used to
//! force the feature on had to go). A `cfg` is a silent switch: misspell the
//! feature and the gated code is not *broken*, it is *absent*, and an absent
//! test reports nothing. Measured, on this branch: writing
//! `#![cfg(feature = "unstabel")]` at the top of `tests/counters.rs` took
//! `cargo nextest run --workspace` from *"825 tests run: 825 passed"* to
//! *"820 tests run: 820 passed"*, still exit 0.
//!
//! # Why this is a `const` and not a test
//!
//! Two of the three things that could catch that misspelling only catch it
//! somewhere:
//!
//! * `rustc`'s `unexpected_cfgs` does catch it, and it is not decoration —
//!   measured, the same typo makes `just lint`'s
//!   `cargo clippy --workspace --all-targets -- -D warnings` exit 101 with
//!   *"unexpected `cfg` condition value: `unstabel` … `-D unexpected-cfgs`
//!   implied by `-D warnings`"*. But it is a **warning** by default, so
//!   `cargo test`, `cargo nextest run` and a packager building the tarball all
//!   sail past it. It catches a name this crate does not declare and nothing
//!   else.
//! * A test-count floor in a `just` recipe catches a target that stops running,
//!   which nothing in this file can see — but only in the recipes that carry it.
//!
//! A `const` assertion fails wherever the target is *compiled*, which is every
//! one of those places and the tarball besides, and it costs no test name, so
//! the counts this release measures do not move. It is the cheap half; the
//! floor in `just` is the half that catches a whole target vanishing, and the
//! two do not overlap.
//!
//! # What it checks
//!
//! Per file: that every `feature = "…"` in the source names a feature this
//! crate actually declares, and that the number of them naming `unstable` has
//! not fallen below what the file is supposed to have. A floor rather than an
//! equality, because the failure being guarded against is a gate that *stops*
//! matching — adding a gated test is a normal thing to do and should not need
//! an edit here. `owned_writer.rs` is the one exception, and it is a ceiling
//! rather than a floor: `just shm-check` runs that target as
//! `cargo nextest run -p tf_tree --features shm --test owned_writer`, so a test
//! gated on `unstable` in that file would run in no recipe in the repository.
//!
//! It reads the sources with `include_str!`, so it is checking the same bytes
//! the compiler is compiling.
//!
//! **The list is the five targets the 0.0.1 refactor touched, and it is not the
//! whole suite.** `tests/rendezvous.rs` and `tests/tsan.rs` gate on `shm` and
//! `test-hooks` too and are not scanned; behind them stands `unexpected_cfgs`
//! and nothing else, which is the weaker half — it fires only in a recipe that
//! passes `-D warnings`. If a gate in either of them ever decides whether a
//! tier runs a test, it belongs on this list.

/// `true` when `needle` sits at `at` in `haystack`.
const fn matches_at(haystack: &[u8], at: usize, needle: &[u8]) -> bool {
    if at + needle.len() > haystack.len() {
        return false;
    }
    let mut i = 0;
    while i < needle.len() {
        if haystack[at + i] != needle[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// `(gates naming "unstable", gates naming a feature this crate does not
/// declare)`.
///
/// The needle is written escaped, so this file's own text does not match it and
/// the scanner cannot be fooled into counting itself.
const fn scan(src: &str) -> (usize, usize) {
    const KEY: &[u8] = b"feature = \"";
    let b = src.as_bytes();
    let mut unstable = 0;
    let mut unknown = 0;
    let mut i = 0;
    while i + KEY.len() <= b.len() {
        if !matches_at(b, i, KEY) {
            i += 1;
            continue;
        }
        // The name starts after the opening quote; the closing quote is part of
        // each candidate, so `"shm"` cannot match a hypothetical `"shmm"`.
        let name = i + KEY.len();
        if matches_at(b, name, b"unstable\"") {
            unstable += 1;
        } else if !matches_at(b, name, b"shm\"")
            && !matches_at(b, name, b"counters\"")
            && !matches_at(b, name, b"default\"")
            && !matches_at(b, name, b"test-hooks\"")
        {
            unknown += 1;
        }
        i = name;
    }
    (unstable, unknown)
}

const BEHAVIOR: &str = include_str!("behavior.rs");
const CONSTRUCTION: &str = include_str!("construction.rs");
const COUNTERS: &str = include_str!("counters.rs");
const FROZEN: &str = include_str!("frozen.rs");
const OWNED_WRITER: &str = include_str!("owned_writer.rs");

// `assert!` in a const context takes a literal message — no formatting — so
// each file gets its own line and says what to do about it.
const _: () = assert!(
    scan(BEHAVIOR).1 == 0,
    "tests/behavior.rs gates on a feature this crate does not declare — a \
     misspelling, and the gated code is compiled nowhere"
);
const _: () = assert!(
    scan(BEHAVIOR).0 >= 1,
    "tests/behavior.rs lost its `unstable` gate: a_tree_can_rescue_a_wedged_intern \
     asks the arena view two questions with no stable-tier spelling"
);
const _: () = assert!(
    scan(CONSTRUCTION).1 == 0,
    "tests/construction.rs gates on a feature this crate does not declare — a \
     misspelling, and the gated code is compiled nowhere"
);
const _: () = assert!(
    scan(CONSTRUCTION).0 >= 1,
    "tests/construction.rs lost its `unstable` gate: the arena-layout assertions \
     read `Tree::arena_view`"
);
const _: () = assert!(
    scan(COUNTERS).1 == 0,
    "tests/counters.rs gates on a feature this crate does not declare — a \
     misspelling, and the whole file is compiled nowhere"
);
const _: () = assert!(
    scan(COUNTERS).0 >= 1,
    "tests/counters.rs lost its `#![cfg(feature = \"unstable\")]`: every test in \
     it reads a counter through `Tree::arena_view`, so it cannot compile on the \
     stable tier"
);
const _: () = assert!(
    scan(FROZEN).1 == 0,
    "tests/frozen.rs gates on a feature this crate does not declare — a \
     misspelling, and the gated code is compiled nowhere"
);
const _: () = assert!(
    scan(FROZEN).0 >= 1,
    "tests/frozen.rs lost its `unstable` gate: freezing_carries_the_counter_regions \
     reads the view three times, and `just shm-check` runs that target with \
     `--features shm,unstable` for exactly that test"
);
const _: () = assert!(
    scan(OWNED_WRITER).1 == 0,
    "tests/owned_writer.rs gates on a feature this crate does not declare — a \
     misspelling, and the gated code is compiled nowhere"
);
const _: () = assert!(
    scan(OWNED_WRITER).0 == 0,
    "tests/owned_writer.rs has grown an `unstable` gate. `just shm-check` runs \
     that target as `--features shm --test owned_writer`, so whatever is behind \
     it executes in no recipe: either keep the assertion on the stable tier, or \
     add `unstable` to that line in the justfile first"
);
