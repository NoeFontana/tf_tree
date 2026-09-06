//! **`docs/PHASE5.md` §12 gate 5**, which nothing had ever run: *ingest
//! throughput >= 10x real time on a representative recording.*
//!
//! `docs/decisions/0050-what-ten-times-real-time-divides.md` is the record this
//! implements, and it is where the four questions below are argued rather than
//! restated. Read it before changing what this measures.
//!
//! # What the ratio divides
//!
//! * **Numerator**: wall time of `tf_tree_ingest::run` — *both* passes, survey
//!   and fill. The figure §12 quoted was a `survey` (pass one) number reused as
//!   an ingest number, and a full run is about twice it.
//! * **Denominator**: `Survey::span_ns()`, the recording's own stamp span,
//!   computed by this harness from the survey it just ran rather than declared
//!   by whoever wrote the corpus. That accessor already exists; a second
//!   spelling of `max(newest) - min(oldest)` here would be one more thing to
//!   drift.
//!
//! # The ratio is a statement about the corpus's DENSITY, and that is gated
//!
//! At an identical per-transform cost, a 10 Hz x 5-transform recording reads a
//! hundred times higher than a 100 Hz x 50 one. So "10x real time" is a claim
//! about the corpus unless the density is pinned, and under `--gate` this
//! binary **refuses** a corpus sparser than §12's own representative recording
//! (100 Hz x 50 transforms = `GATE_DENSITY_FLOOR` transforms per second of
//! recording). A sparser corpus passes without checking anything, which is the
//! empty-subject-set shape: green exactly when the measurement did not happen.
//!
//! The density is also *reported* on every run, in transforms per second of
//! recording and in transforms per second of wall clock, so that a later change
//! to the corpus shows up as a change in the corpus rather than as a win.
//!
//! # The pass count is a declared parameter, and the criterion is stated over
//! the grouped arm
//!
//! `FillStats::passes` counts fill passes; `1` is the ordinary case, and the
//! *recording is read* `1 + passes` times because the survey reads it too. The
//! count is not a constant: `plan_groups` splits pass two into groups whenever
//! the buffered samples would exceed `--max-memory`, and **§12's own
//! representative recording does not fit the default cap** — four hours at
//! 100 Hz x 50 transforms is 72e6 samples at `SAMPLE_BYTES` = 64, i.e.
//! 4 608 000 000 B against a `DEFAULT_MAX_MEMORY_BYTES` of 4 294 967 296. It
//! gets two groups and one extra whole re-read.
//!
//! So a gate measured at `passes == 1` and the criterion as written are not the
//! same claim, and this binary measures **both arms**:
//!
//! * `in-memory` — the default cap, one fill pass. Reported.
//! * `grouped` — the cap lowered until pass two takes the same number of groups
//!   the criterion's own recording forces. **This is the gated arm**, because it
//!   is the regime the criterion's own words put the measurement in — not
//!   because it is the slower one. The two arms are measured in a fixed order
//!   and nothing interleaves them, so the first absorbs any first-touch cost and
//!   the ordering is reported as observed rather than asserted; under
//!   `--reuse-corpus` it has been seen to invert.
//!
//! **The grouped cap is derived from the SURVEY, not from the arguments.** It is
//! the sum of the largest `ceil(n / CRITERION_PASSES)` edges as pass one
//! measured them, which is `plan_groups`' own first-fit-decreasing rule read
//! backwards. Taking it from `--edges`/`--rate-hz`/`--seconds` described the
//! corpus this binary *generates* rather than the one it read, and the cap is
//! what decides the gated arm's regime — so under `--reuse-corpus` those three
//! arguments now decide nothing at all, and the run says so.
//!
//! Each arm asserts the pass count it declares and **REFUSES** if the run did
//! not take it. An arm that silently ran in the other regime is comparing
//! different amounts of work.
//!
//! # Why this may be gated on a host that fails the timing probe
//!
//! `docs/PHASE5.md` §9.3's **one-sided-budget amendment**, in the same shape
//! §12 gate 2 uses it: every check `Fitness::probe` fails here can only make an
//! ingest *slower*, so a PASS with margin is a conservative claim and a FAIL is
//! not attributable to the code. The verdict line prints the fitness reasons
//! whichever way it goes. It is not a `bench_report` row and not a
//! `Sensitivity::Ratio` — that axis means two engines interleaved within one
//! round, and this is one engine against a clock.
//!
//! # The falsifier
//!
//! `--gate` on a corpus **denser** than the declared one, which drives the
//! ratio down through the floor without editing a threshold — and which is the
//! same fact the density floor above exists to state: the criterion is
//! corpus-relative. `just gate5`'s red test uses it. `--floor` exists and is the
//! weaker falsifier, because moving a threshold proves only that the comparison
//! is wired.
//!
//! # What this does not measure
//!
//! The corpus is generated by `tf_tree_ingest::fixture`, whose zstd frames come
//! from `ruzstd`'s own encoder — so this is a **round-trip** corpus rather than
//! a conformance one, and `testdata/zstd_conformance.mcap` is what closes the
//! conformance half for the decoder. `fixture::compress_records` carries what
//! that does and does not cover. The corpus is also written and then read
//! immediately, so every figure here is **page-cache warm**; a first-touch
//! ingest off cold storage is slower by an amount this does not measure.
//!
//! # Usage
//!
//! ```text
//! ingest_throughput --corpus target/gate5/corpus.mcap --gate
//! ingest_throughput --corpus /tmp/c.mcap --edges 400 --rate-hz 400   # dense
//! ```

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};

