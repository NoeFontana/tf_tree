//! `doctor` prints the resolved runtime directory without an arena — `docs/PHASE2.md` §15.
//!
//! `shm`-gated: without `shm` there is no rendezvous, so `resolved_runtime_dir`
//! is `None`. `just shm-check` runs it.

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

/// It is the *resolved* directory: `$TF_TREE_RUNTIME_DIR` overrides it (§3.2).
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
    // The source of the path is named too.
    assert!(
        text.contains("Env"),
        "the report must say which rule produced the path:\n{text}"
    );
}

/// An unresolvable directory degrades to no line; the other checks still run.
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
