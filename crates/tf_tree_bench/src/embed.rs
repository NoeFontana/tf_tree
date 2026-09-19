//! `docs/PHASE5.md` §9.2's embedding measurements: what the facade costs an
//! **embedder**. Two measurements answer two questions and are kept apart.
//!
//! | | question | how | status |
//! | --- | --- | --- | --- |
//! | **`embedding_cross_crate`** | what does *crossing the crate boundary* cost? | one build, one profile, two identical bodies — one in `tf_tree_bench`, one in `tf_tree_core` | §9.2's row, **gated at 5%** |
//! | **profile comparison** | what does *the embedder's own `[profile.*]`* cost? | one body, two builds: `[profile.embedder]` against `[profile.release]` | **exploratory**, `just embed-cost` only, never gated |
//!
//! # The gated row: one build, two crates
//!
//! This module's private `one` and `tf_tree_core::bench_probe::depth3_lookup` are
//! the same three lines with the same `#[inline(never)]`; the timing difference is
//! the crate boundary (`docs/API.md` §2.3 item 3).
//!
//! * The in-crate half must live in `tf_tree_core`, not the `tf_tree` facade.
//! * It must not be generic: a generic body is monomorphized in the caller.
//! * Both columns read one of seven pose components, which suits a ratio of
//!   identical shapes but can invert a marked-vs-unmarked comparison
//!   (`Plan::at_tagged`'s doc).
//! * `benches/lookup.rs` cannot serve: no in-crate column, and criterion does not
//!   pair two columns ([`Run::boundary_ratio`]).
//!
//! The gated row is read off the `[profile.embedder]` run only; under
//! `[profile.release]`'s thin LTO the boundary is erased, so that run is the
//! control (§9.2). Every duration is gated, not only the quotient.
//!
//! # The exploratory measurement: one crate position, two profiles
//!
//! [`Pair::profile_ratio`] divides the `[profile.embedder]` out-of-crate column by
//! the `[profile.release]` one (`docs/API.md` §2.3 item 2). Two processes seconds
//! apart, so it is not gated and does not enter `results.json` (§11.2).
//!
//! # Honesty (§9.3)
//!
//! * Both columns are timed back to back inside a round, so [`Run::verdict`] reads
//!   the band of per-round ratios and answers [`Verdict::Unresolved`] when it
//!   straddles the threshold.
//! * `build.rs` digests the measured sources into [`SOURCE_ID`]; [`Pair::load`]
//!   refuses two runs that disagree, or that are not one `embedder` and one
//!   `release` run (read from `OUT_DIR`).
//! * [`profile_settings_from_manifest`] reads `lto` and `codegen-units` from the
//!   workspace manifest, and a test asserts the profiles still say what this
//!   module claims.
//!
//! [`Plan::at`]: tf_tree::Plan::at

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};

use crate::report::Metric;

// The timing half is behind `embed-probe`; the JSON, gate arithmetic and report
// row stay unconditional (`bench_report` reads a pair it did not measure).
#[cfg(feature = "embed-probe")]
use std::hint::black_box;
#[cfg(feature = "embed-probe")]
use std::time::Instant;

#[cfg(feature = "embed-probe")]
use tf_tree::{Guard, Plan, Stamp};

/// `embed-cost.json` schema identifier. Bump on any consumer-visible change.
pub const SCHEMA: &str = "tf_tree.embed-cost/2";

/// The profile directory an embedder-profile run is built into.
pub const EMBEDDER_PROFILE: &str = "embedder";

/// The profile directory the reference run is built into.
pub const REFERENCE_PROFILE: &str = "release";

/// `docs/PHASE5.md` §9.2's gate on the ratio: 5%; also the tolerance on every
/// directional metric this row hands the regression gate ([`crate::baseline`]).
pub const GATE: f64 = 0.05;

/// Rounds timed per run. Each round times both columns, in that order.
pub const ROUNDS: usize = 9;

/// Sweeps over [`STAMPS`] stamps, per column, per round.
#[cfg(feature = "embed-probe")]
const SWEEPS: usize = 400;

/// Distinct query stamps, all off-grid on all three edges.
#[cfg(feature = "embed-probe")]
const STAMPS: usize = 1024;

/// Lookups run per column before timing starts.
#[cfg(feature = "embed-probe")]
const WARMUP: usize = 200_000;

/// The profile directory *this* binary was built into (see `build.rs`).
pub const PROFILE_DIR: &str = env!("TF_TREE_BENCH_PROFILE_DIR");