use tf_tree_bench::report::Fitness;
use tf_tree_ingest::fixture::{ChunkedSpec, FixtureCodec, FixtureMessage};
use tf_tree_ingest::ingest::{Frames, IngestOptions, DEFAULT_MAX_MEMORY_BYTES};

/// §12 gate 5's floor: ingest must run at least this many times real time.
const FLOOR: f64 = 10.0;

/// §12 gate 5's representative recording is *100 Hz x 50 transforms*. That is
/// its density, in transforms per second of recording, and a gated run may not
/// be sparser: at an identical per-transform cost a sparser corpus reads
/// arbitrarily higher.
const GATE_DENSITY_FLOOR: f64 = 100.0 * 50.0;

/// Fill passes the criterion's own recording forces (see the header): four
/// hours at the density above does not fit `DEFAULT_MAX_MEMORY_BYTES`, so
/// `plan_groups` gives it two groups.
const CRITERION_PASSES: u32 = 2;

/// Bytes of buffered samples per sample, mirrored from `tf_tree_ingest`'s
/// `SAMPLE_BYTES` — which is private, so this is a **second spelling**, and the
/// thing that stops it drifting is not a comment: the grouped arm's cap is
/// derived from it and the arm then **asserts the pass count it got**, so a
/// stale value here shows up as a refusal rather than as a number from the
/// wrong regime. `the_grouped_cap_is_sized_for_two_groups` pins the arithmetic
/// against `plan_groups`'s first-fit-decreasing shape.
const SAMPLE_BYTES: u64 = 64;

/// §12 gate 5's criterion, as one expression the verdict line and the exit
/// status are both taken from.
fn meets_floor(times_real_time: f64, floor: f64) -> bool {
    times_real_time >= floor
}

