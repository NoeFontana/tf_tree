//! `docs/PHASE5.md` §9.2's last row: what the facade costs an **embedder**.
//!
//! ```text
//! | Facade `Plan::at` from a separate crate vs in-crate, depth 3 | ratio, gated at 5% |
//! ```
//!
//! `docs/API.md` §2.3 makes the row normative and says why it is not covered by
//! anything already here: `PHASE4.md` §7 gates the C ABI at 5% against native
//! **in-crate** Rust, and nothing gates native *out-of-crate* Rust, which is
//! what a user's node links.
//!
//! # The profile is the measurement
//!
//! `Plan::at` sits across a crate boundary from every consumer and five
//! functions on the evaluate path carry `#[inline]`. What those attributes buy
//! depends on the **downstream** profile, and this workspace's
//! `[profile.release]` sets `lto = "thin", codegen-units = 1` — so every latency
//! number this repository publishes is taken under whole-program optimisation,
//! and a user's `cargo build --release` is not. §9.2 says so in as many words:
//! *report it with the embedder's default profile, not this workspace's.*
//!
//! So the row is **one program, built twice**:
//!
//! | column | profile | what it is |
//! | --- | --- | --- |
//! | embedder | `[profile.embedder]` — `lto = false`, `codegen-units = 16` | cargo's `--release` defaults, i.e. a user's node |
//! | reference | `[profile.release]` — `lto = "thin"`, `codegen-units = 1` | this workspace's own, where the boundary is erased at link time |
//!
//! and the reported number is `embedder_ns / reference_ns`.
//!
//! ## What "in-crate" means here, precisely, and what it does not
//!
//! The reference column is **not** a probe compiled inside the engine. It is the
//! same out-of-crate source with the crate boundary erased by LTO. That is a
//! deliberate substitution, and these are the numbers behind it — one-off,
//! hand-built, throwaway probes, measured by the method under *Provenance*
//! below and **not** shipped:
//!
//! | probe | ns/lookup |
//! | --- | --- |
//! | out-of-crate (this row's embedder column) | 243.6 |
//! | in-crate in **`tf_tree`** | 241.5 |
//! | in-crate in **`tf_tree_core::plan`** | 199.4 |
//! | out-of-crate under the reference profile (this row's other column) | 194.9 |
//!
//! Two things follow, and the first is why the strict version of this row is not
//! cheap. **A probe placed in the facade is not in-crate**: `Plan::at` and
//! `fold_at` both live in `tf_tree_core`, one crate below, so `tf_tree`'s own
//! `src/` measured the same 1% as an outside caller and a row built on it would
//! report 1.00× for ever. A probe that is genuinely in-crate has to live in
//! `tf_tree_core::plan`, the module that defines the fold — and putting
//! benchmark code in the engine crate is a change this row does not need in
//! order to be useful.
//!
//! Second, **the substitution is close and it errs against us**: the crate
//! boundary alone is 243.6/199.4 = **1.22×**, and this row's LTO reference gives
//! **1.25×**. The reported ratio is the *larger* of the two, not the smaller,
//! and it is the one an embedder can act on — `lto = "thin"` in *their* profile
//! is what collapses it, which is exactly the guidance `docs/API.md` §2.3 item 2
//! puts in the crate docs.
//!
//! ## Exactly one loop shape, and that is a measurement too
//!
//! [`measure`] times one lookup per non-inlinable call — `docs/API.md` R1's
//! tier-3 "`at` in the loop", and the shape the hand-built probe behind §2.3
//! item 1 used, so the numbers are comparable to the tables there.
//!
//! A second shape was written (a whole 1024-stamp sweep inside one
//! `#[inline(never)]` call) and **removed**, because carrying both in one binary
//! changes the answer: with both present the sweep measured 199 ns under the
//! reference profile, and alone it measured 116 ns — a 72% swing in a number
//! nothing in the engine had touched. Whatever else the second shape would have
//! reported, it would have reported it about a binary that no longer existed
//! once it was added. If a batch shape is ever wanted it needs its **own**
//! binary, not another function in this one. It is also why no `--compare` run
//! may mix numbers from binaries built out of different source: the ratio is a
//! property of one program, twice.
//!
//! # Honesty (§9.3)
//!
//! * Both runs are pinned by the recipe and take best-of-[`ROUNDS`]; each
//!   reports the spread across rounds, and the report prints it. A ratio whose
//!   two halves are each unstable to several percent is not a 5% measurement,
//!   and the reader is given what they need to see that.
//! * The profile a run was built under is read out of `OUT_DIR` by this crate's
//!   `build.rs` — a fact about where the object files went — not passed in on
//!   the command line. [`Pair::load`] **refuses** a pair that is not one
//!   `embedder` run and one `release` run, so a report cannot be assembled from
//!   two runs of the same build.
//! * [`profile_settings_from_manifest`] reads `lto` and `codegen-units` back out
//!   of the workspace manifest, and a test asserts the two profiles still say
//!   what this module claims they say.
//!
//! # Provenance of the numbers quoted above
//!
//! Every figure in this file was measured on **2026-08-02**, on the development
//! host — an 8-vCPU AMD EPYC-Milan VM (4 physical cores, SMT on, no frequency
//! governor exposed, load average ~2.5) — with `taskset -c 2` and best-of-9
//! rounds of 409 600 lookups. That host **fails** [`crate::report::Fitness`], so
//! none of them is a claim in the §9.3 sense and the row comes out
//! `unavailable` here; they are quoted to justify a design decision, which is a
//! use a fitness-failing host is good enough for.
//!
//! Run-to-run, the shipped pair moved between 1.236× and 1.249× across two
//! consecutive `just embed-cost` runs, and the per-round spread inside a single
//! run reached 19% on the noisier of the two. **That is the resolution of this
//! measurement on this host: about a percent on the ratio, not a tenth of one.**
//! A 5% criterion is comfortably above it and a 1% one would not be.

