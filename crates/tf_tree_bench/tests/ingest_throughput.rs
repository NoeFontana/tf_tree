//! **`ingest_throughput`'s exit status and its two refusals — the difference
//! between PHASE5 §12 gate 5 being a gate and being a report.**
//!
//! **Unfenced, so this runs in `just test` on every pull request.** Ingesting
//! into an in-process `Tree` needs no `shm` and no frozen backend, so unlike
//! `tests/gate4.rs` and `tests/gate2.rs` this one is reached by
//! `cargo nextest run --workspace`. `crates/tf_tree_bench/Cargo.toml`'s `[[bin]]`
//! stanza for the binary carries the same note.
//!
//! # What each case proves
//!
//! | invocation | what it proves |
//! |---|---|
//! | a corpus **denser** than the criterion's own, with `--gate` | the verdict goes red on a real corpus, editing no threshold — and it is the same fact the density floor states: "10x real time" is corpus-relative |
//! | the same, without `--gate` | it exits 0 while printing FAIL; the caller says whether a run is a gate |
//! | `--floor` far above the measured ratio | the comparison itself is wired, which is the weaker falsifier |
//! | a corpus **sparser** than the criterion's own, with `--gate` | REFUSED, not passed: a sparse corpus reads arbitrarily higher and would pass without checking anything |
//! | a **single-edge** corpus, with `--gate` | REFUSED: one edge over the cap spills rather than grouping, so the grouped arm did not take the pass count it declares, and an arm in the wrong regime is a different claim |
//!
//! # What this deliberately does NOT assert: that the gate is green
//!
//! `just test` builds debug, and the same measurement that reads ~180x real
//! time at `--release` reads ~17x in debug on the development host — a 1.7x
//! margin over the floor, which a loaded runner could genuinely take under it.
//! A flaky gate is a gate somebody disables. So the green direction is asserted
//! by `just gate5` (release, nightly), and what this file asserts about the
//! declared-density corpus is **profile-independent**: that both arms took the
//! pass counts they declare, that the corpus is at the declared density, and
//! that its ratio is far above the dense corpus's — i.e. that the number
//! responds to the corpus rather than being a constant.
//!
//! # The mutants, seeded and observed
//!
//! Each was applied to `src/bin/ingest_throughput.rs`, run, and reverted:
//!
//! * `if false && d.gate && !ok` around the exit — caught by the dense-corpus
//!   case;
//! * `if false && d.gate && density < GATE_DENSITY_FLOOR` — caught by the
//!   sparse-corpus test;
//! * `if false && grouped.passes != CRITERION_PASSES` — caught by the
//!   single-edge test;
//! * the two arms' **labels swapped** at their `measure` call sites, so the
//!   default-cap arm prints as `grouped` and the lowered-cap arm as
//!   `in-memory` — caught by the per-arm pass-count assertions in the
//!   declared-density case. It is seeded at the labels rather than at the caps
//!   on purpose: swapping the caps trips the binary's own per-arm refusals,
//!   which are not the thing under test here. Asserted over the whole of
//!   stdout — which is how they were first written — both assertions pass on
//!   this mutant, because both strings appear whichever arm printed them.
//!
//! Run: `just test`, or `cargo nextest run -p tf_tree_bench --test
//! ingest_throughput`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::process::Output;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tf_tree_gate5_{}_{}", name, std::process::id()));
    std::fs::create_dir_all(&dir).expect("creating the scratch directory");
    dir.join("corpus.mcap")
}

/// Run the shipped driver — `CARGO_BIN_EXE_ingest_throughput` is the binary the
/// recipe runs, not a re-implementation of it.
fn drive(args: &[&str]) -> Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_ingest_throughput"))
        .args(args)
        .output()
        .expect("spawning ingest_throughput")
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// The density the binary **measured** off its own survey — not the one the
/// arguments declared, which is the point of the check it feeds.
fn measured_density(stdout: &str) -> f64 {
    let line = stdout
        .lines()
        .find(|l| l.trim_start().starts_with("density: "))
        .unwrap_or_else(|| panic!("no density line in:\n{stdout}"));
    let n = line
        .split_once("density: ")
        .expect("density line shape")
        .1
        .split_whitespace()
        .next()
        .expect("a number after `density: `");
    n.parse()
        .unwrap_or_else(|e| panic!("parsing `{n}` from `{line}`: {e}"))
}

