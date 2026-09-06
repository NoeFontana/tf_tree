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
//! | `--floor` **below** the criterion's own, with `--gate`, on a run that would pass against it | REFUSED, publishing no verdict line: the floor is the whole of the gated comparison, so one flag would otherwise turn any FAIL into `PASS (gated)` at exit 0 |
//! | the same, without `--gate` | it prints `PASS (reported)` at exit 0 — what is closed is the gate, not the flag |
//! | `--reuse-corpus` on a missing path | REFUSED, rather than fabricating a corpus there and labelling it as one this process wrote |
//! | `--reuse-corpus` on a corpus this process did not write | the file survives the run, twice, and is never reported as `WARM (written by this process)` |
//!
//! # What this deliberately does NOT assert: that the gate is green
//!
//! `just test` builds debug, and the same measurement reads far lower there
//! than at `--release` — close enough to the floor on the development host
//! that a loaded runner could genuinely take it under.
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
//! * `if false && d.gate && ok && d.floor < FLOOR` around the threshold
//!   refusal — caught by `a_loosened_floor_may_not_produce_a_gated_pass`, which
//!   then observes the `PASS (gated)` at exit 0 the refusal exists to prevent;
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

/// **`--reuse-corpus` reads a corpus this process did not write, so it neither
/// deletes it nor invents one.**
///
/// Two halves of the same defect. The end-of-run cleanup was guarded on
/// `--keep-corpus` alone, so a reused corpus — the flag's entire purpose, "one
/// day a real recording" — was unlinked at exit 0. And `generated` was
/// recomputed from `corpus.exists()`, so the *next* `--reuse-corpus` run at that
/// path silently regenerated a synthetic corpus from
/// `--edges`/`--rate-hz`/`--seconds` and printed "page cache WARM (written by
/// this process)" over it: the operator believes they are re-measuring one
/// recording and are measuring another.
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

/// **The threshold floor: a loosened `--floor` may not produce a gated PASS.**
///
/// `--floor` is the whole of gate 5's gated comparison, so a caller who may
/// move it downwards under `--gate` can turn any FAIL into
/// `§12 gate 5 — PASS (gated)` at exit 0 — a verdict about the argument rather
/// than about the code. `frozen_workers`'s `--gate` is the precedent: its
/// threshold is a constant and not a flag.
///
/// The corpus here is the criterion's own declared density, and the floor is
/// `0.5` — far under the ratio this corpus measures in either profile, and the
/// run prints that ratio on its own verdict line rather than leaving a reader
/// to trust a figure written here. That matters: the PASS this refuses has to
/// be a PASS the binary really would have printed, and a *dense* corpus reads
/// under 0.5x in debug, where the refusal would never fire and the test would
/// be green for the wrong reason.
///
/// Three directions in one test, because what matters is that exactly one of
/// them is closed: loosening under `--gate` REFUSES and publishes no verdict;
/// the identical loosening **without** `--gate` still reports a PASS at exit 0;
/// and tightening under `--gate` is untouched (that one is asserted by the
/// `--floor 1000000` case above).
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
    // **A refusal publishes no verdict.** An exit status nobody reads plus a
    // PASS line somebody quotes is the failure `docs/benchmarks/EVIDENCE.md`
    // exists to prevent.
    assert!(
        !out(&refused).contains("GATED") && !out(&refused).contains("— PASS"),
        "a refusal must print neither the gated comparison nor a verdict; got:\n{}",
        out(&refused)
    );

    // The same loosened floor, ungated, is a report and still passes — so what
    // the refusal above closes is the *gate*, not the flag.
    let reported = drive(&args);
    assert!(
        reported.status.success() && out(&reported).contains("§12 gate 5 — PASS (reported)"),
        "got:\n{}{}",
        out(&reported),
        err(&reported)
    );

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
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
