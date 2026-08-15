//! The pin `docs/decisions/0015` step 6 claimed existed and did not.
//!
//! Step 6 deletes `dds_report::MISSING_ARM` — the "NOT MEASURED — tf_tree across
//! processes" paragraph the report printed above its own table on every run —
//! and says the deletion is *"verified by the test that pins the report's
//! required sections"*. There was no such test. `dds_report.rs` had no
//! `mod tests`, nothing under `tests/` mentioned the constant, and the
//! `REQUIRED_ROWS` machinery that sounds like it belongs here belongs to
//! `bench_report`, a different binary. So the sentence was pinned by nothing and
//! deleting it was unverified by construction — and, worse, so is the arm that
//! replaced it: nothing would have noticed if `tf_tree.processes` silently
//! stopped being emitted, which is the exact state the disclosure existed to
//! rule out.
//!
//! This is that pin, and it is deliberately **two** assertions rather than one:
//!
//! * the four arm labels are all in the rendered table — a run that produced
//!   three arms is a run whose fourth arm did not happen, whatever the other
//!   three say;
//! * no "NOT MEASURED" text is anywhere in the output — so the sentence cannot
//!   come back, in any spelling, without this failing.
//!
//! It runs the binary rather than calling `aggregate`, for the reason
//! `bench_report_cli.rs` does the same: `aggregate` prints, and what is being
//! pinned is what an operator reads.

// Assertions are the point of a test binary.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// A scratch directory that removes itself, so a failing assertion does not
/// leave the next run reading this one's fixtures.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!(
            "tf_tree_dds_report-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One consumer process's output, in the line protocol `bench_consumer` prints
/// and `parse_proc` reads.
///
/// The histogram is `mp::Histogram::encode`'s wire format, hand-written here so
/// the fixture does not depend on the C++ side being built: `hist <total> <max>`
/// then `<bucket>:<count>` pairs.
fn consumer_out(consumers: usize) -> String {
    let mut s = format!("warmup_s 3.0\nmeasured_s 15.0\nconsumers {consumers}\n");
    for i in 0..consumers {
        s.push_str(&format!("consumer {i} service hist 100 4000 900:100\n"));
        s.push_str(&format!("consumer {i} cycle hist 100 90000 1100:100\n"));
        s.push_str(&format!("consumer {i} ok 100 err 0\n"));
    }
    s.push_str("cpu_ns 250000000\npss_kib 16000\n");
    s
}

/// The bridge process of the `tf_tree.processes` arm: `consumers 0`, no query
/// threads, and a cost that must land in the arm anyway.
fn bridge_out() -> String {
    "warmup_s 3.0\nmeasured_s 15.0\nconsumers 0\n\
     cpu_ns 900000000\npss_kib 24000\n\
     bridge_transforms 16373\nbridge_applied 16373\nbridge_dropped 0\n\
     bridge_queue_high_water 4\n"
        .to_owned()
}

/// Write the four arms a real `just dds-bench` produces, with the file names
/// `ros/dds_bench.sh` writes.
fn write_four_arms(dir: &Path) {
    for i in 0..4 {
        std::fs::write(dir.join(format!("tf2.processes.{i}.out")), consumer_out(1)).unwrap();
    }
    std::fs::write(dir.join("tf2.composed.0.out"), consumer_out(4)).unwrap();
    std::fs::write(dir.join("tf_tree.composed.0.out"), consumer_out(4)).unwrap();
    // 1 bridge + 4 attached consumers, the arm's N+1 shape.
    std::fs::write(dir.join("tf_tree.processes.0.out"), bridge_out()).unwrap();
    for i in 1..=4 {
        std::fs::write(
            dir.join(format!("tf_tree.processes.{i}.out")),
            consumer_out(1),
        )
        .unwrap();
    }
}

fn run(dir: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_dds_report"))
        .args(["aggregate", "--dir"])
        .arg(dir)
        .output()
        .expect("spawning dds_report")
}

