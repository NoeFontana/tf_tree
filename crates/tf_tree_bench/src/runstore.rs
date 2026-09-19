//! The A/B run store: what every performance harness emits, and how two runs are compared.
//!
//! Each harness writes a [`Run`]: every number with its direction and tolerance. [`diff`] turns two runs
//! into a verdict per row and never infers direction from a key name ([`Metric`], [`Drift`]: `docs/PHASE5.md` §9).
//!
//! Every emitter is covered by [`diff`]'s cross-profile refusal ([`BUILD_CRITICAL_FACTS`],
//! [`crate::baseline::PORTABLE_FACTS`], [`crate::embed::Pair::load`]); `native_arena`'s `.tfstream` is an input.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};

use crate::report::{jmetrics, jnum, jstr, Drift, Fitness, Metric, Provenance};
use crate::workload::Shape;

/// Run-file schema identifier. Bump on any consumer-visible change.
pub const SCHEMA: &str = "tf_tree.bench-run/1";

/// The provenance facts a timing comparison is only meaningful within.
pub const HOST_CRITICAL_FACTS: &[&str] = &[
    "cpu_model",
    "physical_cores",
    "logical_cpus",
    "cpu_governor",
    "kernel",
    "target",
    "counters_feature",
    // Both THP knobs: `enabled` governs heap arenas, `shmem_enabled` the `MAP_SHARED` `memfd`.
    "transparent_hugepage",
    "transparent_hugepage_shmem",
];

/// The provenance facts that make a comparison **impossible**, not merely
/// suspect.
pub const BUILD_CRITICAL_FACTS: &[&str] = &[
    "build_profile",
    "build_lto",
    "dds_cxx_build_type",
    "dds_c_abi_profile",
    "dds_c_abi_lto",
];

/// One measurement point: a harness, a workload, an engine, and a position in
/// whatever the harness sweeps.
#[derive(Debug, Clone)]
pub struct RunRow {
    /// Which harness produced this, e.g. `contended_scaling`.
    pub harness: String,
    /// The [`crate::workload`] name.
    pub workload: String,
    /// `tf_tree` or `tf2`.
    pub engine: String,
    /// The sweep point, e.g. `readers=8,writers=4`. Half the diff identity, so keep it stable.
    pub point: String,
    /// The workload's shape, when known (`docs/PHASE1.md` §11.3).
    pub shape: Option<Shape>,
    /// The numbers.
    pub metrics: Vec<Metric>,
}

impl RunRow {
    /// A row with no metrics yet.
    #[must_use]
    pub fn new(
        harness: impl Into<String>,
        workload: impl Into<String>,
        engine: impl Into<String>,
        point: impl Into<String>,
    ) -> RunRow {
        RunRow {
            harness: harness.into(),
            workload: workload.into(),
            engine: engine.into(),
            point: point.into(),
            shape: None,
            metrics: Vec::new(),
        }
    }

    /// Attach the workload shape.
    #[must_use]
    pub fn with_shape(mut self, shape: Shape) -> RunRow {
        self.shape = Some(shape);
        self
    }

    /// Add a metric.
    #[must_use]
    pub fn metric(mut self, m: Metric) -> RunRow {
        self.metrics.push(m);
        self
    }

    /// The identity a diff matches on.
    #[must_use]
    pub fn key(&self) -> String {
        format!(
            "{}/{}/{}/{}",
            self.harness, self.workload, self.engine, self.point
        )
    }
}

/// One harness invocation's whole output.
#[derive(Debug, Clone)]
pub struct Run {
    /// Environment description, from [`Provenance::collect`].
    pub provenance: Provenance,
    /// Host fitness at the time of the run.
    pub fitness: Fitness,
    /// The rows.
    pub rows: Vec<RunRow>,
}

impl Run {
    /// Start a run, collecting provenance and probing the host.
    #[must_use]
    pub fn begin(consumers: usize) -> Run {
        let mut provenance = Provenance::collect();
        for f in &mut provenance.facts {
            if f.key == "schema" {
                f.value = SCHEMA.to_owned();
            }
        }
        Run {
            provenance,
            fitness: Fitness::probe(consumers),
            rows: Vec::new(),
        }
    }