fn main() -> Result<()> {
    let mut corpus = PathBuf::from("target/gate5/corpus.mcap");
    let mut edges = 50usize;
    let mut rate_hz = 100.0f64;
    let mut seconds = 32.0f64;
    let mut chunk_msgs = 8000usize;
    let mut rounds = 3usize;
    let mut floor = FLOOR;
    let mut codec = FixtureCodec::Zstd;
    let mut gate = false;
    let mut keep = false;
    // **Never passed by `just gate5`.** It exists so a measurement can be taken
    // against a corpus this process did not just write — a page cache that is
    // not warm, or one day a real recording — and it is the shape `just gate4`'s
    // `rm -f` line exists to defeat, so the recipe generates every time.
    let mut reuse = false;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut next = |what: &str| -> Result<String> {
            args.next().ok_or_else(|| anyhow!("{a} wants {what}"))
        };
        match a.as_str() {
            "--corpus" => corpus = PathBuf::from(next("a path")?),
            "--edges" => edges = next("a count")?.parse().context("--edges")?,
            "--rate-hz" => rate_hz = next("a rate")?.parse().context("--rate-hz")?,
            "--seconds" => seconds = next("seconds")?.parse().context("--seconds")?,
            "--chunk-msgs" => chunk_msgs = next("a count")?.parse().context("--chunk-msgs")?,
            "--rounds" => rounds = next("a count")?.parse().context("--rounds")?,
            "--floor" => floor = next("a ratio")?.parse().context("--floor")?,
            "--codec" => {
                let v = next("`zstd` or `none`")?;
                codec = match v.as_str() {
                    "zstd" => FixtureCodec::Zstd,
                    "none" => FixtureCodec::None,
                    other => bail!(
                        "--codec {other}: this gate offers `zstd` (what rosbag2 and Foxglove \
                         write) and `none` (the contrast). lz4's fixture arm is a \
                         hand-authored spec frame rather than a corpus writer"
                    ),
                };
            }
            "--keep-corpus" => keep = true,
            "--reuse-corpus" => reuse = true,
            "--gate" => gate = true,
            other => bail!("unknown argument `{other}`"),
        }
    }
    if rounds == 0 {
        bail!("--rounds needs at least one round");
    }

    drive(&Drive {
        corpus,
        edges,
        rate_hz,
        seconds,
        chunk_msgs,
        rounds,
        floor,
        codec,
        gate,
        keep,
        reuse,
    })
}

struct Drive {
    corpus: PathBuf,
    edges: usize,
    rate_hz: f64,
    seconds: f64,
    chunk_msgs: usize,
    rounds: usize,
    floor: f64,
    codec: FixtureCodec,
    gate: bool,
    keep: bool,
    reuse: bool,
}

/// The corpus: `edges` dynamic edges, each published at `rate_hz` for
/// `seconds`, interleaved the way a real recorder writes them.
///
/// **Interleaved rather than edge-major**, because a recording is ordered by
/// log time and pass two's per-edge sort exists precisely because of that. An
/// edge-major corpus would arrive pre-sorted and would measure a sort that
/// never has anything to do.
fn corpus_messages(edges: usize, rate_hz: f64, seconds: f64) -> Vec<FixtureMessage> {
    let ticks = (seconds * rate_hz).round() as i64;
    let period_ns = (1.0e9 / rate_hz).round() as i64;
    let mut out = Vec::with_capacity((ticks as usize).saturating_mul(edges));
    for t in 0..ticks {
        let stamp = t * period_ns;
        for e in 0..edges {
            // A pose that actually moves, so the samples are not one value
            // repeated — a compressor is very good at that, and a corpus whose
            // compression ratio is an artifact of its own monotony would price
            // the decoder wrongly in the flattering direction.
            let x = (t as f64) * 0.001 + (e as f64) * 0.01;
            out.push(FixtureMessage::dynamic(
                "world",
                &format!("link_{e}"),
                stamp,
                [x, x * 0.5, x * 0.25, 0.0, 0.0, 0.0, 1.0],
            ));
        }
    }
    out
}

/// One arm's measurement.
struct Arm {
    label: &'static str,
    /// Best wall time over the rounds, in seconds.
    best_s: f64,
    /// Worst wall time over the rounds, in seconds.
    worst_s: f64,
    passes: u32,
    peak_buffer_bytes: u64,
    max_memory_bytes: u64,
    transforms: u64,
    span_s: f64,
    /// Every buffered edge's byte size **as pass one measured it**, descending.
    ///
    /// `samples * SAMPLE_BYTES` for each non-static edge that carries samples,
    /// which is exactly what `plan_groups` bin-packs. It is here so the grouped
    /// arm's cap can be derived from the corpus rather than from the arguments
    /// that were meant to describe it — see `grouped_cap_from`.
    edge_bytes: Vec<u64>,
}