fn aggregate(dir: &Path) -> String {
    let out = run(dir);
    assert!(
        out.status.success(),
        "dds_report aggregate failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The other half: what the tool says when it refuses.
///
/// Asserts the refusal is a refusal — non-zero exit **and** nothing resembling
/// a table on stdout — because a diagnostic printed after four plausible rows
/// is a diagnostic somebody quotes the rows from.
fn refusal(dir: &Path) -> String {
    let out = run(dir);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        !out.status.success(),
        "dds_report aggregate SUCCEEDED on a directory it must refuse.\n--- stdout ---\n{stdout}"
    );
    assert!(
        !stdout.lines().any(|l| l.starts_with("tf2.")),
        "the refusal came after the table had started printing rows:\n{stdout}"
    );
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// **The pin.** All four arms are in the table, and nothing says NOT MEASURED.
///
/// Mutant: in `aggregate`, skip the fourth arm —
/// `for arm in arms.values().filter(|a| a.label != "tf_tree.processes")`. The
/// run still succeeds, still prints three plausible rows, and this fails naming
/// the arm that vanished.
#[test]
fn the_report_states_all_four_arms_and_declares_none_unmeasurable() {
    let scratch = Scratch::new("four-arms");
    write_four_arms(scratch.path());
    let text = aggregate(scratch.path());

    // **A row, not a mention.** `text.contains(arm)` is what this assertion said
    // first, and the mutation below defeated it: the table's own footer explains
    // `tf_tree.processes` in prose, so a report that dropped the *row* still
    // contained the string and the test passed. The label is column 0 of a row
    // (`{:<26}`), so a row is a line that starts with it.
    for arm in [
        "tf2.processes",
        "tf2.composed",
        "tf_tree.composed",
        "tf_tree.processes",
    ] {
        assert!(
            text.lines().any(|l| l.starts_with(arm)),
            "the report has no table row for the arm `{arm}` (prose mentioning it does not \
             count). §9.1's comparison is four arms; a run that emits three has an arm \
             that did not happen, and the table says so about none of them.\n\
             --- report ---\n{text}"
        );
    }

    // The `MISSING_ARM` paragraph, in any spelling. `docs/decisions/0015` step 6
    // deletes it *because the gap is closed* — reintroducing the words while the
    // arm exists would be the report disclaiming a measurement it made, and
    // reintroducing them because the arm broke is what the assertion above
    // catches first.
    //
    // **These are fragments the deleted constant actually contained**, checked
    // against `git show origin/main:...dds_report.rs`. Two of the three this
    // list started with (`"not measurable"`, `"no multi-process tf_tree arm"`)
    // were prose from `docs/decisions/0015` and appeared nowhere in the tool's
    // output, so they would not have fired against the code this test was
    // written to pin — an assertion that passes before and after the change it
    // guards is not a pin. Only `"NOT MEASURED"` was load-bearing; the other
    // three below are its neighbours in the same string.
    for stale in [
        "NOT MEASURED",
        "TreeBuilder::build()",
        "HEAP arena",
        "no second process can attach",
    ] {
        assert!(
            !text.contains(stale),
            "the report still says `{stale}`. The multi-process tf_tree arm is measured \
             now (docs/decisions/0015); an honest gap that has been closed is no longer \
             honest.\n--- report ---\n{text}"
        );
    }
}

/// The bridge process's cost is **in** the arm, not beside it.
///
/// This is the half of step 5 a label check cannot see. The bridge contributes
/// `cpu_ns` and `pss_kib` while reporting `consumers 0`, so it raises the arm's
/// CPU-per-consumer and its PSS; an aggregator that skipped `consumers 0`
/// processes, or that divided by `procs` instead of by the summed consumer
/// count, would print a `tf_tree.processes` row that got a whole bridge for
/// free — the single most flattering mistake this table could make.
///
/// The fixture is arithmetic, not a measurement. Four consumer processes at
/// 0.25 s of CPU each is 1.00 s; the bridge adds 0.90 s. Over 4 consumers and a
/// 15 s window that is 1.90/4/15 = **3.167 %** with the bridge's cost and
/// 1.00/4/15 = **1.667 %** without it. PSS is 4 x 16000 + 24000 = **85.94 MiB**
/// against **62.50 MiB**.
///
/// **The control is a bridge that cost nothing, not a missing bridge**, and the
/// change is not cosmetic: `check_structure` now *refuses* a
/// `tf_tree.processes` arm with no `consumers 0` process in it, so deleting the
/// file — which is what this control did first — no longer produces a row to
/// compare against. A zero-cost bridge is the better control anyway. It differs
/// from the real fixture in exactly the quantity under test and in nothing
/// else: same five processes, same four consumers, same `bridge_transforms`.
///
/// Mutant: in `aggregate`'s per-arm fold, `if p.consumers == 0 { continue; }`.
/// The row then reads 1.667 % and 62.50 MiB and every other assertion in this
/// file still passes.
#[test]
fn the_bridge_process_cost_lands_in_the_arm_it_serves() {
    let with_bridge = Scratch::new("with-bridge");
    write_four_arms(with_bridge.path());
    let with = aggregate(with_bridge.path());

    let free_bridge = Scratch::new("free-bridge");
    write_four_arms(free_bridge.path());
    std::fs::write(
        free_bridge.path().join("tf_tree.processes.0.out"),
        bridge_out()
            .replace("cpu_ns 900000000", "cpu_ns 0")
            .replace("pss_kib 24000", "pss_kib 0"),
    )
    .unwrap();
    let without = aggregate(free_bridge.path());

    let with_row = Row::of(&with);
    let without_row = Row::of(&without);

    assert_eq!(
        (with_row.cpu_pct.as_str(), with_row.pss_mib.as_str()),
        ("3.167", "85.94"),
        "the bridge's 0.90 s of CPU and 24000 KiB of PSS did not reach the arm's row.\n\
         with the bridge:      {}\nwith a free one (control): {}",
        with_row.line,
        without_row.line
    );
    assert_eq!(
        (without_row.cpu_pct.as_str(), without_row.pss_mib.as_str()),
        ("1.667", "62.50"),
        "control failed: with a bridge reporting no CPU and no PSS the row should read \
         1.667 % and 62.50 MiB.\n{}",
        without_row.line
    );
    assert_eq!(
        with_row.procs.as_str(),
        "5",
        "the `procs` column must show the bridge as a process this arm runs — N+1 against \
         the tf2 arm's N is a real operational cost §9.3 requires reporting.\n{}",
        with_row.line
    );
    assert_eq!(
        with_row.consumers.as_str(),
        "4",
        "the bridge must not count as a consumer: it is what makes its cost amortize over \
         exactly the consumers it serves.\n{}",
        with_row.line
    );
}

/// A `.out` cut off after its histograms — the shape a killed process, a full
/// disk or a driver that stopped waiting leaves behind.
///
/// It is a *consumer* file with everything except the two cost lines, so the
/// only thing that distinguishes it from a healthy one is the thing the
/// aggregator used to read as a zero.
fn truncated_out() -> String {
    let full = consumer_out(1);
    full.lines()
        .filter(|l| !l.starts_with("cpu_ns") && !l.starts_with("pss_kib"))
        .map(|l| format!("{l}\n"))
        .collect()
}

/// **Gate.** A process file missing a cost line is refused, by name.
///
/// `parse_proc` cannot tell "field absent" from "field zero", and the
/// difference is not academic: against this project's own raw run, a bridge
/// `.out` truncated after its histograms aggregates to 0.146 %/consumer where
/// the truth is 0.847, and to a PSS that puts `tf_tree.processes` on the
/// winning side of the one comparison it loses. Nothing else in the output
/// changes — `procs` still reads 5, `fail%` still reads 0.00, the exit status
/// is still 0 — so the flattering row is indistinguishable from a real one.
///
/// Mutant: in `parse_proc`, drop the `bail!` loop over the two fields (return
/// `Ok(p)` directly) and give the fold `p.cpu_ns.unwrap_or(0)` /
/// `p.pss_kib.unwrap_or(0)`. That is the pre-fix behaviour exactly; this test
/// fails because the run succeeds.
#[test]
fn a_process_file_missing_its_cost_lines_is_refused_by_name() {
    let scratch = Scratch::new("truncated");
    write_four_arms(scratch.path());
    std::fs::write(
        scratch.path().join("tf_tree.processes.2.out"),
        truncated_out(),
    )
    .unwrap();

    let err = refusal(scratch.path());
    assert!(
        err.contains("tf_tree.processes.2.out"),
        "the refusal must name the file an operator has to go and look at:\n{err}"
    );
    assert!(
        err.contains("cpu_ns"),
        "the refusal must name the missing field:\n{err}"
    );
}

/// **Gate.** A `tf_tree.processes` arm with no bridge process in it is refused.
///
/// This is the structural invariant the whole fairness argument rests on: the
/// arm's claim is that *one* process pays the deserialization for all of them,
/// and the accounting that makes it fair is that that process reports
/// `consumers 0` so its cost lands in the numerator and not the denominator. An
/// arm that lost its bridge — a crash, a rendezvous it never published, a
/// driver edited to stop launching it — prints a **better** row than the real
/// one, with no column showing the difference.
///
/// `tf2.processes` ends in the same word and must NOT be subject to this: it
/// has no bridge by construction, which is what the control below asserts.
///
/// Mutant: in `check_structure`, `if !arm.is_bridge_and_attach() { continue; }`
/// → `if true { continue; }`. Both refusals in this file's gate tests stop
/// firing and every other assertion here still passes.
#[test]
fn a_bridge_and_attach_arm_with_no_bridge_process_is_refused() {
    let scratch = Scratch::new("no-bridge");
    write_four_arms(scratch.path());
    // The bridge process replaced by an ordinary consumer: still five
    // processes, still four consumers... plus a fifth nobody accounts for.
    std::fs::write(
        scratch.path().join("tf_tree.processes.0.out"),
        consumer_out(1),
    )
    .unwrap();

    let err = refusal(scratch.path());
    assert!(
        err.contains("tf_tree.processes") && err.contains("consumers 0"),
        "the refusal must name the arm and the missing `consumers 0` process:\n{err}"
    );

    // Control: `tf2.processes` has no bridge and never will. The unmodified
    // fixture must aggregate cleanly, or this gate is refusing the wrong arm.
    let control = Scratch::new("no-bridge-control");
    write_four_arms(control.path());
    let text = aggregate(control.path());
    assert!(
        text.lines().any(|l| l.starts_with("tf2.processes")),
        "the tf2 arm must not be subject to the bridge invariant:\n{text}"
    );
}

/// **Gate.** A bridge that received nothing is refused.
///
/// `bridge_transforms` has been parsed and written to `results.json` since this
/// arm existed and gated *nothing*, while `bench_consumer`'s own comment beside
/// the counter said it exists so that "a run whose bridge dropped everything
/// would otherwise report beautiful latencies for an empty arena". This makes
/// that comment true. It is not hypothetical: the authority-attribution defect
/// in `docs/benchmarks/tf2.md` produced exactly this shape, and it was caught
/// only because it also failed every lookup.
///
/// Mutant: in `check_structure`, `if transforms == 0` → `if false`. This test
/// fails; nothing else does.
#[test]
fn a_bridge_that_received_no_transforms_is_refused() {
    let scratch = Scratch::new("empty-bridge");
    write_four_arms(scratch.path());
    std::fs::write(
        scratch.path().join("tf_tree.processes.0.out"),
        bridge_out().replace("bridge_transforms 16373", "bridge_transforms 0"),
    )
    .unwrap();

    let err = refusal(scratch.path());
    assert!(
        err.contains("tf_tree.processes") && err.contains("0 transforms"),
        "the refusal must name the arm and what its bridge received:\n{err}"
    );
}

/// **Gate.** An arm that performed no lookups at all is flagged, not praised.
///
/// The `<-- FAILING` flag's own comment says it exists so the table cannot
/// print the best latencies for an empty row, and for the emptiest row possible
/// it did not fire: with no lookups `fail_pct` is `NaN`, and `NaN > 5.0` is
/// `false`. An arm whose consumers all timed out on `--attach-timeout` reaches
/// that state — header-only `.out` files, zero everything, `service_p50_ns: 0`
/// recorded under `lower_is_better` in `results.json`.
///
/// Mutant: `let flag = if fail_pct > 5.0` in `aggregate` — the pre-fix
/// spelling. The row prints `0.00` in every latency column with no flag and
/// this test fails.
#[test]
fn an_arm_that_performed_no_lookups_is_flagged_failing() {
    let scratch = Scratch::new("no-lookups");
    write_four_arms(scratch.path());
    // Four processes that started, paid CPU and memory, and answered nothing.
    for i in 0..4 {
        std::fs::write(
            scratch.path().join(format!("tf2.processes.{i}.out")),
            "warmup_s 3.0\nmeasured_s 15.0\nconsumers 1\ncpu_ns 250000000\npss_kib 16000\n",
        )
        .unwrap();
    }

    let text = aggregate(scratch.path());
    let row = text
        .lines()
        .find(|l| l.starts_with("tf2.processes"))
        .unwrap_or_else(|| panic!("no tf2.processes row in\n{text}"));
    assert!(
        row.contains("<-- FAILING"),
        "an arm with zero lookups printed 0.00 in every latency column and no flag. \
         `NaN > 5.0` is false, so the guard against exactly this did not fire.\n{row}"
    );
}

// ---------------------------------------------------------------------------
// The run file states the build of the ARMS, not of the aggregator
// ---------------------------------------------------------------------------

/// A `ros/build.sh` output tree, with the two CMake caches `--ros-out` reads.
///
/// Written by hand rather than by running `ros/build.sh`: that needs the
/// container, and what is under test is the reading, not the building. The
/// format is CMake's own (`NAME:TYPE=VALUE`) and was checked against the real
/// `target/ros/build/tf_tree_bench_ros/CMakeCache.txt` this repository produces.
/// The decoys are deliberate — an `-ADVANCED` twin and a comment line containing
/// the variable's name are both present in a real cache, and a parser that split
/// on `=` alone would return one of them.
fn write_ros_out(dir: &Path, build_type: &str, prebuilt_profile: &str) {
    let bench = dir.join("build/tf_tree_bench_ros");
    let pkg = dir.join("tf_tree-build");
    std::fs::create_dir_all(&bench).unwrap();
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        bench.join("CMakeCache.txt"),
        format!(
            "# This is the CMakeCache file.\n\
             //Choose the type of build, options are: None Debug Release\n\
             CMAKE_BUILD_TYPE:STRING={build_type}\n\
             CMAKE_BUILD_TYPE-ADVANCED:INTERNAL=1\n\
             CMAKE_CXX_STANDARD:STRING=17\n"
        ),
    )
    .unwrap();
    std::fs::write(
        pkg.join("CMakeCache.txt"),
        format!(
            "# This is the CMakeCache file.\n\
             CMAKE_BUILD_TYPE:STRING={build_type}\n\
             //Directory holding a pre-built libtf_tree_c.a\n\
             TF_TREE_PREBUILT_DIR:PATH=/work/target/tf2-docker/{prebuilt_profile}\n"
        ),
    )
    .unwrap();
}

/// A cargo profile directory guaranteed to differ from the one *this* test
/// binary was built into.
///
/// The same rule as `runstore.rs`'s `differing_value`, which this mirrors rather
/// than imports (it is private to that crate's unit tests): **a test about build
/// facts must not hardcode one.** Hardcoding `embedder` here would assert
/// "the arms' profile is not the aggregator's" and pass — until somebody runs
/// the suite under `--profile embedder`, where the two would agree and the
/// assertion would be vacuous rather than failing.
fn a_profile_this_build_is_not() -> &'static str {
    if tf_tree_bench::embed::PROFILE_DIR == "embedder" {
        "release"
    } else {
        "embedder"
    }
}

