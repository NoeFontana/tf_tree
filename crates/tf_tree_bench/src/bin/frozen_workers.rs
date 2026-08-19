//! **`docs/PHASE5.md` §12 gate 4**, which nothing has ever run: *16 workers
//! sharing one `.tft`, total Pss within 1.2× of one worker.*
//!
//! # Why this had never been measured
//!
//! `just bench-report`'s `tft_16_workers_rss` row has always been `UNAVAILABLE`,
//! and for two reasons that both dissolved:
//!
//! * the report binary is built without `shm`, so there is no `Tree::open_frozen`
//!   in it to call — true, and the reason it is a separate binary here;
//! * the core budget refused it, on the grounds that sixteen consumers on four
//!   physical cores measure the scheduler. `report.rs`'s `Sensitivity::Memory`
//!   axis retired that: **sixteen workers mapping one `.tft` share exactly the
//!   pages they would share on sixteen cores.** Pss is not a timing measurement.
//!
//! # What the gate is actually asking, and why the `.tft` must be big
//!
//! Summed Pss over N processes is *total unique bytes*, because Pss divides each
//! shared page by the number of mappers. So with `S` bytes of shared `.tft` that
//! every worker touches and `p` bytes private to each process:
//!
//! ```text
//!   total(N) = S + N·p
//!   total(16) / total(1) = (S + 16p) / (S + p) <= 1.2   <=>   S >= 74·p
//! ```
//!
//! A Rust process with a mapped arena costs roughly 3 MiB private, so the gate
//! is only passable when `S` is on the order of **220 MiB** — which is exactly
//! the "233 MB index" §12 gate 2 names. The gate is calibrated for a real
//! dataloader corpus, and running it against the 24-frame fixture would report a
//! failure that is arithmetic about process overhead rather than anything about
//! the design. `--robots`/`--history` default to a shape that lands in range.
//!
//! # The vacuous pass this deliberately avoids
//!
//! `open_frozen` is an `mmap`. A worker that maps the file and never reads it
//! has almost no resident share of it, `S ≈ 0`, and the ratio collapses to
//! `16p/p = 16` — or, if the private cost were also small, to a meaningless
//! pass. **Every worker sweeps lookups across the whole stamp window before
//! reporting**, so the pages counted are pages actually read. `--no-touch`
//! exists only to show the difference, and prints a warning saying so.
//!
//! # Two worker arms, because `S >= 74p` is arithmetic about the worker
//!
//! `p` is private bytes per process, and nothing about it belongs to `tf_tree`:
//! it is the interpreter, its extension modules and the worker's own
//! allocations. So the gate's verdict is a function of the worker's language,
//! and the default arm — this binary, re-executed with `--worker` — is a Rust
//! one. `--python <interpreter>` runs the same measurement with
//! `python/gate4_worker.py` as the worker instead, which is the arm the wedge's
//! audience actually runs. §12 gate 4's amendment carries the `p` table; the
//! criterion itself is stated over the Rust arm and this binary does not move
//! it.
//!
//! # Usage
//!
//! ```text
//! frozen_workers --build /tmp/x.tft --robots 64 --history 40
//! frozen_workers --tft /tmp/x.tft --workers 1,16
//! frozen_workers --tft /tmp/x.tft --workers 1,16 --python .venv/bin/python
//! frozen_workers --worker /tmp/x.tft          # a child; not run by hand
//! ```

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};

use tf_tree::Tree;
use tf_tree_bench::workload::{Backing, QuerySpec, Topology, Workload};

/// §12 gate 4's threshold.
const GATE: f64 = 1.2;

/// Worker counts the gate is stated over.
const DEFAULT_WORKERS: &[usize] = &[1, 16];

/// The stamp window a sweep walks, in nanoseconds — the default fixture's 40 s
/// of history.
///
/// Passed to a Python worker on its command line rather than restated in that
/// file, so the two arms cannot drift into sweeping different query sets. It
/// does *not* track `--history`: a shorter fixture leaves part of the grid
/// outside every edge's window, which both arms report identically in the
/// `lookups` column.
const SWEEP_WINDOW_NS: i64 = 40_000_000_000;

