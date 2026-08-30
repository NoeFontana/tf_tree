//! `doctor` prints the resolved runtime directory — `docs/PHASE2.md` §15.
//!
//! The checklist box is *"`doctor` prints `instance_uuid` and the resolved
//! runtime dir, **and works without the arena**"*, and the two halves are one
//! requirement rather than two. The run in which an operator most needs to know
//! which directory was searched is the run in which nothing was found in it —
//! so a report that could only name the directory when it had already found an
//! arena there would name it exactly when it does not matter.
//!
//! Every test here therefore runs `doctor` against the in-process fixture: no
//! arena, no rendezvous, nothing mapped.
//!
//! **`shm`-gated, and the reason is the thing under test.** The runtime
//! directory is where the *rendezvous* looks; without `shm` there is no
//! rendezvous, so `resolved_runtime_dir` reports `None` and there is no path to
//! assert on. `just test` builds default features, so an ungated file here fails
//! the ordinary suite for a configuration in which the field is correctly
//! absent — which is `just shm-check`'s job, and this file is on its list.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(all(feature = "shm", target_os = "linux"))]

use std::process::Command;

fn tf_tree() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tf_tree"))
}

/// The runtime directory reaches both renderers, from a source with no arena.
#[test]
fn doctor_reports_the_runtime_dir_without_an_arena() {
    let out = tf_tree().arg("doctor").arg("--json").output().unwrap();
    assert!(out.status.success());
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(
        json.contains("\"runtime_dir\":"),
        "the --json report must carry runtime_dir:\n{json}"
    );

    let human = tf_tree().arg("doctor").output().unwrap();
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(
        text.contains("runtime dir "),
        "the human report must carry the runtime dir:\n{text}"
    );
}

/// **It is the *resolved* directory, not a constant.** `$TF_TREE_RUNTIME_DIR` is
/// the first of the four candidates §3.2 defines, so overriding it must move the
/// reported path — a report that printed `/run/user/<uid>/tf_tree` whatever the
/// environment said would be worse than printing nothing, because an operator
/// would act on it.
#[test]
fn the_reported_dir_follows_the_environment() {
    let out = tf_tree()
        .arg("doctor")
        .env("TF_TREE_RUNTIME_DIR", "/tmp/zz-doctor-probe")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("/tmp/zz-doctor-probe"),
        "the override must reach the report:\n{text}"
    );
    // And the *source* is named, because an unexpected path is usually an
    // unexpected rule and the path alone leaves the operator to guess which.
    assert!(
        text.contains("Env"),
        "the report must say which rule produced the path:\n{text}"
    );
}

/// A directory that cannot be resolved degrades to no line, and the other
/// nineteen checks still run.
///
/// A host with no resolvable runtime dir is a finding for the operator, not a
/// reason to refuse the report — which is the whole point of the box's second
/// half.
#[test]
fn an_unresolvable_dir_does_not_fail_the_command() {
    let out = tf_tree()
        .arg("doctor")
        .env("TF_TREE_RUNTIME_DIR", "/proc/nonexistent/nope")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "doctor must still report: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("catalogue checks:"),
        "the catalogue must still run:\n{text}"
    );
}
