//! `docs/PHASE5.md` §9.2's embedding measurements: what the facade costs an
//! **embedder**.
//!
//! There are two of them and they answer two different questions. Keeping them
//! apart is the whole design of this module, because an earlier revision shipped
//! the second under the first one's title.
//!
//! | | question | how | status |
//! | --- | --- | --- | --- |
//! | **`embedding_cross_crate`** | what does *crossing the crate boundary* cost? | one build, one profile, two identical bodies — one compiled in `tf_tree_bench`, one in `tf_tree_core` | §9.2's row, **gated at 5%** |
//! | **profile comparison** | what does *the embedder's own `[profile.*]`* cost? | one body, two builds: `[profile.embedder]` against `[profile.release]` | **exploratory**, `just embed-cost` only, never gated |
//!
//! `docs/API.md` §2.3 item 3 is the first of the two: *"a benchmark row measures
//! the facade path from a separate crate against the in-crate path, gated at 5%
//! — the same gate `PHASE4.md` §7 applies to the C ABI"*. `PHASE4.md` §7's C ABI
//! row is `tft_plan_at` against native Rust in one binary, one profile, and this
//! is that shape with the crate boundary substituted for the ABI boundary.
//!
//! # The gated row: one build, two crates
//!
//! This module's private `one` and [`tf_tree_core::bench_probe::depth3_lookup`] are the
//! same three lines with the same `#[inline(never)]`. The only difference
//! between them is which crate the compiler put the body in, so the difference
//! between their timings is the crate boundary and nothing else — no profile
//! difference, no second binary, no substitution.
//!
//! **The in-crate half has to live in `tf_tree_core`, and that was measured
//! rather than assumed.** A probe placed in the `tf_tree` facade is *not*
//! in-crate: the facade re-exports the engine rather than containing it, and
//! [`Plan::at`] and the fold beneath it are one crate further down. Hand-built
//! throwaway probes, embedder profile, method under *Provenance* below:
//!
//! | probe | ns/lookup |
//! | --- | --- |
//! | out-of-crate (an embedder's position, and this row's numerator) | 243.6 |
//! | in **`tf_tree`**'s own `src/` | 241.5 |
//! | in **`tf_tree_core::plan`** (this row's denominator) | 199.4 |
//!
//! A row whose denominator came from the facade — or from any existing
//! benchmark in this workspace, all of which sit outside `tf_tree_core` — would
//! report ≈1.00× for ever while the real boundary went unmeasured.
//!
//! **The in-crate body must also not be generic**, and that was found by
//! measuring rather than by reading: the first version of
//! [`tf_tree_core::bench_probe::depth3_lookup`] took `Stamp<D>` and the row
//! reported **1.000×** (240.7 out-of-crate against 240.4 "in-crate"). A generic
//! function is monomorphized in the crate that *calls* it, so both columns had
//! been codegen'd in `tf_tree_bench` and the experiment had no independent
//! variable. Making it concrete moved the same measurement to 1.250×. This is
//! the same mechanism `tf_tree_core::plan::Plan::at`'s own documentation
//! records for `at` itself — its MIR crosses the boundary regardless because it
//! is generic — arriving here as a way to measure nothing.
//!
//! ## The profile is *fixed* for this row, and it is the embedder's
//!
//! §9.2: *report it with the embedder's default profile, **not** this
//! workspace's — `[profile.release]` here sets `lto = "thin"`, which is
//! precisely what hides the effect.* Under thin LTO the crate boundary is erased
//! at link time, so the same comparison run under `[profile.release]` measures
//! nothing by construction. The gated row is therefore read off the
//! `[profile.embedder]` run only; the `[profile.release]` run is reported beside
//! it as the control — and that control is what makes the row's finding
//! believable rather than merely stated.
//!
//! ## What it measured
//!
//! Three consecutive `just embed-cost` runs on the host described under
//! *Provenance*:
//!
//! | profile | out-of-crate | in-crate | boundary ratio (band) |
//! | --- | --- | --- | --- |
//! | `[profile.embedder]` — `lto = false`, `codegen-units = 16` | 240.0–240.1 ns | 191.3–191.8 ns | **1.250, 1.254, 1.254** (rounds spanned 1.216–1.270) |
//! | `[profile.release]` — `lto = "thin"`, `codegen-units = 1` | 193.0–195.0 ns | 194.2–196.2 ns | 0.994–0.996 (rounds spanned 0.985–1.022) |
//!
//! So **§9.2's 5% criterion is not met at an embedder's default profile**, and
//! that is reported rather than engineered around. The second row is the
//! control: the same two bodies, the same host, one profile setting different,
//! and the boundary gone. That is `docs/API.md` §2.3 item 2's LTO guidance
//! measured against the thing it is guidance about.
//!
//! **The absolute columns drift with the host and the ratio does not**, which is
//! the pairing argument arriving as evidence rather than as reasoning. A fifth
//! run taken on a busier machine measured 245.3 ns out-of-crate and 196.7 ns
//! in-crate — both about 2.5% above the table — for a ratio of **1.251**, inside
//! the 1.250–1.254 the three quiet runs gave. That is why the row is denominated
//! in the ratio and why `out_of_crate_ns` and `in_crate_ns` are gated at 5%
//! rather than at something tighter.
//!
//! ## What this row does **not** say, measured rather than hedged
//!
//! An earlier revision of this file printed *"no `#[inline]` placement closes
//! that; the embedder's profile does"* on every failing run. **This row's own
//! toggle refutes the first half.** Removing `#[inline]` from
//! `tf_tree_core::plan::Plan::fold_at` and re-running the whole recipe:
//!
//! | profile | column | with `#[inline]` | without |
//! | --- | --- | --- | --- |
//! | embedder | out-of-crate | 239.9–240.1 ns | **203.9 ns** |
//! | embedder | in-crate | 191.3–191.8 ns | 204.4 ns |
//! | embedder | **boundary ratio** | 1.250–1.254 | **1.001** |
//! | release | out-of-crate | 193.0–195.0 ns | 206.6 ns |
//! | release | in-crate | 194.2–196.2 ns | 209.6 ns |
//!
//! The ratio closes — and it closes the wrong way. The boundary disappears
//! because the *in-crate* column and both *LTO'd* columns get about 7% slower,
//! while the out-of-crate embedder column gets 15% faster. **That is the reason
//! every duration here is gated and not only the quotient §9.2 names**: a
//! ratio-only gate reads the run above as a 20% improvement.
//!
//! Whether the attribute is right on balance is not this row's call. `docs/API.md`
//! §2.3 item 1 makes it normative, `just bench-ab` is workspace-wide and was not
//! run for it, and the table above is one probe shape on one fitness-failing
//! host. What the row is entitled to say is narrower and is what it says: at an
//! embedder's default profile a gap survives the five placements, and the one
//! thing measured here that removes it is `lto = "thin"` in the embedder's own
//! profile.
//!
//! # The exploratory measurement: one crate position, two profiles
//!
//! [`Pair::profile_ratio`] divides the out-of-crate column of the
//! `[profile.embedder]` run by the out-of-crate column of the
//! `[profile.release]` run. That is what `docs/API.md` §2.3 item 2's LTO
//! guidance is worth, and it is genuinely useful: **varying the downstream
//! profile is the shape that caught `fold_at_cursors` being a pessimization**
//! (§2.3's first amendment, whose table is exactly this comparison done by
//! hand), and every other harness in this repository is built under
//! `[profile.release]` only, so nothing else here varies it. It is **not** gated
//! and does not enter `results.json`: it is two processes, seconds apart, and
//! `docs/PHASE1.md` §11.2's exploratory measurements are the shape for a number
//! that informs without flapping a gate. Its own instability is the argument —
//! it moved between 1.188 and 1.235 across three runs in which the gated ratio
//! moved by 0.004.
//!
//! # Honesty (§9.3)
//!
//! * **The spread gates the verdict, it is not advice.** Both columns are timed
//!   back to back inside one round, so a per-round ratio sees the same machine
//!   noise in both halves and most of it cancels; [`Run::verdict`] then reads
//!   the *observed* band of those per-round ratios and answers
//!   [`Verdict::Unresolved`] when the band straddles §9.2's threshold. A gate
//!   whose noise floor exceeds its threshold is not a gate, and the previous
//!   revision of this file printed `pass`/`fail` from an unpaired measurement
//!   whose halves moved 8.7% between runs. Pairing is what bought the
//!   resolution. Note what the test is: not "is the band narrower than 5%" but
//!   "does the band reach the threshold". Across four runs the observed band was
//!   1.0% to 4.4% wide and every one of them resolved, because 1.216–1.270 is
//!   nowhere near 1.05. A band only has to be narrow when the ratio is close.
//! * **Both halves of the exploratory pair must come from one source tree.**
//!   `build.rs` digests the sources that determine the measured program into
//!   [`SOURCE_ID`], and [`Pair::load`] refuses two runs that disagree. Without
//!   it a stale half pairs silently with a fresh one and the quotient is a
//!   property of no program that ever existed.
//! * The profile a run was built under is read out of `OUT_DIR` by `build.rs` —
//!   a fact about where the object files went — not passed in on the command
//!   line, and [`Pair::load`] refuses a pair that is not one `embedder` run and
//!   one `release` run.
//! * [`profile_settings_from_manifest`] reads `lto` and `codegen-units` back out
//!   of the workspace manifest, and a test asserts the two profiles still say
//!   what this module claims they say.
//!
//! # Provenance of the numbers quoted above
//!
//! Measured on **2026-08-02** on the development host — an 8-vCPU AMD
//! EPYC-Milan VM (4 physical cores, SMT on, no frequency governor exposed) —
//! with `taskset -c 2`, [`ROUNDS`] rounds of 409 600 lookups per column, load
//! average ~2. That host **fails** [`crate::report::Fitness`], so none of these
//! figures is a claim in the §9.3 sense and the row comes out `unavailable`
//! here; they are quoted to justify a design decision, which is a use a
//! fitness-failing host is good enough for.
//!
//! The **resolution** is stated because the gate depends on it. Over four runs
//! the paired ratio was 1.250, 1.253, 1.254, 1.254 — 0.3% between runs — with a
//! within-run band of 1.0% to 4.4%. The unpaired numbers do not behave that way:
//! the exploratory profile ratio moved between 1.188 and 1.235 over the same
//! runs, four times the whole 5% allowance. That difference is the reason one of
//! the two measurements is gated and the other is not.
//!
//! [`Plan::at`]: tf_tree::Plan::at

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};