fn main() -> Result<()> {
    let mut mode = Mode::Drive;
    let mut path = PathBuf::from("target/gate4/workers.tft");
    let mut robots = 64usize;
    let mut history = 40.0f64;
    let mut workers: Vec<usize> = DEFAULT_WORKERS.to_vec();
    let mut touch = true;
    // Stamps swept per edge — i.e. how much of the arena a worker's working set
    // covers. **Not a tuning knob for the verdict.** The gate's outcome depends
    // on it, which is the finding, so the driver reports the curve over several
    // values rather than one number from one arbitrary pattern.
    let mut stamps = 64usize;
    let mut interpreter: Option<PathBuf> = None;
    let mut py_worker: Option<PathBuf> = None;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--build" => {
                mode = Mode::Build;
                path = PathBuf::from(args.next().ok_or_else(|| anyhow!("{a} wants a path"))?);
            }
            "--worker" => {
                mode = Mode::Worker;
                path = PathBuf::from(args.next().ok_or_else(|| anyhow!("{a} wants a path"))?);
            }
            "--tft" => {
                path = PathBuf::from(args.next().ok_or_else(|| anyhow!("{a} wants a path"))?)
            }
            "--robots" => {
                robots = args
                    .next()
                    .ok_or_else(|| anyhow!("{a} wants a count"))?
                    .parse()
                    .context("--robots")?
            }
            "--history" => {
                history = args
                    .next()
                    .ok_or_else(|| anyhow!("{a} wants seconds"))?
                    .parse()
                    .context("--history")?
            }
            "--workers" => {
                workers = args
                    .next()
                    .ok_or_else(|| anyhow!("{a} wants a comma-separated list"))?
                    .split(',')
                    .map(|s| s.trim().parse::<usize>().context("--workers"))
                    .collect::<Result<_>>()?;
                if workers.is_empty() {
                    bail!("--workers needs at least one count");
                }
            }
            "--stamps" => {
                stamps = args
                    .next()
                    .ok_or_else(|| anyhow!("{a} wants a count"))?
                    .parse()
                    .context("--stamps")?;
            }
            "--python" => {
                interpreter = Some(PathBuf::from(
                    args.next().ok_or_else(|| anyhow!("{a} wants a path"))?,
                ))
            }
            "--py-worker" => {
                py_worker = Some(PathBuf::from(
                    args.next().ok_or_else(|| anyhow!("{a} wants a path"))?,
                ))
            }
            "--no-touch" => touch = false,
            other => bail!("unknown argument `{other}`"),
        }
    }

    let arm = match interpreter {
        None => {
            if py_worker.is_some() {
                bail!(
                    "--py-worker names the script the Python arm runs, and the Python arm is \
                       selected by --python <interpreter>"
                );
            }
            Arm::Rust
        }
        // The default is baked in at compile time, so the binary can be run
        // from anywhere the way the `just gate4` comment suggests. This crate
        // is `publish = false`, so the absolute build-machine path costs
        // nobody anything.
        Some(interpreter) => Arm::Python {
            interpreter,
            script: py_worker.unwrap_or_else(|| {
                Path::new(env!("CARGO_MANIFEST_DIR")).join("python/gate4_worker.py")
            }),
        },
    };

    match mode {
        Mode::Build => build(&path, robots, history),
        Mode::Worker => worker(&path, touch, stamps),
        Mode::Drive => drive(&path, robots, history, &workers, touch, stamps, &arm),
    }
}

enum Mode {
    Build,
    Worker,
    Drive,
}

/// Which program the driver spawns as a worker.
///
/// The two arms differ in the worker and in nothing else — same fixture, same
/// stamp grid, same barrier, same `smaps_rollup` read — because the finding
/// they exist to separate is that `p` belongs to the worker.
enum Arm {
    Rust,
    Python {
        interpreter: PathBuf,
        script: PathBuf,
    },
}

impl Arm {
    /// The word the verdict line is cited with. §12 gate 4's amendment: a
    /// qualification that does not travel with the number it qualifies has not
    /// been written down anywhere useful.
    fn language(&self) -> &'static str {
        match self {
            Arm::Rust => "Rust",
            Arm::Python { .. } => "Python",
        }
    }
}