fn run_with(dir: &Path, extra: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_dds_report"))
        .args(["aggregate", "--dir"])
        .arg(dir)
        .args(extra)
        .output()
        .expect("spawning dds_report")
}

/// **Gate.** `--json` without `--ros-out` writes nothing.
///
/// A dds run file is the schema `bench_ab` compares two of, and the
/// `build_profile` `Run::begin` collects here is the *aggregator's* — this
/// binary parses `.out` files, it measures nothing. Writing the file without the
/// arms' build in it is what lets a Release run and a `-O0` run compare cleanly,
/// and it is worse than a missing file because the missing fact reads as
/// "predates the gate" forever after.
///
/// Mutant (applied, observed): change the `(Some(path), None)` arm of
/// `aggregate`'s `match` to `(Some(_), None) => None`, i.e. write the file with
/// no arms' build. This test fails on the exit-status assertion — the run
/// succeeds and `results.json` appears.
#[test]
fn a_run_file_without_the_arms_build_is_refused_rather_than_written() {
    let scratch = Scratch::new("json-needs-ros-out");
    write_four_arms(scratch.path());
    let json = scratch.path().join("results.json");

    let out = run_with(scratch.path(), &["--json", json.to_str().unwrap()]);
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        !out.status.success(),
        "aggregate wrote a run file with no statement of how the arms were built"
    );
    assert!(
        stderr.contains("--ros-out"),
        "the refusal must name the flag that fixes it:\n{stderr}"
    );
    assert!(
        !json.exists(),
        "the refusal still left a run file behind: {}",
        json.display()
    );
}

