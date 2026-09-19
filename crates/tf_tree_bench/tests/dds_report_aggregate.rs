//! The pin `docs/decisions/0015` step 6 claimed existed: two assertions, the four arm labels are rendered as rows
//! and no "NOT MEASURED" text appears in any spelling.

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

/// The other half: what the tool says when it refuses — non-zero exit **and**
/// nothing resembling a table on stdout, because a diagnostic printed after
/// four plausible rows is a diagnostic somebody quotes the rows from.
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
#[test]
fn the_report_states_all_four_arms_and_declares_none_unmeasurable() {
    let scratch = Scratch::new("four-arms");
    write_four_arms(scratch.path());
    let text = aggregate(scratch.path());

    // A row, not a mention: the footer explains `tf_tree.processes` in prose, so match a line starting with the label.
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

    // The `MISSING_ARM` paragraph, in any spelling. Reintroducing the words
    // while the arm exists would be the report disclaiming a measurement it
    // made; reintroducing them because the arm broke is what the assertion
    // above catches first.
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

/// The bridge process's cost is **in** the arm: it contributes `cpu_ns` and `pss_kib` while reporting `consumers 0`.
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
fn truncated_out() -> String {
    let full = consumer_out(1);
    full.lines()
        .filter(|l| !l.starts_with("cpu_ns") && !l.starts_with("pss_kib"))
        .map(|l| format!("{l}\n"))
        .collect()
}

/// **Gate.** A process file missing a cost line is refused, by name.
///
/// Mutant: drop the `bail!` loop over the two cost fields in `parse_proc`; the run then succeeds.
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
/// `tf2.processes` ends in the same word and must NOT be subject to this: it
/// has no bridge by construction, which is what the control below asserts.
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

    // Control: the unmodified fixture must aggregate cleanly, or this gate is
    // refusing the wrong arm.
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

/// **Gate.** Two dds runs whose arms were built differently refuse each other (`runstore::diff` over the `dds_*` facts).
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
