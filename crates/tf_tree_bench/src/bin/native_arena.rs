//! Serve the §11.1 fixture from a **named, rendezvous-discoverable** arena, and
//! dump the identical sample stream, so a C++ process can measure both engines
//! with no Rust binding on either side.
//!
//! # Why this binary has to exist
//!
//! `docs/benchmarks/tf2.md` prices four measurement biases, and the one that
//! cannot be removed from an in-process Rust harness is the third: the residual
//! FFI boundary between `tf_tree_tf2_sys` and `tf2::BufferCore` — cross-TU, no
//! inlining, one extra copy, **~21 ns / 8% at depth 3**. It is charged to tf2,
//! so every ratio measured that way *flatters* `tf_tree`.
//!
//! The fix is to put both engines in one **C++** process: tf2 natively, and
//! `tf_tree` through its C ABI. That reverses the direction of the residual
//! cost — `tft_plan_at` is a measured **1.020×** native Rust (`PHASE4.md` §7
//! gate 1), so the 2% is now charged to *us* and the resulting ratio is a
//! conservative lower bound rather than a flattering upper one.
//!
//! **Both arms must still be in one process**, because the pairing is what makes
//! the number resolvable at all: interleaving within a round is why the ratio
//! reports a ~3% band on a host whose absolute latencies are unusable. Two
//! separate binaries cannot be interleaved, and comparing their medians puts the
//! host's ~4% run-to-run spread straight into the answer.
//!
//! # Why it is a separate process from the C++ one
//!
//! `tft_tree_open` **attaches**; it cannot create. That is D18 — a consumer
//! linked against the C ABI joins read-only and the MMU is what stops it
//! corrupting a robot's tree — and it is not something to work around for a
//! benchmark. `Tree::build_shared` does not help either: its segments are "not
//! discoverable by name — the fd is the capability", so a process that is not a
//! child cannot find one. The rendezvous (`Open` with
//! [`CreatePolicy::IfAbsent`]) is the discoverable path, and this binary is the
//! owner that serves it.
//!
//! # Usage
//!
//! ```text
//! native_arena --name tf2_native --stream target/native/fixture.tfstream
//! ```
//!
//! Publishes the history, writes the stream, prints `ready`, and then blocks
//! until stdin closes — so the C++ side runs while the arena is still served and
//! the owner goes away when the harness does.

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

/// Write the `.tfstream` `docker/tf2/native_scaling.cpp` already parses:
///
/// ```text
/// S <parent> <child> qw qx qy qz tx ty tz
/// D <parent> <child> <stamp_ns> qw qx qy qz tx ty tz
/// ```
///
/// **This loop must stay identical to `fixture::spin_up`'s and to
/// `Tf2Fixture::load`'s**, including the `dyn_seed` increment, or the two
/// engines are compared on different data and every observed difference is
/// meaningless. It is the same three lines in all three places for that reason.
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

/// `qw qx qy qz tx ty tz` at full `f64` precision.
///
/// `{:.17e}` and not `{}`: the two engines are checked to agree to 1e-9 on the
/// C++ side, and a stream that round-trips through a shortened decimal would
/// spend that budget on the serializer rather than on the engines.
fn pose(p: &Iso3) -> String {
    format!(
        "{:.17e} {:.17e} {:.17e} {:.17e} {:.17e} {:.17e} {:.17e}",
        p.q.w, p.q.x, p.q.y, p.q.z, p.t.x, p.t.y, p.t.z
    )
}

fn main() -> Result<()> {
    let mut name = "tf2_native".to_owned();
    let mut stream = PathBuf::from("target/native/fixture.tfstream");
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--name" => name = args.next().context("--name wants a value")?,
            "--stream" => stream = PathBuf::from(args.next().context("--stream wants a value")?),
            other => anyhow::bail!("unknown argument `{other}`"),
        }
    }

    // `require_create(true)`: if an arena of this name is already served, the
    // C++ side would measure somebody else's data and never know. Fail instead.
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

    // Hold the arena open — and the writers with it, so the claims stay live —
    // until the harness closes stdin. Dropping `tree` unmaps the segment and the
    // C++ side's next lookup would fault, so this is the lifetime that matters.
    let mut sink = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut sink);
    drop(writers);
    drop(samples);
    Ok(())
}
