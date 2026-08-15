//! The A/B run store: what every performance harness emits, and how two of
//! them are compared.
//!
//! # The question this exists to answer
//!
//! *"I changed the core. Did it help?"*
//!
//! Before this module the answer came from reading two criterion tables side by
//! side, which fails in the two ways that matter: a 3% move reads as a win when
//! it is noise, and a regression on the one row you were not looking at reads as
//! nothing at all. So each harness writes a [`Run`] — every number it took, each
//! with the direction it is allowed to move and the slack below which a move is
//! not news — and [`diff`] turns two of them into a verdict per row.
//!
//! # Why the types come from [`crate::report`]
//!
//! [`Metric`] and [`Drift`] are `docs/PHASE5.md` §9's, and reusing them rather
//! than declaring a parallel pair is the whole design. `report.rs` gives the
//! reason in its own words: a regression gate over untyped numbers "is a coin
//! flip with extra steps", because a checker that infers direction from key
//! names will one day pass a doubled latency because somebody named a field
//! `ops_ns`. [`diff`] never guesses — it reads [`Metric::drift`].
//!
//! # What this deliberately is *not*
//!
//! It is **not** the `docs/PHASE5.md` §9 report, and it is not wired into
//! `just bench-check`. That gate compares a run against a *committed baseline*
//! and must ignore every host fact, because a gate that fails for the CPU model
//! is a gate people learn to ignore. This one compares two runs on the *same*
//! host minutes apart, which is the opposite situation: here a differing host
//! fact invalidates the comparison, so [`diff`] surfaces it loudly
//! ([`Diff::host_drift`]) instead of ignoring it.
//!
//! # A differing *build* fact is not surfaced — it is refused
//!
//! [`HOST_CRITICAL_FACTS`] and [`BUILD_CRITICAL_FACTS`] are two lists because
//! they earn two different responses. A different CPU makes the absolute
//! numbers untrustworthy while leaving the ratios worth reading, so it is a
//! warning above the table. A different `[profile.*]` makes the two runs
//! *measurements of different programs* — this workspace's `[profile.release]`
//! sets `lto = "thin"`, which inlines across the crate boundary that
//! `[profile.embedder]` leaves standing — so there is nothing worth reading and
//! [`render`] prints no table at all. See [`Diff::comparable`].
//!
//! # Schema
//!
//! Emitted by hand, for the reason `report.rs` gives for doing the same: the
//! schema is a compatibility surface, and hand-writing it makes a field rename a
//! deliberate edit rather than a side effect of a `#[derive]`. Reading uses
//! `serde_json`, because a hand-rolled parser for somebody else's committed file
//! fails *open*, which is the one failure mode a comparison tool cannot have.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};

use crate::report::{jmetrics, jnum, jstr, Drift, Fitness, Metric, Provenance};
use crate::workload::Shape;

/// Run-file schema identifier. Bump on any consumer-visible change.
///
/// **Adding a provenance fact is not one, and `build_lto` was deliberately
/// added without a bump.** A bump makes every committed and cached run file
/// unreadable, which is the same damage renaming a row or metric id does — see
/// `bin/scale_sweep.rs`'s note on why `rss`/`pss` was not renamed. An older run
/// file simply carries no `build_lto`, and [`diff`] already treats an absent
/// build fact as a mismatch rather than as agreement, so nothing reads such a
/// file as if it had made a claim it never made.
pub const SCHEMA: &str = "tf_tree.bench-run/1";

/// The provenance facts a timing comparison is only meaningful within.
///
/// Not every fact: `generated_utc` and `load_average` differ between any two
/// runs by construction, and warning about them would train a reader to skip
/// the warning. These are the ones whose change means the two runs measured
/// different machines or different programs.
pub const HOST_CRITICAL_FACTS: &[&str] = &[
    "cpu_model",
    "physical_cores",
    "logical_cpus",
    "cpu_governor",
    "kernel",
    "target",
    "counters_feature",
    "transparent_hugepage",
];

