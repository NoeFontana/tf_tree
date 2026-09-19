//! The benchmark artifact as a **regression gate** (`docs/PHASE5.md` §10): a committed `results.json` and a
//! comparison that fails when this build's report is worse.
//!
//! # What is compared
//!
//! Only claims about the code, not the host:
//!
//! | Compared | Ignored |
//! |---|---|
//! | `schema` | `generated_utc`, `git_commit`, `git_dirty` |
//! | [`PORTABLE_FACTS`] | `cpu_model`, `physical_cores`, `logical_cpus`, `kernel`, governor, THP, load |
//! | the set of row ids, and of `where_we_are_worse` ids | every row's `reason`, `note` and `reproduce` prose |
//! | each row's *status*, one-directionally | `host_fitness`, except to classify a **missing** metric (refusal vs failure) |
//! | directional metric values inside rows both sides call `measured` | metrics whose `drift` is `informational` |
//! | directional metric values inside `where_we_are_worse` entries (`0021` step 4) | those entries' `statement` and `metrics_absent_because` prose |
//!
//! `Measured` in the baseline and not now is a failure (a withdrawn claim, §9.3); the reverse is a `new claim`
//! note. The baseline's tolerance is used, since reading the running build's would let one commit widen the gate.
//!
//! The baseline's: reading the running build's would let one commit widen the gate.
//!
//! # Why `serde_json` here and hand-rolled JSON in [`crate::report`]
//!
//! Writing is hand-rolled so a `#[derive]` cannot rename a field; reading uses `serde_json`, since a parser
//! bug reading a committed file fails open.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

use crate::report::{Drift, Metric, Report, Status};

/// The committed baseline, relative to the workspace root.
pub const BASELINE_PATH: &str = "crates/tf_tree_bench/baseline/results.json";

/// Provenance keys that must match: `format_version`/`layout_hash`, `interp_policy`, `build_profile`, and
/// `counters_feature`/`shm_feature`/`tf2_feature`.
///
/// `target` (the aarch64 job runs the gate) and `build_lto` (see
/// [`crate::runstore::BUILD_CRITICAL_FACTS`]) are absent.
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
    /// One line per row that became a claim the baseline lacks; not a failure.
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

    /// A clean result that compared no directional metric: what a gate that stopped comparing also prints.
    #[must_use]
    pub fn compared_nothing(&self) -> bool {
        self.passed() && self.checked == 0
    }
}

/// Why a metric the baseline records could be missing from this build: the code's
/// doing (FAIL) or the host's (INVALID).
#[derive(Debug, Clone, Copy)]
enum Absence<'a> {
    /// Nothing about the host explains it, so the absence is the code's; every row uses this.
    CodeIsTheOnlyExplanation,
    /// This build's own fitness probe says the host cannot produce the figure,
    /// with the reason it gave.
    HostCannotMeasure(&'a str),
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
/// If the file cannot be read or is not a `results.json` this tool wrote ("could not
/// run" is not "found a regression").
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
/// If the document is not shaped like a `results.json`. Schema mismatch is a comparison
/// failure, not an error.
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
            let what = format!("row `{id}`.{column}");
            let b_metrics = parse_metrics(b_row, column, &what)?;
            compare_metrics(
                &what,
                &b_metrics,
                cur_metrics,
                Absence::CodeIsTheOnlyExplanation,
                &mut out,
            );
        }
    }

    compare_worse(baseline, current, &mut out)?;

    Ok(out)
}

