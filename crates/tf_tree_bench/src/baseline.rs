//! The benchmark artifact as a **regression gate** (`docs/PHASE5.md` §10).
//!
//! §10's CI list ends with "the benchmark artifact as a regression gate". §9
//! built the artifact and made it refuse to over-claim; this is the other half —
//! a committed `results.json` and a comparison that fails loudly when the report
//! this build produces is worse than it.
//!
//! # What is compared, and what is deliberately not
//!
//! A benchmark report is mostly a description of the machine that produced it.
//! Comparing all of it across machines produces a gate that fails for the CPU
//! model and passes for a doubled p99, which is worse than no gate. So the
//! comparison is over exactly the parts of the artifact that are **claims about
//! the code**:
//!
//! | Compared | Ignored |
//! |---|---|
//! | `schema` | `generated_utc`, `git_commit`, `git_dirty` |
//! | [`PORTABLE_FACTS`] — the build's identity, not the host's | `cpu_model`, `physical_cores`, `logical_cpus`, `kernel`, governor, THP, load |
//! | the set of row ids, and of `where_we_are_worse` ids | every row's `reason`, `note` and `reproduce` prose |
//! | each row's *status*, one-directionally (see below) | `host_fitness` |
//! | directional metric values inside rows both sides call `measured` | metrics whose `drift` is `informational` |
//!
//! The prose is ignored on purpose. `reason` strings embed measured host facts
//! ("4 physical cores for 16 consumers"), so comparing them would make the gate
//! a host check wearing a regression check's clothes.
//!
//! # Status is compared in one direction only
//!
//! A row that was [`Status::Measured`] in the baseline and is not one now is a
//! **failure**: a claim was withdrawn, either because the code stopped being
//! measurable or because the build lost a feature. That is exactly the silent
//! rot §9.3 is written against — an artifact quietly shrinking to the rows that
//! still look good.
//!
//! A row that is `measured` now and was not in the baseline is **not** a
//! failure. Running the same commit on a bigger, quieter machine is supposed to
//! fill rows in, and a gate that punished that would be a gate nobody could run
//! anywhere but the one host the baseline came from. It is reported as a
//! `new claim` note, because it does mean the committed baseline is stale in our
//! favour and should be regenerated.
//!
//! # Whose tolerance
//!
//! The **baseline's**, not the running build's. The baseline is the committed
//! contract; regenerating it is a reviewed diff. Reading the tolerance from the
//! live build would let a one-line edit widen the gate and a green run hide the
//! regression it was widened for, in the same commit.
//!
//! # Why `serde_json` here and hand-rolled JSON in [`crate::report`]
//!
//! Not an inconsistency. Writing is hand-rolled because the schema is a
//! compatibility surface and a `#[derive]` would let a field rename happen as a
//! side effect (see [`crate::report`]'s module docs). Reading somebody else's
//! committed file is the opposite problem: a hand-rolled parser is where the
//! bugs would be, and a parser bug here fails *open*. `serde_json` is already in
//! this workspace's lockfile through `criterion`, and this crate is
//! `publish = false`, so no consumer pays for it.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

use crate::report::{Drift, Metric, Report, Status};

/// The committed baseline, relative to the workspace root.
///
/// Named once so the `justfile` recipes, the failure message and the test that
/// reads it cannot disagree about where it lives.
pub const BASELINE_PATH: &str = "crates/tf_tree_bench/baseline/results.json";

/// Provenance keys that describe the **artifact**, and must match.
///
/// Every one of these changes what the numbers mean rather than where they were
/// taken:
///
/// * `format_version` / `layout_hash` — a different arena. A report about arena
///   v3 says nothing about v4, and §1's whole point is that the break is
///   deliberate, so it should be a deliberate baseline regeneration too.
/// * `interp_policy` — a different interpolator is a different `max_deviation`.
/// * `build_profile` — a debug latency number is not comparable to a release
///   one, and `Fitness` already refuses to call debug timings claims.
/// * `counters_feature` / `shm_feature` / `tf2_feature` — each adds rows or
///   changes the read path. `just bench-report-shm` therefore does **not**
///   check against the default build's baseline; it needs its own, and there is
///   none committed because on this host it produces no additional claim.
///
/// `target` is **not** here. The baseline is committed from one architecture and
/// `docs/PHASE5.md` §10 wants the suites on `x86_64` *and* `aarch64`; making the
/// arch a hard mismatch would mean the aarch64 job could never run this gate at
/// all. Cross-arch value drift is instead absorbed by the per-metric tolerances,
/// which is what they are for.
pub const PORTABLE_FACTS: &[&str] = &[
    "format_version",
    "layout_hash",
    "interp_policy",
    "build_profile",
    "counters_feature",
    "shm_feature",
    "tf2_feature",
];