/// The provenance facts that make a comparison **impossible**, not merely
/// suspect.
///
/// [`HOST_CRITICAL_FACTS`] above says "these two numbers came off different
/// machines, so read the verdicts with that in mind" — and that warning is
/// survivable, because a *ratio* often is still meaningful when the absolute
/// numbers are not. These are different in kind: they mean the two runs measured
/// **different programs answering different questions**, and no arithmetic over
/// them means anything at all. So [`diff`] refuses instead of warning, and
/// `bench_ab` exits non-zero.
///
/// `build_profile` moved here from the host list, and it is the reason this list
/// exists. This workspace's `[profile.release]` is `lto = "thin"`;
/// `[profile.embedder]` is `lto = false`. Thin LTO inlines across the crate
/// boundary a boundary measurement is trying to price, so the same binary built
/// the two ways does not measure the same thing — twice in one week that
/// difference was reported as a property of the code (`docs/PHASE4.md` §0.0
/// records both). A warning printed above a table of per-row verdicts is not
/// enough for that: the table is the thing that misleads, and it was still
/// printed.
///
/// `build_lto` is here as well even though it is currently a function of
/// `build_profile`. It is not redundant: it is what fires if a future edit to
/// `[profile.embedder]` or `[profile.release]` changes what a profile *means*
/// while leaving its name alone, which is exactly the change nobody would think
/// to regenerate a comparison for.
pub const BUILD_CRITICAL_FACTS: &[&str] = &["build_profile", "build_lto"];

/// One measurement point: a harness, a workload, an engine, and a position in
/// whatever the harness sweeps.
#[derive(Debug, Clone)]
pub struct RunRow {
    /// Which harness produced this, e.g. `contended_scaling`.
    pub harness: String,
    /// The [`crate::workload`] name.
    pub workload: String,
    /// `tf_tree` or `tf2`. Present so the two engines' rows sort next to each
    /// other and so a diff never silently compares one against the other.
    pub engine: String,
    /// The point in the harness's sweep, e.g. `readers=8,writers=4`. Free-form,
    /// but **stable**: it is half the identity a diff matches on, so a run whose
    /// point strings changed reads as an entirely new set of rows.
    pub point: String,
    /// The workload's shape, when the harness knows it. Carried so a reader can
    /// interpret the latency — `docs/PHASE1.md` §11.3's rule that a row must
    /// state its dynamic-step count.
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
    /// Host fitness at the time of the run. A comparison between two runs on a
    /// host that failed the probe is still useful — the *ratio* survives noise
    /// the absolute number does not — but the reader must be told.
    pub fitness: Fitness,
    /// The rows.
    pub rows: Vec<RunRow>,
}

impl Run {
    /// Start a run, collecting provenance and probing the host.
    ///
    /// `consumers` is what the harness will ask of the machine, so the core
    /// budget in [`Fitness`] is checked against the right number.
    #[must_use]
    pub fn begin(consumers: usize) -> Run {
        let mut provenance = Provenance::collect();
        // `Provenance::collect` stamps the *report's* schema, because it is
        // shared with `docs/PHASE5.md` §9's artifact. A run file that named the
        // report schema inside itself while naming this one at the top level
        // would be a file that disagrees with itself about what it is.
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
    /// Two rules, both structural for the same reason `report::Report::validate`
    /// is:
    ///
    /// * **A directional metric must carry a tolerance.** With `tolerance == 0`
    ///   every last-bit difference becomes a verdict, so a differ over such a
    ///   file reports change on two runs of the same binary — and a tool that
    ///   cries wolf on identical input is one nobody reads.
    /// * **Row keys must be unique.** Two rows with the same identity make the
    ///   diff's answer depend on `Vec` order, which is not something a caller
    ///   can see or control.
    ///
    /// # Errors
    ///
    /// A list of every problem, not just the first: fixing them one run at a
    /// time is what makes a validation step annoying enough to be bypassed.
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
    /// If the JSON is malformed, the schema is not [`SCHEMA`], or a row is
    /// missing a field. Every one of these is a hard error: a comparison tool
    /// that skips what it cannot read reports "no change" for the rows it
    /// dropped.
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
                        // The keys are `&'static str` in the emitting type;
                        // a parsed file's are not, so they are leaked into
                        // `'static`. This is a short-lived CLI reading at most
                        // two files, and the alternative — making `Fact` generic
                        // over its key lifetime — would complicate the emitting
                        // path, which is the one that matters.
                        key: Box::leak(k.into_boxed_str()),
                        value: v,
                    })
                    .collect(),
            },
            // A parsed run's fitness is not reconstructed: nothing in `diff`
            // reads it, and a half-populated `Fitness` would be a value that
            // looks measured and is not. The provenance facts carry what a
            // reader needs.
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
        // An explicit JSON `null` is what `jnum` emits for a non-finite
        // measurement. Mapping it to NaN keeps that distinction — a metric that
        // could not be measured is not a metric that measured zero.
        .unwrap_or(f64::NAN);
    // Leaked for the same reason the key is: `Metric` holds `&'static str`
    // because the emitting side's units are literals, and this is a CLI that
    // reads at most two files before exiting.
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