use crate::report::Metric;

// **The timing half of this module is behind `embed-probe`, and so is every
// item only it uses.** The in-crate column is `tf_tree_core::bench_probe`, which
// exists only under that feature; without it there is no experiment to run, and
// a probe compiled but never callable would be dead code carrying a `#[inline]`
// story nobody can check. The JSON, the gate arithmetic and the report row stay
// unconditional, because `bench_report` reads a pair it did not measure.
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

/// `docs/PHASE5.md` §9.2's gate on the ratio: 5%.
///
/// Used two ways, and they are not the same check:
///
/// * as an **absolute** criterion — §9.2 words it exactly as `PHASE4.md` §7
///   words the C ABI's, so [`Run::verdict`] states whether the measured
///   crate-boundary ratio is within `1.0 + GATE`;
/// * as the **tolerance** on every directional metric this row hands the
///   regression gate ([`crate::baseline`]), which is the form that fires on a
///   change rather than on a standing cost.
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

/// A digest of the sources that determine what this binary measures.
///
/// See `build.rs`. Two runs whose `source_id` differs are two programs, and
/// [`Pair::load`] will not divide one by the other.
pub const SOURCE_ID: &str = env!("TF_TREE_BENCH_SOURCE_ID");

/// What §9.2's 5% criterion says about a measured crate-boundary ratio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The whole observed band is inside `1.0 + `[`GATE`].
    Within,
    /// The whole observed band is outside it.
    Over,
    /// The band straddles the threshold, so this run cannot answer.
    ///
    /// Reported rather than rounded to a pass or a fail. The threshold is 5%
    /// and the band is what the machine actually did across rounds; when the
    /// second is wider than the distance to the first, a verdict would be
    /// arithmetic on noise.
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
    /// Median per-round `out_of_crate / in_crate`.
    ///
    /// A **paired** statistic, and deliberately not the quotient of the two
    /// numbers above: the two columns are timed back to back inside one round,
    /// so machine noise common to both cancels out of each round's ratio in a
    /// way it cannot cancel out of a quotient of two separate best-of-9 minima.
    /// That is what makes a 5% criterion resolvable on a host like this one.
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
    /// `(ratio_hi - ratio_lo) / ratio_lo`: how far the ratio moved between
    /// rounds, and therefore what it can resolve.
    #[must_use]
    pub fn ratio_spread(&self) -> f64 {
        (self.ratio_hi - self.ratio_lo) / self.ratio_lo
    }

    /// §9.2's 5% criterion against the **observed band**, not against a point.
    ///
    /// [`Verdict::Unresolved`] whenever `[ratio_lo, ratio_hi]` contains the
    /// threshold: the run saw rounds on both sides of it and no honest
    /// pass/fail exists. This is the check that stops the spread being an
    /// advisory number printed next to a verdict it does not constrain.
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

    /// The §9.2 criterion as a line of prose, stating the measured value either
    /// way.
    ///
    /// Deliberately not an exit code. `PHASE4.md` §7's C ABI gate is reported
    /// the same way by `crates/tf_tree_c/examples/abi_cost.rs`, and a standing
    /// cost of the crate graph is not something a build of *this* repository can
    /// fix by failing.
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

    /// The row's metrics, in report order.
    ///
    /// **All three durations are directional, not just the ratio §9.2 names.** A
    /// row whose only gated number is a quotient passes a change that moves both
    /// halves the same way, and that is not hypothetical here. Dropping
    /// `#[inline]` from `Plan::fold_at` and re-running this recipe took the
    /// ratio from 1.253 to **1.001** — a passing gate — while `in_crate_ns` went
    /// 191.5 → 204.4 ns (+6.7%) and the `[profile.release]` control went
    /// 193.2 → 206.6 ns (+6.9%). A ratio-only gate reads that as a 20%
    /// improvement. `in_crate_ns` is the metric that fires on it.
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

    /// The `embed-cost.json` document, hand-written for the reason
    /// [`crate::report`] gives: the schema is a compatibility surface, and a
    /// rename should be an edit rather than a side effect of a `#[derive]`.
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
    /// duration or ratio. The last one matters: a `0` here would divide into an
    /// infinite ratio and a gate that never fires.
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