/// The outcome of comparing a fresh report against a committed baseline.
#[derive(Debug, Clone, Default)]
pub struct Comparison {
    /// One line per regression. Non-empty means the gate fails.
    pub failures: Vec<String>,
    /// One line per row that became a claim the baseline does not carry. Not a
    /// failure; the baseline is stale in our favour.
    pub notes: Vec<String>,
    /// Directional metrics that were compared and held.
    pub checked: usize,
}

impl Comparison {
    /// Whether the gate passes.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.failures.is_empty()
    }

    /// A clean result that compared **no** directional metric at all.
    ///
    /// The distinction `bench_report` refuses on: "0 failures" is also what a
    /// gate that has quietly stopped comparing prints, and a regression gate
    /// whose baseline no longer shares a single directional metric with this
    /// build is green for the rest of its life.
    ///
    /// A predicate rather than an `if` inside the binary because reaching that
    /// branch through the binary means assembling a whole benchmark report
    /// first — minutes, in a suite that runs on every branch. Here it is one
    /// unit test; `just bench-check` covers the wiring by running the binary
    /// for real against the committed baseline.
    #[must_use]
    pub fn compared_nothing(&self) -> bool {
        self.passed() && self.checked == 0
    }
}

/// One metric as the baseline file records it.
#[derive(Debug, Clone, Copy)]
struct BaselineMetric {
    value: f64,
    drift: Drift,
    tolerance: f64,
}

/// Read and compare a committed baseline against `current`.
///
/// # Errors
///
/// If the file cannot be read or is not a `results.json` this tool wrote. A
/// malformed baseline is an error rather than a failed comparison: "the gate
/// could not run" and "the gate ran and found a regression" are different
/// answers, and collapsing them is how a gate ends up permanently green.
pub fn check_file(path: &Path, current: &Report) -> Result<Comparison> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading the committed baseline {}", path.display()))?;
    let baseline: Value = serde_json::from_str(&text)
        .with_context(|| format!("parsing {} as JSON", path.display()))?;
    compare(&baseline, current)
}