    /// Append a row.
    pub fn push(&mut self, row: RunRow) {
        self.rows.push(row);
    }

    /// Refuse a run that cannot be compared.
    ///
    /// # Errors
    ///
    /// Every problem found, not just the first.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut bad = Vec::new();
        let mut seen: BTreeMap<String, usize> = BTreeMap::new();

        for row in &self.rows {
            *seen.entry(row.key()).or_insert(0) += 1;
            for m in &row.metrics {
                if m.drift != Drift::Informational && m.tolerance <= 0.0 {
                    bad.push(format!(
                        "{}: metric `{}` is directional but carries tolerance {}; \
                         a zero tolerance makes every last-bit difference a verdict",
                        row.key(),
                        m.key,
                        m.tolerance
                    ));
                }
            }
        }
        for (key, n) in seen {
            if n > 1 {
                bad.push(format!(
                    "row key `{key}` appears {n} times; a diff would match it \
                     arbitrarily"
                ));
            }
        }

        if bad.is_empty() {
            Ok(())
        } else {
            Err(bad)
        }
    }

    /// Serialise.
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut s = String::with_capacity(4096);
        s.push_str("{\n");
        let _ = writeln!(s, "  \"schema\": {},", jstr(SCHEMA));

        s.push_str("  \"provenance\": {\n");
        for (i, f) in self.provenance.facts.iter().enumerate() {
            let comma = if i + 1 == self.provenance.facts.len() {
                ""
            } else {
                ","
            };
            let _ = writeln!(s, "    {}: {}{comma}", jstr(f.key), jstr(&f.value));
        }
        s.push_str("  },\n");

        s.push_str("  \"host_fitness\": {\n");
        let _ = writeln!(
            s,
            "    \"fair_for_timing\": {},",
            self.fitness.fair_for_timing
        );
        let _ = writeln!(s, "    \"enough_cores\": {},", self.fitness.enough_cores);
        let _ = writeln!(s, "    \"forced\": {},", self.fitness.forced);
        let _ = writeln!(
            s,
            "    \"busy_fraction\": {},",
            jnum(self.fitness.busy_fraction)
        );
        s.push_str("    \"reasons\": [");
        for (i, r) in self.fitness.reasons.iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            s.push_str(&jstr(r));
        }
        s.push_str("]\n  },\n");

        s.push_str("  \"rows\": [\n");
        for (i, r) in self.rows.iter().enumerate() {
            s.push_str("    {\n");
            let _ = writeln!(s, "      \"harness\": {},", jstr(&r.harness));
            let _ = writeln!(s, "      \"workload\": {},", jstr(&r.workload));
            let _ = writeln!(s, "      \"engine\": {},", jstr(&r.engine));
            let _ = writeln!(s, "      \"point\": {},", jstr(&r.point));
            let _ = writeln!(s, "      \"shape\": {},", jshape(r.shape.as_ref()));
            let _ = writeln!(s, "      \"metrics\": {}", jmetrics(&r.metrics));
            s.push_str(if i + 1 == self.rows.len() {
                "    }\n"
            } else {
                "    },\n"
            });
        }
        s.push_str("  ]\n}\n");
        s
    }

    /// Validate and write to `path`, creating parent directories.
    ///
    /// # Errors
    ///
    /// If validation fails, or the file cannot be written.
    pub fn write(&self, path: &Path) -> Result<()> {
        if let Err(bad) = self.validate() {
            bail!(
                "refusing to write an uncomparable run:\n  - {}",
                bad.join("\n  - ")
            );
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        std::fs::write(path, self.to_json()).with_context(|| format!("writing {}", path.display()))
    }

    /// Parse a run file.
    ///
    /// # Errors
    ///
    /// If the JSON is malformed, the schema is not [`SCHEMA`], or a row is missing a field.
    pub fn parse(text: &str) -> Result<Run> {
        let v: serde_json::Value =
            serde_json::from_str(text).context("parsing the run file as JSON")?;

        let schema = v
            .get("schema")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow!("run file has no `schema`"))?;
        if schema != SCHEMA {
            bail!("run file schema is {schema:?}, expected {SCHEMA:?}");
        }

        let mut facts = Vec::new();
        if let Some(obj) = v.get("provenance").and_then(serde_json::Value::as_object) {
            for (k, val) in obj {
                facts.push((k.clone(), val.as_str().unwrap_or_default().to_owned()));
            }
        }

        let rows_json = v
            .get("rows")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| anyhow!("run file has no `rows` array"))?;

        let mut rows = Vec::with_capacity(rows_json.len());
        for (i, r) in rows_json.iter().enumerate() {
            let field = |name: &str| -> Result<String> {
                r.get(name)
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned)
                    .ok_or_else(|| anyhow!("row {i} has no `{name}`"))
            };
            let mut row = RunRow::new(
                field("harness")?,
                field("workload")?,
                field("engine")?,
                field("point")?,
            );
            let metrics = r
                .get("metrics")
                .and_then(serde_json::Value::as_object)
                .ok_or_else(|| anyhow!("row {i} has no `metrics` object"))?;
            for (key, m) in metrics {
                row.metrics.push(parse_metric(key, m, i)?);
            }
            rows.push(row);
        }

        Ok(Run {
            provenance: Provenance {
                facts: facts
                    .into_iter()
                    .map(|(k, v)| crate::report::Fact {
                        key: Box::leak(k.into_boxed_str()),
                        value: v,
                    })
                    .collect(),
            },
            fitness: Fitness::assess(0, 1, None, 0.0, None, false, true),
            rows,
        })
    }

    /// Read a run file from disk.
    ///
    /// # Errors
    ///
    /// If the file cannot be read or parsed.
    pub fn load(path: &Path) -> Result<Run> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Run::parse(&text).with_context(|| format!("in {}", path.display()))
    }

    /// A provenance fact by key.
    #[must_use]
    pub fn fact(&self, key: &str) -> Option<&str> {
        self.provenance
            .facts
            .iter()
            .find(|f| f.key == key)
            .map(|f| f.value.as_str())
    }
}

