//! **`docs/PHASE5.md` §12 gate 4**: *16 workers sharing one `.tft`, total Pss
//! within 1.2x of one worker.*
//!
//! `just bench-report`'s `tft_16_workers_rss` row is `UNAVAILABLE` because the
//! report binary is built without `shm`, hence this separate binary; and Pss is not
//! a timing measurement (`report.rs`'s `Sensitivity::Memory`), so sixteen workers
//! on fewer cores share the same pages. `S`, `p` and the criterion are defined in
//! `docs/PHASE5.md` §12 criterion 4; `--robots`/`--history` default to a shape in
//! range.
//!
//! # The vacuous pass avoided
//!
//! `open_frozen` is an `mmap`; a worker that never reads it has `S ~ 0` and a
//! meaningless ratio. **Every worker sweeps lookups across the whole stamp window
//! before reporting.** `--no-touch` shows the difference and warns. The default arm
//! re-executes this binary with `--worker`; `--python <interpreter>` uses
//! `python/gate4_worker.py` instead.
//!
//! # `--gate`
//!
//! `docs/PHASE5.md` §12 criterion 4. The gate is stated over the Rust arm and the
//! Python arm reports without gating (§12 gate 4's amendment), so gating is a flag:
//! `just gate4` passes `--gate`, `just gate4-python` does not. Refused at parse:
//!
//! * `--gate --python`: the deferred second gated arm needs a record;
//! * `--gate --no-touch`: the control is documented to FAIL at 5.32x.
//!
//! Under `--gate`, a run with no N = 1 or N = 16 row **refuses** rather than
//! exiting 0.
//!
//! # Usage
//!
//! ```text
//! frozen_workers --build /tmp/x.tft --robots 64 --history 40
//! frozen_workers --tft /tmp/x.tft --workers 1,16 --gate
//! frozen_workers --tft /tmp/x.tft --workers 1,16 --python .venv/bin/python
//! frozen_workers --tft /tmp/x.tft --workers 1,16 --no-touch   # the control
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

/// §12 gate 4's criterion, one expression for the verdict line and exit status;
/// a free function so `gate_arithmetic_is_not_vacuous` can drive it.
fn gate_4_holds(ratio: f64) -> bool {
    ratio <= GATE
}

/// Worker counts the gate is stated over.
const DEFAULT_WORKERS: &[usize] = &[1, 16];

/// The stamp window a sweep walks, in nanoseconds (the default fixture's 40 s).
/// Passed to a Python worker so the arms sweep the same queries; it does not track
/// `--history`.
const SWEEP_WINDOW_NS: i64 = 40_000_000_000;

fn main() -> Result<()> {
    let mut mode = Mode::Drive;
    let mut path = PathBuf::from("target/gate4/workers.tft");
    let mut robots = 64usize;
    let mut history = 40.0f64;
    let mut workers: Vec<usize> = DEFAULT_WORKERS.to_vec();
    let mut touch = true;
    // Stamps swept per edge (the working set). Not a tuning knob: the outcome
    // depends on it, so the driver reports a curve.
    let mut stamps = 64usize;
    let mut interpreter: Option<PathBuf> = None;
    let mut py_worker: Option<PathBuf> = None;
    // Whether this run is a gate is the caller's statement (header, `--gate`).
    let mut gate = false;

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
            "--gate" => gate = true,
            other => bail!("unknown argument `{other}`"),
        }
    }

    let sweep = Sweep { touch, stamps };

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
        // Baked in at compile time so the binary runs from anywhere (`publish = false`).
        Some(interpreter) => Arm::Python {
            interpreter,
            script: py_worker.unwrap_or_else(|| {
                Path::new(env!("CARGO_MANIFEST_DIR")).join("python/gate4_worker.py")
            }),
        },
    };

    // The two `--gate` combinations refused rather than obeyed; both need a record.
    if gate {
        if let Arm::Python { .. } = arm {
            bail!(
                "--gate --python asks this binary to gate on the Python arm. PHASE5 \
                 §12 gate 4 is stated over the Rust worker, and its amendment is explicit \
                 that giving criterion 4 a second *gated* arm is a decision that needs a \
                 decision record. Run `just gate4-python`, which reports the same \
                 measurement and exits 0."
            );
        }
        if !touch {
            bail!(
                "--gate --no-touch asks this binary to gate on the control. Workers that \
                 map the .tft and never read it have no resident share of it, so the ratio \
                 is arithmetic about process overhead — PHASE5 §12 gate 4 records the \
                 control at 5.32x FAIL, deliberately. Drop --gate to run it."
            );
        }
    }

    match mode {
        Mode::Build => build(&path, robots, history),
        Mode::Worker => worker(&path, sweep.touch, sweep.stamps),
        Mode::Drive => drive(&path, robots, history, &workers, sweep, &arm, gate),
    }
}

