//! **`docs/PHASE5.md` §12 gate 5**: *ingest throughput >= 10x real time on a
//! representative recording.*
//!
//! `docs/decisions/0050-what-ten-times-real-time-divides.md` argues the four
//! questions this binary answers; read it before changing what this measures.
//!
//! # The falsifier
//!
//! `--gate` on a corpus **denser** than the declared one (`just gate5`'s red test).
//! `--floor` is the weaker falsifier: under `--gate` a PASS against a floor below
//! `FLOOR` is refused, and tightening stays legal.
//!
//! # Not measured
//!
//! The corpus (`tf_tree_ingest::fixture`, `ruzstd`'s encoder) is a **round-trip**,
//! not a conformance, corpus (`testdata/zstd_conformance.mcap` covers the decoder;
//! see `fixture::compress_records`). Every figure is **page-cache warm**.
//! `--reuse-corpus` is the one path that does not write the corpus.
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

/// §12 gate 5's representative recording is *100 Hz x 50 transforms*; a gated run
/// may not be sparser.
// See `0050` Q2.
const GATE_DENSITY_FLOOR: f64 = 100.0 * 50.0;

/// Fill passes the criterion's recording forces (`0050` Q4): four hours at that
/// density exceeds `DEFAULT_MAX_MEMORY_BYTES`, so `plan_groups` gives two groups.
const CRITERION_PASSES: u32 = 2;

/// Bytes per buffered sample, a **second spelling** of `tf_tree_ingest`'s private
/// `SAMPLE_BYTES`. The grouped arm asserts the pass count it got, so drift shows as
/// a refusal; `the_grouped_cap_is_sized_for_two_groups` pins the arithmetic.
const SAMPLE_BYTES: u64 = 64;

/// §12 gate 5's criterion, one expression for the verdict line and exit status.
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
    // Never passed by `just gate5` (which regenerates every time): measures a
    // corpus this process did not write. Its implied `--keep-corpus` and missing-path
    // refusal live in `drive`.
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