fn parse_metric(key: &str, m: &serde_json::Value, row: usize) -> Result<Metric> {
    let value = m
        .get("value")
        .ok_or_else(|| anyhow!("row {row}: metric `{key}` has no `value`"))?
        .as_f64()
        .unwrap_or(f64::NAN);
    let unit: &'static str = m
        .get("unit")
        .and_then(serde_json::Value::as_str)
        .map_or("", |s| Box::leak(s.to_owned().into_boxed_str()));
    let drift = match m.get("drift").and_then(serde_json::Value::as_str) {
        Some("lower_is_better") => Drift::LowerIsBetter,
        Some("higher_is_better") => Drift::HigherIsBetter,
        Some("informational") | None => Drift::Informational,
        Some(other) => bail!("row {row}: metric `{key}` has unknown drift {other:?}"),
    };
    let tolerance = m
        .get("tolerance")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0);

    Ok(Metric {
        key: Box::leak(key.to_owned().into_boxed_str()),
        value,
        unit,
        drift,
        tolerance,
    })
}

fn jshape(shape: Option<&Shape>) -> String {
    match shape {
        None => "null".to_owned(),
        Some(s) => format!(
            "{{\"frames\": {}, \"edges\": {}, \"dynamic_edges\": {}, \"samples\": {}, \
             \"slots\": {}, \"arena_bytes\": {}, \"dyn_steps\": {}}}",
            s.frames,
            s.edges,
            s.dynamic_edges,
            s.samples,
            s.slots,
            s.arena_bytes,
            s.dyn_steps
                .map_or_else(|| "null".to_owned(), |n| n.to_string())
        ),
    }
}