enum Mode {
    Build,
    Worker,
    Drive,
}

/// What every worker is asked to do; belongs to the measurement, not the verdict.
#[derive(Clone, Copy)]
struct Sweep {
    /// Whether workers read the mapping they open. `false` is the control.
    touch: bool,
    /// Stamps swept per edge.
    stamps: usize,
}

/// Which program the driver spawns. The arms differ only in the worker: `p`
/// belongs to the worker.
enum Arm {
    Rust,
    Python {
        interpreter: PathBuf,
        script: PathBuf,
    },
}

impl Arm {
    /// The word the verdict line is cited with (§12 gate 4's amendment).
    fn language(&self) -> &'static str {
        match self {
            Arm::Rust => "Rust",
            Arm::Python { .. } => "Python",
        }
    }
}

/// A fleet workload big enough for the gate to be about sharing: `fleet_64`'s
/// shape with longer history, since `workload::by_name` tops out near 46 MiB.
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
    // All-zero `source_digest`, `source` None: synthesized, as `freeze_to` documents.
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

/// One worker: map the `.tft`, read it, report Pss, then hold the mapping until
/// stdin closes; an exited worker would unmap and the sum would share nothing.
fn worker(path: &Path, touch: bool, stamps: usize) -> Result<()> {
    let tree = Tree::open_frozen(path).map_err(|e| anyhow!("opening {}: {e:?}", path.display()))?;

    let read = if touch { sweep(&tree, stamps)? } else { 0 };

    // Two phases with a barrier. Pss divides a shared page by the processes
    // *currently* mapping it, so a worker that reports early is divided by too few
    // and the total inflates. So: announce readiness, block until the driver has
    // heard from all, then read `smaps_rollup`.
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

/// Read across the whole tree so the pages counted are pages touched. Returns
/// the successful lookup count, printed so a silent no-op is visible.
fn sweep(tree: &Tree, stamps: usize) -> Result<u64> {
    let mut ok = 0u64;
    let guard = tree.guard();
    // `Tree::edges()`: one plan per edge reads every edge's ring, so `S` is the
    // whole `.tft`.
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
        // Stamps spread across the whole history: a single stamp touches one cache line
        // of the ring.
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
    sweep: Sweep,
    arm: &Arm,
    gate: bool,
) -> Result<()> {
    // Checked before building or spawning: a missing script otherwise presents as
    // sixteen tracebacks.
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

    if !sweep.touch {
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
        let (total_kib, reads) = spawn_and_measure(&me, path, n, sweep, arm)?;
        let mib = total_kib as f64 / 1024.0;
        println!(
            "  {n:>7}  {:>9.1} MiB  {:>9.2} MiB  {reads:>10}",
            mib,
            mib / n as f64
        );
        totals.push((n, mib));
    }
    println!();

    // The gate needs the N = 1 row. **Under `--gate` an unevaluable run is a
    // refusal**, not a zero exit.
    let Some(&(_, one)) = totals.iter().find(|(n, _)| *n == 1) else {
        println!("  no N = 1 row, so the gate cannot be evaluated — include 1 in --workers");
        if gate {
            bail!("--gate was passed and the criterion was not evaluated: no N = 1 row");
        }
        return Ok(());
    };
    let Some(&(_, sixteen)) = totals.iter().find(|(n, _)| *n == 16) else {
        println!("  no N = 16 row, so the gate cannot be evaluated — include 16 in --workers");
        if gate {
            bail!("--gate was passed and the criterion was not evaluated: no N = 16 row");
        }
        return Ok(());
    };

    let ratio = sixteen / one;
    let holds = gate_4_holds(ratio);
    let verdict = if holds { "PASS" } else { "FAIL" };
    println!(
        "  gate 4, {} worker: {sixteen:.1} MiB / {one:.1} MiB = {ratio:.3}x against {GATE}x \
         — {verdict}",
        arm.language()
    );

    // The decomposition, since a bare ratio does not say why: total(N) = S + N.p,
    // so two rows solve for both.
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

    // The difference between a gate and a report: `just gate4` (`--gate`) fails
    // `nightly.yml`'s `gate4` job; `just gate4-python` exits 0 on its FAIL (§12 gate 4's
    // amendment).
    if gate && !holds {
        bail!(
            "PHASE5 §12 criterion 4 is not met on this host: {sixteen:.1} MiB / \
             {one:.1} MiB = {ratio:.3}x against {GATE}x, with S = {shared:.1} MiB shared \
             and p = {private:.2} MiB private per worker. This exits non-zero so the \
             recipe is a gate rather than a report."
        );
    }
    Ok(())
}

/// Spawn `n` workers, wait for every one to report, sum their Pss, then stop
/// them. Every worker is alive when the sum is taken.
fn spawn_and_measure(
    me: &Path,
    tft: &Path,
    n: usize,
    sweep: Sweep,
    arm: &Arm,
) -> Result<(u64, u64)> {
    let mut kids = Vec::with_capacity(n);
    for _ in 0..n {
        // Spawned, never forked: a forked Python worker inherits the parent's heap
        // (§4.3's amendment).
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
        cmd.arg("--stamps").arg(sweep.stamps.to_string());
        if !sweep.touch {
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

    // Phase 1: every worker maps, sweeps and announces.
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
            // An empty line is end-of-pipe: the worker died before reporting; its
            // diagnosis is on the inherited stderr.
            (None, _) => bail!(
                "worker {i} exited without reporting — its stderr is above (Python arm: is \
                 the extension installed in that interpreter? `just gate4-python` does it)"
            ),
            _ => bail!("worker {i} said {line:?}, which is not a `ready <n>` line"),
        }
        outs.push(rdr);
    }

    // The barrier: every Pss after this divides by the same N.
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{gate_4_holds, GATE};

    /// **The negative control for the gate's arithmetic**, as `owner_migration.rs`.
    /// Ratios: 1.024x is §12 gate 4's **MET** with a Rust worker, 1.806x is `just
    /// gate4-python`'s reading, 5.32x the `--no-touch` control.
    #[test]
    fn gate_arithmetic_is_not_vacuous() {
        assert!(gate_4_holds(1.024), "the measured Rust arm must PASS");
        assert!(!gate_4_holds(1.806), "the measured Python arm must FAIL");
        assert!(!gate_4_holds(5.32), "the no-touch control must FAIL");
    }

    /// The boundary is closed: `GATE` itself passes, one ULP above does not (exact
    /// `f64` literals against the run's own constant).
    #[test]
    fn the_threshold_is_inclusive_and_one_ulp_above_it_fails() {
        assert!(gate_4_holds(GATE), "1.2x exactly is within 1.2x");
        assert!(
            !gate_4_holds(f64::from_bits(GATE.to_bits() + 1)),
            "the next representable f64 above the threshold is outside it"
        );
    }
}