// ---------------------------------------------------------------------------
// The comparison
// ---------------------------------------------------------------------------

/// What a metric did between two runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Moved the good way by more than its tolerance.
    Better,
    /// Moved the bad way by more than its tolerance.
    Worse,
    /// Moved less than its tolerance, or is not directional but moved.
    Noise,
    /// Informational and therefore never a claim either way. Reported so a
    /// changed sample count or query count is visible, since those change what
    /// the directional rows *mean*.
    Info,
    /// One side could not be measured (a non-finite value). Never a verdict:
    /// comparing against `NaN` is how a regression gets reported as an
    /// improvement.
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
    /// Row keys in `a` but not `b`. **Reported, never dropped**: a workload
    /// that stopped running must not read as one that did not change.
    pub only_in_a: Vec<String>,
    /// Row keys in `b` but not `a`.
    pub only_in_b: Vec<String>,
    /// `(fact, a, b)` for each [`HOST_CRITICAL_FACTS`] entry that differs.
    pub host_drift: Vec<(String, String, String)>,
    /// `(fact, a, b)` for each [`BUILD_CRITICAL_FACTS`] entry that differs.
    ///
    /// Non-empty means the two runs are **not comparable**; see
    /// [`Diff::comparable`]. Kept as a separate field rather than folded into
    /// `host_drift` so the difference between "read this carefully" and "this
    /// comparison does not exist" survives into every consumer.
    pub build_mismatch: Vec<(String, String, String)>,
}

impl Diff {
    /// Whether the two runs describe the same program built the same way.
    ///
    /// `false` means every delta below is arithmetic between two different
    /// questions. [`render`] prints the reason and **not** the table, and
    /// `bench_ab` exits non-zero — a refusal, not a caveat.
    #[must_use]
    pub fn comparable(&self) -> bool {
        self.build_mismatch.is_empty()
    }

    /// Whether any metric regressed beyond its tolerance.
    ///
    /// Meaningless — and never consulted — when [`Diff::comparable`] is false.
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
///
/// Matching is on `harness/workload/engine/point` plus the metric key, so a run
/// whose sweep changed produces `only_in_*` entries rather than a comparison
/// between two different measurements that happen to sit at the same index.
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

    // One walk, two lists. `absent` rather than skipping: a run file written
    // before a fact existed must read as a mismatch, not as agreement — the
    // whole point of a build fact is that its silence is not consent.
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
    // The direction is taken from the **baseline**, not from `b`. A change that
    // retyped a metric would otherwise be judged by its own new rules, which is
    // exactly the case a reviewer most wants flagged — and it shows up as a
    // mismatch here rather than as a silent re-interpretation.
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
        // `a == 0` lands here: a relative change against zero is undefined, and
        // the alternative — treating any move off zero as infinite regression —
        // fires on counters that were legitimately zero in the baseline.
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
///
/// The table is withheld rather than annotated. A banner above a screen of
/// per-row verdicts loses to the verdicts: somebody reads "+31% worse" and
/// carries that number into a commit message, and the fact that one run had the
/// crate boundary inlined away does not travel with it. Withholding the numbers
/// is the only presentation that cannot be misquoted.
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
        // A run file written before a build fact existed reads as `absent`, and
        // that lands here rather than being waved through — but it is not the
        // same news as a profile that changed, so it says so instead of leaving
        // a reader to infer a build change from a missing field.
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

