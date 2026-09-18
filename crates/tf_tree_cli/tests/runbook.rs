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

/// `docs/RUNBOOK.md`'s `HandshakeRejected` section.
fn section() -> &'static str {
    const RUNBOOK: &str = include_str!("../../../docs/RUNBOOK.md");
    let after = RUNBOOK
        .split_once("### `HandshakeRejected`")
        .map_or("", |(_, after)| after);
    // An empty parse is not a pass: every assertion below is a `contains`, and
    // all of them hold vacuously against nothing.
    assert!(
        !after.is_empty(),
        "docs/RUNBOOK.md must carry a `HandshakeRejected` section: it holds the remedies \
         that variant's message stopped carrying"
    );
    after.split_once("\n### ").map_or(after, |(body, _)| body)
}

/// Words a remedy is written with.
///
/// **Required in the runbook and forbidden in the message**, which is the only
/// pairing that keeps either half honest: a forbidden list nobody would ever
/// write is vacuous, and a row can be checked for existing without being
/// checked for saying anything.
const REMEDY_WORDS: [&str; 5] = ["rebuild", "restart", "read-only", "/proc", "doctor"];

/// Every refusal has a row, the rows say something, and the example is real.
#[test]
fn the_runbook_answers_every_status_the_message_stopped_explaining() {
    let section = section();

    for status in [
        HelloStatus::VersionMismatch,
        HelloStatus::LayoutMismatch,
        HelloStatus::BootIdMismatch,
        HelloStatus::NoParticipantSlots,
        HelloStatus::ModeNotPermitted,
        HelloStatus::Malformed,
    ] {
        // A **row**, not a mention: the section's prose names `VersionMismatch`
        // and `LayoutMismatch` while distinguishing them from the
        // header-validation checks that share those names, so a `contains` over
        // the section is satisfied for two of the six by paragraphs that answer
        // nothing.
        let row = format!("| `{status:?}` |");
        assert!(
            section.lines().any(|l| l.starts_with(&row)),
            "docs/RUNBOOK.md's `HandshakeRejected` section has no table row for {status:?}, \
             so the message's search key leads an operator to a table that does not answer \
             the status they were given"
        );
    }

    // **The worked example is the real rendering.** A quoted message is the
    // shape that drifts; this repository has corrected one figure across three
    // documents more than once.
    let example = IpcError::HandshakeRejected {
        status: HelloStatus::LayoutMismatch,
        owner_format_version: 3,
        owner_layout_hash: 0x3D10_4195,
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
        for status in [
            HelloStatus::Ok,
            HelloStatus::VersionMismatch,
            HelloStatus::LayoutMismatch,
            HelloStatus::BootIdMismatch,
            HelloStatus::NoParticipantSlots,
            HelloStatus::ModeNotPermitted,
            HelloStatus::Malformed,
        ] {
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