use std::hint::black_box;
use std::path::Path;
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use tf_tree::{Guard, InterpPolicy, Plan, Stamp};

use crate::report::Metric;

/// `embed-cost.json` schema identifier. Bump on any consumer-visible change.
pub const SCHEMA: &str = "tf_tree.embed-cost/1";

/// The profile directory an embedder-profile run is built into.
pub const EMBEDDER_PROFILE: &str = "embedder";

/// The profile directory the reference run is built into.
pub const REFERENCE_PROFILE: &str = "release";

/// `docs/PHASE5.md` §9.2's gate on the ratio: 5%.
///
/// Used two ways, and they are not the same check:
///
/// * as an **absolute** criterion — §9.2 words it exactly as `PHASE4.md` §7
///   words the C ABI's, so [`Pair::verdict`] states whether the measured ratio
///   is within `1.0 + GATE`, and `docs/API.md` §2.3 records what it currently
///   is;
/// * as the **tolerance** on every directional metric this row hands the
///   regression gate ([`crate::baseline`]), which is the form that fires on a
///   change rather than on a standing cost.
pub const GATE: f64 = 0.05;

/// Rounds timed per run; the reported number is the fastest.
pub const ROUNDS: usize = 9;

/// Lookups per round: `SWEEPS` passes over [`STAMPS`] stamps.
const SWEEPS: usize = 400;

/// Distinct query stamps, all off-grid on all three edges.
const STAMPS: usize = 1024;

/// Lookups run before timing starts.
const WARMUP: usize = 200_000;

/// The profile directory *this* binary was built into (see `build.rs`).
pub const PROFILE_DIR: &str = env!("TF_TREE_BENCH_PROFILE_DIR");

/// One timed run of the probe, under one profile.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    /// Profile directory this run's binary was compiled into.
    pub profile_dir: String,
    /// Fastest round, in nanoseconds per lookup.
    pub ns_per_lookup: f64,
    /// `(slowest - fastest) / fastest`, over the timed rounds.
    pub spread: f64,
    /// Rounds timed.
    pub rounds: usize,
    /// Lookups per round.
    pub lookups_per_round: u64,
}

impl Run {
    /// The `embed-cost.json` document, hand-written for the reason
    /// [`crate::report`] gives: the schema is a compatibility surface, and a
    /// rename should be an edit rather than a side effect of a `#[derive]`.
    #[must_use]
    pub fn to_json(&self) -> String {
        format!(
            "{{\n  \"schema\": \"{}\",\n  \"profile_dir\": \"{}\",\n  \
             \"ns_per_lookup\": {:.3},\n  \"spread\": {:.5},\n  \
             \"rounds\": {},\n  \"lookups_per_round\": {}\n}}\n",
            SCHEMA,
            self.profile_dir,
            self.ns_per_lookup,
            self.spread,
            self.rounds,
            self.lookups_per_round
        )
    }