/// One arm's own line, found by the label it starts with.
///
/// **Not a substring search over the whole of stdout, which is what this
/// replaced.** Both arms print `fill passes N,` on their own lines, so
/// `stdout.contains("fill passes 1,")` and `stdout.contains("fill passes 2,")`
/// are both satisfied by the *swapped* assignment — the one arrangement in
/// which the gated number comes from the wrong regime, and the one a reader
/// comes to this file to see excluded. The shipped binary does hold the
/// property, in two separate per-arm refusals; this is the test catching up
/// with it.
fn arm_line<'a>(stdout: &'a str, label: &str) -> &'a str {
    stdout
        .lines()
        .map(str::trim_start)
        .find(|l| l.starts_with(label))
        .unwrap_or_else(|| panic!("no `{label}` arm line in:\n{stdout}"))
}

/// The gated arm's ratio, off the verdict line the recipe reads.
fn gated_ratio(stdout: &str) -> f64 {
    let line = stdout
        .lines()
        .find(|l| l.contains("GATED") && l.contains("grouped arm"))
        .unwrap_or_else(|| panic!("no gated verdict line in:\n{stdout}"));
    let after = line.split_once(": ").expect("verdict line shape").1;
    let n = after.split('x').next().expect("a ratio before the `x`");
    n.trim()
        .parse()
        .unwrap_or_else(|e| panic!("parsing `{n}` from `{line}`: {e}"))
}

#[test]
fn the_verdict_goes_red_on_a_denser_corpus_and_the_two_premises_refuse() {
    // The declared corpus: §12 gate 5's own 100 Hz x 50 transforms. Eight
    // seconds rather than a four-hour bag — the ratio is a rate, so its length
    // is not what makes it representative; its density is, and that is what the
    // gate floors.
    let declared = scratch("declared");
    let clean = drive(&[
        "--corpus",
        &declared.display().to_string(),
        "--edges",
        "50",
        "--rate-hz",
        "100",
        "--seconds",
        "8",
        "--rounds",
        "1",
        "--gate",
    ]);
    let clean_out = out(&clean);
    assert!(
        measured_density(&clean_out) >= 5000.0,
        "the corpus must measure at the criterion's own density, else every comparison \
         below is against nothing; got:\n{clean_out}{}",
        err(&clean)
    );
    // **Per arm, on that arm's own line.** The assignment is the property:
    // one fill pass on the in-memory arm and two on the grouped one. Asserted
    // over the whole of stdout these two pass on the swap as well, which is
    // exactly the arrangement that would put the gated number in the wrong
    // regime.
    let in_memory_line = arm_line(&clean_out, "in-memory");
    let grouped_line = arm_line(&clean_out, "grouped");
    assert!(
        in_memory_line.contains("fill passes 1,"),
        "the in-memory arm must take one fill pass; got:\n{in_memory_line}"
    );
    assert!(
        grouped_line.contains("fill passes 2,"),
        "the grouped arm must take the two fill passes the criterion's own recording \
         forces, else the gated number is from the wrong regime; got:\n{grouped_line}"
    );
    let clean_ratio = gated_ratio(&clean_out);

    // **The falsifier, and it edits no threshold**: a corpus forty times denser
    // than the criterion's own. This is the same fact the density floor states
    // from the other side — the ratio is a statement about the corpus.
    let dense_path = scratch("dense");
    let dense_args = [
        "--corpus",
        &dense_path.display().to_string(),
        "--edges",
        "2000",
        "--rate-hz",
        "100",
        "--seconds",
        "0.5",
        "--rounds",
        "1",
    ];
    let mut gated: Vec<&str> = dense_args.to_vec();
    gated.push("--gate");
    let dense = drive(&gated);
    let dense_out = out(&dense);
    assert!(
        dense_out.contains("§12 gate 5 — FAIL"),
        "got:\n{dense_out}{}",
        err(&dense)
    );
    assert!(
        !dense.status.success(),
        "a FAIL under --gate must be a non-zero exit — that is the whole difference \
         between this recipe and a report; got:\n{dense_out}"
    );
    let dense_ratio = gated_ratio(&dense_out);

    // **Profile-independent, and the header says why the green direction is not
    // asserted here.** What is asserted is that the number responds to the
    // corpus: a constant-valued ratio would fail this whatever it was.
    assert!(
        clean_ratio >= dense_ratio * 4.0,
        "the declared corpus read {clean_ratio}x and a 40x denser one read {dense_ratio}x; \
         a ratio that does not move with the corpus is not measuring the corpus"
    );

    // The same measurement, ungated, exits 0 while printing FAIL.
    let reported = drive(&dense_args);
    assert!(
        reported.status.success() && out(&reported).contains("§12 gate 5 — FAIL"),
        "got:\n{}{}",
        out(&reported),
        err(&reported)
    );

    // **The weaker falsifier**: the comparison is wired. Kept because it fails
    // for a different reason than the corpus does, and a union of two paths is
    // where one of them stops being read.
    let floor_path = scratch("floor");
    let floored = drive(&[
        "--corpus",
        &floor_path.display().to_string(),
        "--edges",
        "50",
        "--rate-hz",
        "100",
        "--seconds",
        "2",
        "--rounds",
        "1",
        "--floor",
        "1000000",
        "--gate",
    ]);
    assert!(
        !floored.status.success() && out(&floored).contains("§12 gate 5 — FAIL"),
        "got:\n{}{}",
        out(&floored),
        err(&floored)
    );

    for p in [&declared, &dense_path, &floor_path] {
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }
}

