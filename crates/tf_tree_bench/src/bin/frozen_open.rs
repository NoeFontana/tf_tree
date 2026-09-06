//! **`docs/PHASE5.md` §12 gate 2**, which nothing had ever run: *`.tft` open
//! time under 10 ms for a 233 MB index.*
//!
//! # What the criterion is actually claiming
//!
//! Its parenthesis is the claim: *"it is an `mmap` plus header validation;
//! anything more means work is happening that should not"*. That is a statement
//! about **complexity**, not about a duration — every step of
//! [`tf_tree::Tree::open_frozen`] is O(1) in the index size, so the open of a
//! 338 MiB `.tft` and the open of a 2 MiB one should cost the same. The 10 ms
//! is a budget that a size-proportional step would blow through; it is not a
//! latency target anybody is optimising towards.
//!
//! So this binary measures two things and gates on both:
//!
//! * the **budget** — the open must fit in 10 ms at gate scale;
//! * **scale invariance** — the same measurement on a fixture two orders of
//!   magnitude smaller must come out within a small factor, which is the half
//!   that is a claim about `open_frozen` rather than about this host.
//!
//! # Two cache states, and only one of them is a claim about this code
//!
//! The evicted arm is measured with the file's page cache dropped, the resident
//! arm with it warm. They differ by two orders of magnitude, and the difference
//! is **the one major fault**, not the steps around it. This binary prints the
//! fault counts: exactly one major fault when the cache was dropped and zero
//! when it was not, *at both fixture sizes*. Under `strace -T` the syscalls are
//! flat across a 2550x range of mapped length — `mmap` 21 us against 19,
//! `madvise(MADV_HUGEPAGE)` 18 against 16, the 128-byte header `pread64` 193
//! against 167 — so what the evicted arm's size dependence measures is how much
//! the kernel reads to satisfy that single fault, which is a property of the
//! file and the mapping rather than of a step `open_frozen` takes. A gate on
//! the evicted number is a gate on the storage under the runner.
//!
//! **The evicted arm is therefore REPORTED and the resident arm is GATED**, and
//! the evicted numbers are printed with this host's CPU and the fixture's
//! filesystem beside them so nobody quotes one without them.
//!
//! The words are `evicted`/`resident` rather than `cold`/`warm` on purpose:
//! `src/bin/attach_bench.rs`, in this same crate and this same evidence
//! register, defines *cold* as "the first attach in this process to a segment
//! it has never mapped … **not** a cold page cache". Two columns of one
//! register must not use one word for two things.
//!
//! # Why a gate here may publish an absolute duration when `bench_report` will
//! not
//!
//! `docs/PHASE5.md` §9.3's `Sensitivity::AbsoluteTiming` axis refuses a
//! duration on a host that fails [`tf_tree_bench::report::Fitness::probe`], and
//! this host fails it. §9.3's *"a one-sided budget with a stated margin"*
//! amendment is what admits this measurement anyway, and the argument is in the
//! direction of the error: every timing check the probe applies — SMT, a busy
//! machine, an unreadable governor — can only make a measured open **slower**,
//! so a PASS with margin on an unfit host is a conservative claim, and a FAIL
//! is not attributable to the code. That is the whole of the licence. It buys a
//! PASS and it does not buy a FAIL, which is why the verdict line prints the
//! fitness reasons whichever way it goes, and why the gated arm is the resident
//! one: its margin against the budget is two orders of magnitude, where the
//! evicted arm's is a factor of four and one seek on a slower device would eat
//! it.
//!
//! **A debug build is not refused, and that is the direction argument again
//! rather than an exception to it.** §9.3 says a debug build reaches every axis
//! because it is a different program rather than a slower one, and that is
//! right for a row compared against a committed baseline. Against a *budget* a
//! debug build is strictly conservative: it is slower, so a debug PASS is a
//! stronger statement than a release one. What a debug build cannot support is
//! a **FAIL**, which is not attributable to the code — so the profile is
//! printed on every run and the FAIL line says so. `just gate2` builds
//! `--release`, which is what makes the wired gate a release measurement;
//! `crates/tf_tree_bench/tests/gate2.rs` drives the same binary in debug, where
//! what it proves is the wiring and the arithmetic rather than a duration.
//!
//! # The vacuous passes this deliberately avoids
//!
//! An `mmap` of a small file is fast for reasons that have nothing to do with
//! this design, so two floors are checked before any verdict:
//!
//! * the gated fixture must be at least `GATE_INDEX_FLOOR_BYTES` — the
//!   criterion's own "233 MB index". A gate run against the 24-frame fixture
//!   passes 10 ms trivially and says nothing;
//! * the two fixtures must differ in size by at least `SCALE_SPAN`x, or the
//!   scale-invariance arm is comparing two files of the same size and cannot
//!   fail.
//!
//! Both **refuse** rather than passing, because a check whose subject set is
//! empty is green exactly when the measurement did not happen.
//!
//! Likewise the evicted arm verifies its own premise: `dd oflag=nocache` is
//! asked to drop the file's cache and the child then reports its own major
//! fault count, which is 1 when the open genuinely faulted the file in and 0
//! when it did not. This is not hypothetical — a "cold" loop that keeps an
//! earlier mapping alive reports the resident number and calls it cold, because
//! `POSIX_FADV_DONTNEED` cannot evict a page another mapping still holds.
//!
//! **A `--gate` run whose eviction did not take REFUSES; an ungated one voids
//! the arm and says so.** The evicted arm gates nothing, so a report that loses
//! it still carries every gated number — but a gate that loses it did not
//! measure what it names. The common cause is not a broken `dd`: it is a
//! **filesystem whose pages are RAM**, where nothing can evict. `$TMPDIR` is a
//! tmpfs on many hosts, which is why `tests/gate2.rs` puts its fixtures beside
//! this binary in the cargo target directory — the filesystem `just gate2`
//! itself writes to — rather than under `std::env::temp_dir()`.
//!
//! # `--gate` — the caller says whether this run is a gate
//!
//! `src/bin/frozen_workers.rs`'s shape, for its reason: printing FAIL and
//! exiting 0 is the failure `docs/benchmarks/EVIDENCE.md` exists to prevent.
//! Unlike gate 4 there is no second arm to defer, so `--gate` refuses no **mode**
//! — in particular it does **not** refuse `--prefault`, which is this gate's
//! falsifier and must be able to turn it red. What it does refuse is a *run it
//! cannot evaluate*: a fixture under the criterion's own scale, two fixtures too
//! close in size to compare, an evicted arm whose eviction was not witnessed,
//! and a run that would PASS against a `--budget-ms` **above** the criterion's
//! own. Those are refusals rather than verdicts, and they are a different thing
//! from declining a flag pair. The third is a refusal **only under `--gate`** —
//! see the evicted arm's premise above. Gate 4 refuses `--gate --no-touch`
//! because that control produces a meaningless *pass*; a control that produces
//! a real FAIL is the opposite case and gating on it is the point.
//!
//! The fourth is that rule applied to a threshold rather than to a mode. Gate 4
//! has no threshold flag at all, and a gate whose comparison a caller can move
//! in the loosening direction is a gate a caller can green. So `--budget-ms`
//! keeps both of the directions that can only make a run *harder* — lowering
//! it, and raising it on a run that still fails, which is how `tests/gate2.rs`
//! isolates each half of the conjunctive verdict — and loses the one that can
//! only make it easier.
//!
//! # The falsifier
//!
//! `--prefault` reads every byte of the `.tft` inside the timed region. It is a
//! stand-in for the live regression this gate exists to catch — a `populate_hot`
//! arm reaching the frozen backing, which
//! `crates/tf_tree/src/tree.rs`'s `populate_edge_rings` today refuses by
//! matching on the backing rather than by discipline — and it edits no
//! threshold: it is the gate's own arithmetic on an open that does
//! size-proportional work. `--budget-ms` exists too and is the **weaker** of the
//! two, because moving a threshold proves only that the comparison is wired —
//! and under `--gate` it is the weaker one in one direction only: a raised
//! budget that would produce a PASS is refused, per the section above.
//!
//! A fault-count assertion would *not* catch that regression in every form:
//! prefaulting inside `mmap(MAP_POPULATE)` costs the time and generates no
//! counted minor faults. The fault counts are printed as corroboration; the
//! verdict is on time.
//!
//! # What this gate does not cover
//!
//! §2.2 is explicit that a `.tft` is deliberately not prefaulted, so a fast open
//! is by design and the cost is deferred to first touch. A gate on the open
//! alone cannot see work *moved* into the first lookup, and does not claim to.
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