/// What a metric did between two runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Moved the good way by more than its tolerance.
    Better,
    /// Moved the bad way by more than its tolerance.
    Worse,
    /// Moved less than its tolerance, or is not directional but moved.
    Noise,
    /// Informational; never a claim either way.
    Info,
    /// One side could not be measured (non-finite). Never a verdict.
    Unmeasured,
}

impl Verdict {
    /// The display spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Better => "better",
            Verdict::Worse => "worse",
            Verdict::Noise => "noise",
            Verdict::Info => "info",
            Verdict::Unmeasured => "unmeasured",
        }
    }
}

/// One metric, compared.
#[derive(Debug, Clone)]
pub struct Delta {
    /// `harness/workload/engine/point`.
    pub row: String,
    /// The metric key.
    pub metric: &'static str,
    /// Its unit.
    pub unit: &'static str,
    /// Baseline value.
    pub a: f64,
    /// New value.
    pub b: f64,
    /// `(b - a) / a`, or `NaN` when `a` is zero or either side is unmeasured.
    pub rel: f64,
    /// The tolerance the verdict was taken against.
    pub tolerance: f64,
    /// The verdict.
    pub verdict: Verdict,
}

/// Two runs, compared.
#[derive(Debug, Clone)]
pub struct Diff {
    /// Every metric present in both runs.
    pub deltas: Vec<Delta>,
    /// Row keys in `a` but not `b`; reported, never dropped.
    pub only_in_a: Vec<String>,
    /// Row keys in `b` but not `a`.
    pub only_in_b: Vec<String>,
    /// `(fact, a, b)` for each [`HOST_CRITICAL_FACTS`] entry that differs.
    pub host_drift: Vec<(String, String, String)>,
    /// `(fact, a, b)` for each [`BUILD_CRITICAL_FACTS`] entry that differs.
    pub build_mismatch: Vec<(String, String, String)>,
}

impl Diff {
    /// Whether the two runs describe the same program built the same way.
    #[must_use]
    pub fn comparable(&self) -> bool {
        self.build_mismatch.is_empty()
    }

    /// Whether any metric regressed beyond its tolerance.
    #[must_use]
    pub fn regressed(&self) -> bool {
        self.deltas.iter().any(|d| d.verdict == Verdict::Worse)
    }

    /// How many deltas landed on each verdict.
    #[must_use]
    pub fn tally(&self) -> BTreeMap<&'static str, usize> {
        let mut out = BTreeMap::new();
        for d in &self.deltas {
            *out.entry(d.verdict.as_str()).or_insert(0) += 1;
        }
        out
    }
}

/// Compare `b` against baseline `a`.
#[must_use]
pub fn diff(a: &Run, b: &Run) -> Diff {
    let index = |run: &Run| -> BTreeMap<String, Vec<Metric>> {
        run.rows
            .iter()
            .map(|r| (r.key(), r.metrics.clone()))
            .collect()
    };
    let (ia, ib) = (index(a), index(b));

    let mut deltas = Vec::new();
    for (key, ma) in &ia {
        let Some(mb) = ib.get(key) else { continue };
        for m_a in ma {
            let Some(m_b) = mb.iter().find(|x| x.key == m_a.key) else {
                continue;
            };
            deltas.push(compare(key, m_a, m_b));
        }
    }

    let only_in_a = ia
        .keys()
        .filter(|k| !ib.contains_key(*k))
        .cloned()
        .collect();
    let only_in_b = ib
        .keys()
        .filter(|k| !ia.contains_key(*k))
        .cloned()
        .collect();

    let drift = |facts: &[&str]| -> Vec<(String, String, String)> {
        facts
            .iter()
            .filter_map(|fact| {
                let (va, vb) = (a.fact(fact), b.fact(fact));
                (va != vb).then(|| {
                    (
                        (*fact).to_owned(),
                        va.unwrap_or("absent").to_owned(),
                        vb.unwrap_or("absent").to_owned(),
                    )
                })
            })
            .collect()
    };

    Diff {
        deltas,
        only_in_a,
        only_in_b,
        host_drift: drift(HOST_CRITICAL_FACTS),
        build_mismatch: drift(BUILD_CRITICAL_FACTS),
    }
}