/// **The density floor.** §12 gate 5's representative recording is 100 Hz x 50
/// transforms; at an identical per-transform cost a sparser corpus reads
/// arbitrarily higher, so a gated run on one would pass without checking
/// anything. It refuses rather than passing, and reports rather than refusing
/// when nobody claimed it was a gate.
#[test]
fn a_corpus_sparser_than_the_criterions_own_is_refused_rather_than_passed() {
    let path = scratch("sparse");
    let args = [
        "--corpus",
        &path.display().to_string(),
        "--edges",
        "5",
        "--rate-hz",
        "10",
        "--seconds",
        "4",
        "--rounds",
        "1",
    ];
    let mut gated: Vec<&str> = args.to_vec();
    gated.push("--gate");
    let refused = drive(&gated);
    let e = err(&refused);
    assert!(!refused.status.success(), "got:\n{}{e}", out(&refused));
    assert!(
        e.contains("transforms per second of recording"),
        "the refusal must name the density it measured and the one it wanted; got:\n{e}"
    );

    let reported = drive(&args);
    assert!(
        reported.status.success(),
        "without --gate the same corpus reports; got:\n{}{}",
        out(&reported),
        err(&reported)
    );

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// **The pass-count premise, red-tested with a corpus shape rather than a
/// flag.** One edge whose samples exceed the cap is `Group::Spilled`, not two
/// groups — so the grouped arm takes one fill pass and is not the regime the
/// gated number is stated over. It refuses; a run that reported anyway would be
/// comparing different amounts of work.
#[test]
fn an_arm_that_did_not_take_its_declared_pass_count_refuses() {
    let path = scratch("onedge");
    let refused = drive(&[
        "--corpus",
        &path.display().to_string(),
        "--edges",
        "1",
        "--rate-hz",
        "5000",
        "--seconds",
        "4",
        "--rounds",
        "1",
        "--gate",
    ]);
    let e = err(&refused);
    assert!(!refused.status.success(), "got:\n{}{e}", out(&refused));
    assert!(
        e.contains("fill passes, not 2"),
        "the refusal must say which arm was in the wrong regime; got:\n{e}"
    );
    assert!(
        !out(&refused).contains("§12 gate 5 — PASS"),
        "a refused run must publish no verdict; got:\n{}",
        out(&refused)
    );

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}