    // Worse first: the reason anybody runs this is to find out whether
    // something got worse, and a regression twelve screens down is a regression
    // nobody read.
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
        // Keep the *tail*: the row key's distinguishing part (the sweep point)
        // is at the end, while the shared prefix is what every row has.
        let skip = s.chars().count() - (n - 1);
        format!("…{}", s.chars().skip(skip).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    // Same reasoning as `report.rs`'s test module: a failed assertion is the
    // intended failure mode here, and `panic!`/`expect` are what let a failure
    // name the row it came from rather than the line number it fired on.
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
        // The property that makes this tool worth running: two runs of the same
        // build must produce no verdict at all. A differ that reports change on
        // identical input teaches its reader to ignore it.
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
        // `p99_ns` rising is a regression; the same number under
        // `higher_is_better` is an improvement. Nothing about the key name says
        // which, which is the whole reason `Drift` is carried in the file.
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
        // A change that flipped a metric's direction must not be judged by its
        // own new rules — that is precisely the change a reviewer wants to see.
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
        // And the round-tripped run compares as unchanged against the original,
        // which is the property the A/B recipe actually depends on.
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
        // Host drift is a *warning*: the table is still printed, because a
        // ratio between two rows of the same run survives a different CPU.
        // This is the half that distinguishes it from a build mismatch below.
        assert!(d.comparable(), "a different CPU is not a build mismatch");
        assert!(
            render(&d).contains("row / metric"),
            "the table is still printed"
        );
    }

    /// A value for `key` guaranteed to differ from whatever *this* build
    /// recorded, chosen from the realistic alternatives rather than invented.
    ///
    /// **A test about build facts must not hardcode one.**
    /// `a_profile_that_changed_its_lto_without_changing_its_name_is_refused`
    /// originally set `build_lto` to `"thin"` and asserted a mismatch. That
    /// passed under `just test` — debug, where lto is off — and **failed under
    /// `just tf2-check`, which runs `--release`**: there the real value *is*
    /// `"thin"`, so the two agreed, no mismatch was detected, and the assertion
    /// fired. The bug was in the test, not the differ.
    ///
    /// It is worth naming what caught it: the failing configuration is
    /// `--features tf2 --release`, which only the container recipe compiles.
    /// `just test` builds default features in debug and cannot see it. That is
    /// the same shape as every other feature-gated hole this repository has
    /// found, and it is why the container gate is not optional.
    fn differing_value(r: &Run, key: &str) -> String {
        let real = r
            .provenance
            .facts
            .iter()
            .find(|f| f.key == key)
            .map_or("", |f| f.value.as_str());
        match key {
            // `lto` is a TOML scalar as written in the manifest, so a quoted
            // `"thin"` and a bare `false` are the two shapes in play.
            "build_lto" if real == "false" => "\"thin\"".to_owned(),
            "build_lto" => "false".to_owned(),
            // Profile *directory* names. `embedder` unless we are already there.
            _ if real == "embedder" => "release".to_owned(),
            _ => "embedder".to_owned(),
        }
    }

    /// A run taken at `--profile embedder` and a run taken at `--release`
    /// measure different programs — thin LTO inlines across the crate boundary
    /// the embedder profile leaves standing — so there is no comparison to make.
    ///
    /// Mutant (applied, observed): delete `"build_profile"` from
    /// [`BUILD_CRITICAL_FACTS`], leaving only `build_lto`. This test fails with
    /// `left: true, right: false` on the `comparable()` assertion, because the
    /// two synthetic runs differ only in the profile name.
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

        // **The numbers are withheld, not annotated.** A banner over a table of
        // verdicts loses to the table; somebody quotes the percentage.
        let text = render(&d);
        assert!(text.contains("REFUSED"), "{text}");
        assert!(
            !text.contains("row / metric"),
            "the delta table must not be printed for an incomparable pair:\n{text}"
        );
    }

    /// The other build fact, and the one that catches a profile whose *meaning*
    /// changed while its name did not — an edit to `[profile.embedder]`'s `lto`,
    /// say, which nobody would think to regenerate a comparison for.
    ///
    /// Mutant (applied, observed): delete `"build_lto"` from
    /// [`BUILD_CRITICAL_FACTS`]. This test fails on the `!d.comparable()`
    /// assertion; `two_runs_built_at_different_profiles_refuse_to_be_compared`
    /// above still passes, which is what makes the two rows independent.
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

    /// A run file written before `build_lto` existed must read as a mismatch,
    /// not as agreement. Silence is not consent for a build fact: the whole
    /// hazard is a number whose profile nobody wrote down.
    ///
    /// Mutant (applied, observed): change [`diff`]'s `drift` closure to skip a
    /// fact absent from either side (`if va.is_none() || vb.is_none() { return
    /// None }`). This test fails on `!d.comparable()`.
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