/// The two profile runs the **exploratory** profile comparison is made of.
///
/// Not §9.2's gated row — that one is [`Run`] on its own, from the
/// `[profile.embedder]` half. This type exists for the second question: what
/// does an embedder's choice of `[profile.*]` cost them, holding the crate
/// position fixed.
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
    /// profile its name claims; or two files built from different source. The
    /// last two are the checks worth having. Two runs of the same build produce
    /// a ratio of 1.0 and a green comparison that measured nothing; two runs of
    /// different source produce a ratio that is a property of neither program,
    /// and nothing else in the two documents would show it — they are supposed
    /// to differ in their durations.
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
    ///
    /// **Exploratory.** Two processes, seconds apart, so it carries the full
    /// between-run noise of the host and it is not gated anywhere.
    #[must_use]
    pub fn profile_ratio(&self) -> f64 {
        self.embedder.out_of_crate_ns / self.reference.out_of_crate_ns
    }
}

/// Time both columns, under whatever profile this binary was built with.
///
/// The fixture is [`crate::fixture`]'s, so the tree is the same one every other
/// benchmark here measures. The stamps are **off-grid on all three dynamic
/// edges**, which `docs/decisions/0013` shows the Phase 1 lookup benchmark was
/// not: an on-grid stamp never runs `I::eval`, so it measures a lookup with the
/// interpolation removed.
///
/// # Errors
///
/// A fixture that cannot be built, a lookup that fails, or the two columns
/// disagreeing on the value they computed — each means the probe measured
/// something other than a working depth-3 evaluation.
#[cfg(feature = "embed-probe")]
pub fn measure() -> Result<Run> {
    measure_with(ROUNDS, SWEEPS, WARMUP)
}