/// **Gate.** The three `dds_*` facts describe the arms, and are read rather than
/// assumed.
///
/// The fixture's profile directory is one this build is not, so a version of
/// `MeasuredBuild::read` that reported the aggregator's own profile would fail
/// here rather than coincidentally agree.
///
/// Mutant (applied, observed): in `MeasuredBuild::read`, replace `c_abi_profile`
/// with `tf_tree_bench::embed::PROFILE_DIR.to_owned()` — the plausible
/// "the archive is built beside us" assumption. This test fails with
/// `left: "debug", right: "embedder"` under `just test`.
#[test]
fn the_run_file_records_the_arms_build_and_not_the_aggregators() {
    let scratch = Scratch::new("arms-build");
    write_four_arms(scratch.path());
    let ros_out = scratch.path().join("ros");
    let profile = a_profile_this_build_is_not();
    write_ros_out(&ros_out, "RelWithDebInfo", profile);
    let json = scratch.path().join("results.json");

    let out = run_with(
        scratch.path(),
        &[
            "--ros-out",
            ros_out.to_str().unwrap(),
            "--json",
            json.to_str().unwrap(),
        ],
    );
    assert!(
        out.status.success(),
        "aggregate failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let text = std::fs::read_to_string(&json).unwrap();
    let run = tf_tree_bench::runstore::Run::parse(&text).expect("the run file parses");
    assert_eq!(run.fact("dds_cxx_build_type"), Some("RelWithDebInfo"));
    assert_eq!(run.fact("dds_c_abi_profile"), Some(profile));
    assert_ne!(
        run.fact("dds_c_abi_profile"),
        run.fact("build_profile"),
        "the arms' profile and the aggregator's are different facts and the file must \
         carry both"
    );

    // Computed from the manifest at test time, never spelled: `[profile.*]`
    // sections inherit, and what `embedder` means is a property of the manifest
    // in front of us rather than of what it said when this test was written.
    let manifest =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml"))
            .unwrap();
    assert_eq!(
        run.fact("dds_c_abi_lto"),
        Some(tf_tree_bench::embed::lto_for_profile_dir(&manifest, profile).as_str())
    );

    // And the header line an operator reads carries the same three.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("arms built: C++ RelWithDebInfo"),
        "the table must say what it is a table of:\n{stdout}"
    );
}