/// A digest of the sources that determine what this binary measures (see
/// `build.rs`).
pub const SOURCE_ID: &str = env!("TF_TREE_BENCH_SOURCE_ID");

/// What §9.2's 5% criterion says about a measured crate-boundary ratio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The whole observed band is inside `1.0 + `[`GATE`].
    Within,
    /// The whole observed band is outside it.
    Over,
    /// The band straddles the threshold; reported, not rounded to pass or fail.
    Unresolved,
}

impl Verdict {
    /// The JSON/CLI spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Within => "within",
            Verdict::Over => "over",
            Verdict::Unresolved => "unresolved",
        }
    }
}

/// One timed run: both columns, under one profile, from one binary.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    /// Profile directory this run's binary was compiled into.
    pub profile_dir: String,
    /// Digest of the source this run's binary was compiled from.
    pub source_id: String,
    /// Fastest round of the probe compiled in `tf_tree_bench`, ns per lookup.
    pub out_of_crate_ns: f64,
    /// Fastest round of the probe compiled in `tf_tree_core`, ns per lookup.
    pub in_crate_ns: f64,
    /// Median per-round `out_of_crate / in_crate`: paired, so machine noise cancels per round.
    pub boundary_ratio: f64,
    /// Smallest per-round ratio observed.
    pub ratio_lo: f64,
    /// Largest per-round ratio observed.
    pub ratio_hi: f64,
    /// `(slowest - fastest) / fastest` over the out-of-crate rounds.
    pub out_of_crate_spread: f64,
    /// `(slowest - fastest) / fastest` over the in-crate rounds.
    pub in_crate_spread: f64,
    /// Rounds timed.
    pub rounds: usize,
    /// Lookups per column per round.
    pub lookups_per_round: u64,
}

impl Run {
    /// `(ratio_hi - ratio_lo) / ratio_lo`: what the ratio can resolve.
    #[must_use]
    pub fn ratio_spread(&self) -> f64 {
        (self.ratio_hi - self.ratio_lo) / self.ratio_lo
    }

    /// §9.2's 5% criterion against the **observed band**: [`Verdict::Unresolved`]
    /// whenever `[ratio_lo, ratio_hi]` contains the threshold.
    #[must_use]
    pub fn verdict(&self) -> Verdict {
        let threshold = 1.0 + GATE;
        if self.ratio_hi <= threshold {
            Verdict::Within
        } else if self.ratio_lo > threshold {
            Verdict::Over
        } else {
            Verdict::Unresolved
        }
    }

    /// The §9.2 criterion as a line of prose, stating the measured value either way.
    #[must_use]
    pub fn verdict_line(&self) -> String {
        let (r, lo, hi) = (self.boundary_ratio, self.ratio_lo, self.ratio_hi);
        let pct = GATE * 100.0;
        match self.verdict() {
            Verdict::Within => format!(
                "{r:.3}x (rounds spanned {lo:.3}-{hi:.3}), within PHASE5 §9.2's {pct:.0}% gate"
            ),
            Verdict::Over => format!(
                "{r:.3}x (rounds spanned {lo:.3}-{hi:.3}), OVER PHASE5 §9.2's {pct:.0}% gate: a \
                 depth-3 lookup called from outside `tf_tree_core` costs {:.0}% more than the \
                 identical body called from inside it. The control run printed beside this one \
                 — the same two bodies under `lto = \"thin\"` — measures the boundary gone, so \
                 what is measured to close it is the embedder's own profile (`docs/API.md` \
                 §2.3 item 2). This run measures nothing about whether a different `#[inline]` \
                 placement would, and does not claim it either way",
                (r - 1.0) * 100.0
            ),
            Verdict::Unresolved => format!(
                "{r:.3}x, but the rounds spanned {lo:.3}-{hi:.3}, which straddles PHASE5 §9.2's \
                 {pct:.0}% threshold — this run cannot answer. A verdict here would be \
                 arithmetic on noise. Pin the run (`taskset`), quieten the host, or raise \
                 ROUNDS until the band clears the threshold"
            ),
        }
    }