/// Compare a parsed baseline document against a fresh report.
///
/// # Errors
///
/// If the document is not shaped like a `results.json` at all — a missing
/// `rows` array, say. Schema *mismatch* is a comparison failure rather than an
/// error, because it is a real regression signal: the artifact's contract moved
/// without the baseline moving with it.
pub fn compare(baseline: &Value, current: &Report) -> Result<Comparison> {
    let mut out = Comparison::default();

    let b_schema = baseline
        .get("schema")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("the baseline has no top-level `schema` string"))?;
    if b_schema != crate::report::SCHEMA {
        out.failures.push(format!(
            "schema moved: the baseline is `{b_schema}`, this build emits `{}`. Every \
             comparison below would be over a different document, so regenerate the \
             baseline deliberately (`just bench-baseline-update`) and review the diff",
            crate::report::SCHEMA
        ));
        // Everything after this reads fields whose meaning just changed.
        return Ok(out);
    }

    let b_prov = baseline
        .get("provenance")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("the baseline has no `provenance` object"))?;
    for key in PORTABLE_FACTS {
        let want = b_prov.get(*key).and_then(Value::as_str);
        let got = current.provenance.get(key);
        match (want, got) {
            (Some(w), Some(g)) if w == g => {}
            (Some(w), Some(g)) => out.failures.push(format!(
                "provenance `{key}`: baseline `{w}`, this build `{g}`. This is not a \
                 property of the host — it changes what the numbers describe"
            )),
            (Some(w), None) => out.failures.push(format!(
                "provenance `{key}` is `{w}` in the baseline and absent from this report"
            )),
            (None, _) => out.failures.push(format!(
                "the baseline records no `{key}`, so it cannot be checked; it predates \
                 this gate and must be regenerated"
            )),
        }
    }

    let b_rows = baseline
        .get("rows")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("the baseline has no `rows` array"))?;
    let b_ids: Vec<&str> = b_rows
        .iter()
        .filter_map(|r| r.get("id").and_then(Value::as_str))
        .collect();
    let c_ids: Vec<&str> = current.rows.iter().map(|r| r.id).collect();
    diff_ids("row", &b_ids, &c_ids, &mut out);

    let b_worse: Vec<&str> = baseline
        .get("where_we_are_worse")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("the baseline has no `where_we_are_worse` array"))?
        .iter()
        .filter_map(|w| w.get("id").and_then(Value::as_str))
        .collect();
    let c_worse: Vec<&str> = current.worse.iter().map(|w| w.id).collect();
    diff_ids("where_we_are_worse", &b_worse, &c_worse, &mut out);

    for b_row in b_rows {
        let Some(id) = b_row.get("id").and_then(Value::as_str) else {
            bail!("a baseline row has no `id`");
        };
        let Some(cur) = current.rows.iter().find(|r| r.id == id) else {
            continue; // Already reported by `diff_ids`.
        };
        let b_status = b_row
            .get("status")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("baseline row `{id}` has no `status`"))?;

        if b_status == Status::Measured.as_str() && cur.status != Status::Measured {
            out.failures.push(format!(
                "row `{id}` was `measured` in the baseline and is `{}` now — a claim was \
                 withdrawn. This build says: {}",
                cur.status.as_str(),
                if cur.reason.trim().is_empty() {
                    "(no reason recorded)"
                } else {
                    cur.reason.trim()
                }
            ));
            continue;
        }
        if b_status != Status::Measured.as_str() && cur.status == Status::Measured {
            out.notes.push(format!(
                "row `{id}` is `measured` here and `{b_status}` in the baseline — this host \
                 can make a claim the committed baseline does not. Not a regression; \
                 regenerate the baseline to gate it"
            ));
            continue;
        }
        if b_status != Status::Measured.as_str() {
            continue; // Neither side claims anything; there is nothing to gate.
        }

        for (column, cur_metrics) in [("tf_tree", &cur.tf_tree), ("tf2", &cur.tf2)] {
            let b_metrics = parse_metrics(b_row, column, id)?;
            compare_column(id, column, &b_metrics, cur_metrics, &mut out);
        }
    }

    Ok(out)
}

/// Report ids present on one side and not the other.
fn diff_ids(what: &str, baseline: &[&str], current: &[&str], out: &mut Comparison) {
    for id in baseline {
        if !current.contains(id) {
            out.failures.push(format!(
                "{what} `{id}` is in the baseline and missing from this report — the \
                 artifact shrank"
            ));
        }
    }
    for id in current {
        if !baseline.contains(id) {
            out.failures.push(format!(
                "{what} `{id}` is in this report and not in the baseline; regenerate the \
                 baseline so the new entry is gated from here on"
            ));
        }
    }
}

/// Pull one column's metrics out of a baseline row.
fn parse_metrics(row: &Value, column: &str, id: &str) -> Result<BTreeMap<String, BaselineMetric>> {
    let obj = row
        .get(column)
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("baseline row `{id}` has no `{column}` object"))?;
    let mut out = BTreeMap::new();
    for (key, v) in obj {
        // A `null` value is how the writer emits a non-finite number. It cannot
        // be compared, so it is carried as NaN and skipped below, rather than
        // silently treated as zero.
        let value = v.get("value").and_then(Value::as_f64).unwrap_or(f64::NAN);
        let drift = match v.get("drift").and_then(Value::as_str) {
            Some("lower_is_better") => Drift::LowerIsBetter,
            Some("higher_is_better") => Drift::HigherIsBetter,
            Some("informational") => Drift::Informational,
            Some(other) => bail!(
                "baseline row `{id}`.{column}.{key} has drift `{other}`, which this build \
                 does not know how to compare"
            ),
            None => bail!(
                "baseline row `{id}`.{column}.{key} records no `drift`; it predates this \
                 gate and must be regenerated"
            ),
        };
        let tolerance = v
            .get("tolerance")
            .and_then(Value::as_f64)
            .unwrap_or(f64::NAN);
        out.insert(
            key.clone(),
            BaselineMetric {
                value,
                drift,
                tolerance,
            },
        );
    }
    Ok(out)
}