    /// Parse one `embed-cost.json`.
    ///
    /// # Errors
    ///
    /// A schema mismatch, a missing field, or a non-finite / non-positive
    /// duration. The last one matters: a `0` here would divide into an infinite
    /// ratio and a gate that never fires.
    pub fn from_json(text: &str) -> Result<Run> {
        let v: serde_json::Value = serde_json::from_str(text).context("parsing embed-cost json")?;
        let schema = v
            .get("schema")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow!("no `schema` field"))?;
        if schema != SCHEMA {
            bail!("embed-cost schema is `{schema}`, this build reads `{SCHEMA}`");
        }
        let num = |k: &str| -> Result<f64> {
            v.get(k)
                .and_then(serde_json::Value::as_f64)
                .ok_or_else(|| anyhow!("no numeric `{k}` field"))
        };
        let ns = num("ns_per_lookup")?;
        if !(ns.is_finite() && ns > 0.0) {
            bail!("ns_per_lookup is {ns}, which is not a duration");
        }
        Ok(Run {
            profile_dir: v
                .get("profile_dir")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow!("no `profile_dir` field"))?
                .to_owned(),
            ns_per_lookup: ns,
            spread: num("spread")?,
            rounds: num("rounds")? as usize,
            lookups_per_round: num("lookups_per_round")? as u64,
        })
    }
}

/// The two runs the row is made of.
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
    /// Either file missing or unparseable, or — the check worth having — a file
    /// whose `profile_dir` is not the profile its name claims. Two runs of the
    /// same build would produce a ratio of 1.0 and a green row that measured
    /// nothing, which is the failure mode this whole artifact is written
    /// against.
    pub fn load(dir: &Path) -> Result<Pair> {
        let one = |name: &str, want: &str| -> Result<Run> {
            let path = dir.join(format!("{name}.json"));
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let run = Run::from_json(&text).with_context(|| format!("in {}", path.display()))?;
            if run.profile_dir != want {
                bail!(
                    "{} was built into the `{}` profile directory, not `{want}` — the two \
                     columns of this row must be two different builds of the same program",
                    path.display(),
                    run.profile_dir
                );
            }
            Ok(run)
        };
        Ok(Pair {
            embedder: one("embedder", EMBEDDER_PROFILE)?,
            reference: one("release", REFERENCE_PROFILE)?,
        })
    }

    /// `embedder_ns / reference_ns`: what an embedder's default build pays.
    #[must_use]
    pub fn ratio(&self) -> f64 {
        self.embedder.ns_per_lookup / self.reference.ns_per_lookup
    }

    /// Whether the ratio is inside §9.2's 5%.
    #[must_use]
    pub fn within_gate(&self) -> bool {
        self.ratio() <= 1.0 + GATE
    }

    /// The §9.2 criterion as a line of prose, stating the measured value either
    /// way.
    ///
    /// Deliberately not an exit code. `PHASE4.md` §7's C ABI gate is reported
    /// the same way by `crates/tf_tree_c/examples/abi_cost.rs`, and the standing
    /// cost of a profile an embedder chose is not something a build of *this*
    /// repository can fix by failing.
    #[must_use]
    pub fn verdict(&self) -> String {
        let r = self.ratio();
        if self.within_gate() {
            format!("{r:.3}x, within PHASE5 §9.2's {:.0}% gate", GATE * 100.0)
        } else {
            format!(
                "{r:.3}x, OVER PHASE5 §9.2's {:.0}% gate: a depth-3 lookup costs an embedder \
                 building with cargo's `--release` defaults {:.0}% more than the same program \
                 built with this workspace's `lto = \"thin\", codegen-units = 1`. No \
                 `#[inline]` placement closes that; the embedder's profile does, which is what \
                 `docs/API.md` §2.3 item 2 puts in the crate docs",
                GATE * 100.0,
                (r - 1.0) * 100.0
            )
        }
    }

    /// The row's metrics, in report order.
    ///
    /// **All three durations are directional, not just the ratio §9.2 names.** A
    /// row whose only gated number is a quotient passes a change that moves both
    /// halves the same way — and that is not hypothetical here: dropping
    /// `#[inline]` from `Plan::fold_at` moves this ratio *down* (the embedder's
    /// build gets faster, ours gets slower), so a ratio-only gate would read a
    /// 6.8% regression in the number every other benchmark in this repository
    /// reports as an improvement.
    #[must_use]
    pub fn metrics(&self) -> Vec<Metric> {
        vec![
            Metric::new("ratio", self.ratio(), "x").lower_is_better(GATE),
            Metric::new("embedder_ns", self.embedder.ns_per_lookup, "ns").lower_is_better(GATE),
            Metric::new("reference_ns", self.reference.ns_per_lookup, "ns").lower_is_better(GATE),
            Metric::new("gate_ratio", 1.0 + GATE, "x"),
            Metric::new("embedder_spread", self.embedder.spread, "fraction"),
            Metric::new("reference_spread", self.reference.spread, "fraction"),
            Metric::new(
                "lookups_per_round",
                self.embedder.lookups_per_round as f64,
                "lookups",
            ),
        ]
    }
}