    /// The row's metrics, in report order. All three durations are directional, not just the ratio.
    #[must_use]
    pub fn metrics(&self) -> Vec<Metric> {
        vec![
            Metric::new("boundary_ratio", self.boundary_ratio, "x").lower_is_better(GATE),
            Metric::new("out_of_crate_ns", self.out_of_crate_ns, "ns").lower_is_better(GATE),
            Metric::new("in_crate_ns", self.in_crate_ns, "ns").lower_is_better(GATE),
            Metric::new("gate_ratio", 1.0 + GATE, "x"),
            Metric::new("ratio_lo", self.ratio_lo, "x"),
            Metric::new("ratio_hi", self.ratio_hi, "x"),
            Metric::new("out_of_crate_spread", self.out_of_crate_spread, "fraction"),
            Metric::new("in_crate_spread", self.in_crate_spread, "fraction"),
            Metric::new(
                "lookups_per_round",
                self.lookups_per_round as f64,
                "lookups",
            ),
        ]
    }

    /// The `embed-cost.json` document, hand-written: the schema is a compatibility surface.
    #[must_use]
    pub fn to_json(&self) -> String {
        format!(
            "{{\n  \"schema\": \"{}\",\n  \"profile_dir\": \"{}\",\n  \
             \"source_id\": \"{}\",\n  \"out_of_crate_ns\": {:.3},\n  \
             \"in_crate_ns\": {:.3},\n  \"boundary_ratio\": {:.5},\n  \
             \"ratio_lo\": {:.5},\n  \"ratio_hi\": {:.5},\n  \
             \"out_of_crate_spread\": {:.5},\n  \"in_crate_spread\": {:.5},\n  \
             \"rounds\": {},\n  \"lookups_per_round\": {}\n}}\n",
            SCHEMA,
            self.profile_dir,
            self.source_id,
            self.out_of_crate_ns,
            self.in_crate_ns,
            self.boundary_ratio,
            self.ratio_lo,
            self.ratio_hi,
            self.out_of_crate_spread,
            self.in_crate_spread,
            self.rounds,
            self.lookups_per_round
        )
    }

    /// Parse one `embed-cost.json`.
    ///
    /// # Errors
    ///
    /// A schema mismatch, a missing field, or a non-finite / non-positive
    /// duration or ratio (a `0` would divide into an infinite ratio).
    pub fn from_json(text: &str) -> Result<Run> {
        let v: serde_json::Value = serde_json::from_str(text).context("parsing embed-cost json")?;
        let schema = v
            .get("schema")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow!("no `schema` field"))?;
        if schema != SCHEMA {
            bail!("embed-cost schema is `{schema}`, this build reads `{SCHEMA}`");
        }
        let text_field = |k: &str| -> Result<String> {
            v.get(k)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("no `{k}` field"))
        };
        let num = |k: &str| -> Result<f64> {
            v.get(k)
                .and_then(serde_json::Value::as_f64)
                .ok_or_else(|| anyhow!("no numeric `{k}` field"))
        };
        let positive = |k: &str| -> Result<f64> {
            let x = num(k)?;
            if !(x.is_finite() && x > 0.0) {
                bail!("{k} is {x}, which is not a duration or a ratio");
            }
            Ok(x)
        };
        Ok(Run {
            profile_dir: text_field("profile_dir")?,
            source_id: text_field("source_id")?,
            out_of_crate_ns: positive("out_of_crate_ns")?,
            in_crate_ns: positive("in_crate_ns")?,
            boundary_ratio: positive("boundary_ratio")?,
            ratio_lo: positive("ratio_lo")?,
            ratio_hi: positive("ratio_hi")?,
            out_of_crate_spread: num("out_of_crate_spread")?,
            in_crate_spread: num("in_crate_spread")?,
            rounds: num("rounds")? as usize,
            lookups_per_round: num("lookups_per_round")? as u64,
        })
    }
}

/// The two profile runs of the **exploratory** comparison; the gated row is [`Run`] alone.
#[derive(Debug, Clone)]
pub struct Pair {
    /// Built with cargo's `--release` defaults (`[profile.embedder]`).
    pub embedder: Run,
    /// Built with this workspace's `[profile.release]`.
    pub reference: Run,
}

impl Pair {
    /// Load `<dir>/embedder.json` and `<dir>/release.json`.
    ///
    /// # Errors
    ///
    /// Either file missing or unparseable; a file whose `profile_dir` is not the
    /// profile its name claims (two runs of one build give a ratio of 1.0); or
    /// two files built from different source.
    pub fn load(dir: &Path) -> Result<Pair> {
        let one = |name: &str, want: &str| -> Result<Run> {
            let path = dir.join(format!("{name}.json"));
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let run = Run::from_json(&text).with_context(|| format!("in {}", path.display()))?;
            if run.profile_dir != want {
                bail!(
                    "{} was built into the `{}` profile directory, not `{want}` — the two \
                     columns of this comparison must be two different builds of the same \
                     program",
                    path.display(),
                    run.profile_dir
                );
            }
            Ok(run)
        };
        let embedder = one("embedder", EMBEDDER_PROFILE)?;
        let reference = one("release", REFERENCE_PROFILE)?;
        if embedder.source_id != reference.source_id {
            bail!(
                "the two runs were built from different source ({} and {}), so their quotient \
                 is not a property of any one program — one half is stale. Re-run \
                 `just embed-cost`, which builds and runs both",
                embedder.source_id,
                reference.source_id
            );
        }
        Ok(Pair {
            embedder,
            reference,
        })
    }