/// Compare the `where_we_are_worse` entries' metrics (`0021` step 4); the caller diffs entry ids.
fn compare_worse(baseline: &Value, current: &Report, out: &mut Comparison) -> Result<()> {
    let Some(b_worse) = baseline.get("where_we_are_worse").and_then(Value::as_array) else {
        return Ok(());
    };
    // Read from this report: `Worse` entries carry no per-metric sensitivity. A passing axis does not prove
    // the absence is the code's, so the message names both possibilities.
    let memory_reasons = if current.fitness.memory_reasons.is_empty() {
        String::from("(no reason recorded, which is itself a bug)")
    } else {
        current.fitness.memory_reasons.join("; ")
    };
    let absence = if current.fitness.fair_for_memory {
        Absence::CodeIsTheOnlyExplanation
    } else {
        Absence::HostCannotMeasure(&memory_reasons)
    };
    for b_entry in b_worse {
        let Some(id) = b_entry.get("id").and_then(Value::as_str) else {
            bail!("a baseline `where_we_are_worse` entry has no `id`");
        };
        let Some(cur) = current.worse.iter().find(|w| w.id == id) else {
            continue; // Already reported by `diff_ids`.
        };
        let what = format!("where_we_are_worse `{id}`");
        let b_metrics = parse_metrics(b_entry, "metrics", &what)?;
        compare_metrics(&what, &b_metrics, &cur.metrics, absence, out);
    }
    Ok(())
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

/// Pull one metric map out of a baseline entry; `what` names it in every message.
fn parse_metrics(
    entry: &Value,
    field: &str,
    what: &str,
) -> Result<BTreeMap<String, BaselineMetric>> {
    let obj = entry
        .get(field)
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("baseline {what} has no `{field}` object"))?;
    let mut out = BTreeMap::new();
    for (key, v) in obj {
        // A `null` is a non-finite number: carried as NaN and skipped, not read as zero.
        let value = v.get("value").and_then(Value::as_f64).unwrap_or(f64::NAN);
        let drift = match v.get("drift").and_then(Value::as_str) {
            Some("lower_is_better") => Drift::LowerIsBetter,
            Some("higher_is_better") => Drift::HigherIsBetter,
            Some("informational") => Drift::Informational,
            Some(other) => bail!(
                "baseline {what}.{key} has drift `{other}`, which this build does not know \
                 how to compare"
            ),
            None => bail!(
                "baseline {what}.{key} records no `drift`; it predates this gate and must \
                 be regenerated"
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

/// Compare one metric map; `absence` decides whether a missing metric is a failure or a refusal.
fn compare_metrics(
    what: &str,
    baseline: &BTreeMap<String, BaselineMetric>,
    current: &[Metric],
    absence: Absence<'_>,
    out: &mut Comparison,
) {
    for (key, b) in baseline {
        let Some(c) = current.iter().find(|m| m.key == key) else {
            // The baseline's drift decides the wording: informational keys are context, not gated.
            let gated = if b.drift == Drift::Informational {
                "which the baseline records as context rather than gating, so the artifact \
                 is smaller than the baseline describes"
            } else {
                "which the baseline gates"
            };
            match absence {
                Absence::HostCannotMeasure(why) => out.notes.push(format!(
                    "{what} no longer emits `{key}`, {gated} — and this build's own fitness \
                     probe says this host cannot produce it: {why}. So the comparison was \
                     REFUSED here, not passed. On a host that can measure it the same \
                     absence is a failure; regenerate the baseline only if the metric is \
                     meant to be gone"
                )),
                Absence::CodeIsTheOnlyExplanation => out
                    .failures
                    .push(format!("{what} no longer emits `{key}`, {gated}")),
            }
            continue;
        };
        if b.drift != c.drift {
            out.failures.push(format!(
                "{what}.{key} changed direction: baseline `{}`, this build `{}`. One of \
                 the two is wrong about what an improvement looks like",
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
                "{what}.{key} is directional in the baseline but its value or tolerance \
                 is not a finite number, so nothing can be compared"
            ));
            continue;
        }
        if !c.value.is_finite() {
            out.failures.push(format!(
                "{what}.{key} is {} here against a baseline of {}",
                c.value, b.value
            ));
            continue;
        }
        // Slack is a fraction of the baseline's magnitude, so a negative baseline widens correctly.
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
                "{what}.{key} regressed: {} {} against a baseline of {} ({pct}), past \
                 the {:.0}% the baseline allows (bound {bound})",
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
                "{what} emits a new directional metric `{}` that the baseline does not \
                 gate; regenerate the baseline",
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

    fn report_with(value: f64, drift_hi: bool) -> Report {
        report_tuned(value, drift_hi, 0.10)
    }

    fn report_tuned(value: f64, drift_hi: bool, tolerance: f64) -> Report {
        let m = crate::report::Metric::new("max_deviation", value, "rad or m");
        let m = if drift_hi {
            m.higher_is_better(tolerance)
        } else {
            m.lower_is_better(tolerance)
        };
        Report {
            provenance: Provenance {
                facts: FIXTURE_FACTS
                    .iter()
                    .map(|(k, v)| crate::report::Fact {
                        key: k,
                        value: (*v).to_owned(),
                    })
                    .collect(),
            },
            build: crate::report::Build::current(),
            fitness: Fitness::probe(1),
            warmup_discarded_s: 0.0,
            rows: vec![Row {
                id: "differential_agreement",
                title: "t",
                note: String::new(),
                sensitivity: crate::report::Sensitivity::HostIndependent,
                needs_n_cores: false,
                status: Status::Measured,
                reason: String::new(),
                grounds: Vec::new(),
                reproduce: "just bench-report",
                tf_tree: vec![m],
                tf2: Vec::new(),
            }],
            worse: Vec::new(),
        }
    }

    fn baseline_of(r: &Report) -> Value {
        serde_json::from_str(&r.to_json()).expect("the writer emits valid JSON")
    }

    /// An identical report passes and compares something (`checked` > 0).
    #[test]
    fn an_identical_report_passes_and_compares_something() {
        let r = report_with(2.5e-16, false);
        let c = compare(&baseline_of(&r), &r).expect("well-formed baseline");
        assert!(c.passed(), "identical report failed: {:?}", c.failures);
        assert_eq!(c.checked, 1, "nothing was actually compared");
    }

    /// Growth inside the tolerance passes; past it fails, naming the metric.
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

    /// A metric that changes direction fails rather than comparing inverted.
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

    /// A claim withdrawn fails; a claim new here does not.
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

    /// Host facts are ignored, build facts are not.
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

    /// The set of rows and of `where_we_are_worse` entries must match in both directions.
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

        let mut worse = report_with(100.0, false);
        worse.worse.push(crate::report::Worse {
            id: "attach_latency",
            topic: "attach latency",
            statement: String::from("we are slower to attach"),
            metrics: Vec::new(),
            metrics_absent_because: Some(String::from("this fixture states no numbers")),
            metrics_withheld: Vec::new(),
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
        r.rows[0].sensitivity = crate::report::Sensitivity::AbsoluteTiming;
        r.rows[0].tf_tree = run.metrics();
        r
    }

    /// §9.2's 5% on the embedding row through the real gate; the middle case is why every duration is gated.
    #[test]
    fn the_embedding_row_is_gated_at_five_percent_in_every_direction() {
        let base = baseline_of(&embedding_report(240.0, 200.0));

        let quiet = compare(&base, &embedding_report(247.2, 206.0)).expect("baseline");
        assert!(quiet.passed(), "a 3% shift in both halves: {quiet:?}");

        let skewed = compare(&base, &embedding_report(240.0, 190.0)).expect("baseline");
        assert!(!skewed.passed(), "a 5.3% ratio regression passed the gate");
        assert!(
            skewed.failures.iter().any(|f| f.contains("boundary_ratio")),
            "the failure must name the ratio: {:?}",
            skewed.failures
        );

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

    /// `bench-check` and `bench-baseline-update` must pass `bench_report` the same `--embed-cost`.
    #[test]
    fn a_baseline_that_measured_the_embedding_row_fails_a_check_that_did_not() {
        let measured = embedding_report(240.0, 200.0);
        let base = baseline_of(&measured);

        let mut flagless = embedding_report(240.0, 200.0);
        flagless.rows[0] =
            crate::report::embedding_row(&crate::report::Options::default(), &measured.fitness)
                .expect("the flagless row");
        assert_eq!(
            flagless.rows[0].status,
            Status::Unavailable,
            "a `bench_report` without --embed-cost must not claim this row"
        );

        let c = compare(&base, &flagless).expect("baseline");
        assert!(
            !c.passed(),
            "a baseline that measured the row passed a check that could not"
        );
        assert!(
            c.failures.iter().any(|f| {
                f.contains("embedding_cross_crate") && f.contains("claim was withdrawn")
            }),
            "the failure must name the row and the withdrawal: {:?}",
            c.failures
        );
    }

    /// A directional metric the baseline does not carry fails; an informational one does not.
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

    /// The tolerance used is the baseline's.
    #[test]
    fn the_tolerance_is_the_baselines_not_the_running_builds() {
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

        let generous = baseline_of(&report_tuned(100.0, false, 1.00));
        let c = compare(&generous, &report_tuned(150.0, false, 1.00)).expect("baseline");
        assert!(
            c.passed(),
            "growth inside the baseline's own tolerance failed: {:?}",
            c.failures
        );
    }

    /// A comparison that matched nothing is not a pass.
    #[test]
    fn a_comparison_that_matched_nothing_is_not_a_pass() {
        let r = report_with(2.5e-16, false);
        let c = compare(&baseline_of(&r), &r).expect("baseline");
        assert!(c.passed() && !c.compared_nothing(), "a real comparison");

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

    /// A baseline with no `drift` is rejected, not compared as context.
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

    /// A `where_we_are_worse` entry with one directional metric, and its baseline.
    fn report_with_worse(row_value: f64, worse_value: f64, tolerance: f64) -> Report {
        let mut r = report_tuned(row_value, false, 0.10);
        r.worse = vec![crate::report::Worse {
            id: "arena_memory_floor",
            topic: "Arena memory floor",
            statement: "an idle arena reserves its whole size".to_owned(),
            metrics: vec![
                crate::report::Metric::new("idle_arena_bytes", 2_405_696.0, "B"),
                crate::report::Metric::new("idle_arena_resident_bytes", worse_value, "B")
                    .lower_is_better(tolerance),
            ],
            metrics_absent_because: None,
            metrics_withheld: Vec::new(),
        }];
        r
    }

    /// A directional metric inside a `where_we_are_worse` entry is gated (`0021` step 4).
    #[test]
    fn a_directional_metric_inside_a_worse_entry_is_gated() {
        let base = baseline_of(&report_with_worse(100.0, 24_576.0, 3.0));

        let identical = compare(&base, &report_with_worse(100.0, 24_576.0, 3.0)).expect("baseline");
        assert!(identical.passed(), "identical: {:?}", identical.failures);
        assert_eq!(
            identical.checked, 2,
            "the row's metric and the entry's must both be compared, not just the row's"
        );

        let within = compare(&base, &report_with_worse(100.0, 90_000.0, 3.0)).expect("baseline");
        assert!(
            within.passed(),
            "3.7x under a 4x bound: {:?}",
            within.failures
        );

        let over = compare(&base, &report_with_worse(100.0, 2_408_448.0, 3.0)).expect("baseline");
        assert!(!over.passed(), "a 98x residency regression passed the gate");
        assert!(
            over.failures.iter().any(|f| {
                f.contains("where_we_are_worse `arena_memory_floor`")
                    && f.contains("idle_arena_resident_bytes")
                    && f.contains("regressed")
            }),
            "the failure must name the entry and the metric: {:?}",
            over.failures
        );
    }

    /// A gated metric this build no longer emits fails on a host that could have measured it and is a
    /// refusal on one that could not; informational vanishings are context.
    #[test]
    fn a_withheld_worse_metric_is_a_failure_or_a_refusal_depending_on_the_host() {
        let base = baseline_of(&report_with_worse(100.0, 24_576.0, 3.0));

        let mut gone = report_with_worse(100.0, 24_576.0, 3.0);
        gone.worse[0]
            .metrics
            .retain(|m| m.key != "idle_arena_resident_bytes");
        gone.fitness.fair_for_memory = true;
        let c = compare(&base, &gone).expect("baseline");
        assert!(
            !c.passed(),
            "a gated metric vanished on a fit host and the gate passed"
        );
        assert!(
            c.failures
                .iter()
                .any(|f| f.contains("no longer emits") && f.contains("which the baseline gates")),
            "a host that could measure it must be told the absence is gated: {:?}",
            c.failures
        );

        let mut unfit = gone.clone();
        unfit.fitness.fair_for_memory = false;
        unfit.fitness.memory_reasons = vec!["/proc/self/smaps_rollup is unreadable".to_owned()];
        let c = compare(&base, &unfit).expect("baseline");
        assert!(
            c.passed(),
            "a host that cannot measure Pss must not read as a code regression: {:?}",
            c.failures
        );
        assert!(
            c.notes
                .iter()
                .any(|n| { n.contains("REFUSED") && n.contains("smaps_rollup is unreadable") }),
            "the refusal must be recorded, with the host's own reason: {:?}",
            c.notes
        );

        let mut ctx_gone = report_with_worse(100.0, 24_576.0, 3.0);
        ctx_gone.fitness.fair_for_memory = true;
        ctx_gone.worse[0]
            .metrics
            .retain(|m| m.key != "idle_arena_bytes");
        let c = compare(&base, &ctx_gone).expect("baseline");
        assert!(!c.passed(), "the artifact shrank and the gate passed");
        assert!(
            c.failures.iter().any(|f| {
                f.contains("idle_arena_bytes") && f.contains("context rather than gating")
            }),
            "a context metric must not be described as gated: {:?}",
            c.failures
        );
    }

    /// A `where_we_are_worse` entry with no directional baseline metric contributes no
    /// comparison.
    #[test]
    fn an_all_informational_worse_entry_contributes_no_comparison() {
        let mut r = report_with_worse(100.0, 24_576.0, 3.0);
        r.worse[0].metrics = vec![
            crate::report::Metric::new("idle_arena_bytes", 2_405_696.0, "B"),
            crate::report::Metric::new("idle_arena_resident_bytes", 24_576.0, "B"),
        ];
        let c = compare(&baseline_of(&r), &r).expect("baseline");
        assert!(
            c.passed(),
            "informational metrics cannot fail: {:?}",
            c.failures
        );
        assert_eq!(
            c.checked, 1,
            "only the row's metric is a comparison; the entry's two are context"
        );
    }
}
