//! **`docs/RUNBOOK.md` holds what an error message gave up** — `0055` step 7.
//!
//! `IpcError::HandshakeRejected` used to carry a remedy per `HelloStatus`, and
//! those seven texts were 112 to 378 bytes against a C buffer of 256. They are
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

/// Every `HelloStatus`.
///
/// **A total `match` here is impossible, and that is deliberate rather than an
/// oversight.** `HelloStatus` is `#[non_exhaustive]`, because a newer owner may
/// refuse for a reason this build has no name for and a downstream `match` must
/// keep compiling when one is added — the same argument that makes
/// `HelloStatus::from_u32` fold every unknown code onto `Malformed`. So the
/// compile-time prompt for a new status cannot live in this crate; it is
/// `status_is_a_refusal` in `tf_tree_ipc`'s `error.rs`, whose doc names this
/// list among the things its author then owes an entry.
///
/// What covers the gap from *here* is on the wire:
/// `a_status_this_build_cannot_receive_needs_no_row` in that same module. A
/// variant `from_u32` does not produce cannot arrive in a `HelloResponse`, so a
/// client never renders it and no operator ever follows a search key to a
/// missing row.
const ALL: [HelloStatus; 7] = [
    HelloStatus::Ok,
    HelloStatus::VersionMismatch,
    HelloStatus::LayoutMismatch,
    HelloStatus::BootIdMismatch,
    HelloStatus::NoParticipantSlots,
    HelloStatus::ModeNotPermitted,
    HelloStatus::Malformed,
];

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
/// **A floor, not a target.** It exists because the remedy-word check below is
/// over the section and a row can be emptied without touching a word anywhere
/// else — measured, on review round 2: the `ModeNotPermitted` cell was blanked,
/// the row stayed, and this file passed. The shortest cell today is several
/// times this.
const REMEDY_FLOOR: usize = 60;

/// Every refusal has a row, the rows say something, and the example is real.
#[test]
fn the_runbook_answers_every_status_the_message_stopped_explaining() {
    let section = section();

    // Every status but the acceptance, which is not a refusal and has no row.
    for status in ALL.into_iter().filter(|s| *s != HelloStatus::Ok) {
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
        for status in ALL {
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