    /// What the embedder's default `[profile.*]` costs, crate position held
    /// fixed: `embedder.out_of_crate_ns / reference.out_of_crate_ns`.
    /// Exploratory, never gated.
    #[must_use]
    pub fn profile_ratio(&self) -> f64 {
        self.embedder.out_of_crate_ns / self.reference.out_of_crate_ns
    }
}

/// Time both columns under this binary's profile, on [`crate::fixture`]'s tree
/// with stamps off-grid on all three dynamic edges (`docs/decisions/0013`).
///
/// # Errors
///
/// A fixture that cannot be built, a lookup that fails, or the two columns
/// disagreeing on the value they computed.
#[cfg(feature = "embed-probe")]
pub fn measure() -> Result<Run> {
    measure_with(ROUNDS, SWEEPS, WARMUP)
}

/// [`measure`] with the loop counts as parameters, for debug-build unit tests.
///
/// # Errors
///
/// As [`measure`].
#[cfg(feature = "embed-probe")]
pub fn measure_with(rounds: usize, sweeps: usize, warmup: usize) -> Result<Run> {
    use tf_tree::InterpPolicy;

    let tree = crate::fixture::build_tree_with(InterpPolicy::LerpSlerp)?;
    let (_writers, _pushed) = crate::fixture::spin_up(&tree)?;
    let target = tree
        .frame("imu_link")
        .map_err(|e| anyhow!("fixture frame `imu_link` is missing: {e:?}"))?;
    let source = tree
        .frame("map")
        .map_err(|e| anyhow!("fixture frame `map` is missing: {e:?}"))?;
    let plan = tree
        .plan(target, source)
        .map_err(|e| anyhow!("compiling the map <- imu_link plan: {e:?}"))?;
    let guard = tree.guard();

    let stamps: Vec<Stamp> = (0..STAMPS as i64)
        .map(|i| Stamp::from_nanos(stamp_ns(i)))
        .collect();

    // The bodies must agree before timing, or a fast denominator computing something else looks like a boundary cost.
    for &s in &stamps {
        let out = one(&plan, &guard, s);
        let inside = tf_tree_core::bench_probe::depth3_lookup(&plan, &guard, s);
        if !(out == inside) {
            bail!(
                "the out-of-crate and in-crate probes disagree at stamp {}: {out} vs {inside}. \
                 They are the same three lines, so this is not a rounding difference — one of \
                 them is not evaluating the plan this row is about",
                s.nanos()
            );
        }
    }

    let mut sink = 0.0f64;
    for i in 0..warmup {
        sink += one(&plan, &guard, stamps[i % STAMPS]);
        sink += tf_tree_core::bench_probe::depth3_lookup(&plan, &guard, stamps[i % STAMPS]);
    }

    let per_round = (sweeps * STAMPS) as f64;
    let mut out_ns = Vec::with_capacity(rounds);
    let mut in_ns = Vec::with_capacity(rounds);
    let mut ratios = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        // Fixed order, not alternated: alternation moves the cold-predictor cost between columns.
        let t0 = Instant::now();
        for _ in 0..sweeps {
            for &s in &stamps {
                sink += one(black_box(&plan), black_box(&guard), black_box(s));
            }
        }
        let o = t0.elapsed().as_secs_f64() * 1e9 / per_round;

        let t1 = Instant::now();
        for _ in 0..sweeps {
            for &s in &stamps {
                sink += tf_tree_core::bench_probe::depth3_lookup(
                    black_box(&plan),
                    black_box(&guard),
                    black_box(s),
                );
            }
        }
        let i = t1.elapsed().as_secs_f64() * 1e9 / per_round;

        black_box(sink);
        out_ns.push(o);
        in_ns.push(i);
        ratios.push(o / i);
    }

    // A failed lookup returns NaN and poisons the sum: the run is discarded.
    if sink.is_nan() {
        bail!(
            "a lookup failed during the probe (the accumulator went NaN), so the timing \
             describes an error path rather than a depth-3 evaluation"
        );
    }

    Ok(Run {
        profile_dir: PROFILE_DIR.to_owned(),
        source_id: SOURCE_ID.to_owned(),
        out_of_crate_ns: min_of(&out_ns),
        in_crate_ns: min_of(&in_ns),
        boundary_ratio: median_of(&ratios),
        ratio_lo: min_of(&ratios),
        ratio_hi: max_of(&ratios),
        out_of_crate_spread: spread_of(&out_ns),
        in_crate_spread: spread_of(&in_ns),
        rounds,
        lookups_per_round: (sweeps * STAMPS) as u64,
    })
}