/// [`measure`], with the loop counts as parameters.
///
/// Exists so the unit test can run the *same* code in a debug build without
/// spending minutes on it: at [`ROUNDS`] × `SWEEPS` the probe is a couple of
/// seconds in a release build and two orders of magnitude worse under `cargo
/// nextest`, and a test that is too slow to run is a test that gets `#[ignore]`d.
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

    // **The two bodies must agree on the answer before either is timed.** A
    // denominator that is fast because it evaluates something else would make
    // the boundary look expensive and nothing in the timing would say so.
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
        // Back to back inside one round, so a scheduling artefact lands on both
        // and cancels out of the per-round ratio. Ordering is fixed rather than
        // alternated: an alternation would put a different column first in
        // different rounds, which changes which one pays for a cold branch
        // predictor after the timing call.
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

    // A failed lookup returns NaN from either probe, and NaN propagates through
    // the sum: the run is discarded rather than reported as a fast one.
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

/// Middle element by value. Even lengths take the upper middle, which is the
/// conservative half for a ratio that is being checked against a ceiling.
#[cfg(feature = "embed-probe")]
fn median_of(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    s[s.len() / 2]
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
#[cfg(feature = "embed-probe")]
const fn stamp_ns(i: i64) -> i64 {
    crate::fixture::NOW_NS - 3_700_000 - i * 9_631
}

/// One lookup per non-inlinable call, **compiled in `tf_tree_bench`** — an
/// embedder's position, and the numerator of §9.2's ratio.
///
/// The body is byte-identical to [`tf_tree_core::bench_probe::depth3_lookup`],
/// which is the denominator. That is the entire experiment: same three lines,
/// same attribute, different crate.
///
/// `#[inline(never)]` is what makes this a measurement of the *call*: without it
/// the timing loop and the fold merge, and the number becomes a property of the
/// loop this file happens to be written with.
///
/// The error arm returns `NaN` rather than propagating: a `Result` in the hot
/// loop would add a branch the shipped call does not have, and [`measure_with`]
/// checks the accumulator afterwards, which catches it just as surely.
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
/// This is what keeps a run's statement about its own build honest: the profile
/// *directory* comes from `OUT_DIR` (see `build.rs`), and this maps that
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

    /// To the precision [`Run::to_json`] emits, so a fixture can round-trip
    /// exactly. A fixture that cannot is a fixture, not a bug in the writer.
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
            // `SWEEPS * STAMPS`, spelled out: those two constants exist only
            // under `embed-probe` and this fixture must build without it.
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

    // Mutant: `if schema != SCHEMA` -> `if false` in `Run::from_json`.
    #[test]
    fn a_foreign_schema_is_refused() {
        let text = run(EMBEDDER_PROFILE, 240.0, 200.0)
            .to_json()
            .replace(SCHEMA, "tf_tree.embed-cost/99");
        assert!(Run::from_json(&text).is_err());
    }

    // Mutant: drop the `!(x.is_finite() && x > 0.0)` guard in `positive`.
    #[test]
    fn a_zero_duration_is_refused_rather_than_dividing_into_infinity() {
        let text = run(EMBEDDER_PROFILE, 240.0, 200.0)
            .to_json()
            .replace("\"in_crate_ns\": 200.000", "\"in_crate_ns\": 0.000");
        assert!(Run::from_json(&text).is_err());
    }

    // Mutant: drop the `run.profile_dir != want` check in `Pair::load`.
    #[test]
    fn two_runs_of_the_same_build_are_refused() {
        // The reference file, but built into the embedder profile.
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

    /// Nothing else in the two documents distinguishes a stale half from a
    /// fresh one: they are supposed to differ in their durations.
    ///
    /// Mutant: drop the `embedder.source_id != reference.source_id` check in
    /// `Pair::load`.
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
    ///
    /// Mutant: make `MEASURED_SOURCES` empty in `build.rs` — the digest becomes
    /// `unknown`, every pair then agrees, and the check above is vacuous.
    #[test]
    fn this_build_knows_what_source_it_came_from() {
        assert_ne!(SOURCE_ID, "unknown", "build.rs found no source to digest");
        assert_eq!(SOURCE_ID.len(), 16, "source id is not a 64-bit digest");
    }

    /// The exploratory profile comparison, and it is not the gated ratio.
    #[test]
    fn the_profile_ratio_divides_the_two_out_of_crate_columns() {
        let p = Pair {
            embedder: run(EMBEDDER_PROFILE, 240.0, 200.0),
            reference: run(REFERENCE_PROFILE, 192.0, 190.0),
        };
        assert!((p.profile_ratio() - 1.25).abs() < 1e-12);
    }

    // Mutant: `self.ratio_hi <= threshold` -> `self.boundary_ratio <= threshold`
    // in `Run::verdict` — the band check collapses to a point check and the
    // straddling case below reports `Within`.
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

        // The same central ratio, measured tightly enough to resolve it.
        r.ratio_lo = 1.039;
        r.ratio_hi = 1.041;
        assert_eq!(r.verdict(), Verdict::Within);
    }

    // Mutant: `let threshold = 1.0 + GATE` -> `1.0 + 2.0 * GATE` in
    // `Run::verdict`.
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

    /// The failure line must not repeat the claim this row's own toggle
    /// refuted, and must cite the evidence it does have.
    ///
    /// An earlier revision printed *"No `#[inline]` placement closes that"* on
    /// every failing run. Removing `#[inline]` from `Plan::fold_at` and
    /// re-running took the ratio from 1.253 to 1.001 — see this module's docs
    /// for the table — so the sentence was false, and a tool that prints a
    /// refuted claim on every failure is worse than one that prints nothing.
    /// What the line may cite is the control column, which is measured in the
    /// same recipe run.
    ///
    /// Mutant: put that sentence back in `verdict_line`'s `Over` arm.
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

    // Mutant: give `boundary_ratio` `Drift::Informational` instead of
    // `lower_is_better`.
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

    /// The row states what the two profiles are; this is what makes that true.
    ///
    /// Mutant: change `[profile.embedder]`'s `codegen-units` to `1` in the
    /// workspace manifest — the exploratory comparison becomes a second
    /// measurement of the reference profile, and nothing else in the tree would
    /// notice.
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
    ///
    /// Mutant (applied, observed): replace `one`'s body with
    /// `let _ = (plan, g, s); 0.0`, so the timing loop runs and never evaluates
    /// a plan. What fires is the **agreement check**, before any timing:
    ///
    /// ```text
    /// probe: the out-of-crate and in-crate probes disagree at stamp
    /// 9896300000: 0 vs 0.7042655572553356. They are the same three lines, so
    /// this is not a rounding difference — one of them is not evaluating the
    /// plan this row is about
    /// ```
    ///
    /// That check exists precisely because this row has two bodies that must
    /// agree, and it is a stronger guard than a timing bound: it catches a
    /// column that evaluates *something else* as well as one that evaluates
    /// nothing. The 20 ns lower bound below is the second line of defence, for
    /// the case where both columns are broken the same way — an
    /// `#[inline(never)]` call returning a constant measured 10.8 ns in an
    /// earlier round of this probe, against a depth-3 interpolating lookup that
    /// has never measured below ~115 ns in any build here.
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

    /// Every stamp the probe queries must fall strictly between two knots on
    /// every dynamic edge of the path.
    ///
    /// Mutant: drop the `- 3_700_000` offset from [`stamp_ns`] — `i = 0` is then
    /// `NOW_NS`, which is on the knot of all three edges, and the first stamp of
    /// every sweep stops interpolating. This is `docs/decisions/0013`'s defect:
    /// the Phase 1 lookup benchmark queried on-grid stamps, so `I::eval` never
    /// ran and the number it published described a lookup with the interpolation
    /// taken out.
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
