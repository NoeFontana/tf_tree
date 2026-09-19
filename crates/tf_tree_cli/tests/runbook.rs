//! `docs/RUNBOOK.md` holds what an error message gave up — `0055` step 7.
//!
//! `IpcError::HandshakeRejected`'s message ends `(HandshakeRejected)` and its
//! remedies live in the runbook's table, so the table is gated: a row per
//! refusal status, rows that say something, and a worked example equal to
//! `Display`'s output. It lives in `tf_tree_cli` because `tf_tree_ipc` is
//! published and `cargo package` omits files outside the package; the file-free
//! half is in `tf_tree_ipc`'s `error.rs`.
#![cfg(all(feature = "shm", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use tf_tree::{HelloStatus, IpcError};

/// How far up the wire numbering the derivation probes (wire discriminants are
/// assigned explicitly in `wire.rs`, so every `u16` is past what is contemplated).
const PROBE_RANGE: u32 = u16::MAX as u32 + 1;

/// Every `HelloStatus` this build can receive, derived from the codec rather
/// than copied: `HelloStatus` is `#[non_exhaustive]`, so the whole probe range
/// is walked through `from_u32` and each distinct status kept. The full sweep is
/// deliberate: stopping at the first repeat assumes contiguous numbering and
/// misses a status added past a gap.
fn receivable() -> Vec<HelloStatus> {
    let mut seen: Vec<HelloStatus> = Vec::new();
    // Every value in the range, not until the first repeat: numbering need not be contiguous.
    for v in 0..PROBE_RANGE {
        let status = HelloStatus::from_u32(v);
        if !seen.contains(&status) {
            seen.push(status);
        }
    }
    // A derivation that collapses (everything folded onto one status) checks nothing.
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
    // `\n### `, not `### `, so a `#### ` subheading does not match. The end
    // bound is any heading (one to six `#`s then a space), so a later section
    // cannot lend the table a word.
    let after = RUNBOOK
        .split_once("\n### `HandshakeRejected`")
        .map_or("", |(_, after)| after);
    // An empty parse is not a pass: every `contains` below would hold vacuously.
    assert!(
        !after.is_empty(),
        "docs/RUNBOOK.md must carry a `HandshakeRejected` section: it holds the remedies \
         that variant's message stopped carrying"
    );
    let is_heading = |line: &str| {
        let hashes = line.len() - line.trim_start_matches('#').len();
        (1..=6).contains(&hashes) && line[hashes..].starts_with(' ')
    };
    let end = after
        .match_indices('\n')
        .map(|(i, _)| i + 1)
        .find(|&start| is_heading(after[start..].split('\n').next().unwrap_or("")))
        .unwrap_or(after.len());
    &after[..end]
}

/// Words a remedy is written with. Required in the runbook remedy cells (their
/// union, not per row, which would dictate vocabulary; [`REMEDY_FLOOR`] holds a
/// single row) and forbidden in the message.
const REMEDY_WORDS: [&str; 5] = ["rebuild", "restart", "read-only", "/proc", "doctor"];

/// The shortest a row's *what to do* cell may be: a floor that catches an
/// emptied cell, not a length target.
const REMEDY_FLOOR: usize = 60;

/// Every refusal has a row, the rows say something, and the example is real.
#[test]
fn the_runbook_answers_every_status_the_message_stopped_explaining() {
    let section = section();

    // Every status but the acceptance has a row. The remedy column is found by
    // its heading, since `just artifact-versions` checks cell count, not order.
    let header = section
        .lines()
        .find(|l| l.starts_with("| `status` |"))
        .unwrap_or_else(|| panic!("docs/RUNBOOK.md's `HandshakeRejected` table has no header"));
    let headers: Vec<&str> = header.trim_matches('|').split(" | ").collect();
    let remedy_column = headers
        .iter()
        .position(|h| h.trim() == "What to do")
        .unwrap_or_else(|| {
            panic!("the `HandshakeRejected` table has no `What to do` column: {header}")
        });

    // `!= Ok` duplicates `tf_tree_ipc`'s `#[cfg(test)]` `status_is_a_refusal`,
    // which this crate cannot call; whoever adds a non-refusal status edits both.
    for status in receivable().into_iter().filter(|s| s != &HelloStatus::Ok) {
        // A row, not a mention: the section's prose names some statuses too.
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

        // The row's own remedy cell, not the section's prose.
        let cells: Vec<&str> = line.trim_matches('|').split(" | ").collect();
        assert_eq!(
            cells.len(),
            headers.len(),
            "{status:?}'s row does not match the header's cell count: {line}"
        );
        assert!(
            cells[remedy_column].trim().len() >= REMEDY_FLOOR,
            "{status:?}'s remedy cell is {} bytes, under the {REMEDY_FLOOR}-byte floor; the \
             message stopped explaining this status on the promise that this cell would: \
             {line}",
            cells[remedy_column].trim().len()
        );
    }

    // The worked example is the real rendering, with the build's own values.
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

    // The remedies say something and the message says none of it. Searched over
    // the remedy cells only, case-folded on both sides.
    let folded = section
        .lines()
        .filter(|l| l.starts_with("| `") && !l.starts_with("| `status` |"))
        .filter_map(|l| {
            l.trim_matches('|')
                .split(" | ")
                .nth(remedy_column)
                .map(str::trim)
        })
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase();
    for word in REMEDY_WORDS {
        assert!(
            folded.contains(word),
            "no *remedy* in docs/RUNBOOK.md's `HandshakeRejected` table says {word:?}, \
             which the message is forbidden to say: the remedy is in neither place. \
             Neither the prose around the table nor the `What the owner compared` \
             column counts — a search key lands a reader on a row, and the remedy \
             cell is what they act on"
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