fn compare(row: &str, a: &Metric, b: &Metric) -> Delta {
    let drift = a.drift;
    let tolerance = a.tolerance;

    let unmeasured = !a.value.is_finite() || !b.value.is_finite();
    let rel = if unmeasured || a.value == 0.0 {
        f64::NAN
    } else {
        (b.value - a.value) / a.value.abs()
    };

    let verdict = if unmeasured {
        Verdict::Unmeasured
    } else if drift == Drift::Informational {
        Verdict::Info
    } else if !rel.is_finite() || rel.abs() <= tolerance {
        Verdict::Noise
    } else {
        let improved = match drift {
            Drift::LowerIsBetter => rel < 0.0,
            Drift::HigherIsBetter => rel > 0.0,
            Drift::Informational => true,
        };
        if improved {
            Verdict::Better
        } else {
            Verdict::Worse
        }
    };

    Delta {
        row: row.to_owned(),
        metric: a.key,
        unit: a.unit,
        a: a.value,
        b: b.value,
        rel,
        tolerance,
        verdict,
    }
}

/// Render a diff as a table — or, when the two runs are not comparable, as the
/// reason there is no table.
#[must_use]
pub fn render(d: &Diff) -> String {
    let mut s = String::with_capacity(4096);

    if !d.comparable() {
        s.push_str(
            "REFUSED — these two runs were built differently, so they do not answer\n\
             the same question and no verdict over them means anything. The numbers\n\
             are deliberately not shown.\n",
        );
        for (fact, a, b) in &d.build_mismatch {
            let _ = writeln!(s, "  {fact}: {a:?} -> {b:?}");
        }
        s.push_str(
            "\nThis workspace's [profile.release] is `lto = \"thin\"`, which inlines\n\
             across the crate boundary that [profile.embedder] (`lto = false`) leaves\n\
             in the binary. A cost measured under one is not the cost under the other.\n\
             Re-run both halves at the same profile.\n",
        );
        if d.build_mismatch
            .iter()
            .any(|(_, a, b)| a == "absent" || b == "absent")
        {
            s.push_str(
                "\n`absent` above means that run file predates the fact and cannot state\n\
                 what it was built with. That is not agreement, so it is not treated as\n\
                 agreement; re-run the older half.\n",
            );
        }
        return s;
    }

    if !d.host_drift.is_empty() {
        s.push_str(
            "HOST DRIFT — these two runs were not taken on the same machine, so every\n\
             timing verdict below is about the host as much as about the change:\n",
        );
        for (fact, a, b) in &d.host_drift {
            let _ = writeln!(s, "  {fact}: {a:?} -> {b:?}");
        }
        s.push('\n');
    }

    let _ = writeln!(
        s,
        "{:<58} {:>12} {:>14} {:>14} {:>9}  verdict",
        "row / metric", "unit", "a", "b", "change"
    );
    let _ = writeln!(s, "{}", "-".repeat(120));

    let mut ordered: Vec<&Delta> = d.deltas.iter().collect();
    ordered.sort_by_key(|x| {
        (
            match x.verdict {
                Verdict::Worse => 0,
                Verdict::Better => 1,
                Verdict::Unmeasured => 2,
                Verdict::Noise => 3,
                Verdict::Info => 4,
            },
            x.row.clone(),
            x.metric,
        )
    });

    for x in ordered {
        let change = if x.rel.is_finite() {
            format!("{:+.1}%", x.rel * 100.0)
        } else {
            "n/a".to_owned()
        };
        let _ = writeln!(
            s,
            "{:<58} {:>12} {:>14} {:>14} {:>9}  {}",
            truncate(&format!("{}  {}", x.row, x.metric), 58),
            x.unit,
            fmt_value(x.a),
            fmt_value(x.b),
            change,
            x.verdict.as_str(),
        );
    }

    if !d.only_in_a.is_empty() {
        let _ = writeln!(s, "\nRows only in a ({}):", d.only_in_a.len());
        for k in &d.only_in_a {
            let _ = writeln!(s, "  {k}");
        }
    }
    if !d.only_in_b.is_empty() {
        let _ = writeln!(s, "\nRows only in b ({}):", d.only_in_b.len());
        for k in &d.only_in_b {
            let _ = writeln!(s, "  {k}");
        }
    }

    s.push('\n');
    let tally = d.tally();
    let summary: Vec<String> = tally.iter().map(|(k, n)| format!("{n} {k}")).collect();
    let _ = writeln!(
        s,
        "{} compared: {}",
        d.deltas.len(),
        if summary.is_empty() {
            "nothing".to_owned()
        } else {
            summary.join(", ")
        }
    );
    s
}

