//! **`docs/PHASE5.md` §12 gate 2**: *`.tft` open time under 10 ms for a 233 MB index.*
//!
//! The claim is **complexity**: every step of [`tf_tree::Tree::open_frozen`] is O(1) in the index size. The
//! binary gates on the **budget** (open fits in 10 ms at gate scale) and on **scale invariance** (a fixture
//! two orders of magnitude smaller opens within `SCALE_BOUND`x).
//!
//! # Cache states
//!
//! Evicted = page cache dropped, resident = warm; each open's major-fault count is printed. **The evicted arm
//! is REPORTED, the resident arm is GATED** (§12 criterion 2).
//!
//! # Fitness
//!
//! Per §9.3's one-sided budget amendment, every [`tf_tree_bench::report::Fitness::probe`] check can only slow an
//! open, so a PASS is conservative and a FAIL is not attributable to the code. A debug build is not refused
//! (its profile is printed). `tests/gate2.rs` drives the debug binary to prove wiring.
//!
//! # Refusals
//!
//! `--gate` **refuses** rather than passes on: a gated fixture under `GATE_INDEX_FLOOR_BYTES`; fixtures closer
//! than `SCALE_SPAN`x; an evicted arm whose eviction the child's major-fault count does not witness (ungated,
//! the arm is voided; usually a RAM-backed `$TMPDIR`, hence `tests/gate2.rs` uses the cargo target directory);
//! a PASS against a `--budget-ms` **above** the criterion's. It refuses no mode, not `--prefault`. Printing FAIL
//! and exiting 0 is what `docs/benchmarks/EVIDENCE.md` exists to prevent (as `src/bin/frozen_workers.rs`).
//!
//! # The falsifier
//!
//! `--prefault` reads every byte of the `.tft` inside the timed region, standing in for a `populate_hot` arm
//! reaching the frozen backing (`populate_edge_rings` refuses it today). The verdict is on time; fault counts
//! only corroborate. §2.2: a `.tft` is deliberately not prefaulted, so this gate cannot see work moved into the
//! first lookup.
//!
//! # Usage
//!
//! ```text
//! frozen_open --build target/gate2/index.tft --robots 64 --history 40
//! frozen_open --tft target/gate2/index.tft --small-tft target/gate2/small.tft --gate
//! frozen_open --tft ... --prefault           # the control; documented to FAIL
//! frozen_open --open target/gate2/index.tft  # a child; not run by hand
//! ```

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};

use tf_tree::Tree;
use tf_tree_bench::report::Fitness;
use tf_tree_bench::workload::{Backing, QuerySpec, Topology, Workload};

const BUDGET_MS: f64 = 10.0;

/// §12 gate 2's "233 MB index", as a floor on the gated fixture.
const GATE_INDEX_FLOOR_BYTES: u64 = 233_000_000;

const SCALE_SPAN: u64 = 8;

/// The factor the two fixtures' open times must agree within; the expectation is 1.0.
const SCALE_BOUND: f64 = 4.0;

fn within_budget(worst_ms: f64, budget_ms: f64) -> bool {
    worst_ms <= budget_ms
}

/// The open does not grow with the index (best open per arm); the control fails it by an order of magnitude.
fn scale_invariant(large_ms: f64, small_ms: f64) -> bool {
    large_ms <= small_ms * SCALE_BOUND
}