/// Aggregate the same four arms against a given `ros/build.sh` output tree.
fn run_file_for(scratch: &Scratch, tag: &str, build_type: &str, profile: &str) -> RunFile {
    let ros_out = scratch.path().join(format!("ros-{tag}"));
    write_ros_out(&ros_out, build_type, profile);
    let json = scratch.path().join(format!("results-{tag}.json"));
    let out = run_with(
        scratch.path(),
        &[
            "--ros-out",
            ros_out.to_str().unwrap(),
            "--json",
            json.to_str().unwrap(),
        ],
    );
    assert!(
        out.status.success(),
        "aggregate failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    tf_tree_bench::runstore::Run::parse(&std::fs::read_to_string(&json).unwrap()).unwrap()
}

type RunFile = tf_tree_bench::runstore::Run;

/// **Gate.** Two dds runs whose arms were built differently refuse each other.
///
/// This is the whole point of the two above: the facts exist so that
/// `runstore::diff` — which `bench_ab` is a thin shell over — declines to print
/// verdicts over arms built two ways. Before the `dds_*` keys existed this pair
/// compared cleanly and printed a full per-row table, because the only build
/// facts in the file described the parser, and the parser was `--release` in
/// both.
///
/// **One axis at a time**, so each key carries its own weight rather than
/// hiding behind another. The exception is stated rather than papered over:
/// `dds_c_abi_lto` is a function of `dds_c_abi_profile` and one manifest, so
/// they cannot be varied independently here — that pair is exactly the
/// `build_profile` / `build_lto` relationship, and `build_lto` earns its place
/// against a *manifest* edit, which no fixture in this file can make.
///
/// Mutant (applied, observed): delete all three `dds_*` keys from
/// `runstore::BUILD_CRITICAL_FACTS`. Both cases below fail on `!comparable()`
/// and `render` prints the table.
///
/// Mutant (applied, observed, and the reason this test varies one axis at a
/// time): delete only `"dds_cxx_build_type"` and `"dds_c_abi_profile"`, leaving
/// `dds_c_abi_lto`. An earlier version of this test varied both axes together
/// (`Release`/`release` against `Debug`/`debug`) and **passed** under that
/// mutation, because the surviving key moved too. It now fails.
#[test]
fn two_dds_runs_whose_arms_were_built_differently_do_not_compare() {
    let scratch = Scratch::new("cross-build");
    write_four_arms(scratch.path());

    let base = run_file_for(&scratch, "base", "Release", "release");
    for (tag, other) in [
        (
            "the C++ arms' build type",
            run_file_for(&scratch, "cxx", "Debug", "release"),
        ),
        (
            "the archive the tf_tree arms linked",
            run_file_for(&scratch, "abi", "Release", "debug"),
        ),
    ] {
        let d = tf_tree_bench::runstore::diff(&base, &other);
        assert!(
            !d.comparable(),
            "{tag} changed and the two runs compared cleanly anyway; \
             mismatch was {:?}",
            d.build_mismatch
        );
        let text = tf_tree_bench::runstore::render(&d);
        assert!(text.contains("REFUSED"), "{tag}:\n{text}");
        assert!(
            !text.contains("row / metric"),
            "{tag}: the delta table must not be printed for an incomparable pair:\n{text}"
        );
    }
}

/// The `tf_tree.processes` row, split into the columns `aggregate` prints.
struct Row {
    line: String,
    procs: String,
    consumers: String,
    cpu_pct: String,
    pss_mib: String,
}

impl Row {
    /// Column order is `label procs cons | p50 p99 p99.9 | cyc | cpu pss fail`.
    /// Split on whitespace rather than matched as a substring so a number
    /// landing in the wrong column fails instead of being found somewhere.
    fn of(text: &str) -> Row {
        let line = text
            .lines()
            .find(|l| l.starts_with("tf_tree.processes"))
            .unwrap_or_else(|| panic!("no tf_tree.processes row in\n{text}"))
            .to_owned();
        let f: Vec<&str> = line.split_whitespace().collect();
        assert!(
            f.len() >= 13,
            "the row has {} columns, not the 13 this test reads: {line}",
            f.len()
        );
        Row {
            procs: f[1].to_owned(),
            consumers: f[2].to_owned(),
            cpu_pct: f[10].to_owned(),
            pss_mib: f[11].to_owned(),
            line,
        }
    }
}