/// Compare one column of one row.
fn compare_column(
    id: &str,
    column: &str,
    baseline: &BTreeMap<String, BaselineMetric>,
    current: &[Metric],
    out: &mut Comparison,
) {
    for (key, b) in baseline {
        let Some(c) = current.iter().find(|m| m.key == key) else {
            out.failures.push(format!(
                "row `{id}`.{column} no longer emits `{key}`, which the baseline gates"
            ));
            continue;
        };
        if b.drift != c.drift {
            out.failures.push(format!(
                "row `{id}`.{column}.{key} changed direction: baseline `{}`, this build \
                 `{}`. One of the two is wrong about what an improvement looks like",
                b.drift.as_str(),
                c.drift.as_str()
            ));
            continue;
        }
        if b.drift == Drift::Informational {
            continue;
        }
        if !b.value.is_finite() || !b.tolerance.is_finite() {
            out.failures.push(format!(
                "row `{id}`.{column}.{key} is directional in the baseline but its value or \
                 tolerance is not a finite number, so nothing can be compared"
            ));
            continue;
        }
        if !c.value.is_finite() {
            out.failures.push(format!(
                "row `{id}`.{column}.{key} is {} here against a baseline of {}",
                c.value, b.value
            ));
            continue;
        }
        // The slack is a fraction of the baseline's *magnitude*, so a negative
        // baseline widens the band in the same direction a positive one does
        // instead of inverting it.
        let slack = b.value.abs() * b.tolerance;
        let (bad, bound) = match b.drift {
            Drift::LowerIsBetter => (c.value > b.value + slack, b.value + slack),
            Drift::HigherIsBetter => (c.value < b.value - slack, b.value - slack),
            Drift::Informational => (false, 0.0),
        };
        if bad {
            let pct = if b.value == 0.0 {
                String::from("(baseline is zero)")
            } else {
                format!("{:+.1}%", (c.value - b.value) / b.value.abs() * 100.0)
            };
            out.failures.push(format!(
                "row `{id}`.{column}.{key} regressed: {} {} against a baseline of {} \
                 ({pct}), past the {:.0}% the baseline allows (bound {bound})",
                c.value,
                c.unit,
                b.value,
                b.tolerance * 100.0
            ));
        } else {
            out.checked += 1;
        }
    }
    for c in current {
        if !baseline.contains_key(c.key) && c.drift != Drift::Informational {
            out.failures.push(format!(
                "row `{id}`.{column} emits a new directional metric `{}` that the baseline \
                 does not gate; regenerate the baseline",
                c.key
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;
    use crate::report::{Fitness, Provenance, Row};

    /// The provenance every fixture report carries: the seven artifact facts
    /// **and** one host fact, so a test can tell the two apart.
    const FIXTURE_FACTS: &[(&str, &str)] = &[
        ("format_version", "3"),
        ("layout_hash", "0x3D104195"),
        ("interp_policy", "LerpSlerp (tf2's policy)"),
        ("build_profile", "release"),
        ("counters_feature", "true"),
        ("shm_feature", "false"),
        ("tf2_feature", "false"),
        ("cpu_model", "a CPU the baseline was taken on"),
    ];

    /// A one-row report whose row is `measured` with one directional metric —
    /// the shape the real `differential_agreement` row has.
    fn report_with(value: f64, drift_hi: bool) -> Report {
        report_tuned(value, drift_hi, 0.10)
    }

    /// [`report_with`], with the metric's tolerance spelled out.
    ///
    /// Separate because the two sides of a comparison must be able to disagree
    /// about the tolerance: which of them the gate reads is a property this
    /// module claims in its docs, and a fixture that hard-codes 0.10 on both
    /// sides cannot tell the two apart.
    fn report_tuned(value: f64, drift_hi: bool, tolerance: f64) -> Report {
        let m = crate::report::Metric::new("max_deviation", value, "rad or m");
        let m = if drift_hi {
            m.higher_is_better(tolerance)
        } else {
            m.lower_is_better(tolerance)
        };
        Report {
            // **Spelled out, not derived from `PORTABLE_FACTS`.** An earlier
            // revision built this list by mapping over that constant, which made
            // the fixture move with the thing it was supposed to pin: adding
            // `cpu_model` to `PORTABLE_FACTS` also added it to both sides of
            // every comparison, so the test that says host facts are ignored
            // passed with `cpu_model` in the compared set. It has to be a fixed
            // list for the mutant to be fatal.
            provenance: Provenance {
                facts: FIXTURE_FACTS
                    .iter()
                    .map(|(k, v)| crate::report::Fact {
                        key: k,
                        value: (*v).to_owned(),
                    })
                    .collect(),
            },
            fitness: Fitness::probe(1),
            warmup_discarded_s: 0.0,
            rows: vec![Row {
                id: "differential_agreement",
                title: "t",
                note: String::new(),
                timing_sensitive: false,
                needs_n_cores: false,
                status: Status::Measured,
                reason: String::new(),
                reproduce: "just bench-report",
                tf_tree: vec![m],
                tf2: Vec::new(),
            }],
            worse: Vec::new(),
        }
    }

    /// The baseline document for [`report_with`], rendered by the real writer so
    /// the test can never agree with a shape the tool does not emit.
    fn baseline_of(r: &Report) -> Value {
        serde_json::from_str(&r.to_json()).expect("the writer emits valid JSON")
    }

    /// A report identical to its baseline passes, and reports having actually
    /// compared something.
    ///
    /// `checked` is the load-bearing half: a comparison that silently matched
    /// nothing would also produce zero failures, which is the way this kind of
    /// gate usually dies.
    ///
    /// Mutant (applied, confirmed fatal): make `compare_column`'s outer loop
    /// `for (key, b) in baseline.iter().filter(|(_, b)| false)` — no failures,
    /// but `checked` is 0 and this fails on the second assertion.
    #[test]
    fn an_identical_report_passes_and_compares_something() {
        let r = report_with(2.5e-16, false);
        let c = compare(&baseline_of(&r), &r).expect("well-formed baseline");
        assert!(c.passed(), "identical report failed: {:?}", c.failures);
        assert_eq!(c.checked, 1, "nothing was actually compared");
    }

    /// Growth inside the tolerance passes; growth past it fails and the message
    /// names the metric.
    ///
    /// Mutant (applied, confirmed fatal): change `LowerIsBetter`'s test to
    /// `c.value > b.value + slack * 100.0` — the 2x case then passes and the
    /// second half of this test fails.
    #[test]
    fn growth_past_the_baselines_tolerance_is_a_regression() {
        let base = baseline_of(&report_with(100.0, false));

        let within = compare(&base, &report_with(109.0, false)).expect("baseline");
        assert!(within.passed(), "9% growth under a 10% bound: {within:?}");

        let over = compare(&base, &report_with(200.0, false)).expect("baseline");
        assert!(!over.passed(), "2x growth passed a 10% bound");
        assert!(
            over.failures[0].contains("max_deviation") && over.failures[0].contains("+100.0%"),
            "the failure must name the metric and the size: {:?}",
            over.failures
        );
    }

    /// A metric that changes direction is a failure rather than a silently
    /// inverted comparison.
    ///
    /// Without this, flipping `lower_is_better` to `higher_is_better` on a
    /// latency would make every future regression *pass*, and the gate would go
    /// green precisely when it mattered.
    ///
    /// Mutant (applied, confirmed fatal): delete the `b.drift != c.drift` arm —
    /// the flipped report then compares 100.0 against a 100.0 baseline as
    /// `higher_is_better`, passes, and this fails.
    #[test]
    fn a_metric_that_changes_direction_fails() {
        let base = baseline_of(&report_with(100.0, false));
        let flipped = compare(&base, &report_with(100.0, true)).expect("baseline");
        assert!(!flipped.passed(), "a direction flip passed");
        assert!(
            flipped.failures[0].contains("changed direction"),
            "got: {:?}",
            flipped.failures
        );
    }

    /// A claim in the baseline that is no longer a claim fails; a claim that is
    /// new here does not.
    ///
    /// Mutant (applied, confirmed fatal): make the withdrawal arm push into
    /// `out.notes` instead of `out.failures` — the first assertion fails.
    #[test]
    fn status_is_compared_in_one_direction_only() {
        let base = baseline_of(&report_with(100.0, false));

        let mut withdrawn = report_with(100.0, false);
        withdrawn.rows[0].status = Status::Unavailable;
        withdrawn.rows[0].reason = "no ROS 2 in this build".to_owned();
        withdrawn.rows[0].tf_tree.clear();
        let c = compare(&base, &withdrawn).expect("baseline");
        assert!(!c.passed(), "a withdrawn claim passed");
        assert!(
            c.failures[0].contains("claim was withdrawn"),
            "got: {:?}",
            c.failures
        );

        // The mirror image: the baseline could not measure it, this host can.
        let mut unavailable_base = report_with(100.0, false);
        unavailable_base.rows[0].status = Status::Unavailable;
        unavailable_base.rows[0].reason = "the baseline host was busy".to_owned();
        unavailable_base.rows[0].tf_tree.clear();
        let c =
            compare(&baseline_of(&unavailable_base), &report_with(100.0, false)).expect("baseline");
        assert!(
            c.passed(),
            "a host that can measure more failed the gate: {:?}",
            c.failures
        );
        assert_eq!(c.notes.len(), 1, "the stale baseline was not reported");
    }

    /// Host facts differ on every machine and must not be compared; build facts
    /// must be.
    ///
    /// This is the rule that decides whether the gate is usable at all. If
    /// `cpu_model` were compared, CI on any runner but the baseline's would fail
    /// for a reason that says nothing about the code.
    ///
    /// Mutant (applied, confirmed fatal): add `"cpu_model"` to
    /// [`PORTABLE_FACTS`] — the first half then fails. It is fatal only because
    /// `FIXTURE_FACTS` is a fixed list; see the note there.
    #[test]
    fn host_facts_are_ignored_and_build_facts_are_not() {
        let r = report_with(100.0, false);
        let base = baseline_of(&r);

        let mut other_host = report_with(100.0, false);
        for f in &mut other_host.provenance.facts {
            if f.key == "cpu_model" {
                f.value = "a completely different CPU".to_owned();
            }
        }
        assert!(
            compare(&base, &other_host).expect("baseline").passed(),
            "a different CPU model failed the gate"
        );

        let mut other_layout = report_with(100.0, false);
        for f in &mut other_layout.provenance.facts {
            if f.key == "layout_hash" {
                f.value = "0xDEADBEEF".to_owned();
            }
        }
        let c = compare(&base, &other_layout).expect("baseline");
        assert!(!c.passed(), "a different arena layout passed the gate");
        assert!(
            c.failures[0].contains("layout_hash"),
            "got: {:?}",
            c.failures
        );
    }

    /// The **set** of rows and of `where_we_are_worse` entries must match, in
    /// both directions.
    ///
    /// Half of this is redundant with [`Report::validate`]'s `REQUIRED_ROWS`
    /// rule and half is not, and the half that is not is the one that matters:
    /// a row present *here* and absent from the baseline is a claim nothing
    /// gates. Nobody notices, because the gate still prints PASS on the rows it
    /// does compare.
    ///
    /// Mutant (applied, confirmed fatal): make `diff_ids` return immediately —
    /// both halves of this test fail, and nothing else in the crate does.
    #[test]
    fn a_row_set_that_does_not_match_the_baseline_fails_in_both_directions() {
        let base = baseline_of(&report_with(100.0, false));

        let mut shrunk = report_with(100.0, false);
        shrunk.rows.clear();
        let c = compare(&base, &shrunk).expect("baseline");
        assert!(!c.passed(), "a report with no rows at all passed");
        assert!(
            c.failures.iter().any(|f| f.contains("the artifact shrank")),
            "got: {:?}",
            c.failures
        );

        // The other direction: a row this build emits that the baseline has
        // never seen. Not a regression in itself — and precisely for that
        // reason the easiest one to leave ungated forever.
        let mut grown = report_with(100.0, false);
        let mut extra = grown.rows[0].clone();
        extra.id = "lookup_latency";
        grown.rows.push(extra);
        let c = compare(&base, &grown).expect("baseline");
        assert!(!c.passed(), "an ungated new row passed");
        assert!(
            c.failures
                .iter()
                .any(|f| f.contains("lookup_latency") && f.contains("regenerate the baseline")),
            "got: {:?}",
            c.failures
        );

        // The same rule for §9.3's list, which is checked by the same function
        // and would otherwise be pinned by nothing at all.
        let mut worse = report_with(100.0, false);
        worse.worse.push(crate::report::Worse {
            id: "attach_latency",
            topic: "attach latency",
            statement: String::from("we are slower to attach"),
            metrics: Vec::new(),
        });
        let c = compare(&base, &worse).expect("baseline");
        assert!(!c.passed(), "an ungated new `worse` entry passed");
        assert!(
            c.failures
                .iter()
                .any(|f| f.contains("where_we_are_worse") && f.contains("attach_latency")),
            "got: {:?}",
            c.failures
        );
    }

    /// A report carrying only `docs/PHASE5.md` §9.2's embedding row, with the
    /// metrics [`crate::embed::Run`] really builds.
    fn embedding_report(out_of_crate_ns: f64, in_crate_ns: f64) -> Report {
        let ratio = out_of_crate_ns / in_crate_ns;
        let run = crate::embed::Run {
            profile_dir: crate::embed::EMBEDDER_PROFILE.to_owned(),
            source_id: "0123456789abcdef".to_owned(),
            out_of_crate_ns,
            in_crate_ns,
            boundary_ratio: ratio,
            ratio_lo: ratio * 0.999,
            ratio_hi: ratio * 1.001,
            out_of_crate_spread: 0.004,
            in_crate_spread: 0.004,
            rounds: crate::embed::ROUNDS,
            lookups_per_round: 409_600,
        };
        let mut r = report_with(1.0, false);
        r.rows[0].id = "embedding_cross_crate";
        r.rows[0].timing_sensitive = true;
        r.rows[0].tf_tree = run.metrics();
        r
    }

    /// §9.2's 5% on the embedding row, through the real gate and the real
    /// metrics — not a fixture that resembles them.
    ///
    /// The three cases are the three ways this row can move, and the middle one
    /// is why every duration is gated rather than only the ratio §9.2 names.
    ///
    /// Mutant (applied, confirmed fatal): give `boundary_ratio` `tolerance`
    /// `0.10` in `Run::metrics` — case 2 then passes and this test fails on its
    /// assertion. Second mutant (applied, confirmed fatal): make
    /// `out_of_crate_ns` and `in_crate_ns` `Metric::new(..)` (informational) —
    /// case 3 passes and this fails there.
    #[test]
    fn the_embedding_row_is_gated_at_five_percent_in_every_direction() {
        let base = baseline_of(&embedding_report(240.0, 200.0));

        // 1. Both halves move 3% the same way: the ratio is unchanged and no
        //    duration moved past the bound.
        let quiet = compare(&base, &embedding_report(247.2, 206.0)).expect("baseline");
        assert!(quiet.passed(), "a 3% shift in both halves: {quiet:?}");

        // 2. The out-of-crate half loses ground against the in-crate one — the
        //    exact shape a lost `#[inline]` on the cross-crate path has. The
        //    ratio moves 5.3% while neither absolute number moves more than 5%.
        let skewed = compare(&base, &embedding_report(240.0, 190.0)).expect("baseline");
        assert!(!skewed.passed(), "a 5.3% ratio regression passed the gate");
        assert!(
            skewed.failures.iter().any(|f| f.contains("boundary_ratio")),
            "the failure must name the ratio: {:?}",
            skewed.failures
        );

        // 3. Both halves get 6% slower and the ratio does not move at all. A
        //    row gated only on §9.2's quotient would call this clean.
        let slower = compare(&base, &embedding_report(254.4, 212.0)).expect("baseline");
        assert!(!slower.passed(), "6% slower on both sides passed the gate");
        assert!(
            slower
                .failures
                .iter()
                .any(|f| f.contains("out_of_crate_ns"))
                && slower.failures.iter().any(|f| f.contains("in_crate_ns")),
            "both durations must be named: {:?}",
            slower.failures
        );
    }

    /// A directional metric the baseline does not carry fails; an informational
    /// one does not.
    ///
    /// [`Report::validate`] only forces *at least one* directional metric per
    /// row, so a second claim arriving next to an already-gated one is caught
    /// only here. The failure it prevents: `p99_ns` is gated, a later commit
    /// adds `p999_9_ns` with a direction, the baseline is never regenerated, and
    /// the new number is compared against nothing while the gate reports "2
    /// directional metrics held".
    ///
    /// Mutant (applied, confirmed fatal): `if false && !baseline.contains_key(…)`
    /// in `compare_column`'s trailing loop — the first half passes and this
    /// fails.
    #[test]
    fn a_directional_metric_the_baseline_does_not_gate_fails() {
        let base = baseline_of(&report_with(100.0, false));

        let mut ungated = report_with(100.0, false);
        ungated.rows[0]
            .tf_tree
            .push(crate::report::Metric::new("p999_9_ns", 4200.0, "ns").lower_is_better(0.25));
        let c = compare(&base, &ungated).expect("baseline");
        assert!(!c.passed(), "a new directional metric passed ungated");
        assert!(
            c.failures
                .iter()
                .any(|f| f.contains("p999_9_ns") && f.contains("does not gate")),
            "got: {:?}",
            c.failures
        );

        // The control, and the reason the rule is written on `drift` rather
        // than on the key set: most report numbers are context, and a report
        // that could not add one without a baseline regeneration would have its
        // baseline regenerated unread.
        let mut context = report_with(100.0, false);
        context.rows[0]
            .tf_tree
            .push(crate::report::Metric::new("compared", 47922.0, "lookups"));
        let c = compare(&base, &context).expect("baseline");
        assert!(
            c.passed(),
            "a new informational metric failed the gate: {:?}",
            c.failures
        );
    }

    /// The tolerance the comparison uses is the **baseline's**.
    ///
    /// This is the module's stated security property: reading the tolerance
    /// from the running build would let one commit widen the gate and land the
    /// regression it was widened for, with a green run.
    ///
    /// Mutant (applied, confirmed fatal): `let slack = b.value.abs() *
    /// c.tolerance;` in `compare_column` — the widened build's 50% growth then
    /// passes and the first assertion fails.
    #[test]
    fn the_tolerance_is_the_baselines_not_the_running_builds() {
        // Baseline: 100, 10% allowed. This build: 150, and it says 100% is fine.
        let base = baseline_of(&report_tuned(100.0, false, 0.10));
        let widened = report_tuned(150.0, false, 1.00);
        let c = compare(&base, &widened).expect("baseline");
        assert!(
            !c.passed(),
            "the running build widened its own gate and passed"
        );
        assert!(
            c.failures[0].contains("+50.0%") && c.failures[0].contains("10%"),
            "the failure must quote the baseline's tolerance, got: {:?}",
            c.failures
        );

        // The control: a *committed* 100% tolerance really does allow it, so
        // the assertion above is about whose tolerance, not about 150 > 100.
        let generous = baseline_of(&report_tuned(100.0, false, 1.00));
        let c = compare(&generous, &report_tuned(150.0, false, 1.00)).expect("baseline");
        assert!(
            c.passed(),
            "growth inside the baseline's own tolerance failed: {:?}",
            c.failures
        );
    }

    /// A comparison that matched nothing is not a pass.
    ///
    /// The predicate `bench_report` refuses on. It lives here, next to the
    /// thing it is a statement about, because reaching it through the binary
    /// costs a full benchmark assembly — see the note on
    /// [`Comparison::compared_nothing`].
    ///
    /// Mutant (applied, confirmed fatal): make `compared_nothing` return
    /// `false` — the second assertion fails.
    #[test]
    fn a_comparison_that_matched_nothing_is_not_a_pass() {
        let r = report_with(2.5e-16, false);
        let c = compare(&baseline_of(&r), &r).expect("baseline");
        assert!(c.passed() && !c.compared_nothing(), "a real comparison");

        // A baseline whose only directional metric became context: it parses,
        // it compares cleanly, and it gates nothing.
        let mut informational = baseline_of(&r);
        informational["rows"][0]["tf_tree"]["max_deviation"]["drift"] =
            Value::String(String::from("informational"));
        let mut also_context = report_with(2.5e-16, false);
        also_context.rows[0].tf_tree[0].drift = Drift::Informational;
        let c = compare(&informational, &also_context).expect("baseline");
        assert!(
            c.passed() && c.compared_nothing(),
            "a gate that compared nothing did not say so: {c:?}"
        );
    }

    /// A baseline written before this gate existed carries no `drift`, and must
    /// be rejected loudly rather than compared as if every metric were context.
    ///
    /// Mutant (applied, confirmed fatal): make `parse_metrics` default a missing
    /// `drift` to `Drift::Informational` — the call then succeeds, every
    /// comparison is skipped, and `is_err` fails.
    #[test]
    fn a_pre_gate_baseline_is_rejected_not_silently_skipped() {
        let r = report_with(100.0, false);
        let mut base = baseline_of(&r);
        base["rows"][0]["tf_tree"]["max_deviation"]
            .as_object_mut()
            .expect("metric object")
            .remove("drift");
        let err = compare(&base, &r).expect_err("a driftless baseline must not compare clean");
        assert!(
            err.to_string().contains("must be regenerated"),
            "got: {err}"
        );
    }
}
