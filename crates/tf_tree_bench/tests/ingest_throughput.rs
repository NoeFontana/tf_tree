//! `ingest_throughput`'s exit status and refusals — what makes PHASE5 §12
//! gate 5 a gate.
//!
//! Unfenced: ingest into an in-process `Tree` needs no `shm`, so `just test`
//! runs this on every PR.
//!
//! | invocation | what it proves |
//! |---|---|
//! | a corpus denser than the criterion's, with `--gate` | the verdict goes red with no threshold edited |
//! | the same, without `--gate` | exits 0 while printing FAIL |
//! | `--floor` far above the measured ratio | the comparison is wired |
//! | a sparser corpus, with `--gate` | REFUSED: it would pass without checking anything |
//! | a single-edge corpus, with `--gate` | REFUSED: the grouped arm did not take its declared pass count |
//! | `--floor` below the criterion's, with `--gate` | REFUSED, no verdict line |
//! | the same, without `--gate` | `PASS (reported)` at exit 0 |
//! | `--reuse-corpus` on a missing path | REFUSED, not fabricated |
//! | `--reuse-corpus` on a foreign corpus | file survives, never reported as `WARM (written by this process)` |
//!
//! The green direction is not asserted here: debug reads far lower than
//! `--release`, so `just gate5` owns it. The declared-density case asserts only
//! profile-independent facts: per-arm pass counts, declared density, and a ratio
//! far above the dense corpus's.
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

/// Run the shipped `ingest_throughput` binary.
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

/// The density the binary measured off its own survey, not the declared one.
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

/// One arm's own line by its label; a substring search over all of stdout passes
/// on swapped arms, since both print `fill passes N,`.
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
    // §12 gate 5's own 100 Hz x 50 transforms; the ratio is a rate, so density
    // rather than length makes it representative.
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
    // Per arm, on that arm's own line: one fill pass in-memory, two grouped.
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

    // The falsifier, no threshold edited: a corpus forty times denser.
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

    // Profile-independent: the number must respond to the corpus.
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

    // The comparison is wired; fails for a different reason than the corpus does.
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

/// `--reuse-corpus` neither deletes a corpus it did not write nor fabricates one
/// at a missing path.
#[test]
fn reuse_corpus_neither_deletes_the_corpus_nor_fabricates_one() {
    let path = scratch("reuse");
    let corpus = path.display().to_string();
    let generate = [
        "--corpus",
        &corpus,
        "--edges",
        "50",
        "--rate-hz",
        "100",
        "--seconds",
        "2",
        "--rounds",
        "1",
    ];

    // A path that does not exist REFUSES rather than fabricating a corpus there.
    let mut reuse: Vec<&str> = generate.to_vec();
    reuse.push("--reuse-corpus");
    let missing = drive(&reuse);
    assert!(
        !missing.status.success() && err(&missing).contains("REFUSED — --reuse-corpus"),
        "--reuse-corpus on a missing path must refuse; got:\n{}{}",
        out(&missing),
        err(&missing)
    );

    // Write one, keeping it, and record its size.
    let mut kept: Vec<&str> = generate.to_vec();
    kept.push("--keep-corpus");
    let written = drive(&kept);
    assert!(
        written.status.success(),
        "got:\n{}{}",
        out(&written),
        err(&written)
    );
    let bytes = std::fs::metadata(&path)
        .expect("the corpus --keep-corpus wrote must be on disk")
        .len();

    // Reuse it: the run must read it, report it as found, and leave it there.
    let reused = drive(&reuse);
    let reused_out = out(&reused);
    assert!(
        reused.status.success(),
        "got:\n{reused_out}{}",
        err(&reused)
    );
    assert!(
        reused_out.contains("page cache as found (--reuse-corpus)"),
        "got:\n{reused_out}"
    );
    assert_eq!(
        std::fs::metadata(&path)
            .expect("a corpus this process did not write must survive the run")
            .len(),
        bytes,
        "--reuse-corpus must not delete, nor rewrite, the corpus it was pointed at"
    );

    // And again — the run before it cannot have substituted a different corpus.
    let again = drive(&reuse);
    let again_out = out(&again);
    assert!(again.status.success(), "got:\n{again_out}{}", err(&again));
    assert!(
        !again_out.contains("WARM (written by this process)"),
        "a reused corpus must never be reported as one this process wrote; got:\n{again_out}"
    );
    assert_eq!(
        std::fs::metadata(&path).expect("still there").len(),
        bytes,
        "the second reuse must read the same bytes as the first"
    );

    // The generating path still cleans up after itself.
    let generated = drive(&generate);
    assert!(
        generated.status.success(),
        "got:\n{}{}",
        out(&generated),
        err(&generated)
    );
    assert!(
        !path.exists(),
        "a corpus this process generated must still be removed at exit without --keep-corpus"
    );

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// A loosened `--floor` may not produce a gated PASS: loosening under `--gate`
/// refuses with no verdict, without `--gate` still reports PASS, and tightening
/// is covered by the `--floor 1000000` case above. The corpus is the declared
/// density and the floor `0.5`, so the refused PASS is one the binary would have
/// printed.
#[test]
fn a_loosened_floor_may_not_produce_a_gated_pass() {
    let path = scratch("loosened");
    let args = [
        "--corpus",
        &path.display().to_string(),
        "--edges",
        "50",
        "--rate-hz",
        "100",
        "--seconds",
        "2",
        "--rounds",
        "1",
        "--floor",
        "0.5",
    ];

    let mut gated: Vec<&str> = args.to_vec();
    gated.push("--gate");
    let refused = drive(&gated);
    assert!(
        !refused.status.success(),
        "a gated PASS against a floor below the criterion's own must refuse; got:\n{}{}",
        out(&refused),
        err(&refused)
    );
    assert!(
        err(&refused).contains("REFUSED — --floor"),
        "the refusal must name the flag that caused it; got:\n{}{}",
        out(&refused),
        err(&refused)
    );
    // A refusal publishes no verdict.
    assert!(
        !out(&refused).contains("GATED") && !out(&refused).contains("— PASS"),
        "a refusal must print neither the gated comparison nor a verdict; got:\n{}",
        out(&refused)
    );

    // Ungated, the loosened floor is a report: the refusal closes the gate only.
    let reported = drive(&args);
    assert!(
        reported.status.success() && out(&reported).contains("§12 gate 5 — PASS (reported)"),
        "got:\n{}{}",
        out(&reported),
        err(&reported)
    );

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// The density floor: a sparser corpus would read arbitrarily higher and pass
/// vacuously, so gated it refuses and ungated it reports.
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

/// One edge whose samples exceed the cap is `Group::Spilled`, so the grouped arm
/// takes one fill pass, not the declared regime; it refuses.
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