/// Time the depth-3 out-of-crate lookup, under whatever profile this binary was
/// built with.
///
/// The fixture is [`crate::fixture`]'s, so the tree is the same one every other
/// benchmark here measures. The stamps are **off-grid on all three dynamic
/// edges**, which `docs/decisions/0013` shows the Phase 1 lookup benchmark was
/// not: an on-grid stamp never runs `I::eval`, so it measures a lookup with the
/// interpolation removed.
///
/// # Errors
///
/// A fixture that cannot be built or a lookup that fails — either means the
/// probe measured something other than a working depth-3 evaluation.
pub fn measure() -> Result<Run> {
    measure_with(ROUNDS, SWEEPS, WARMUP)
}

/// [`measure`], with the loop counts as parameters.
///
/// Exists so the unit test can run the *same* code in a debug build without
/// spending minutes on it: at [`ROUNDS`] × [`SWEEPS`] the probe is about a
/// second in a release build and two orders of magnitude worse under `cargo
/// nextest`, and a test that is too slow to run is a test that gets `#[ignore]`d.
///
/// # Errors
///
/// As [`measure`].
pub fn measure_with(rounds: usize, sweeps: usize, warmup: usize) -> Result<Run> {
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

    let mut sink = 0.0f64;
    for i in 0..warmup {
        sink += one(&plan, &guard, stamps[i % STAMPS]);
    }

    let mut best = f64::MAX;
    let mut worst = 0.0f64;
    for _ in 0..rounds {
        let t0 = Instant::now();
        for _ in 0..sweeps {
            for &s in &stamps {
                sink += one(black_box(&plan), black_box(&guard), black_box(s));
            }
        }
        let ns = t0.elapsed().as_secs_f64() * 1e9 / (sweeps * STAMPS) as f64;
        black_box(sink);
        best = best.min(ns);
        worst = worst.max(ns);
    }

    // A failed lookup returns NaN from `one`, and NaN propagates through the
    // sum: the run is discarded rather than reported as a fast one.
    if sink.is_nan() {
        bail!(
            "a lookup failed during the probe (the accumulator went NaN), so the timing \
             describes an error path rather than a depth-3 evaluation"
        );
    }

    Ok(Run {
        profile_dir: PROFILE_DIR.to_owned(),
        ns_per_lookup: best,
        spread: (worst - best) / best,
        rounds,
        lookups_per_round: (sweeps * STAMPS) as u64,
    })
}

/// The `i`th query stamp.
///
/// `NOW_NS` is [`crate::fixture`]'s "inside every retained window" stamp and is
/// a whole number of milliseconds, so it lands **on** the knots of all three
/// dynamic edges on this path (1 kHz, 200 Hz, 50 Hz). The 3.7 ms offset moves
/// off all three, and 9631 ns — prime, so coprime with every grid — keeps the
/// whole sweep off them. 1024 steps reach back 9.9 ms, which stays inside every
/// ring.
///
/// This is `docs/decisions/0013`'s finding applied in advance: the Phase 1
/// lookup benchmark queried on-grid stamps, `I::eval` never ran, and the number
/// it published described a lookup with the interpolation taken out.
const fn stamp_ns(i: i64) -> i64 {
    crate::fixture::NOW_NS - 3_700_000 - i * 9_631
}

