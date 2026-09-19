//! The guard on this suite's own `cfg` gates. It contains no `#[test]`.
//!
//! A misspelt `feature = "unstable"` makes gated tests absent, not broken, and
//! `cargo test` reports nothing. A `const` assertion fails wherever the target
//! compiles, tarball included; a test-count floor in `just` catches a whole
//! target vanishing, and the two do not overlap.
//!
//! Per file: every `feature = "…"` names a declared feature, and the count
//! naming `unstable` has not fallen below a floor (a ceiling for
//! `owned_writer.rs`, which runs only under `just shm-check`). Sources are read
//! with `include_str!`. The list is the five targets the 0.0.1 refactor
//! touched; `tests/rendezvous.rs` and `tests/tsan.rs` are not scanned, so a
//! gate there that decides whether a tier runs a test belongs on it.

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

/// `(gates naming "unstable", gates naming an undeclared feature)`. The needle is
/// written escaped so the scanner cannot count itself.
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
        // Each candidate includes its closing quote, so `"shm"` cannot match `"shmm"`.
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

// `assert!` in a const context takes a literal message, so each file gets its own line.
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
