//! **`docs/RUNBOOK.md` holds what an error message gave up** — `0055` step 7.
//!
//! `IpcError::HandshakeRejected` used to carry a remedy per `HelloStatus`, and
//! the messages they made were 112 to 378 bytes against a C buffer of 256. They are
//! now the runbook's `HandshakeRejected` table, and the message ends
//! `(HandshakeRejected)` so a reader can find it. That split is only as good as
//! the table, so the table is gated: a row per refusal status, rows that say
//! something, and a worked example that is `Display`'s own output rather than a
//! transcription of it.
//!
//! # Why this test is in `tf_tree_cli` and not beside the type
//!
//! It reads a file outside any crate directory. `tf_tree_ipc` is **published**,
//! and `cargo package` does not put a file from outside the package into the
//! tarball — an `include_str!("../../../docs/RUNBOOK.md")` there ships a crate
//! whose tests cannot build. `crates/tf_tree_cli/src/lib.rs` writes that rule
//! down for the README, and `checks.rs`'s `docs/API.md` gate is the precedent
//! for putting the docs-reading half here, where `publish = false` makes the
//! failure mode not exist.
//!
//! What stays beside the type, in `tf_tree_ipc`'s `error.rs`, is everything
//! that needs no file: `every_rejection_names_only_the_status_it_carries` (the
//! facts, the search key, the 140-byte budget, and the rule that no rendering
//! may name a status it did not get) and
//! `every_ipc_error_message_fits_the_c_abis_buffer`.
#![cfg(all(feature = "shm", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use tf_tree::{HelloStatus, IpcError};

/// Every `HelloStatus` this build can **receive**, derived from the codec
/// rather than copied.
///
/// **A hand-copied list here left a hole, and it took three review rounds to
/// find.** `HelloStatus` is `#[non_exhaustive]`, so a downstream total `match`
/// is impossible — deliberately, because a newer owner may refuse for a reason
/// this build has no name for. From that I concluded there could be no
/// downstream tripwire at all, and that was wrong: a `match` is not the only
/// way to enumerate. `HelloStatus::from_u32` is injective on the values it
/// names and folds every other onto `Malformed`, so walking `v` upward until a
/// status repeats yields exactly the set the wire can deliver — and a status
/// the wire cannot deliver is one no operator will ever be handed.
///
/// With the list copied, adding a variant, wiring it into `from_u32`, fixing
/// `tf_tree_ipc`'s `status_is_a_refusal` and extending its `ALL_STATUSES` left
/// every gate green and this table without a row. Derived, the new status
/// appears here the moment the codec can produce it, and the row is owed.
fn receivable() -> Vec<HelloStatus> {
    let mut seen: Vec<HelloStatus> = Vec::new();
    // 64 is a bound, not an expectation: the walk ends at the fold.
    for v in 0..64u32 {
        let status = HelloStatus::from_u32(v);
        if seen.contains(&status) {
            break;
        }
        seen.push(status);
    }
    // **A derivation that collapses is a gate that checks nothing.** Every
    // assertion below iterates this, so a `from_u32` that folded everything
    // onto one status would leave them all holding over a single row. Seven
    // exist today and a wire contract does not shrink.
    assert!(
        seen.len() >= 7,
        "the codec delivers {} distinct statuses, fewer than the seven that exist: this \
         list is derived from `from_u32`, and it has stopped enumerating",
        seen.len()
    );
    seen
}

/// `docs/RUNBOOK.md`'s `HandshakeRejected` section.
fn section() -> &'static str {
    const RUNBOOK: &str = include_str!("../../../docs/RUNBOOK.md");
    // `\n### `, not `### `: a `#### ` subheading of the same name would match
    // the start, and the end bound must stop at a `## ` too, or a section that
    // becomes the last `###` under its chapter swallows the rest of the file
    // and every `contains` below goes vacuous with nothing firing.
    let after = RUNBOOK
        .split_once("\n### `HandshakeRejected`")
        .map_or("", |(_, after)| after);
    // An empty parse is not a pass: every assertion below is a `contains`, and
    // all of them hold vacuously against nothing.
    assert!(
        !after.is_empty(),
        "docs/RUNBOOK.md must carry a `HandshakeRejected` section: it holds the remedies \
         that variant's message stopped carrying"
    );
    let end = ["\n### ", "\n## "]
        .iter()
        .filter_map(|h| after.find(h))
        .min()
        .unwrap_or(after.len());
    &after[..end]
}