/// One lookup per non-inlinable call — the shape of a control loop, and the unit
/// the row is denominated in.
///
/// `#[inline(never)]` is what makes this a measurement of the *call*: without it
/// the timing loop and the fold merge, and the number becomes a property of the
/// loop this file happens to be written with.
///
/// The error arm returns `NaN` rather than propagating: a `Result` in the hot
/// loop would add a branch the shipped call does not have, and [`measure`]
/// checks the accumulator afterwards, which catches it just as surely.
#[inline(never)]
fn one(plan: &Plan, g: &Guard, s: Stamp) -> f64 {
    match plan.at(g, s) {
        Ok(iso) => iso.t.x,
        Err(_) => f64::NAN,
    }
}

/// `(lto, codegen-units)` as the workspace manifest declares them for `profile`.
///
/// This is what keeps the row's statement about its own build honest: the
/// profile *directory* comes from `OUT_DIR` (see `build.rs`), and this maps that
/// directory to the settings the manifest gives it. The two together are why the
/// report can say `lto = false, codegen-units = 16` without anyone having
/// retyped it.
///
/// A deliberately small TOML reader — the value is `[profile.<name>]`'s `lto`
/// and `codegen-units`, and `tf_tree_bench` has no TOML dependency.
///
/// # Errors
///
/// The section missing, or either key missing from it.
pub fn profile_settings_from_manifest(manifest: &str, profile: &str) -> Result<(String, String)> {
    let header = format!("[profile.{profile}]");
    let body = manifest
        .split(&header)
        .nth(1)
        .ok_or_else(|| anyhow!("the workspace manifest has no {header}"))?;
    // Stop at the next section header so a key from a later profile cannot be
    // read as this one's.
    let body = body.split("\n[").next().unwrap_or(body);
    let key = |k: &str| -> Result<String> {
        body.lines()
            .map(str::trim)
            .find_map(|l| l.strip_prefix(k)?.trim().strip_prefix('=').map(str::trim))
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("{header} declares no `{k}`"))
    };
    Ok((key("lto")?, key("codegen-units")?))
}

#[cfg(test)]
mod tests {
    // As in `crate::report`'s test module: a failed assertion is the intended
    // failure mode here, and the messages name the field they came from.
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;
    use crate::report::Drift;

    fn run(dir: &str, ns: f64) -> Run {
        Run {
            profile_dir: dir.to_owned(),
            ns_per_lookup: ns,
            spread: 0.004,
            rounds: ROUNDS,
            lookups_per_round: (SWEEPS * STAMPS) as u64,
        }
    }

    fn manifest() -> String {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        std::fs::read_to_string(root.join("Cargo.toml")).expect("workspace manifest")
    }

    #[test]
    fn json_round_trips() {
        let r = run(EMBEDDER_PROFILE, 239.712);
        let back = Run::from_json(&r.to_json()).expect("parse");
        assert_eq!(back, r);
    }

    // Mutant: `if schema != SCHEMA` -> `if false` in `Run::from_json`.
    #[test]
    fn a_foreign_schema_is_refused() {
        let text = run(EMBEDDER_PROFILE, 240.0)
            .to_json()
            .replace(SCHEMA, "tf_tree.embed-cost/99");
        assert!(Run::from_json(&text).is_err());
    }

    // Mutant: drop the `!(ns.is_finite() && ns > 0.0)` guard.
    #[test]
    fn a_zero_duration_is_refused_rather_than_dividing_into_infinity() {
        let text = run(EMBEDDER_PROFILE, 240.0)
            .to_json()
            .replace("240.000", "0.000");
        assert!(Run::from_json(&text).is_err());
    }