/// The corpus: `edges` dynamic edges, each published at `rate_hz` for `seconds`,
/// **interleaved** like a real recorder so pass two's per-edge sort has work.
fn corpus_messages(edges: usize, rate_hz: f64, seconds: f64) -> Vec<FixtureMessage> {
    let ticks = (seconds * rate_hz).round() as i64;
    let period_ns = (1.0e9 / rate_hz).round() as i64;
    let mut out = Vec::with_capacity((ticks as usize).saturating_mul(edges));
    for t in 0..ticks {
        let stamp = t * period_ns;
        for e in 0..edges {
            // A pose that moves, so compression is not flattered by monotony.
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
    /// Every buffered edge's byte size **as pass one measured it**, descending;
    /// `grouped_cap_from` derives the grouped arm's cap from it.
    edge_bytes: Vec<u64>,
}

impl Arm {
    /// **The worst run, not the best**: a gate is a worst-case claim.
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
/// [`CRITERION_PASSES`] groups on **this** corpus, derived from the survey rather
/// than `--edges`/`--rate-hz`/`--seconds` (which may describe no corpus under
/// `--reuse-corpus`). `plan_groups` is first-fit-**decreasing** over whole edges
/// and packs against `sum + the group's largest member` (sort scratch), so the cap
/// is the sum of the largest `ceil(n / CRITERION_PASSES)` edges **plus the largest
/// edge**; without that term 50 equal edges yield three groups and the gated arm
/// refuses. For `n < CRITERION_PASSES` no cap exists and the arm's pass-count
/// assertion reports it.
/// `tf_tree_ingest`'s spill path raises a cap below its private floor, so a
/// corpus whose whole buffered size is under it plans one group whatever this
/// returns; unreachable from any corpus this binary generates, and the pass-count
/// assertion reports it.
// See `0050` Q4.
fn grouped_cap_from(edge_bytes_desc: &[u64]) -> u64 {
    let first_group = edge_bytes_desc
        .len()
        .div_ceil(CRITERION_PASSES as usize)
        .max(1);
    let packed = edge_bytes_desc
        .iter()
        .take(first_group)
        .fold(0u64, |a, b| a.saturating_add(*b));
    // `max`, not `first`: the reserve is a property of the group, not the sort order.
    packed.saturating_add(edge_bytes_desc.iter().copied().max().unwrap_or(0))
}

fn drive(d: &Drive) -> Result<()> {
    if let Some(dir) = d.corpus.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    // `--reuse-corpus` on a missing path is a REFUSAL, not a fabrication of a
    // synthetic corpus labelled as this process's.
    if d.reuse && !d.corpus.exists() {
        bail!(
            "REFUSED — --reuse-corpus names {}, which does not exist. The flag measures a \
             corpus this process did not write; generating one here would report a synthetic \
             corpus as the one you named. Drop --reuse-corpus to generate, or fix the path.",
            d.corpus.display()
        );
    }
    let generated = !d.reuse;
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

    // The in-memory arm runs first because the grouped cap derives from what pass
    // one measured. Not `total / 2`: for odd `n` that yields three groups; the arm
    // still asserts the count it got.
    let in_memory = measure("in-memory", &d.corpus, d.rounds, DEFAULT_MAX_MEMORY_BYTES)?;
    let grouped_cap = grouped_cap_from(&in_memory.edge_bytes);
    let grouped = measure("grouped", &d.corpus, d.rounds, grouped_cap)?;

    let fitness = Fitness::probe(1);
    // The density is MEASURED (`transforms_read / span` from the survey), as is the
    // grouped cap, which is what makes `--reuse-corpus` safe.
    let density = in_memory.transforms as f64 / in_memory.span_s;

    println!(
        "PHASE5 §12 gate 5 — ingest throughput, {} rounds per arm",
        d.rounds
    );
    // Print what pass one measured; the arguments describe a corpus only where they
    // produced it.
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

    // Each arm's premise is checked: an arm with a different pass count measured
    // different work.
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
    // **A loosened threshold may not produce a gated PASS**: `--floor` is the whole
    // comparison (`frozen_workers`'s `GATE` is a constant). Refused before any verdict
    // line; tightening stays legal (`tests/ingest_throughput.rs` uses it).
    if d.gate && ok && d.floor < FLOOR {
        bail!(
            "REFUSED — --floor {:.1} is below PHASE5 §12 gate 5's own {FLOOR:.1}x, and this \
             run would have PASSED against it. The floor is the entire gated comparison, \
             so a gated PASS against a loosened one is a statement about the argument and \
             not about the code. Tighten it, or drop --gate to report.",
            d.floor
        );
    }
    println!(
        "  GATED   grouped arm: {ratio:.1}x real time against {:.1}x — {}",
        d.floor,
        if ok { "PASS" } else { "FAIL" }
    );
    // Reported as measured, never asserted: arm order lets the first absorb
    // first-touch cost, and nothing here controls for that.
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

    // `&& generated`: this process only removes what it wrote, or `--reuse-corpus`
    // would delete the corpus it exists to reuse.
    if !d.keep && generated {
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

    /// **The gate's arithmetic can say no**, as `frozen_workers`'s
    /// `gate_arithmetic_is_not_vacuous`.
    #[test]
    fn the_floor_comparison_is_not_vacuous() {
        assert!(meets_floor(160.0, FLOOR));
        assert!(meets_floor(FLOOR, FLOOR), "the floor is inclusive");
        assert!(!meets_floor(9.9, FLOOR));
    }

    /// The floor is §12's own recording, not a passable number.
    #[test]
    fn the_density_floor_is_the_criterions_own_recording() {
        assert!((GATE_DENSITY_FLOOR - 5000.0).abs() < f64::EPSILON);
    }

    /// **The pass count the criterion's recording forces**: four hours at 100 Hz x 50
    /// transforms at `SAMPLE_BYTES` against `DEFAULT_MAX_MEMORY_BYTES`.
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

    /// **`grouped_cap_from` yields exactly [`CRITERION_PASSES`] groups**, over unequal
    /// edge sizes as well as equal ones (`total / CRITERION_PASSES` needs three for odd
    /// `n`).
    ///
    /// It drives a MODEL of the private `plan_groups` (first-fit-decreasing shape);
    /// run-time coupling is the arm's `grouped.passes != CRITERION_PASSES` refusal, and
    /// `tf_tree_ingest`'s `a_cap_below_the_floor_is_planned_at_the_floor` and
    /// `groups_respect_the_cap` pin the real rule. The model omits `spill::cap_of`, so
    /// every fixture is a whole number of `SAMPLE_BYTES` with its cap at or above the
    /// floor.
    #[test]
    fn the_grouped_cap_is_sized_for_two_groups() {
        // `plan_groups`' loop: first fit over descending sizes, flushing when the next
        // edge plus the group's largest member (its sort reserve) would not fit.
        fn ffd_groups(desc: &[u64], cap: u64) -> u64 {
            let mut groups = 0u64;
            let mut cur: Option<(u64, u64)> = None;
            for &need in desc {
                assert!(
                    need.saturating_mul(2) <= cap,
                    "an edge over half the cap would spill instead — a group of \
                     one still pays for its own sort scratch"
                );
                match cur {
                    Some((bytes, max)) if bytes + need + max.max(need) > cap => {
                        groups += 1;
                        cur = Some((need, need));
                    }
                    Some((bytes, max)) => cur = Some((bytes + need, max.max(need))),
                    None => cur = Some((need, need)),
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

        // Unequal edges, which the harness's equal-sized corpus does not cover.
        for desc in [
            vec![960u64, 832, 704, 128, 64, 64],
            vec![1_984u64, 64],
            // Seven equal edges whose derived cap lands exactly on the spill floor.
            vec![256u64; 7],
            vec![5_056u64, 4_032, 64],
        ] {
            let cap = grouped_cap_from(&desc);
            assert_eq!(
                ffd_groups(&desc, cap),
                u64::from(CRITERION_PASSES),
                "{desc:?} at cap {cap}"
            );
        }
    }

    /// Interleaved, not edge-major: pass two's sort has work.
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