/// §12 gate 2's budget, in milliseconds.
const BUDGET_MS: f64 = 10.0;

/// §12 gate 2's "233 MB index", as a floor on the gated fixture.
///
/// Decimal MB, because §2.5's sizing arithmetic that produces the figure is in
/// decimal MB (115 + 92 + 26).
const GATE_INDEX_FLOOR_BYTES: u64 = 233_000_000;

/// How much bigger the gated fixture must be than the small one for the
/// scale-invariance arm to be about scale.
const SCALE_SPAN: u64 = 8;

/// The factor the two fixtures' open times must agree within.
///
/// Not a tuned number: `open_frozen` does the same O(1) work at both sizes, so
/// the honest expectation is 1.0 and everything above it is this host's noise
/// at the tens-of-microseconds scale the resident arm measures. It is set far
/// enough above that noise that a run does not flap, and far enough below the
/// regression it guards — any step proportional to the index moves this by the
/// size ratio, which the fixtures hold at `SCALE_SPAN`x or more — that the
/// distance is orders of magnitude rather than a margin.
const SCALE_BOUND: f64 = 4.0;

/// §12 gate 2's criterion, as one expression the verdict line and the exit
/// status are both taken from.
fn within_budget(worst_ms: f64, budget_ms: f64) -> bool {
    worst_ms <= budget_ms
}