/// Smallest element. `f64::MAX` on an empty slice, which no caller produces.
#[cfg(feature = "embed-probe")]
fn min_of(v: &[f64]) -> f64 {
    v.iter().copied().fold(f64::MAX, f64::min)
}

/// Largest element. `0.0` on an empty slice, which no caller produces.
#[cfg(feature = "embed-probe")]
fn max_of(v: &[f64]) -> f64 {
    v.iter().copied().fold(0.0, f64::max)
}

/// `(max - min) / min`.
#[cfg(feature = "embed-probe")]
fn spread_of(v: &[f64]) -> f64 {
    (max_of(v) - min_of(v)) / min_of(v)
}

/// Middle element by value; even lengths take the upper middle (conservative).
#[cfg(feature = "embed-probe")]
fn median_of(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    s[s.len() / 2]
}

/// The `i`th query stamp: an offset from `NOW_NS` plus a prime step keeps the
/// sweep off every grid; 1024 steps stay inside every ring.
#[cfg(feature = "embed-probe")]
const fn stamp_ns(i: i64) -> i64 {
    crate::fixture::NOW_NS - 3_700_000 - i * 9_631
}

/// One lookup per non-inlinable call, **compiled in `tf_tree_bench`**: the
/// numerator of §9.2's ratio, byte-identical to
/// `tf_tree_core::bench_probe::depth3_lookup`. The error arm returns `NaN`, which
/// [`measure_with`] checks.
#[cfg(feature = "embed-probe")]
#[inline(never)]
fn one(plan: &Plan, g: &Guard, s: Stamp) -> f64 {
    match plan.at(g, s) {
        Ok(iso) => iso.t.x,
        Err(_) => f64::NAN,
    }
}

/// `(lto, codegen-units)` as the workspace manifest declares them for `profile`.
///
/// The report's `EMBEDDING_NOTE` prose is checked by
/// `the_row_note_states_the_settings_the_manifest_declares` and
/// `the_two_profiles_still_say_what_this_module_says_they_say`. A small TOML
/// reader; the crate has no TOML dependency.
///
/// # Errors
///
/// The section missing, or either key missing from it.
pub fn profile_settings_from_manifest(manifest: &str, profile: &str) -> Result<(String, String)> {
    Ok((
        profile_key(manifest, profile, "lto")?,
        profile_key(manifest, profile, "codegen-units")?,
    ))
}

/// One key out of one `[profile.*]` section of the workspace manifest.
///
/// # Errors
///
/// The section missing, or the key missing from it (reported differently by
/// [`lto_for_profile_dir`]).
pub fn profile_key(manifest: &str, profile: &str, key: &str) -> Result<String> {
    let header = format!("[profile.{profile}]");
    let body = manifest
        .split(&header)
        .nth(1)
        .ok_or_else(|| anyhow!("the workspace manifest has no {header}"))?;
    // Stop at the next section header.
    let body = body.split("\n[").next().unwrap_or(body);
    body.lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix(key)?.trim().strip_prefix('=').map(str::trim))
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("{header} declares no `{key}`"))
}

/// Whether the workspace manifest declares a `[profile.<profile>]` at all.
#[must_use]
pub fn profile_section_exists(manifest: &str, profile: &str) -> bool {
    manifest.contains(&format!("[profile.{profile}]"))
}