/// A fleet workload big enough for the gate to be about sharing.
///
/// Constructed here rather than taken from `workload::by_name`, because the
/// named table tops out at `extreme_wide` (512 robots × 1 s ≈ 46 MiB) and the
/// arithmetic above needs roughly five times that. The shape is a `Fleet`
/// exactly like `fleet_64`; only the history is longer.
fn spec(robots: usize, history: f64) -> Workload {
    Workload {
        name: "gate4_fleet",
        topology: Topology::Fleet {
            robots,
            history_secs: history,
        },
        queries: QuerySpec::CrossFleet,
        note: "PHASE5 §12 gate 4: many workers, one frozen arena",
    }
}

fn build(path: &Path, robots: usize, history: f64) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let w = spec(robots, history);
    let est = w.estimate()?;
    eprintln!(
        "building {robots} robots x {history} s: {} frames, {} edges, {} samples, {:.1} MiB arena",
        est.frames,
        est.edges,
        est.samples,
        est.arena_bytes as f64 / (1024.0 * 1024.0)
    );
    let built = w.build(tf_tree::InterpPolicy::LerpSlerp, Backing::Heap)?;
    // `source_digest` all-zero and `source` None: this arena came from a
    // synthesized workload, not a recording, which is exactly the `--from-live`
    // case `freeze_to` documents.
    let header = built
        .tree
        .freeze_to(path, None, [0u8; 32], 0)
        .map_err(|e| anyhow!("freezing to {}: {e:?}", path.display()))?;
    let bytes = std::fs::metadata(path)?.len();
    println!(
        "wrote {} — {:.1} MiB on disk (format {})",
        path.display(),
        bytes as f64 / (1024.0 * 1024.0),
        header.format_version
    );
    Ok(())
}

/// One worker: map the `.tft`, read it, report Pss, then hold the mapping open
/// until stdin closes.
///
/// **Holding it open is the whole point.** The driver measures while every
/// worker is alive; a worker that exited would have its pages unmapped and the
/// sum would be of processes that no longer share anything.
fn worker(path: &Path, touch: bool, stamps: usize) -> Result<()> {
    let tree = Tree::open_frozen(path).map_err(|e| anyhow!("opening {}: {e:?}", path.display()))?;

    let read = if touch { sweep(&tree, stamps)? } else { 0 };

    // **Two phases, and the barrier between them is not optional.**
    //
    // Pss divides each shared page by the number of processes *currently*
    // mapping it. A worker that reports as soon as it finishes sweeping is
    // measured while later workers have not faulted the pages in yet, so its
    // share is divided by three rather than by sixteen — and the total comes out
    // inflated. The first version of this binary did exactly that, and the tell
    // was that the solved-for private cost per worker *grew with sweep length*
    // (3.4 MiB at 16 stamps/edge to 10.8 MiB at 4096), which is not a thing
    // private memory does. Longer sweep, more skew between first and last
    // finisher, more inflation.
    //
    // So: announce readiness, block until the driver has heard from all of
    // them, and only then read `smaps_rollup`.
    println!("ready {read}");
    std::io::stdout().flush()?;

    let mut go = String::new();
    std::io::stdin()
        .read_line(&mut go)
        .context("waiting for the driver's go-ahead")?;
    if go.is_empty() {
        bail!("the driver closed stdin before releasing the barrier");
    }

    let pss = tf_tree_bench::mp::self_pss_kib();
    println!("pss {pss}");
    std::io::stdout().flush()?;

    let mut sink = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut sink);
    drop(tree);
    Ok(())
}

/// Read across the whole tree so the pages counted are pages actually touched.
///
/// A dataloader worker's access pattern is the reason this is a *sweep* rather
/// than a hot loop on one pair: it walks every plan across the full stamp
/// window, which is what faults in the sample rings. Returns the number of
/// successful lookups, printed so a run that silently answered nothing cannot
/// be mistaken for a run that shared everything.
fn sweep(tree: &Tree, stamps: usize) -> Result<u64> {
    let mut ok = 0u64;
    let guard = tree.guard();
    // `Tree::edges()` — every declared `(parent, child)`. Better than walking
    // frames: one plan per edge reads that edge's ring, so the sweep touches
    // every sample region in the file rather than only those on paths back to
    // some chosen root. That is what makes `S` the whole `.tft` and not a
    // fraction of it.
    let edges = tree
        .edges()
        .map_err(|e| anyhow!("enumerating the frozen tree's edges: {e:?}"))?;
    if edges.is_empty() {
        bail!("the frozen tree declares no edges, so there is nothing to read");
    }
    for (parent, child) in &edges {
        let (Ok(t), Ok(s)) = (tree.frame(child), tree.frame(parent)) else {
            continue;
        };
        let Ok(plan) = tree.plan(t, s) else { continue };
        // Stamps spread across the whole history rather than one: a single
        // stamp lands in one cache line of the ring and would leave most of it
        // unread, which is the vacuous-pass failure this binary exists to
        // avoid. The step shrinks as `stamps` grows, so a larger count means a
        // denser walk of the same window, not a longer one.
        let step = (SWEEP_WINDOW_NS / stamps.max(1) as i64).max(1);
        for k in 0..stamps as i64 {
            let stamp = tf_tree::Stamp::<tf_tree::SystemDomain>::from_nanos(k * step);
            if plan.at(&guard, stamp).is_ok() {
                ok += 1;
            }
        }
    }
    Ok(ok)
}