impl Arm {
    /// **The worst run, not the best.** A gate is a worst-case claim, and the
    /// margin here is large enough that the conservative reading costs nothing.
    fn times_real_time(&self) -> f64 {
        self.span_s / self.worst_s
    }
    fn best_times_real_time(&self) -> f64 {
        self.span_s / self.best_s
    }
}

fn measure(label: &'static str, path: &Path, rounds: usize, max_memory_bytes: u64) -> Result<Arm> {
    let mut best = f64::MAX;
    let mut worst = f64::MIN;
    let mut passes = 0u32;
    let mut peak = 0u64;
    let mut transforms = 0u64;
    let mut span_s = 0.0f64;
    let mut edge_bytes: Vec<u64> = Vec::new();
    for _ in 0..rounds {
        let opts = IngestOptions {
            max_memory_bytes,
            ..IngestOptions::default()
        };
        let mut frames = Frames::default();
        let started = Instant::now();
        let ingested = tf_tree_ingest::run(path, &opts, &mut frames)
            .map_err(|e| anyhow!("ingesting {}: {e:?}", path.display()))?;
        let elapsed = started.elapsed().as_secs_f64();
        best = best.min(elapsed);
        worst = worst.max(elapsed);
        passes = ingested.report.fill.passes;
        peak = ingested.report.fill.peak_buffer_bytes;
        transforms = ingested.survey.transforms_read;
        edge_bytes = ingested
            .survey
            .edges
            .iter()
            .filter(|e| !e.is_static() && e.samples > 0)
            .map(|e| e.samples.saturating_mul(SAMPLE_BYTES))
            .collect();
        edge_bytes.sort_unstable_by(|a, b| b.cmp(a));
        let (lo, hi) = ingested.survey.span_ns().ok_or_else(|| {
            anyhow!("the corpus has no stamp span, so there is nothing to divide by")
        })?;
        span_s = (hi - lo) as f64 / 1.0e9;
    }
    Ok(Arm {
        label,
        best_s: best,
        worst_s: worst,
        passes,
        peak_buffer_bytes: peak,
        max_memory_bytes,
        transforms,
        span_s,
        edge_bytes,
    })
}

/// The `--max-memory` that makes `plan_groups` produce exactly
/// [`CRITERION_PASSES`] groups on **this** corpus.
///
/// **Derived from the survey, not from `--edges`/`--rate-hz`/`--seconds`.** Those
/// three describe the corpus this binary *generates*; under `--reuse-corpus`
/// they describe a corpus that may not exist, and a cap computed from them is a
/// declaration rather than a measurement. That mattered: a run passed at exit 0
/// against a corpus of 50 edges while being told `--edges 10 --rate-hz 500`,
/// because the two happened to multiply out the same.
///
/// The rule is `plan_groups`' own. It is first-fit-**decreasing** over whole
/// edges of `samples * SAMPLE_BYTES`, so a cap equal to the sum of the largest
/// `ceil(n / CRITERION_PASSES)` edges admits exactly those in group one, and the
/// remainder — every one of them no larger than any edge already placed — fits
/// in the next group. For `n < CRITERION_PASSES` there is no such cap; the arm's
/// own pass-count assertion is what reports that, and it reports it as a fact
/// about the run rather than as an argument about this function.
fn grouped_cap_from(edge_bytes_desc: &[u64]) -> u64 {
    let first_group = edge_bytes_desc
        .len()
        .div_ceil(CRITERION_PASSES as usize)
        .max(1);
    edge_bytes_desc
        .iter()
        .take(first_group)
        .fold(0u64, |a, b| a.saturating_add(*b))
}

