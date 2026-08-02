//! The CLI against a **live** arena — `docs/decisions/0005` step 11.
//!
//! This is the milestone's acceptance test in the plainest sense available: a
//! publisher runs, and the shipped binary is asked to describe it. Everything
//! upstream of here is tested by code that arranges its own processes and knows
//! where the seams are. This does not — it goes through `clap`, through
//! `tf_tree::open()`, and through whatever the arena actually says.
//!
//! `tf_tree participants` gets its own test because its contract is the
//! opposite of the others': §3.3 requires it to work **without the arena**, and
//! the only way to show that is to ask it about a lock file whose segment never
//! existed.
#![cfg(all(feature = "shm", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::process::Command;

use tf_tree::{AttachMode, Capacity, CreatePolicy, EdgeCfg, InterpPolicy, Tree, TreeBuilder};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let p = std::env::temp_dir().join(format!("tf_tree_cli-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        std::env::set_var("TF_TREE_RUNTIME_DIR", &p);
        Scratch(p)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// **The test process is the publisher.**
///
/// A helper binary would be more ceremony for less fidelity: `CARGO_BIN_EXE_*`
/// only names bins of the *same* package, and the thing under test is the CLI
/// joining somebody else's arena — which this is, exactly. The test owns the
/// arena and serves it from its owner thread; the CLI runs as a real subprocess
/// and comes in over the socket like any other consumer.
///
/// Returned by value and held by the caller: dropping it releases the ownership
/// byte and stops the server, so it has to outlive the CLI invocations.
fn publish(_scratch: &Scratch) -> Tree {
    let tree = tf_tree::Open::new()
        .mode(AttachMode::ReadWrite)
        .create(CreatePolicy::IfAbsent)
        .layout_if_creating(
            TreeBuilder::new()
                .default_interp(InterpPolicy::LerpSlerp)
                .dynamic_edge("map", "base", EdgeCfg::new(Capacity::slots(64)))
                .dynamic_edge("base", "cam", EdgeCfg::new(Capacity::slots(64))),
        )
        .open()
        .expect("create the arena");

    let child = tree.frame("base").unwrap();
    let parent = tree.frame("map").unwrap();
    let w = tree.claim(child, parent).expect("claim");
    // A short run of history, so `echo` has something to interpolate between
    // and the rate check has intervals to look at.
    for i in 0..16i64 {
        w.push(
            1_000_000_000 + i * 10_000_000,
            &tf_tree_math::exp_se3([0.0, 0.0, 0.01 * i as f64, i as f64, 0.0, 0.0]),
        )
        .expect("push");
    }
    // The writer is leaked so the claim stays held for the duration: an edge
    // that reports UNCLAIMED would change what `tree` and `doctor` print.
    core::mem::forget(w);
    tree
}

fn cli(dir: &PathBuf, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_tf_tree"))
        .args(args)
        .env("TF_TREE_RUNTIME_DIR", dir)
        .output()
        .expect("run tf_tree");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// **`--attach` reads the publisher's tree, not a fixture.**
///
/// The frame names are the tell. The publisher's topology is `map -> base ->
/// cam`; the in-process fixture is a mobile-robot rig with `odom`, `base_link`
/// and a laser. If `--attach` were quietly falling back to the
/// fixture — the failure mode that matters, because it prints a perfectly
/// plausible tree — the fixture's frames would be here instead.
#[test]
fn attach_shows_the_live_publishers_topology() {
    let scratch = Scratch::new("tree");
    let _pubr = publish(&scratch);

    let (ok, out) = cli(&scratch.0, &["tree", "--attach"]);
    assert!(ok, "tf_tree tree --attach failed:\n{out}");
    assert!(
        out.contains("live arena"),
        "banner still says fixture:\n{out}"
    );
    assert!(out.contains("map"), "no `map` frame:\n{out}");
    assert!(out.contains("base"), "no `base` frame:\n{out}");
    assert!(
        !out.contains("base_link"),
        "this is the in-process fixture, not the live arena:\n{out}"
    );
}

/// A lookup through the shipped binary must return the publisher's transform.
#[test]
fn echo_attaches_and_resolves() {
    let scratch = Scratch::new("echo");
    let _pubr = publish(&scratch);

    let (ok, out) = cli(&scratch.0, &["echo", "map", "base", "--attach"]);
    assert!(ok, "tf_tree echo --attach failed:\n{out}");
    assert!(
        out.contains("q=[") && !out.contains("error:"),
        "echo did not resolve against the live arena:\n{out}"
    );
}

/// **`doctor` must not claim a clean bill of health it did not earn.**
///
/// A live arena has no recorded push stream, and the two checks that depend on
/// one degrade differently — which is why the report has two ways of saying so.
/// `TFT001` loses its only evidence (a ring remembers the current claim owner,
/// not the sequence of writers) and is reported *not run*. `TFT011` keeps its
/// counter evidence and loses only the capacity-vs-latency half, so it runs and
/// carries a note. A bare `pass` on either would be the lie by omission this
/// asserts against.
///
/// **And `TFT019` needs a third thing said, because an attach is not a source
/// it can answer from.** A reader who meets a silent `TFT019` here must not read
/// it as "the clock did not step" — the skip line itself is the only place that
/// can be corrected, since `docs/` is not what an operator has open at 3 a.m.,
/// and what it has to carry is the source that *can* answer:
/// `doctor --from-bag`. This is the end-to-end half of the unit assertion in
/// `checks::tests::tft019_inherits_tft018s_replayed_stream_skip` — that one pins
/// the string, this one pins that it survives into the printed report.
///
/// Mutant: return `Vec::new()` from `evidence_notes`. Applied: the
/// `TFT011` note assertion fails while the rest still passes, which is the
/// half-blind case going unreported.
/// Mutant B: delete the `--from-bag` sentence from
/// `PushStream::RingsUnderWriter`'s `no_rejected_arrivals` reason. Applied: the
/// `--from-bag` assertion fails — the redirection is stated in the check and
/// lost on the way to the operator.
#[test]
fn doctor_names_the_checks_it_cannot_run_on_a_live_arena() {
    let scratch = Scratch::new("doctor");
    let _pubr = publish(&scratch);

    let (_ok, out) = cli(&scratch.0, &["doctor", "--attach"]);
    assert!(
        out.contains("live arena"),
        "banner still says fixture:\n{out}"
    );
    assert!(
        out.contains("not run:"),
        "doctor did not disclose its blind checks:\n{out}"
    );
    assert!(
        out.contains("TFT001"),
        "doctor did not name the check that lost all its evidence:\n{out}"
    );
    assert!(
        out.contains("note: TFT011 ran on its counter evidence only"),
        "doctor did not disclose the half-blind check:\n{out}"
    );
    assert!(
        out.contains("instance "),
        "doctor did not report which arena instance it looked at:\n{out}"
    );
    // TFT018/TFT019 have no verdict on an attach, so the skip has to send the
    // operator to the source that does rather than let its silence read as an
    // all-clear.
    let not_run_reasons = out.split("not run:").nth(1).unwrap_or("");
    assert!(
        not_run_reasons.contains("--from-bag"),
        "TFT019's skip must reach the operator naming the source that can answer:\n{out}"
    );
    // `TFT014` resolves a claim through the *shared* arena's participant
    // table, which is the case a single-process test cannot reach. A leaked
    // claim would be a finding; the publisher is alive, so it must be silent.
    let not_run = out.split("not run:").nth(1).unwrap_or("");
    assert!(
        !not_run.contains("TFT014"),
        "TFT014 must run against a real participant table:\n{out}"
    );
    assert!(
        !out.contains("TFT014  participant"),
        "a live publisher's claim was reported as leaked:\n{out}"
    );
}

/// **`participants` must work with no arena at all** (§3.3).
///
/// The lock file is the source of truth about who is attached, and it is a
/// separate file precisely so that it survives a segment this build cannot map:
/// a format-version mismatch, a layout-hash mismatch, a wedged owner. Those are
/// the moments somebody reaches for a diagnostic tool, so this is the command
/// that must not need the thing that is broken.
#[test]
fn participants_lists_a_live_publisher() {
    let scratch = Scratch::new("participants");
    let _pubr = publish(&scratch);

    let (ok, out) = cli(&scratch.0, &["participants"]);
    assert!(ok, "tf_tree participants failed:\n{out}");
    assert!(out.contains("live"), "no live participant listed:\n{out}");
    assert!(
        out.contains("rw"),
        "the publisher attached read-write; that is not shown:\n{out}"
    );
}

/// Nothing running is an *answer*, not a failure.
///
/// Exiting non-zero here would make "no publisher" indistinguishable from "the
/// tool could not look", which is the distinction an operator is running it to
/// find out.
#[test]
fn participants_on_an_empty_machine_says_so_and_succeeds() {
    let scratch = Scratch::new("empty");
    let (ok, out) = cli(&scratch.0, &["participants"]);
    assert!(ok, "an empty machine must not be an error:\n{out}");
    assert!(
        out.contains("no lock file"),
        "did not say the machine is empty:\n{out}"
    );
}

/// A wrong `--domain` must report *nothing there*, not a stale snapshot of
/// something else.
///
/// This is the mistake an operator actually makes, and the dangerous version of
/// it is silent: attaching to domain 7 and being shown domain 0's tree looks
/// exactly like a working system.
#[test]
fn a_different_domain_is_a_different_arena() {
    let scratch = Scratch::new("domain");
    let _pubr = publish(&scratch);

    let (ok, out) = cli(&scratch.0, &["participants", "--domain", "7"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("no lock file"),
        "domain 7 reported something; the domains are not isolated:\n{out}"
    );

    let (ok, out) = cli(&scratch.0, &["tree", "--attach", "--domain", "7"]);
    assert!(!ok, "attaching to an empty domain must fail:\n{out}");
}

/// `tf_tree top` against a live arena, including the observer's own row.
///
/// Two properties that only a real attach can show:
///
/// 1. **The read-only observer is visible and is marked as one.** A read-only
///    participant holds a lock-file byte and writes *no* arena participant
///    record (`Tree::participant_slot` returns `u32::MAX`), so a participant
///    pane built from the arena table alone would show only the publisher — and
///    `top` would be invisible in its own output while sitting in the table's
///    capacity.
/// 2. **It says it is a read-only observer**, which is the claim the rest of
///    the pane is asking to be believed.
///
/// Mutant: replace `cmd_top`'s lock-file `merge` closure with the no-op one
/// used on the non-attach path. Applied: no `ro` row and no `record=no` row
/// exist, so the `mode ro` assertion fails while the frame otherwise renders
/// perfectly — exactly the silent half-picture this asserts against.
#[test]
fn top_shows_the_live_arena_and_its_own_read_only_row() {
    let scratch = Scratch::new("top");
    let _pubr = publish(&scratch);

    let (ok, out) = cli(
        &scratch.0,
        &["top", "--attach", "--iterations", "2", "--interval", "50"],
    );
    assert!(ok, "tf_tree top --attach failed:\n{out}");
    assert_eq!(
        out.matches("tf_tree top").count(),
        2,
        "not two frames:\n{out}"
    );
    assert!(out.contains("live arena"), "banner says fixture:\n{out}");
    assert!(out.contains("read-only observer"), "{out}");
    assert!(
        out.contains("no arena participant record"),
        "the observer did not disclose how it is attached:\n{out}"
    );
    // The publisher's topology, not the in-process fixture's.
    assert!(out.contains("map->base"), "no live edge:\n{out}");
    assert!(
        !out.contains("base_link"),
        "this is the fixture, not the live arena:\n{out}"
    );
    // The observer's own lock-file row: read-only, and with no arena record.
    let pane = out
        .split("participants")
        .nth(1)
        .expect("no participants pane");
    let ro_rows: Vec<&str> = pane
        .lines()
        .filter(|l| l.split_whitespace().nth(2) == Some("ro"))
        .collect();
    assert!(!ro_rows.is_empty(), "no read-only participant row:\n{pane}");
    assert!(
        ro_rows.iter().any(|l| l.contains(" no ")),
        "the read-only row claims an arena record it cannot have:\n{ro_rows:?}"
    );
}

/// **`top` refuses `--rw` rather than quietly downgrading it.**
///
/// D18 is why a diagnostic tool maps `PROT_READ`: the MMU is what stops a bug
/// in this binary from corrupting a robot's transform tree. `--rw` is a global
/// flag, so `tf_tree --rw top` parses; accepting it would put the longest-lived
/// diagnostic process on the robot inside the blast radius the mapping exists
/// to define.
///
/// Mutant: delete the `anyhow::ensure!(!live.rw, ...)` in `cmd_top`. Applied:
/// the command exits 0 and both assertions fail.
#[test]
fn top_refuses_a_read_write_attach() {
    let scratch = Scratch::new("top-rw");
    let _pubr = publish(&scratch);

    let out = Command::new(env!("CARGO_BIN_EXE_tf_tree"))
        .args(["top", "--attach", "--rw", "--iterations", "1"])
        .env("TF_TREE_RUNTIME_DIR", &scratch.0)
        .output()
        .expect("run tf_tree");
    assert!(!out.status.success(), "--rw was accepted");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("read-only observer") && err.contains("--rw"),
        "unhelpful refusal: {err}"
    );
}