/// The `lto` setting behind a *profile directory*, for a provenance block.
///
/// `debug/` maps to `dev`; `release/` is shared with `[profile.bench]`, so this
/// reports `[profile.release]`'s setting
/// (`the_two_profiles_that_share_the_release_directory_agree_about_lto`). Returns
/// a `String`: `lto` may be `false`, `true`, `"thin"` or `"fat"`, and an unknown
/// is spelled `unknown (…)`. An undeclared `lto` reports cargo's default, saying
/// so. `inherits` is followed (bounded walk of 8).
#[must_use]
pub fn lto_for_profile_dir(manifest: &str, profile_dir: &str) -> String {
    let mut profile = if profile_dir == "debug" {
        "dev".to_owned()
    } else {
        profile_dir.to_owned()
    };
    let start = profile.clone();
    // Bounded: a longer chain is a malformed manifest and is reported.
    for _ in 0..8 {
        if let Ok(v) = profile_key(manifest, &profile, "lto") {
            return if profile == start {
                v
            } else {
                format!("{v} (inherited from [profile.{profile}])")
            };
        }
        if !profile_section_exists(manifest, &profile) {
            return format!("unknown (the workspace manifest has no [profile.{profile}])");
        }
        let Ok(parent) = profile_key(manifest, &profile, "inherits") else {
            return format!("false (cargo's default; [profile.{profile}] declares no `lto`)");
        };
        let parent = parent.trim_matches('"').to_owned();
        if parent == profile {
            return format!("unknown ([profile.{profile}] inherits itself)");
        }
        profile = parent;
    }
    format!("unknown ([profile.{start}]'s `inherits` chain does not terminate in 8 steps)")
}

#[cfg(test)]
mod tests {
    // A failed assertion is the intended failure mode here.
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;
    use crate::report::Drift;

    /// To the precision [`Run::to_json`] emits, so a fixture round-trips exactly.
    fn round5(x: f64) -> f64 {
        (x * 1e5).round() / 1e5
    }

    fn run(dir: &str, out_ns: f64, in_ns: f64) -> Run {
        let r = round5(out_ns / in_ns);
        Run {
            profile_dir: dir.to_owned(),
            source_id: "0123456789abcdef".to_owned(),
            out_of_crate_ns: out_ns,
            in_crate_ns: in_ns,
            boundary_ratio: r,
            ratio_lo: round5(r * 0.999),
            ratio_hi: round5(r * 1.001),
            out_of_crate_spread: 0.004,
            in_crate_spread: 0.004,
            rounds: ROUNDS,
            // `SWEEPS * STAMPS`, spelled out: they exist only under `embed-probe`.
            lookups_per_round: 409_600,
        }
    }

    fn manifest() -> String {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        std::fs::read_to_string(root.join("Cargo.toml")).expect("workspace manifest")
    }