fn drive(
    path: &Path,
    robots: usize,
    history: f64,
    workers: &[usize],
    touch: bool,
    stamps: usize,
    arm: &Arm,
) -> Result<()> {
    // Checked before anything is built or spawned: a missing script otherwise
    // presents as sixteen tracebacks and a driver complaining about a `ready`
    // line it never got.
    if let Arm::Python { script, .. } = arm {
        if !script.exists() {
            bail!(
                "no Python worker script at {} — name one with --py-worker, the way \
                 `just gate4-python` does",
                script.display()
            );
        }
    }

    if !path.exists() {
        build(path, robots, history)?;
    }
    let file_mib = std::fs::metadata(path)?.len() as f64 / (1024.0 * 1024.0);
    let me = std::env::current_exe().context("locating this executable")?;

    if !touch {
        eprintln!(
            "WARNING --no-touch: workers map the .tft and never read it, so almost none of it \
             is resident and the ratio is not a measurement of sharing."
        );
    }

    println!("PHASE5 §12 gate 4 — 16 workers sharing one .tft, total Pss within {GATE}x of one");
    println!("  .tft {} ({file_mib:.1} MiB)", path.display());
    match arm {
        Arm::Rust => println!("  worker  Rust — this binary, re-executed with --worker"),
        Arm::Python {
            interpreter,
            script,
        } => println!(
            "  worker  Python — {} {}",
            interpreter.display(),
            script.display()
        ),
    }
    println!();
    println!(
        "  {:>7}  {:>12}  {:>12}  {:>10}",
        "workers", "total Pss", "per worker", "lookups"
    );

    let mut totals: Vec<(usize, f64)> = Vec::new();
    for &n in workers {
        let (total_kib, reads) = spawn_and_measure(&me, path, n, touch, stamps, arm)?;
        let mib = total_kib as f64 / 1024.0;
        println!(
            "  {n:>7}  {:>9.1} MiB  {:>9.2} MiB  {reads:>10}",
            mib,
            mib / n as f64
        );
        totals.push((n, mib));
    }
    println!();

    // The gate is stated against one worker, so it needs the N = 1 row.
    let Some(&(_, one)) = totals.iter().find(|(n, _)| *n == 1) else {
        println!("  no N = 1 row, so the gate cannot be evaluated — include 1 in --workers");
        return Ok(());
    };
    let Some(&(_, sixteen)) = totals.iter().find(|(n, _)| *n == 16) else {
        println!("  no N = 16 row, so the gate cannot be evaluated — include 16 in --workers");
        return Ok(());
    };

    let ratio = sixteen / one;
    let verdict = if ratio <= GATE { "PASS" } else { "FAIL" };
    println!(
        "  gate 4, {} worker: {sixteen:.1} MiB / {one:.1} MiB = {ratio:.3}x against {GATE}x \
         — {verdict}",
        arm.language()
    );

    // The decomposition, because a bare ratio does not say *why*, and the two
    // terms have completely different remedies: more sharing is a design
    // property, less private is a process-count question.
    //
    // total(N) = S + N.p, so two rows solve for both.
    let n1 = 1.0;
    let n16 = 16.0;
    let private = (sixteen - one) / (n16 - n1);
    let shared = one - private * n1;
    println!(
        "  solving total(N) = S + N*p over the two rows: S = {shared:.1} MiB shared, \
         p = {private:.2} MiB private per worker"
    );
    if private > 0.0 {
        println!(
            "  the gate needs S >= {:.0}x p, i.e. >= {:.0} MiB, and S is {shared:.0} MiB",
            (n16 - GATE) / (GATE - n1),
            (n16 - GATE) / (GATE - n1) * private
        );
    }
    if let Arm::Python { .. } = arm {
        println!();
        println!(
            "  This arm REPORTS. Criterion 4 is stated over the Rust worker and its MET is that \
             row; giving the gate a second arm is a decision and needs a record (PHASE5 §12 gate \
             4's amendment). `p` above is a property of the interpreter and its extension \
             modules, not of tf_tree."
        );
    }
    Ok(())
}