fn fmt_value(v: f64) -> String {
    if !v.is_finite() {
        "-".to_owned()
    } else if v == 0.0 || (v.abs() >= 0.01 && v.abs() < 1e7) {
        format!("{v:.3}")
    } else {
        format!("{v:.3e}")
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_owned()
    } else {
        let skip = s.chars().count() - (n - 1);
        format!("…{}", s.chars().skip(skip).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;

    fn row(point: &str, p99: f64, tol: f64) -> RunRow {
        RunRow::new("h", "robot", "tf_tree", point)
            .metric(Metric::new("p99_ns", p99, "ns").lower_is_better(tol))
    }

    fn run(rows: Vec<RunRow>) -> Run {
        let mut r = Run::begin(1);
        for x in rows {
            r.push(x);
        }
        r
    }

    #[test]
    fn an_identical_run_is_all_noise() {
        let a = run(vec![row("n=1", 100.0, 0.25), row("n=2", 200.0, 0.25)]);
        let b = run(vec![row("n=1", 100.0, 0.25), row("n=2", 200.0, 0.25)]);
        let d = diff(&a, &b);
        assert_eq!(d.deltas.len(), 2);
        assert!(d.deltas.iter().all(|x| x.verdict == Verdict::Noise));
        assert!(!d.regressed());
    }

    #[test]
    fn a_move_inside_the_tolerance_is_not_news() {
        let a = run(vec![row("n=1", 100.0, 0.25)]);
        let b = run(vec![row("n=1", 120.0, 0.25)]);
        let d = diff(&a, &b);
        assert_eq!(d.deltas[0].verdict, Verdict::Noise);
    }

    #[test]
    fn direction_is_read_not_guessed() {
        let a = run(vec![row("n=1", 100.0, 0.10)]);
        let b = run(vec![row("n=1", 200.0, 0.10)]);
        assert_eq!(diff(&a, &b).deltas[0].verdict, Verdict::Worse);

        let up = |v: f64| {
            RunRow::new("h", "robot", "tf_tree", "n=1")
                .metric(Metric::new("ops", v, "ops/s").higher_is_better(0.10))
        };
        let d = diff(&run(vec![up(100.0)]), &run(vec![up(200.0)]));
        assert_eq!(d.deltas[0].verdict, Verdict::Better);
    }

    #[test]
    fn the_baselines_direction_wins_when_a_metric_was_retyped() {
        let a = run(vec![row("n=1", 100.0, 0.10)]);
        let b = run(vec![RunRow::new("h", "robot", "tf_tree", "n=1")
            .metric(Metric::new("p99_ns", 200.0, "ns").higher_is_better(0.10))]);
        assert_eq!(diff(&a, &b).deltas[0].verdict, Verdict::Worse);
    }

    #[test]
    fn an_unmeasured_side_is_never_a_verdict() {
        let a = run(vec![row("n=1", f64::NAN, 0.10)]);
        let b = run(vec![row("n=1", 50.0, 0.10)]);
        let d = diff(&a, &b);
        assert_eq!(d.deltas[0].verdict, Verdict::Unmeasured);
        assert!(!d.regressed());
    }

    #[test]
    fn a_vanished_row_is_reported_rather_than_ignored() {
        let a = run(vec![row("n=1", 100.0, 0.10), row("n=2", 100.0, 0.10)]);
        let b = run(vec![row("n=1", 100.0, 0.10)]);
        let d = diff(&a, &b);
        assert_eq!(d.only_in_a, vec!["h/robot/tf_tree/n=2".to_owned()]);
        assert!(d.only_in_b.is_empty());
        assert!(render(&d).contains("Rows only in a"));
    }

    #[test]
    fn a_directional_metric_without_tolerance_is_refused() {
        let mut r = Run::begin(1);
        r.push(
            RunRow::new("h", "robot", "tf_tree", "n=1")
                .metric(Metric::new("p99_ns", 1.0, "ns").lower_is_better(0.0)),
        );
        let bad = r.validate().expect_err("should be refused");
        assert!(bad[0].contains("tolerance"), "{bad:?}");
    }

    #[test]
    fn duplicate_row_keys_are_refused() {
        let r = run(vec![row("n=1", 1.0, 0.1), row("n=1", 2.0, 0.1)]);
        let bad = r.validate().expect_err("should be refused");
        assert!(bad[0].contains("appears 2 times"), "{bad:?}");
    }

    #[test]
    fn a_run_round_trips_through_json() {
        let mut a = run(vec![row("n=1", 123.5, 0.25)]);
        a.rows[0].shape = Some(Shape {
            frames: 24,
            edges: 23,
            dynamic_edges: 4,
            samples: 12600,
            slots: 19072,
            arena_bytes: 1_400_000,
            dyn_steps: Some(3),
        });
        let text = a.to_json();
        let back = Run::parse(&text).expect("round trip");
        assert_eq!(back.rows.len(), 1);
        assert_eq!(back.rows[0].key(), "h/robot/tf_tree/n=1");
        assert_eq!(back.rows[0].metrics[0].value, 123.5);
        assert_eq!(back.rows[0].metrics[0].drift, Drift::LowerIsBetter);
        assert_eq!(back.rows[0].metrics[0].tolerance, 0.25);
        assert!(diff(&a, &back)
            .deltas
            .iter()
            .all(|d| d.verdict == Verdict::Noise));
    }

    #[test]
    fn a_foreign_schema_is_refused_rather_than_half_read() {
        let err =
            Run::parse(r#"{"schema": "something.else/1", "rows": []}"#).expect_err("should refuse");
        assert!(err.to_string().contains("schema"), "{err}");
    }

    #[test]
    fn host_drift_is_surfaced() {
        let mut a = run(vec![row("n=1", 1.0, 0.1)]);
        let b = run(vec![row("n=1", 1.0, 0.1)]);
        for f in &mut a.provenance.facts {
            if f.key == "cpu_model" {
                f.value = "a different CPU".to_owned();
            }
        }
        let d = diff(&a, &b);
        assert!(d.host_drift.iter().any(|(k, _, _)| k == "cpu_model"));
        assert!(render(&d).contains("HOST DRIFT"));
        assert!(d.comparable(), "a different CPU is not a build mismatch");
        assert!(
            render(&d).contains("row / metric"),
            "the table is still printed"
        );
    }

    /// [`HOST_CRITICAL_FACTS`]'s membership is pinned, and every key is shown to be diffed and produced.
    #[test]
    fn every_host_critical_fact_is_pinned_and_diffed() {
        const EXPECTED: &[&str] = &[
            "cpu_model",
            "physical_cores",
            "logical_cpus",
            "cpu_governor",
            "kernel",
            "target",
            "counters_feature",
            "transparent_hugepage",
            "transparent_hugepage_shmem",
        ];
        assert_eq!(
            HOST_CRITICAL_FACTS, EXPECTED,
            "HOST_CRITICAL_FACTS changed. That is allowed — update EXPECTED in \
             the same commit — but it is never a silent edit: a key dropped \
             from this list retires drift detection for it, which is how \
             `transparent_hugepage` went missing"
        );

        let produced = Provenance::collect();
        for key in HOST_CRITICAL_FACTS {
            assert!(
                produced.get(key).is_some(),
                "`{key}` is in HOST_CRITICAL_FACTS and `Provenance::collect` no \
                 longer emits it. Drift detection for it is retired: a key \
                 absent from both runs compares equal, so every future pair of \
                 runs agrees on it"
            );
        }

        for key in HOST_CRITICAL_FACTS {
            let mut a = run(vec![row("n=1", 1.0, 0.1)]);
            let mut b = run(vec![row("n=1", 1.0, 0.1)]);
            for (r, v) in [(&mut a, "before"), (&mut b, "after")] {
                match r.provenance.facts.iter_mut().find(|f| &f.key == key) {
                    Some(f) => f.value = v.to_owned(),
                    None => r.provenance.facts.push(crate::report::Fact {
                        key,
                        value: v.to_owned(),
                    }),
                }
            }
            let d = diff(&a, &b);
            assert!(
                d.host_drift.iter().any(|(k, _, _)| k == key),
                "`{key}` is in HOST_CRITICAL_FACTS but a change to it is not \
                 reported as host drift, so nothing would notice the machine \
                 moving underneath two runs"
            );
        }
    }

    /// A value for `key` guaranteed to differ from this build's recorded one.
    fn differing_value(r: &Run, key: &str) -> String {
        let real = r
            .provenance
            .facts
            .iter()
            .find(|f| f.key == key)
            .map_or("", |f| f.value.as_str());
        match key {
            "build_lto" if real == "false" => "\"thin\"".to_owned(),
            "build_lto" => "false".to_owned(),
            _ if real == "embedder" => "release".to_owned(),
            _ => "embedder".to_owned(),
        }
    }

    /// A run at `--profile embedder` and one at `--release` measure different programs.
    #[test]
    fn two_runs_built_at_different_profiles_refuse_to_be_compared() {
        let mut a = run(vec![row("n=1", 1.0, 0.1)]);
        let b = run(vec![row("n=1", 1.0, 0.1)]);
        let other = differing_value(&b, "build_profile");
        for f in &mut a.provenance.facts {
            if f.key == "build_profile" {
                f.value.clone_from(&other);
            }
        }

        let d = diff(&a, &b);
        assert!(
            !d.comparable(),
            "two profiles must not compare; mismatch was {:?}",
            d.build_mismatch
        );
        assert!(d
            .build_mismatch
            .iter()
            .any(|(k, _, _)| k == "build_profile"));

        let text = render(&d);
        assert!(text.contains("REFUSED"), "{text}");
        assert!(
            !text.contains("row / metric"),
            "the delta table must not be printed for an incomparable pair:\n{text}"
        );
    }

    /// A profile whose meaning changed but not its name.
    #[test]
    fn a_profile_that_changed_its_lto_without_changing_its_name_is_refused() {
        let mut a = run(vec![row("n=1", 1.0, 0.1)]);
        let b = run(vec![row("n=1", 1.0, 0.1)]);
        let other = differing_value(&b, "build_lto");
        for f in &mut a.provenance.facts {
            if f.key == "build_lto" {
                f.value.clone_from(&other);
            }
        }
        let d = diff(&a, &b);
        assert!(
            !d.comparable(),
            "an lto change under a stable profile name must refuse"
        );
        assert!(d.build_mismatch.iter().any(|(k, _, _)| k == "build_lto"));
    }

    /// A run file predating `build_lto` reads as a mismatch, not agreement.
    #[test]
    fn a_run_that_records_no_build_fact_does_not_compare_as_matching() {
        let mut a = run(vec![row("n=1", 1.0, 0.1)]);
        let b = run(vec![row("n=1", 1.0, 0.1)]);
        a.provenance.facts.retain(|f| f.key != "build_lto");
        let d = diff(&a, &b);
        assert!(
            !d.comparable(),
            "an absent build fact is not a matching one"
        );
        assert!(d
            .build_mismatch
            .iter()
            .any(|(k, va, _)| k == "build_lto" && va == "absent"));
    }
}