    fn write_pair(tag: &str, embedder: &Run, reference: &Run) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tf-tree-embed-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("tmpdir");
        std::fs::write(dir.join("embedder.json"), embedder.to_json()).expect("write");
        std::fs::write(dir.join("release.json"), reference.to_json()).expect("write");
        dir
    }

    #[test]
    fn json_round_trips() {
        let r = run(EMBEDDER_PROFILE, 239.712, 199.4);
        let back = Run::from_json(&r.to_json()).expect("parse");
        assert_eq!(back, r);
    }

    #[test]
    fn a_foreign_schema_is_refused() {
        let text = run(EMBEDDER_PROFILE, 240.0, 200.0)
            .to_json()
            .replace(SCHEMA, "tf_tree.embed-cost/99");
        assert!(Run::from_json(&text).is_err());
    }

    #[test]
    fn a_zero_duration_is_refused_rather_than_dividing_into_infinity() {
        let text = run(EMBEDDER_PROFILE, 240.0, 200.0)
            .to_json()
            .replace("\"in_crate_ns\": 200.000", "\"in_crate_ns\": 0.000");
        assert!(Run::from_json(&text).is_err());
    }

    #[test]
    fn two_runs_of_the_same_build_are_refused() {
        let dir = write_pair(
            "same-build",
            &run(EMBEDDER_PROFILE, 240.0, 200.0),
            &run(EMBEDDER_PROFILE, 240.0, 200.0),
        );
        let err = Pair::load(&dir).expect_err("a same-build pair must be refused");
        assert!(
            format!("{err:#}").contains("two different builds"),
            "unexpected error: {err:#}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn two_runs_built_from_different_source_are_refused() {
        let mut stale = run(REFERENCE_PROFILE, 200.0, 195.0);
        stale.source_id = "fedcba9876543210".to_owned();
        let dir = write_pair("stale", &run(EMBEDDER_PROFILE, 240.0, 200.0), &stale);
        let err = Pair::load(&dir).expect_err("a stale half must be refused");
        assert!(
            format!("{err:#}").contains("different source"),
            "unexpected error: {err:#}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The build script must produce a digest, not the empty-set sentinel.
    #[test]
    fn this_build_knows_what_source_it_came_from() {
        assert_ne!(SOURCE_ID, "unknown", "build.rs found no source to digest");
        assert_eq!(SOURCE_ID.len(), 16, "source id is not a 64-bit digest");
    }

    /// The exploratory profile comparison, not the gated ratio.
    #[test]
    fn the_profile_ratio_divides_the_two_out_of_crate_columns() {
        let p = Pair {
            embedder: run(EMBEDDER_PROFILE, 240.0, 200.0),
            reference: run(REFERENCE_PROFILE, 192.0, 190.0),
        };
        assert!((p.profile_ratio() - 1.25).abs() < 1e-12);
    }

    #[test]
    fn a_band_that_straddles_the_threshold_is_unresolved_not_a_verdict() {
        let mut r = run(EMBEDDER_PROFILE, 104.0, 100.0);
        r.ratio_lo = 1.01;
        r.ratio_hi = 1.09;
        assert_eq!(r.verdict(), Verdict::Unresolved);
        assert!(
            r.verdict_line().contains("cannot answer"),
            "{}",
            r.verdict_line()
        );

        r.ratio_lo = 1.039;
        r.ratio_hi = 1.041;
        assert_eq!(r.verdict(), Verdict::Within);
    }

    #[test]
    fn the_gate_boundary_is_five_percent() {
        let mut inside = run(EMBEDDER_PROFILE, 104.9, 100.0);
        inside.ratio_lo = 1.049;
        inside.ratio_hi = 1.049;
        let mut outside = run(EMBEDDER_PROFILE, 105.1, 100.0);
        outside.ratio_lo = 1.051;
        outside.ratio_hi = 1.051;
        assert_eq!(inside.verdict(), Verdict::Within, "4.9% must be inside");
        assert_eq!(outside.verdict(), Verdict::Over, "5.1% must be outside");
        assert!(outside.verdict_line().contains("OVER"));
    }

    /// The failure line must not repeat the refuted claim that no `#[inline]` placement closes the gap.
    #[test]
    fn the_failure_line_does_not_claim_inline_placement_cannot_help() {
        let mut over = run(EMBEDDER_PROFILE, 124.0, 100.0);
        over.ratio_lo = 1.23;
        over.ratio_hi = 1.25;
        let line = over.verdict_line();
        assert!(
            !line.contains("placement closes"),
            "the verdict repeats a claim this row's own toggle refutes: {line}"
        );
        assert!(
            line.contains("control run"),
            "the verdict must cite the control it actually measured: {line}"
        );
    }

    #[test]
    fn every_duration_this_row_reports_is_gated() {
        let m = run(EMBEDDER_PROFILE, 240.0, 200.0).metrics();
        for key in ["boundary_ratio", "out_of_crate_ns", "in_crate_ns"] {
            let got = m.iter().find(|m| m.key == key).expect(key);
            assert_eq!(got.drift, Drift::LowerIsBetter, "`{key}` must be gated");
            assert!(
                (got.tolerance - GATE).abs() < 1e-12,
                "`{key}` must be gated at PHASE5 §9.2's 5%"
            );
        }
    }

    /// The row states what the two profiles are; this makes that true.
    #[test]
    fn the_two_profiles_still_say_what_this_module_says_they_say() {
        let m = manifest();
        assert_eq!(
            profile_settings_from_manifest(&m, EMBEDDER_PROFILE).expect("embedder profile"),
            ("false".to_owned(), "16".to_owned()),
            "[profile.embedder] must stay cargo's --release defaults"
        );
        assert_eq!(
            profile_settings_from_manifest(&m, REFERENCE_PROFILE).expect("release profile"),
            ("\"thin\"".to_owned(), "1".to_owned()),
            "[profile.release] must stay this workspace's own"
        );
    }

    #[test]
    fn a_missing_profile_is_an_error_not_a_default() {
        assert!(profile_settings_from_manifest(&manifest(), "no-such-profile").is_err());
    }

    /// A profile *directory* is not a profile *name*; the two places they differ.
    #[test]
    fn a_profile_directory_maps_to_the_section_that_built_it() {
        let m = manifest();
        assert_eq!(lto_for_profile_dir(&m, EMBEDDER_PROFILE), "false");
        assert_eq!(lto_for_profile_dir(&m, REFERENCE_PROFILE), "\"thin\"");
        assert_eq!(
            lto_for_profile_dir(&m, "debug"),
            "false (cargo's default; [profile.dev] declares no `lto`)"
        );
        let unknown = lto_for_profile_dir(&m, "no-such-dir");
        assert!(unknown.starts_with("unknown"), "{unknown}");
    }

    #[test]
    fn the_two_profiles_that_share_the_release_directory_agree_about_lto() {
        let m = manifest();
        assert_eq!(
            profile_key(&m, "bench", "lto").expect("[profile.bench] lto"),
            profile_key(&m, REFERENCE_PROFILE, "lto").expect("[profile.release] lto"),
            "[profile.bench] and [profile.release] share target/release/, so a binary \
             built into it cannot say which one it came from. While they agree about \
             `lto` that does not matter; the moment they disagree, `lto_for_profile_dir` \
             reports the wrong one and says nothing about it"
        );
    }

    /// A profile that declares no `lto` but declares `inherits` gets its parent's.
    #[test]
    fn a_profile_with_no_lto_of_its_own_reports_the_one_it_inherits() {
        let m = "[profile.base]\nlto = \"thin\"\n\n\
                 [profile.child]\ninherits = \"base\"\ncodegen-units = 1\n\n\
                 [profile.grandchild]\ninherits = \"child\"\n\n\
                 [profile.orphan]\ninherits = \"nowhere\"\n\n\
                 [profile.ouroboros]\ninherits = \"ouroboros\"\n\n\
                 [profile.plain]\ncodegen-units = 4\n";

        assert_eq!(
            lto_for_profile_dir(m, "child"),
            "\"thin\" (inherited from [profile.base])"
        );
        assert_eq!(
            lto_for_profile_dir(m, "grandchild"),
            "\"thin\" (inherited from [profile.base])"
        );
        assert_eq!(lto_for_profile_dir(m, "base"), "\"thin\"");
        assert!(lto_for_profile_dir(m, "plain").starts_with("false (cargo's default;"));
        assert!(lto_for_profile_dir(m, "orphan").starts_with("unknown ("));
        assert!(lto_for_profile_dir(m, "ouroboros").starts_with("unknown ("));
        assert!(lto_for_profile_dir(m, "absent").starts_with("unknown ("));
    }

    /// `[profile.profiling]` must still share `release`'s codegen (`docs/benchmarks/tf2.md`); compares two manifest reads.
    #[test]
    fn the_profiling_profile_reports_the_reference_profiles_lto() {
        let m = manifest();
        let reference = profile_key(&m, REFERENCE_PROFILE, "lto").expect("[profile.release] lto");
        assert_eq!(
            lto_for_profile_dir(&m, "profiling"),
            format!("{reference} (inherited from [profile.{REFERENCE_PROFILE}])"),
            "[profile.profiling] is the control that isolates `lto` from `debuginfo` in \
             `docs/benchmarks/tf2.md`'s profile table. If it stops inheriting \
             [profile.{REFERENCE_PROFILE}]'s `lto`, that control stops controlling for \
             anything and the table's third row has to be re-taken or dropped"
        );
    }

    /// The reader must not read a later section's keys as this profile's.
    #[test]
    fn a_profile_does_not_borrow_the_next_profiles_keys() {
        let m = "[profile.a]\nlto = false\n\n[profile.b]\nlto = true\ncodegen-units = 16\n";
        assert!(profile_settings_from_manifest(m, "a").is_err());
    }

    /// The probe has to actually run: the agreement check catches one broken column, the 20 ns lower bound both.
    #[cfg(feature = "embed-probe")]
    #[test]
    fn the_probe_measures_a_working_depth_three_lookup() {
        let r = measure_with(2, 1, 64).expect("probe");
        assert_eq!(r.profile_dir, PROFILE_DIR);
        for ns in [r.out_of_crate_ns, r.in_crate_ns] {
            assert!(ns > 20.0 && ns < 1_000_000.0, "implausible ns/lookup: {ns}");
        }
        assert_eq!(r.lookups_per_round, STAMPS as u64);
        assert!(r.ratio_lo <= r.boundary_ratio && r.boundary_ratio <= r.ratio_hi);
    }

    /// Every stamp the probe queries falls strictly between two knots on every dynamic edge (`docs/decisions/0013`).
    #[cfg(feature = "embed-probe")]
    #[test]
    fn every_probe_stamp_is_off_grid_on_every_edge() {
        for period in [1_000_000i64, 5_000_000, 20_000_000] {
            for i in 0..STAMPS as i64 {
                let t = stamp_ns(i);
                assert!(
                    t % period != 0,
                    "stamp {t} lands on the {period} ns grid, so that edge does not interpolate"
                );
            }
        }
    }
}