/// Spawn `n` workers, wait for every one to report, sum their Pss, then stop
/// them.
///
/// **Every worker is alive when the sum is taken.** The children each print one
/// line and then block on stdin, so the driver reads `n` lines before closing
/// any of them; measuring as they exit would sum processes that had already
/// dropped the mapping.
fn spawn_and_measure(
    me: &Path,
    tft: &Path,
    n: usize,
    touch: bool,
    stamps: usize,
    arm: &Arm,
) -> Result<(u64, u64)> {
    let mut kids = Vec::with_capacity(n);
    for _ in 0..n {
        // Spawned in both arms, never forked. A forked Python worker inherits
        // the parent's heap and measures a `p` no `DataLoader` on CPython 3.14
        // pays — §4.3's amendment — and the Rust arm has never had another
        // shape.
        let (program, mut cmd) = match arm {
            Arm::Rust => {
                let mut cmd = Command::new(me);
                cmd.arg("--worker").arg(tft);
                (me, cmd)
            }
            Arm::Python {
                interpreter,
                script,
            } => {
                let mut cmd = Command::new(interpreter);
                cmd.arg(script)
                    .arg(tft)
                    .arg("--window-ns")
                    .arg(SWEEP_WINDOW_NS.to_string());
                (interpreter.as_path(), cmd)
            }
        };
        cmd.arg("--stamps").arg(stamps.to_string());
        if !touch {
            cmd.arg("--no-touch");
        }
        let child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("spawning worker {}", program.display()))?;
        kids.push(child);
    }

    // Phase 1: every worker maps, sweeps and announces. Readers are kept so the
    // second phase can go on reading the same pipes.
    let mut outs = Vec::with_capacity(n);
    let mut reads = 0u64;
    for (i, kid) in kids.iter_mut().enumerate() {
        let out = kid
            .stdout
            .take()
            .ok_or_else(|| anyhow!("worker {i} has no stdout"))?;
        let mut rdr = BufReader::new(out);
        let mut line = String::new();
        rdr.read_line(&mut line)
            .with_context(|| format!("reading worker {i}"))?;
        let mut f = line.split_whitespace();
        match (f.next(), f.next()) {
            (Some("ready"), Some(r)) => reads += r.parse::<u64>().context("worker read count")?,
            // An empty line is end-of-pipe: the worker died before reporting,
            // and its own diagnosis is already on the inherited stderr above.
            // For the Python arm that is almost always an interpreter with no
            // extension installed in it.
            (None, _) => bail!(
                "worker {i} exited without reporting — its stderr is above (Python arm: is \
                 the extension installed in that interpreter? `just gate4-python` does it)"
            ),
            _ => bail!("worker {i} said {line:?}, which is not a `ready <n>` line"),
        }
        outs.push(rdr);
    }

    // The barrier. Every worker now holds the same pages, so every Pss taken
    // after this divides by the same N.
    for (i, kid) in kids.iter_mut().enumerate() {
        let stdin = kid
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("worker {i} has no stdin"))?;
        writeln!(stdin, "go").with_context(|| format!("releasing worker {i}"))?;
        stdin.flush().ok();
    }

    // Phase 2: collect.
    let mut total = 0u64;
    for (i, rdr) in outs.iter_mut().enumerate() {
        let mut line = String::new();
        rdr.read_line(&mut line)
            .with_context(|| format!("reading worker {i} Pss"))?;
        let mut f = line.split_whitespace();
        match (f.next(), f.next()) {
            (Some("pss"), Some(kib)) => total += kib.parse::<u64>().context("worker Pss")?,
            _ => bail!("worker {i} said {line:?}, which is not a `pss <kib>` line"),
        }
    }

    for mut kid in kids {
        drop(kid.stdin.take());
        let _ = kid.wait();
    }
    Ok((total, reads))
}