/// The other half of the criterion: the open does not grow with the index.
///
/// **Taken over each arm's *best* open, where the budget is taken over its
/// worst, and the asymmetry is deliberate.** A budget is a worst-case claim, so
/// it reads the worst. A ratio is an estimate of a fixed cost, and the noise on
/// this host is one-sided — a descheduled process is slower, never faster — so
/// the minimum of N is the least contaminated estimate of what an open costs,
/// while a quotient of two worsts multiplies two independent noise draws.
/// Measured on the development host: over ten runs of eight rounds the
/// best/best ratio spanned **1.02-1.42** against a bound of 4, while an earlier
/// five-run sitting on the same fixtures measured worst/worst at **0.63-1.90** —
/// twice the spread, in both directions, off the same opens. The control fails
/// it by more than an order of magnitude either way.
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

/// Everything the driver was asked for, as one value.
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

/// A fixture at §12 gate 2's own scale.
///
/// The shape is `frozen_workers`'s `Fleet`, and the two binaries name it
/// separately rather than sharing one function: this one needs the shape at
/// *two* sizes, gate 4's needs the one size its arithmetic (`S >= 74p`) is
/// calibrated for, and lifting gate 4's private `spec()` into the library to
/// serve gate 2 would edit the gate-4 binary for gate 2's benefit. What is
/// shared is the thing that matters — `workload::Workload` itself, which is the
/// one path either binary has to build a tree.
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
    // `source_digest` all-zero and `source` None: a synthesized workload, not a
    // recording — the `--from-live` case `freeze_to` documents.
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