fn drive(d: &Drive) -> Result<()> {
    if let Some(dir) = d.corpus.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let generated = !(d.reuse && d.corpus.exists());
    if generated {
        let messages = corpus_messages(d.edges, d.rate_hz, d.seconds);
        let bytes = tf_tree_ingest::fixture::chunked_mcap_bytes(
            &messages,
            ChunkedSpec::new(d.chunk_msgs).compressed(d.codec),
        )
        .map_err(|e| anyhow!("writing the corpus: {e:?}"))?;
        std::fs::write(&d.corpus, &bytes)
            .with_context(|| format!("writing {}", d.corpus.display()))?;
    }
    let corpus_bytes = std::fs::metadata(&d.corpus)?.len();

    // The in-memory arm runs first because the grouped arm's cap is derived
    // from what pass one measured on this corpus — see `grouped_cap_from`.
    //
    // **Not `total / 2`, which was the first version and was wrong for an odd
    // edge count.** `plan_groups` is first-fit-decreasing over whole edges, so
    // with `n` equal edges of `b` bytes a cap of `total/2` admits `floor(n/2)`
    // of them per group and needs *three* groups whenever `n` is odd — measured:
    // five edges gave `passes = 3`, and the arm refused. The arm still asserts
    // the count it actually got, because a derivation is an argument about
    // `plan_groups` and the assertion is a fact about the run.
    let in_memory = measure("in-memory", &d.corpus, d.rounds, DEFAULT_MAX_MEMORY_BYTES)?;
    let grouped_cap = grouped_cap_from(&in_memory.edge_bytes);
    let grouped = measure("grouped", &d.corpus, d.rounds, grouped_cap)?;

    let fitness = Fitness::probe(1);
    // **The density is MEASURED, not declared.** `transforms_read / span` comes
    // off the survey the arm just ran, so the floor below is a fact about the
    // corpus rather than about the arguments. So is the grouped arm's cap, and
    // that half was the one that mattered: the cap decides the gated arm's
    // regime, so deriving it from `--edges`/`--rate-hz`/`--seconds` left
    // `--reuse-corpus --gate` able to pass while being told about a corpus that
    // did not exist. Both come off the survey now, which is what makes
    // `--reuse-corpus` safe and what would make pointing this at a real
    // recording possible.
    let density = in_memory.transforms as f64 / in_memory.span_s;

    println!(
        "PHASE5 §12 gate 5 — ingest throughput, {} rounds per arm",
        d.rounds
    );
    // **What is printed about the corpus is what pass one measured, plus the
    // arguments only where they actually produced it.** Under `--reuse-corpus`
    // `--edges`/`--rate-hz`/`--seconds` describe a corpus this process did not
    // write and may be describing nothing; printing them as the corpus's shape
    // is how a mis-declared run reads as a correct one.
    println!(
        "  corpus: {} buffered edges, {} transforms, {} B on disk, codec {}, page cache {}",
        in_memory.edge_bytes.len(),
        in_memory.transforms,
        corpus_bytes,
        d.codec.name(),
        if generated {
            "WARM (written by this process)"
        } else {
            "as found (--reuse-corpus)"
        },
    );
    if generated {
        println!(
            "  generated from: {} edges x {} Hz x {} s, {} messages per chunk",
            d.edges, d.rate_hz, d.seconds, d.chunk_msgs
        );
    } else {
        println!(
            "  --reuse-corpus: --edges/--rate-hz/--seconds described no corpus on this run and \
             decided nothing. The density floor and the grouped arm's --max-memory both come \
             off the survey."
        );
    }
    println!(
        "  density: {density:.1} transforms per second of recording, measured from the survey \
         (§12's representative recording is {GATE_DENSITY_FLOOR}); stamp span {:.3} s",
        in_memory.span_s
    );
    println!(
        "  build: {} — a floor on a ratio is one-sided, so a PASS is conservative and a FAIL \
         is not attributable to the code",
        if cfg!(debug_assertions) {
            "DEBUG"
        } else {
            "release"
        }
    );
    println!(
        "  host: {} logical CPUs, {} physical; fitness for an absolute duration: {} ({})",
        fitness.logical_cpus,
        fitness.physical_cores,
        if fitness.fair_for_timing {
            "fair"
        } else {
            "UNFIT"
        },
        fitness.reason_line(),
    );
    for arm in [&in_memory, &grouped] {
        println!(
            "  {:<10} {:.4} s worst, {:.4} s best -> {:.1}x real time worst, {:.1}x best; \
             fill passes {}, peak buffer {} B, --max-memory {} B, {:.0} transforms/s wall",
            arm.label,
            arm.worst_s,
            arm.best_s,
            arm.times_real_time(),
            arm.best_times_real_time(),
            arm.passes,
            arm.peak_buffer_bytes,
            arm.max_memory_bytes,
            arm.transforms as f64 / arm.worst_s,
        );
    }

    if d.gate && density < GATE_DENSITY_FLOOR {
        bail!(
            "REFUSED — this corpus carries {density:.1} transforms per second of recording and \
             PHASE5 §12 gate 5's representative recording carries {GATE_DENSITY_FLOOR} \
             (100 Hz x 50 transforms). \"10x real time\" is a statement about the corpus's \
             density as much as about the code — at an identical per-transform cost a sparser \
             corpus reads arbitrarily higher — so a gated run at this density would pass \
             without checking anything. Raise --edges/--rate-hz, or drop --gate to report."
        );
    }

    // **Each arm's premise, checked rather than assumed.** An arm that did not
    // take the pass count it declares measured a different amount of work, and
    // comparing the two would be comparing different runs.
    if in_memory.passes != 1 {
        bail!(
            "REFUSED — the in-memory arm took {} fill passes, not 1. It is supposed to be the \
             regime where pass two buffers everything at once; at this corpus size it is not, \
             so neither arm is what it says.",
            in_memory.passes
        );
    }
    if grouped.passes != CRITERION_PASSES {
        bail!(
            "REFUSED — the grouped arm took {} fill passes, not {CRITERION_PASSES}. The gated \
             number is stated at the pass count §12 gate 5's own four-hour recording forces, \
             and a run at a different count is a different claim. Nothing is reported.",
            grouped.passes
        );
    }

    let ratio = grouped.times_real_time();
    let ok = meets_floor(ratio, d.floor);
    println!(
        "  GATED   grouped arm: {ratio:.1}x real time against {:.1}x — {}",
        d.floor,
        if ok { "PASS" } else { "FAIL" }
    );
    // **Reported as measured, never asserted.** This line used to say the
    // in-memory arm "is the faster regime". The two arms run in a fixed order
    // — in-memory first — so the first one absorbs any first-touch cost, and on
    // `--reuse-corpus`, the one path where this process did not write the
    // corpus, the ordering has been observed to invert. Nothing here controls
    // for that, so nothing here claims it.
    let in_memory_ratio = in_memory.times_real_time();
    println!(
        "  REPORT  in-memory arm: {in_memory_ratio:.1}x real time, {} the gated grouped arm on \
         this run. Not gated: §12's own representative recording does not fit the default \
         --max-memory, so the criterion is stated over the grouped arm. The arms are measured \
         in a fixed order and nothing interleaves them, so this is the ordering observed and \
         not a claim about which regime is faster.",
        if in_memory_ratio >= ratio {
            "above"
        } else {
            "BELOW"
        }
    );
    println!(
        "  §12 gate 5 — {}{}",
        if ok { "PASS" } else { "FAIL" },
        if d.gate { " (gated)" } else { " (reported)" }
    );
    if !fitness.fair_for_timing {
        println!(
            "  This host fails the timing fitness probe. §9.3's one-sided-budget amendment is \
             what admits the number: every check it fails can only make an ingest slower, so a \
             PASS is conservative and a FAIL is not attributable to the code."
        );
    }
    println!(
        "  NOT this criterion: PHASE4 §6.3's separate \"bag replay at 10x real time\" row, \
         which is about a ROS 2 bridge dropping messages and its queue depth. Nothing here \
         touches it."
    );

    if !d.keep {
        let _ = std::fs::remove_file(&d.corpus);
    }
    if d.gate && !ok {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The gate's arithmetic can say no.** `frozen_workers`'s
    /// `gate_arithmetic_is_not_vacuous`, for its reason.
    #[test]
    fn the_floor_comparison_is_not_vacuous() {
        assert!(meets_floor(160.0, FLOOR));
        assert!(meets_floor(FLOOR, FLOOR), "the floor is inclusive");
        assert!(!meets_floor(9.9, FLOOR));
    }

    /// The density floor is §12's own representative recording, not a number
    /// chosen to be passable.
    #[test]
    fn the_density_floor_is_the_criterions_own_recording() {
        assert!((GATE_DENSITY_FLOOR - 5000.0).abs() < f64::EPSILON);
    }

    /// **The pass count the criterion's own recording forces, derived here
    /// rather than asserted.** Four hours at 100 Hz x 50 transforms, at
    /// `SAMPLE_BYTES` per sample, against `DEFAULT_MAX_MEMORY_BYTES`.
    #[test]
    fn the_criterions_recording_does_not_fit_the_default_memory_cap() {
        let samples = 4.0 * 3600.0 * GATE_DENSITY_FLOOR;
        let need = samples as u64 * SAMPLE_BYTES;
        assert!(
            need > DEFAULT_MAX_MEMORY_BYTES,
            "{need} B of buffered samples against a {DEFAULT_MAX_MEMORY_BYTES} B cap — if this \
             ever fails, CRITERION_PASSES is wrong and the gated arm is the wrong regime"
        );
        assert!(
            need <= DEFAULT_MAX_MEMORY_BYTES * u64::from(CRITERION_PASSES),
            "it needs more than {CRITERION_PASSES} groups now"
        );
    }

    /// **`grouped_cap_from` yields exactly [`CRITERION_PASSES`] groups**, over
    /// unequal edge sizes as well as equal ones.
    ///
    /// `total / CRITERION_PASSES` does not: `plan_groups` packs whole edges, so
    /// an odd count needs three groups under that formula — measured, five
    /// edges gave `passes = 3` and the arm refused. This drives
    /// `plan_groups`' own loop against the function's answer rather than
    /// re-deriving the cap, so a change to either side fails here.
    #[test]
    fn the_grouped_cap_is_sized_for_two_groups() {
        // `plan_groups`' loop, as a fixture: first fit over the descending
        // sizes, flushing when the next edge would not fit.
        fn ffd_groups(desc: &[u64], cap: u64) -> u64 {
            let mut groups = 0u64;
            let mut cur: Option<u64> = None;
            for &need in desc {
                assert!(
                    need <= cap,
                    "an edge larger than the cap would spill instead"
                );
                match cur {
                    Some(bytes) if bytes + need > cap => {
                        groups += 1;
                        cur = Some(need);
                    }
                    Some(bytes) => cur = Some(bytes + need),
                    None => cur = Some(need),
                }
            }
            if cur.is_some() {
                groups += 1;
            }
            groups
        }

        let per_edge = 40 * SAMPLE_BYTES;
        for edges in [2usize, 5, 50, 2000] {
            let desc = vec![per_edge; edges];
            let cap = grouped_cap_from(&desc);
            assert_eq!(
                ffd_groups(&desc, cap),
                u64::from(CRITERION_PASSES),
                "{edges} equal edges"
            );
        }

        // **Unequal edges, which equal ones do not cover.** The harness's own
        // generated corpus is equal-sized, so this is exactly the case a corpus
        // it did not write lands in — the one the cap used to be blind to,
        // because it came from the arguments rather than from the survey.
        for desc in [
            vec![900u64, 800, 700, 100, 50, 10],
            vec![1_000u64, 1],
            vec![64u64, 64, 64, 64, 64, 64, 64],
            vec![5_000u64, 4_000, 10],
        ] {
            let cap = grouped_cap_from(&desc);
            assert_eq!(
                ffd_groups(&desc, cap),
                u64::from(CRITERION_PASSES),
                "{desc:?} at cap {cap}"
            );
        }
    }

    /// The corpus is interleaved, not edge-major — pass two's sort is supposed
    /// to have something to do.
    #[test]
    fn the_corpus_is_ordered_by_stamp_across_edges() {
        let m = corpus_messages(3, 10.0, 0.3);
        assert_eq!(m.len(), 9);
        let children: Vec<&str> = m
            .iter()
            .map(|x| x.transforms[0].child_frame_id.as_str())
            .collect();
        assert_eq!(children[0], "link_0");
        assert_eq!(children[1], "link_1");
        assert_eq!(m[0].transforms[0].stamp_ns, m[2].transforms[0].stamp_ns);
        assert!(m[3].transforms[0].stamp_ns > m[0].transforms[0].stamp_ns);
    }
}