fn main() -> Result<()> {
    let mut mode = Mode::Drive;
    let mut path = PathBuf::from("target/gate2/index.tft");
    let mut small = PathBuf::from("target/gate2/small.tft");
    let mut robots = 64usize;
    let mut history = 40.0f64;
    let mut small_robots = 2usize;
    let mut small_history = 0.5f64;
    let mut rounds = 8usize;
    let mut budget_ms = BUDGET_MS;
    let mut prefault = false;
    let mut evict = true;
    let mut gate = false;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--build" => {
                mode = Mode::Build;
                path = PathBuf::from(args.next().ok_or_else(|| anyhow!("{a} wants a path"))?);
            }
            "--open" => {
                mode = Mode::Open;
                path = PathBuf::from(args.next().ok_or_else(|| anyhow!("{a} wants a path"))?);
            }
            "--tft" => {
                path = PathBuf::from(args.next().ok_or_else(|| anyhow!("{a} wants a path"))?);
            }
            "--small-tft" => {
                small = PathBuf::from(args.next().ok_or_else(|| anyhow!("{a} wants a path"))?);
            }
            "--robots" => {
                robots = args
                    .next()
                    .ok_or_else(|| anyhow!("{a} wants a count"))?
                    .parse()
                    .context("--robots")?;
            }
            "--history" => {
                history = args
                    .next()
                    .ok_or_else(|| anyhow!("{a} wants seconds"))?
                    .parse()
                    .context("--history")?;
            }
            "--small-robots" => {
                small_robots = args
                    .next()
                    .ok_or_else(|| anyhow!("{a} wants a count"))?
                    .parse()
                    .context("--small-robots")?;
            }
            "--small-history" => {
                small_history = args
                    .next()
                    .ok_or_else(|| anyhow!("{a} wants seconds"))?
                    .parse()
                    .context("--small-history")?;
            }
            "--rounds" => {
                rounds = args
                    .next()
                    .ok_or_else(|| anyhow!("{a} wants a count"))?
                    .parse()
                    .context("--rounds")?;
                if rounds == 0 {
                    bail!("--rounds needs at least one round");
                }
            }
            "--budget-ms" => {
                budget_ms = args
                    .next()
                    .ok_or_else(|| anyhow!("{a} wants milliseconds"))?
                    .parse()
                    .context("--budget-ms")?;
            }
            "--prefault" => prefault = true,
            "--no-evict" => evict = false,
            "--gate" => gate = true,
            other => bail!("unknown argument `{other}`"),
        }
    }

    match mode {
        Mode::Build => build(&path, robots, history),
        Mode::Open => open_once(&path, prefault),
        Mode::Drive => drive(&Drive {
            path,
            small,
            robots,
            history,
            small_robots,
            small_history,
            rounds,
            budget_ms,
            prefault,
            evict,
            gate,
        }),
    }
}

enum Mode {
    Build,
    Open,
    Drive,
}

struct Drive {
    path: PathBuf,
    small: PathBuf,
    robots: usize,
    history: f64,
    small_robots: usize,
    small_history: f64,
    rounds: usize,
    budget_ms: f64,
    prefault: bool,
    evict: bool,
    gate: bool,
}

/// A fixture at §12 gate 2's scale, like `frozen_workers`'s `Fleet`; this binary needs two sizes.
fn spec(name: &'static str, robots: usize, history: f64) -> Workload {
    Workload {
        name,
        topology: Topology::Fleet {
            robots,
            history_secs: history,
        },
        queries: QuerySpec::CrossFleet,
        note: "PHASE5 §12 gate 2: open time against index size",
    }
}

fn build(path: &Path, robots: usize, history: f64) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let w = spec("gate2_fleet", robots, history);
    let built = w.build(tf_tree::InterpPolicy::LerpSlerp, Backing::Heap)?;
    built
        .tree
        .freeze_to(path, None, [0u8; 32], 0)
        .map_err(|e| anyhow!("freezing to {}: {e:?}", path.display()))?;
    let bytes = std::fs::metadata(path)?.len();
    eprintln!(
        "built {} — {:.1} MiB",
        path.display(),
        bytes as f64 / (1024.0 * 1024.0)
    );
    Ok(())
}

/// One child: one timed `Tree::open_frozen`, its fault counts, one line out; a fresh process per open, since
/// a repeat in-process costs a fraction of the first.
fn open_once(path: &Path, prefault: bool) -> Result<()> {
    let before = faults()?;
    let started = Instant::now();
    let tree = Tree::open_frozen(path).map_err(|e| anyhow!("opening {}: {e:?}", path.display()))?;
    let read = if prefault { read_whole(path)? } else { 0 };
    let elapsed = started.elapsed();
    let after = faults()?;
    drop(tree);
    println!(
        "ns={} minflt={} majflt={} prefaulted={}",
        elapsed.as_nanos(),
        after.0.saturating_sub(before.0),
        after.1.saturating_sub(before.1),
        read
    );
    Ok(())
}

/// Read every byte of the file, returning how many (the `--prefault` control).
fn read_whole(path: &Path) -> Result<u64> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(total);
        }
        total += n as u64;
    }
}

