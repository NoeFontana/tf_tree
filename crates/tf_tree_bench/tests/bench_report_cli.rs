//! The `bench_report` binary's argument surface (`docs/PHASE5.md` §9.1).

// Assertions are the point of a test binary.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bench_report"))
        .args(args)
        .output()
        .expect("spawning bench_report")
}

/// §9.1 spells flags that would govern nothing on this host. Each is rejected,
/// naming the reason — never accepted and ignored.
///
/// `--duration` is the steady-state window per point. Every point it would
/// govern is an N-way comparison row that comes out UNAVAILABLE on any host
/// without ROS 2 and 17 cores, and the one row this tool measures itself is
/// bounded by lookup samples. An operator who passes `--duration 300s`, waits,
/// and receives a report identical in every measured field has been misled by
/// the tool whose entire thesis is that it does not mislead.
#[test]
fn a_flag_that_would_govern_nothing_is_rejected_not_ignored() {
    let help = run(&["--help"]);
    assert!(
        help.status.success(),
        "control failed: --help must exit 0, got {:?}",
        help.status
    );

    for (flag, value, needle) in [
        ("--duration", "300s", "steady-state window"),
        ("--bag", "run.mcap", "not wired up"),
    ] {
        // A refusal must not explain itself with a claim about the roadmap:
        // `report::tests::no_unavailable_reason_claims_a_phase_is_unimplemented`
        // is the same rule for the report's own rows, and this is the CLI half.
        let out = run(&[flag, value, "--help"]);
        assert!(
            !out.status.success(),
            "`{flag} {value}` was accepted; it governs nothing here"
        );
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains(flag) && err.contains(needle),
            "the refusal for `{flag}` must say why, got: {err}"
        );
        for stale in ["is not implemented", "are not implemented"] {
            assert!(
                !err.contains(stale),
                "the refusal for `{flag}` explains itself with `{stale}`, a claim about the \
                 roadmap that §0.0 owns and that has already gone stale once: {err}"
            );
        }
    }
}

/// `--consumers 0` describes no comparison at all: a publisher and nobody. It
/// would still be reported on, down to a `bridge_supervision` cost stated about
/// zero consumers.
#[test]
fn a_zero_consumer_comparison_is_refused() {
    let out = run(&["--consumers", "0", "--help"]);
    assert!(
        !out.status.success(),
        "`--consumers 0` was accepted as a comparison"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--consumers 0"), "got: {err}");

    let ok = run(&["--consumers", "4", "--help"]);
    assert!(
        ok.status.success(),
        "control failed: a real consumer count must be accepted, got {:?}",
        ok.status
    );
}
