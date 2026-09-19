//! Serve the §11.1 fixture from a **named, rendezvous-discoverable** arena and
//! dump the identical sample stream, so a C++ process can measure both engines
//! with no Rust binding on either side.
//!
//! # Why this binary exists
//!
//! `docs/benchmarks/tf2.md` prices four measurement biases; the one an in-process
//! Rust harness cannot remove is the FFI boundary between `tf_tree_tf2_sys` and
//! `tf2::BufferCore`, charged to tf2 so every such ratio *flatters* `tf_tree`
//! (figure in that document's bracket table). Putting both engines in one **C++**
//! process (tf2 natively, `tf_tree` through its C ABI) charges the residual to
//! *us*, so the ratio is a lower bound. `PHASE4.md` §7 gate 1 and `tf2.md` state
//! the bracket; `just abi-split` ([`tf_tree_bench::backing`]) splits the C++ cost:
//! the mapping, read-only attach and linkage are near zero and the remainder is the
//! C ABI's per-call `Guard` (`tft_plan_at_many` amortises it per batch).
//!
//! **Both arms must be in one process**: interleaving within a round is what makes
//! the ratio resolvable on a host whose absolute latencies are unusable.
//!
//! # Why a separate process from the C++ one
//!
//! `tft_tree_open` **attaches**, it cannot create (D18: a C ABI consumer joins
//! read-only and the MMU protects the tree). `Tree::build_shared` segments are
//! "not discoverable by name", so a non-child cannot find one. The rendezvous
//! (`Open` with [`CreatePolicy::IfAbsent`]) is the discoverable path and this
//! binary is the owner that serves it.
//!
//! # Usage
//!
//! ```text
//! native_arena --name tf2_native --stream target/native/fixture.tfstream
//! ```
//!
//! Publishes the history, writes the stream, prints `ready`, then blocks until
//! stdin closes so the arena is served while the C++ side runs.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::io::{BufWriter, Read, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};

use tf_tree::{AttachMode, Capacity, CreatePolicy, EdgeCfg, InterpPolicy, Iso3, Open, TreeBuilder};
use tf_tree_bench::fixture::{self, EdgeDefKind, EDGES};

/// The fixture's topology, as `mp_bench::build_shared` declares it.
fn layout() -> TreeBuilder {
    let mut b = TreeBuilder::new().default_interp(InterpPolicy::LerpSlerp);
    for e in EDGES {
        b = match e.kind {
            EdgeDefKind::Static { xi } => {
                b.static_edge(e.parent, e.child, &tf_tree_math::exp_se3(xi))
            }
            EdgeDefKind::Dynamic { rate_hz } => b.dynamic_edge(
                e.parent,
                e.child,
                EdgeCfg::new(Capacity::history(rate_hz, fixture::HISTORY_SECS)),
            ),
        };
    }
    b
}

/// Write the `.tfstream` `docker/tf2/native_scaling.cpp` parses:
///
/// ```text
/// S <parent> <child> qw qx qy qz tx ty tz
/// D <parent> <child> <stamp_ns> qw qx qy qz tx ty tz
/// ```
///
/// **This loop must stay identical to `fixture::spin_up`'s and
/// `Tf2Fixture::load`'s**, including the `dyn_seed` increment, or the engines are
/// compared on different data.
fn dump_stream(path: &PathBuf) -> Result<usize> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let f = std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut w = BufWriter::new(f);
    writeln!(
        w,
        "# tf_tree §11.1 fixture, dumped by native_arena. {} s of history.",
        fixture::HISTORY_SECS
    )?;

    let mut wrote = 0usize;
    let mut dyn_seed = 0.0f64;
    for e in EDGES {
        match e.kind {
            EdgeDefKind::Static { xi } => {
                let p = tf_tree_math::exp_se3(xi);
                writeln!(w, "S {} {} {}", e.parent, e.child, pose(&p))?;
                wrote += 1;
            }
            EdgeDefKind::Dynamic { rate_hz } => {
                let period_ns = (1e9 / rate_hz) as i64;
                let count = (fixture::HISTORY_SECS * rate_hz) as i64;
                for k in 0..count {
                    let stamp = k * period_ns;
                    let p = fixture::dynamic_pose(dyn_seed, stamp);
                    writeln!(w, "D {} {} {} {}", e.parent, e.child, stamp, pose(&p))?;
                    wrote += 1;
                }
                dyn_seed += 1.0;
            }
        }
    }
    w.flush()?;
    Ok(wrote)
}

/// `qw qx qy qz tx ty tz` at full `f64` precision (`{:.17e}`): the C++ side
/// checks agreement to 1e-9, which a shortened decimal would spend.
fn pose(p: &Iso3) -> String {
    format!(
        "{:.17e} {:.17e} {:.17e} {:.17e} {:.17e} {:.17e} {:.17e}",
        p.q.w, p.q.x, p.q.y, p.q.z, p.t.x, p.t.y, p.t.z
    )
}

fn main() -> Result<()> {
    let mut name = "tf2_native".to_owned();
    let mut stream = PathBuf::from("target/native/fixture.tfstream");
    let mut dump_only = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--name" => name = args.next().context("--name wants a value")?,
            "--stream" => stream = PathBuf::from(args.next().context("--stream wants a value")?),
            // Write the `.tfstream` and exit, serving no arena: `native_footprint.cpp`
            // needs the data only, and `dump_stream` regenerates poses from
            // `fixture::dynamic_pose` without reading the tree.
            "--dump-only" => dump_only = true,
            other => anyhow::bail!("unknown argument `{other}`"),
        }
    }

    if dump_only {
        let wrote = dump_stream(&stream)?;
        println!("dumped {} {}", stream.display(), wrote);
        return Ok(());
    }

    // `require_create(true)`: an already-served arena would be somebody else's data.
    let tree = Open::new()
        .name(&name)?
        .mode(AttachMode::ReadWrite)
        .create(CreatePolicy::IfAbsent)
        .require_create(true)
        .layout_if_creating(layout())
        .open()
        .with_context(|| format!("serving an arena named `{name}`"))?;

    let (writers, samples) = fixture::spin_up(&tree)?;
    let wrote = dump_stream(&stream)?;

    println!("ready {name} {} {}", stream.display(), wrote);
    std::io::stdout().flush()?;

    // Hold the arena (and the writers' claims) until stdin closes; dropping `tree`
    // unmaps the segment and the C++ side would fault.
    let mut sink = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut sink);
    drop(writers);
    drop(samples);
    Ok(())
}