/// `(minflt, majflt)` from `/proc/self/stat`, parsed after the last `)`.
fn faults() -> Result<(u64, u64)> {
    let stat = std::fs::read_to_string("/proc/self/stat").context("reading /proc/self/stat")?;
    let rest = stat
        .rsplit_once(')')
        .ok_or_else(|| anyhow!("/proc/self/stat has no `)` — not a Linux stat line"))?
        .1;
    let f: Vec<&str> = rest.split_whitespace().collect();
    let get = |i: usize, what: &str| -> Result<u64> {
        f.get(i)
            .ok_or_else(|| anyhow!("/proc/self/stat is too short for {what}"))?
            .parse::<u64>()
            .with_context(|| format!("parsing {what} from /proc/self/stat"))
    };
    Ok((get(7, "minflt")?, get(9, "majflt")?))
}

/// Drop `path`'s page cache and say whether the request was made: `dd oflag=nocache conv=notrunc,fdatasync
/// count=0`, not `posix_fadvise` (no new `unsafe` kind). Nothing trusts it; the witness is the child's
/// major-fault count.
fn evict(path: &Path) -> Result<()> {
    let out = Command::new("dd")
        .arg(format!("of={}", path.display()))
        .arg("oflag=nocache")
        .arg("conv=notrunc,fdatasync")
        .arg("count=0")
        .output()
        .context(
            "spawning `dd` to drop the fixture's page cache — the evicted arm needs it, and \
             this gate refuses rather than reporting a resident number as an evicted one",
        )?;
    if !out.status.success() {
        bail!(
            "`dd oflag=nocache` on {} exited {}: {}",
            path.display(),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn warm(path: &Path) -> Result<()> {
    read_whole(path).map(|_| ())
}

struct Arm {
    worst_ms: f64,
    best_ms: f64,
    majflt_seen: Vec<u64>,
    minflt_seen: Vec<u64>,
    samples: usize,
}

impl Arm {
    fn all_major(&self) -> bool {
        self.majflt_seen.iter().all(|m| *m > 0)
    }
    fn any_major(&self) -> bool {
        self.majflt_seen.iter().any(|m| *m > 0)
    }
    fn faults_line(&self) -> String {
        format!(
            "minflt {}-{}, majflt {}-{}",
            self.minflt_seen.iter().min().copied().unwrap_or(0),
            self.minflt_seen.iter().max().copied().unwrap_or(0),
            self.majflt_seen.iter().min().copied().unwrap_or(0),
            self.majflt_seen.iter().max().copied().unwrap_or(0),
        )
    }
}

/// Which cache state an arm is measured in; `Evicted { requested: false }` is the witness's control.
#[derive(Clone, Copy)]
enum Cache {
    Evicted { requested: bool },
    Resident,
}

fn measure(exe: &Path, path: &Path, rounds: usize, cache: Cache, prefault: bool) -> Result<Arm> {
    let mut worst = f64::MIN;
    let mut best = f64::MAX;
    let mut majflt_seen = Vec::with_capacity(rounds);
    let mut minflt_seen = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        match cache {
            Cache::Evicted { requested: true } => evict(path)?,
            Cache::Evicted { requested: false } | Cache::Resident => warm(path)?,
        }
        let mut cmd = Command::new(exe);
        cmd.arg("--open").arg(path);
        if prefault {
            cmd.arg("--prefault");
        }
        let out = cmd.output().context("spawning the open child")?;
        if !out.status.success() {
            bail!(
                "the open child exited {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let line = String::from_utf8_lossy(&out.stdout);
        let (ns, minflt, majflt) = parse_child(&line)?;
        let ms = ns as f64 / 1.0e6;
        worst = worst.max(ms);
        best = best.min(ms);
        minflt_seen.push(minflt);
        majflt_seen.push(majflt);
    }
    Ok(Arm {
        worst_ms: worst,
        best_ms: best,
        majflt_seen,
        minflt_seen,
        samples: rounds,
    })
}

fn parse_child(line: &str) -> Result<(u64, u64, u64)> {
    let mut ns = None;
    let mut minflt = None;
    let mut majflt = None;
    for tok in line.split_whitespace() {
        let Some((k, v)) = tok.split_once('=') else {
            continue;
        };
        let v = v.parse::<u64>().ok();
        match k {
            "ns" => ns = v,
            "minflt" => minflt = v,
            "majflt" => majflt = v,
            _ => {}
        }
    }
    Ok((
        ns.ok_or_else(|| anyhow!("the open child printed no `ns=`: {line}"))?,
        minflt.ok_or_else(|| anyhow!("the open child printed no `minflt=`: {line}"))?,
        majflt.ok_or_else(|| anyhow!("the open child printed no `majflt=`: {line}"))?,
    ))
}

fn drive(d: &Drive) -> Result<()> {
    if !d.path.exists() {
        build(&d.path, d.robots, d.history)?;
    }
    if !d.small.exists() {
        build(&d.small, d.small_robots, d.small_history)?;
    }
    let large_bytes = std::fs::metadata(&d.path)?.len();
    let small_bytes = std::fs::metadata(&d.small)?.len();

    if d.gate && large_bytes < GATE_INDEX_FLOOR_BYTES {
        bail!(
            "{} is {large_bytes} B and PHASE5 §12 gate 2 is stated over a 233 MB index. An \
             `mmap` of a small file fits 10 ms for reasons that have nothing to do with this \
             design, so this run would pass without checking anything. Build a bigger fixture \
             (--robots/--history) or drop --gate to report.",
            d.path.display()
        );
    }
    if d.gate && small_bytes.saturating_mul(SCALE_SPAN) > large_bytes {
        bail!(
            "the two fixtures are {small_bytes} B and {large_bytes} B, under {SCALE_SPAN}x \
             apart. The scale-invariance arm compares an open at two index sizes; at this \
             span it cannot fail."
        );
    }

    let exe = std::env::current_exe().context("locating this binary to spawn children with")?;

    let dropped = Cache::Evicted { requested: d.evict };
    let evicted_large = measure(&exe, &d.path, d.rounds, dropped, d.prefault)?;
    let evicted_small = measure(&exe, &d.small, d.rounds, dropped, d.prefault)?;
    let resident_large = measure(&exe, &d.path, d.rounds, Cache::Resident, d.prefault)?;
    let resident_small = measure(&exe, &d.small, d.rounds, Cache::Resident, d.prefault)?;

    let fitness = Fitness::probe(1);

    println!(
        "PHASE5 §12 gate 2 — .tft open time, {} rounds per arm, fresh process per open",
        d.rounds
    );
    println!(
        "  fixtures: {} ({:.1} MiB), {} ({:.1} MiB)",
        d.path.display(),
        large_bytes as f64 / (1024.0 * 1024.0),
        d.small.display(),
        small_bytes as f64 / (1024.0 * 1024.0),
    );
    println!(
        "  host: {} logical CPUs, {} physical{}; fitness for an absolute duration: {} ({})",
        fitness.logical_cpus,
        fitness.physical_cores,
        if fitness.physical_cores_known {
            ""
        } else {
            " (unknown, logical-CPU fallback)"
        },
        if fitness.fair_for_timing {
            "fair"
        } else {
            "UNFIT"
        },
        fitness.reason_line(),
    );
    println!(
        "  build: {} — a budget is one-sided, so a debug PASS is conservative and a debug \
         FAIL is not attributable to the code",
        if cfg!(debug_assertions) {
            "DEBUG"
        } else {
            "release"
        }
    );
    if d.prefault {
        println!(
            "  --prefault: every byte of the .tft is read inside the timed region (the control)"
        );
    }
    if !d.evict {
        println!("  --no-evict: the evicted arm's cache was left resident (the control)");
    }
    // Without `--gate` the floors are not refusals; print a line for both so an ungated PASS is not misread.
    if !d.gate && large_bytes < GATE_INDEX_FLOOR_BYTES {
        println!(
            "  NOT AT GATE SCALE: {large_bytes} B is under §12 gate 2's 233 MB index, so this \
             run reports an open time and does not evaluate the criterion"
        );
    }
    if !d.gate && small_bytes.saturating_mul(SCALE_SPAN) > large_bytes {
        println!(
            "  SPAN TOO NARROW: {small_bytes} B and {large_bytes} B are under {SCALE_SPAN}x \
             apart, so the scale-invariance line below compares an open with itself and \
             cannot fail. It is not a verdict on this run."
        );
    }
    for (label, arm, bytes) in [
        ("evicted large", &evicted_large, large_bytes),
        ("evicted small", &evicted_small, small_bytes),
        ("resident large", &resident_large, large_bytes),
        ("resident small", &resident_small, small_bytes),
    ] {
        println!(
            "  {label:<15} {:.4} ms worst, {:.4} ms best over {} opens of {:.1} MiB ({})",
            arm.worst_ms,
            arm.best_ms,
            arm.samples,
            bytes as f64 / (1024.0 * 1024.0),
            arm.faults_line(),
        );
    }

    // The evicted arm's premise is checked: a gated run REFUSES, an ungated one degrades and says so.
    let evicted_premise = evicted_large.all_major() && evicted_small.all_major();
    if !evicted_premise {
        let why = format!(
            "the evicted arm did not evict. An open that faults the file in from storage takes \
             at least one major fault; a run reporting zero measured a resident page cache. \
             `dd oflag=nocache` was {}, and a RAM-backed filesystem (tmpfs) cannot evict at \
             all whatever it returns.",
            if d.evict {
                "run"
            } else {
                "NOT run (--no-evict)"
            }
        );
        if d.gate {
            bail!("REFUSED — {why} Nothing is reported from this run.");
        }
        println!(
            "  EVICTED ARM VOID — {why} The two evicted rows above are a resident arm's; no \
             evicted number is published from this run."
        );
    }
    if resident_large.any_major() || resident_small.any_major() {
        bail!(
            "REFUSED — the resident arm took a major fault, so its page cache was not \
             resident and its numbers are an evicted arm's. Nothing is reported from this run."
        );
    }

    let budget_ok = within_budget(resident_large.worst_ms, d.budget_ms);
    let scale_ok = scale_invariant(resident_large.best_ms, resident_small.best_ms);
    let ratio = resident_large.best_ms / resident_small.best_ms;

    // A loosened budget may not produce a gated PASS; lowering stays legal, as does raising while the run fails.
    if d.gate && budget_ok && scale_ok && d.budget_ms > BUDGET_MS {
        bail!(
            "REFUSED — --budget-ms {:.4} is above PHASE5 §12 gate 2's own {BUDGET_MS:.4} ms, \
             and this run would have PASSED against it. A gated PASS against a loosened \
             budget is a statement about the argument and not about the code. Tighten it, \
             or drop --gate to report.",
            d.budget_ms
        );
    }

    println!(
        "  GATED   budget:          {:.4} ms worst resident against {:.4} ms — {}",
        resident_large.worst_ms,
        d.budget_ms,
        if budget_ok { "PASS" } else { "FAIL" }
    );
    println!(
        "  GATED   scale invariance: {ratio:.3}x large/small (best of each) against \
         {SCALE_BOUND:.1}x — {}",
        if scale_ok { "PASS" } else { "FAIL" }
    );
    if evicted_premise {
        println!(
            "  REPORT  evicted:          {:.4} ms worst against {:.4} ms — {}. Not gated: the \
             size-dependence of an evicted open is the storage device fetching pages, and the \
             fault counts above are what say so.",
            evicted_large.worst_ms,
            d.budget_ms,
            if within_budget(evicted_large.worst_ms, d.budget_ms) {
                "within"
            } else {
                "OVER"
            }
        );
    } else {
        println!(
            "  REPORT  evicted:          not measured — the premise failed, see EVICTED ARM \
             VOID above."
        );
    }

    let verdict = budget_ok && scale_ok;
    println!(
        "  §12 gate 2 — {}{}",
        if verdict { "PASS" } else { "FAIL" },
        if d.gate { " (gated)" } else { " (reported)" }
    );
    if !fitness.fair_for_timing {
        println!(
            "  This host fails the timing fitness probe. §9.3's one-sided-budget amendment is \
             what admits the number: every check it fails can only make an open slower, so a \
             PASS is conservative and a FAIL is not attributable to the code."
        );
    }

    if d.gate && !verdict {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_budget_comparison_is_not_vacuous() {
        assert!(within_budget(2.5, BUDGET_MS));
        assert!(
            within_budget(BUDGET_MS, BUDGET_MS),
            "the bound is inclusive"
        );
        assert!(!within_budget(126.8, BUDGET_MS));
    }

    #[test]
    fn the_scale_comparison_is_not_vacuous() {
        assert!(scale_invariant(0.09, 0.10));
        assert!(scale_invariant(0.40, 0.10), "4x is the bound, inclusive");
        assert!(!scale_invariant(0.41, 0.10));
        assert!(!scale_invariant(20.0, 0.15), "a size-proportional open");
    }

    #[test]
    fn the_gate_scale_floor_is_the_criterions_own_number() {
        assert_eq!(GATE_INDEX_FLOOR_BYTES, 233_000_000);
        assert_eq!(SCALE_SPAN, 8);
    }
}