/// Words a remedy is written with.
///
/// **Required in the runbook and forbidden in the message**, which is the only
/// pairing that keeps either half honest: a forbidden list nobody would ever
/// write is vacuous, and a row can be checked for existing without being
/// checked for saying anything.
const REMEDY_WORDS: [&str; 5] = ["rebuild", "restart", "read-only", "/proc", "doctor"];

/// The shortest a row's *what to do* cell may be.
///
/// **A floor, not a target, and deliberately far below what these cells are.**
/// Its job is to catch a cell that was emptied, not to police length: the
/// remedy-word check below is over the section, so a row can be blanked without
/// touching a word anywhere else — measured, on review round 2, when the
/// `ModeNotPermitted` cell was emptied, the row stayed, and this file passed.
/// The assertion prints the offending cell's actual length, which is where a
/// number belongs; an earlier version of this sentence claimed the shortest
/// cell was "several times this" and it was 1.7×.
const REMEDY_FLOOR: usize = 60;

/// Every refusal has a row, the rows say something, and the example is real.
#[test]
fn the_runbook_answers_every_status_the_message_stopped_explaining() {
    let section = section();

    // Every status but the acceptance, which is not a refusal and has no row.
    for status in receivable().into_iter().filter(|s| s != &HelloStatus::Ok) {
        // A **row**, not a mention: the section's prose names `VersionMismatch`
        // and `LayoutMismatch` while distinguishing them from the
        // header-validation checks that share those names, so a `contains` over
        // the section is satisfied for two of the six by paragraphs that answer
        // nothing.
        let row = format!("| `{status:?}` |");
        let line = section
            .lines()
            .find(|l| l.starts_with(&row))
            .unwrap_or_else(|| {
                panic!(
                    "docs/RUNBOOK.md's `HandshakeRejected` section has no table row for \
                     {status:?}, so the message's search key leads an operator to a table \
                     that does not answer the status they were given"
                )
            });

        // **The row's own remedy cell, not the section's prose.** A first
        // version checked the remedy words over the whole section, and blanking
        // this cell — leaving the row in place — passed: an operator followed
        // the search key to an empty answer with the gate green.
        let cells: Vec<&str> = line.trim_matches('|').split(" | ").collect();
        assert_eq!(
            cells.len(),
            3,
            "{status:?}'s row is not three cells; `just artifact-versions` holds the count \
             and this holds what is in them: {line}"
        );
        assert!(
            cells[2].trim().len() >= REMEDY_FLOOR,
            "{status:?}'s remedy cell is {} bytes, under the {REMEDY_FLOOR}-byte floor; the \
             message stopped explaining this status on the promise that this cell would: \
             {line}",
            cells[2].trim().len()
        );
    }

    // **The worked example is the real rendering.** A quoted message is the
    // shape that drifts; this repository has corrected one figure across three
    // documents more than once.
    // **The values are the build's, not a second transcription.** A first cut
    // hard-coded `3` and `0x3D104195` on both sides, so the format break `0032`
    // already owes would have left the runbook quoting numbers no build
    // produces with this assertion green.
    let example = IpcError::HandshakeRejected {
        status: HelloStatus::LayoutMismatch,
        owner_format_version: tf_tree_arena::header::FORMAT_VERSION,
        owner_layout_hash: tf_tree_arena::layout::layout_hash(),
    }
    .to_string();
    assert!(
        section.contains(&example),
        "docs/RUNBOOK.md's `HandshakeRejected` section quotes a message this code does not \
         produce; it must contain, verbatim: {example}"
    );

    // The rows say something, and the message says none of it.
    for word in REMEDY_WORDS {
        assert!(
            section.contains(word),
            "docs/RUNBOOK.md's `HandshakeRejected` section no longer says {word:?}, which \
             the message is forbidden to say: the remedy is in neither place"
        );
        for status in receivable() {
            let text = IpcError::HandshakeRejected {
                status,
                owner_format_version: u32::MAX,
                owner_layout_hash: u32::MAX,
            }
            .to_string()
            .to_ascii_lowercase();
            assert!(
                !text.contains(word),
                "{status:?}'s message prescribes ({word:?}); the remedy belongs in the \
                 runbook, which the C ABI's 255 bytes cannot hold: {text}"
            );
        }
    }
}