    // Mutant: drop the `run.profile_dir != want` check in `Pair::load`.
    #[test]
    fn two_runs_of_the_same_build_are_refused() {
        let dir = std::env::temp_dir().join(format!("tf-tree-embed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmpdir");
        std::fs::write(
            dir.join("embedder.json"),
            run(EMBEDDER_PROFILE, 240.0).to_json(),
        )
        .expect("write");
        // The reference file, but built into the embedder profile.
        std::fs::write(
            dir.join("release.json"),
            run(EMBEDDER_PROFILE, 240.0).to_json(),
        )
        .expect("write");
        let err = Pair::load(&dir).expect_err("a same-build pair must be refused");
        assert!(
            format!("{err:#}").contains("two different builds"),
            "unexpected error: {err:#}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_ratio_is_embedder_over_reference() {
        let p = Pair {
            embedder: run(EMBEDDER_PROFILE, 240.0),
            reference: run(REFERENCE_PROFILE, 200.0),
        };
        assert!((p.ratio() - 1.2).abs() < 1e-12);
        assert!(!p.within_gate());
        assert!(p.verdict().contains("OVER"));
    }

    // Mutant: `self.ratio() <= 1.0 + GATE` -> `self.ratio() <= 1.0 + 2.0 * GATE`.
    #[test]
    fn the_gate_boundary_is_five_percent() {
        let inside = Pair {
            embedder: run(EMBEDDER_PROFILE, 104.9),
            reference: run(REFERENCE_PROFILE, 100.0),
        };
        let outside = Pair {
            embedder: run(EMBEDDER_PROFILE, 105.1),
            reference: run(REFERENCE_PROFILE, 100.0),
        };
        assert!(inside.within_gate(), "4.9% must be inside the gate");
        assert!(!outside.within_gate(), "5.1% must be outside the gate");
    }

    // Mutant: give `ratio` `Drift::Informational` instead of `lower_is_better`.
    #[test]
    fn every_duration_this_row_reports_is_gated() {
        let p = Pair {
            embedder: run(EMBEDDER_PROFILE, 240.0),
            reference: run(REFERENCE_PROFILE, 200.0),
        };
        let m = p.metrics();
        for key in ["ratio", "embedder_ns", "reference_ns"] {
            let got = m.iter().find(|m| m.key == key).expect(key);
            assert_eq!(got.drift, Drift::LowerIsBetter, "`{key}` must be gated");
            assert!(
                (got.tolerance - GATE).abs() < 1e-12,
                "`{key}` must be gated at PHASE5 §9.2's 5%"
            );
        }
    }

    /// The row states what the two profiles are; this is what makes that true.
    ///
    /// Mutant: change `[profile.embedder]`'s `codegen-units` to `1` in the
    /// workspace manifest — the whole row becomes a second measurement of the
    /// reference profile, and nothing else in the tree would notice.
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

    /// The reader must not read a later section's keys as this profile's.
    ///
    /// Mutant: drop the `split("\n[")` truncation — `[profile.a]` then inherits
    /// `[profile.b]`'s `codegen-units` and the test above passes on a manifest
    /// where `[profile.embedder]` declares nothing at all.
    #[test]
    fn a_profile_does_not_borrow_the_next_profiles_keys() {
        let m = "[profile.a]\nlto = false\n\n[profile.b]\nlto = true\ncodegen-units = 16\n";
        assert!(profile_settings_from_manifest(m, "a").is_err());
    }

    /// The probe has to actually run — a `measure()` that returns without
    /// evaluating anything would still produce a plausible number.
    /// Mutant: replace `one`'s body with `let _ = (plan, g, s); 0.0`, so the
    /// timing loop runs and never evaluates a plan. The accumulator stays 0.0
    /// rather than going NaN, so [`measure_with`]'s own check does not fire and
    /// the probe reports a number for nothing.
    ///
    /// The lower bound is what makes that fatal, and it is not arbitrary: an
    /// `#[inline(never)]` call returning a constant measured **10.8 ns** under
    /// the mutant, while a depth-3 interpolating lookup has never measured below
    /// ~115 ns in any build here. 20 ns sits between them with room on both
    /// sides. (Discarding only the *result* — `Ok(_iso) => 0.0` — is **not**
    /// fatal and was tried: the lookup still happens, so the number is still
    /// real.)
    #[test]
    fn the_probe_measures_a_working_depth_three_lookup() {
        let r = measure_with(2, 1, 64).expect("probe");
        assert_eq!(r.profile_dir, PROFILE_DIR);
        assert!(
            r.ns_per_lookup > 20.0 && r.ns_per_lookup < 1_000_000.0,
            "implausible ns/lookup: {}",
            r.ns_per_lookup
        );
        assert_eq!(r.lookups_per_round, STAMPS as u64);
    }

    /// Every stamp the probe queries must fall strictly between two knots on
    /// every dynamic edge of the path.
    ///
    /// Mutant: drop the `- 3_700_000` offset from [`stamp_ns`] — `i = 0` is then
    /// `NOW_NS`, which is on the knot of all three edges, and the first stamp of
    /// every sweep stops interpolating. This is `docs/decisions/0013`'s defect:
    /// the Phase 1 lookup benchmark queried on-grid stamps, so `I::eval` never
    /// ran and the number it published described a lookup with the interpolation
    /// taken out.
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