/// One child: one timed `Tree::open_frozen`, its own fault counts, one line out.
///
/// **A fresh process per open, and that is not incidental.** Within one process
/// the second open of the same file costs a fraction of the first, and a `p50`
/// over in-process repeats reports the number that costs nothing while never
/// reporting the one that does. §12 says "open time"; a gate is a worst-case
/// claim, so the driver takes the worst of N fresh processes.
fn open_once(path: &Path, prefault: bool) -> Result<()> {
    let before = faults()?;
    let started = Instant::now();
    let tree = Tree::open_frozen(path).map_err(|e| anyhow!("opening {}: {e:?}", path.display()))?;
    // **The control, inside the timed region.** Any step of `open_frozen` that
    // was proportional to the index would land exactly here.
    let read = if prefault { read_whole(path)? } else { 0 };
    let elapsed = started.elapsed();
    let after = faults()?;
    // The tree is dropped after the clock stops, so an unmap does not land in
    // the measurement.
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

/// Read every byte of the file, returning how many.
///
/// The `--prefault` control. It is a `read(2)` walk rather than a page-touch
/// walk of the mapping, and the header says why that is the right stand-in: the
/// property under test is that no step of an open is proportional to the index,
/// and the mechanism by which a proportional step would arrive does not change
/// whether it fits in 10 ms.
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

/// `(minflt, majflt)` from `/proc/self/stat`.
///
/// Fields 10 and 12, parsed after the last `)` because the second field is the
/// executable name and may contain spaces and parentheses — the same care
/// `crate::mp`'s CPU-time reader takes with fields 14 and 15 of the same line.
fn faults() -> Result<(u64, u64)> {
    let stat = std::fs::read_to_string("/proc/self/stat").context("reading /proc/self/stat")?;
    let rest = stat
        .rsplit_once(')')
        .ok_or_else(|| anyhow!("/proc/self/stat has no `)` — not a Linux stat line"))?
        .1;
    // After the `)` the fields are 3..; `state` is the first, so minflt is at
    // index 7 and majflt at index 9 of what remains.
    let f: Vec<&str> = rest.split_whitespace().collect();
    let get = |i: usize, what: &str| -> Result<u64> {
        f.get(i)
            .ok_or_else(|| anyhow!("/proc/self/stat is too short for {what}"))?
            .parse::<u64>()
            .with_context(|| format!("parsing {what} from /proc/self/stat"))
    };
    Ok((get(7, "minflt")?, get(9, "majflt")?))
}

/// Drop `path`'s page cache, and say whether the request was even made.
///
/// **`dd` rather than `posix_fadvise`.** `tf_tree_bench`'s library is
/// `#![forbid(unsafe_code)]` and `CLAUDE.md`'s unsafe budget routes a *new kind*
/// of `unsafe` to a decision record; `src/bin/contended_scaling.rs` and
/// `src/bin/load_child.rs` both take the same way out for the same reason, by
/// reaching for a process (`taskset -c N`) instead of a syscall. GNU `dd`'s
/// `oflag=nocache conv=notrunc,fdatasync count=0` is the documented idiom for
/// dropping one file's cache, and the `fdatasync` half closes the hazard that a
/// `posix_fadvise` alone would leave: `DONTNEED` does not write back dirty
/// pages, so on a fixture this run has just frozen the eviction could quietly
/// no-op.
///
/// Nothing here trusts it. The witness is the child's own major-fault count.
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

/// Read the whole file so its pages are resident.
fn warm(path: &Path) -> Result<()> {
    read_whole(path).map(|_| ())
}

/// One arm's measurement of one fixture.
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

/// Which cache state an arm is measured in.
///
/// `Evicted { requested: false }` is the control: the arm that is *about* an
/// evicted page cache, run without evicting, so the witness below has something
/// to catch.
#[derive(Clone, Copy)]
enum Cache {
    Evicted { requested: bool },
    Resident,
}

/// Run one arm: `rounds` fresh processes, each preceded by whatever cache state
/// this arm is about.
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

    // **The two floors, checked before anything is measured.** Both refuse
    // rather than passing: a gate whose subject is too small to fail is green
    // exactly when it checked nothing.
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
    // Without `--gate` the floors above are not refusals, so say it here rather
    // than letting a PASS read as a PASS of the criterion. **Both floors get a
    // line**: the scale-invariance one had none, so an ungated run over two
    // fixtures of the same size printed a line prefixed `GATED` over a
    // comparison that is structurally green.
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

    // **The evicted arm's premise, checked rather than assumed — and the
    // consequence of a failed premise depends on whether this run is a gate.**
    //
    // A gated run REFUSES: a gate that cannot establish its own premise must
    // not publish. An ungated run degrades and says so, which is
    // `src/bin/contended_scaling.rs`'s shape when its `taskset` helper is
    // unavailable. The asymmetry is not a softening: the evicted arm gates
    // nothing (the size dependence there is the storage device), so a *report*
    // that loses it still carries every gated number, while a **gate** that
    // loses it is a gate that did not measure what it names.
    //
    // The ordinary way this fires is not a bug in `dd` — it is a filesystem
    // that cannot evict, because its pages are RAM. `$TMPDIR` is a tmpfs on
    // many hosts, and a container can have a RAM-backed workdir.
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

    // **A loosened budget may not produce a gated PASS.** `SCALE_BOUND` is a
    // constant, but `--budget-ms` is not, and the verdict is a conjunction of
    // the two — so a raised budget can only ever help a run pass. Refused in
    // the loosening direction only, and before any verdict line is printed: a
    // refusal must publish nothing. Lowering it stays legal, which is what
    // `tests/gate2.rs` uses to drive the budget half red on its own; so does
    // raising it *while the run still fails*, which is how that file isolates
    // the scale half.
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

    /// **The gate's arithmetic can say no.** `frozen_workers`'s
    /// `gate_arithmetic_is_not_vacuous` in the same crate, for the same reason:
    /// a comparison that has never been observed to fail is a comparison
    /// nobody has tested.
    #[test]
    fn the_budget_comparison_is_not_vacuous() {
        assert!(within_budget(2.5, BUDGET_MS));
        assert!(
            within_budget(BUDGET_MS, BUDGET_MS),
            "the bound is inclusive"
        );
        assert!(!within_budget(126.8, BUDGET_MS));
    }

    /// The other half, separately — a union of two checks is where one of them
    /// stops being read.
    #[test]
    fn the_scale_comparison_is_not_vacuous() {
        assert!(scale_invariant(0.09, 0.10));
        assert!(scale_invariant(0.40, 0.10), "4x is the bound, inclusive");
        assert!(!scale_invariant(0.41, 0.10));
        assert!(!scale_invariant(20.0, 0.15), "a size-proportional open");
    }

    /// The two floors are what stop a small fixture passing trivially.
    #[test]
    fn the_gate_scale_floor_is_the_criterions_own_number() {
        assert_eq!(GATE_INDEX_FLOOR_BYTES, 233_000_000);
        assert_eq!(SCALE_SPAN, 8);
    }
}
