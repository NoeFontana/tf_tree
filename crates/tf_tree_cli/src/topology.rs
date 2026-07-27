//! `tf_tree topology` — obtain, validate and explain a bridge topology file.
//!
//! `docs/PHASE4.md` §5.8's amendment: the bridge takes its topology from a
//! config file up front, because the engine has no runtime edge declaration,
//! *"and a `--discover` mode that subscribes, collects and prints a config file
//! is how an operator obtains that file"*.
//!
//! Two modes, and they are the two halves of that sentence:
//!
//! * `--discover <source>` — read a recorded `/tf` stream, print the config it
//!   implies, and report what the stream contains that a config **cannot**
//!   (a child with two parents; an edge on both topics).
//! * `--config <file.toml>` — parse it, build the arena it describes, and print
//!   what the bridge will accept. This is the pre-flight: a file that fails
//!   here fails at bridge startup, and failing on a laptop is cheaper.
//!
//! # Why the source is a `.tfstream` and not a subscription
//!
//! There is no ROS 2 in this environment (`docs/PHASE4.md` §0.0 records it),
//! and `--discover`'s value does not depend on one: the collector in
//! [`tf_tree_bridge::Discovery`] takes `(topic, sample)` pairs and does not
//! know where they came from. The `rclcpp` half feeds it from a subscription;
//! this feeds it from a recording, and both print the same file. That is also
//! what makes §6.3 and §6.4 self-contained — the corpus in
//! `testdata/tfstream/` is a real robot's `/tf`, not a fixture somebody
//! invented (see its `ATTRIBUTION.md`).

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};

use tf_tree::InterpPolicy;
use tf_tree_bench::replay::TfStream;
use tf_tree_bridge::{Discovery, EdgeShape, Sample, Topic, TopologyConfig};

/// Read a `.tfstream`, collect its topology, and return the config it implies.
///
/// # Errors
///
/// If the stream cannot be read or parsed.
pub fn discover_from_tfstream(
    path: &Path,
    history_secs: f64,
    tf_prefix: Option<&str>,
    interp: Option<InterpPolicy>,
) -> Result<Discovery> {
    let stream = TfStream::load(path)?;
    let mut d = Discovery::new(history_secs);
    // §5.6's `tf_prefix` belongs here and not only in the bridge: a discovered
    // config keyed on `base_link` while the bridge that will read it keys on
    // `robot1/base_link` declares every edge and matches none.
    if let Some(p) = tf_prefix {
        d = d.with_prefix(p);
    }
    if let Some(i) = interp {
        d = d.with_interp(i);
    }
    // Statics first, matching the wire: `/tf_static` is transient-local, so a
    // late-joining bridge receives the latched set before the first `/tf`
    // message it sees. Feeding them in the other order would let a static edge
    // be discovered as dynamic and mask §5.7's kind clash — the collector
    // resolves a clash to whichever topic it saw first, so the order it is fed
    // is part of what it reports.
    for (parent, child, iso) in &stream.static_edges {
        d.observe(
            Topic::TfStatic,
            &Sample {
                frame_id: parent.clone(),
                child_frame_id: child.clone(),
                stamp_nanos: 0,
                pose: pose_of(iso),
            },
        );
    }
    for s in &stream.samples {
        let (parent, child) = stream
            .dynamic_edges
            .get(s.edge)
            .ok_or_else(|| anyhow!("sample references edge {} which does not exist", s.edge))?;
        d.observe(
            Topic::Tf,
            &Sample {
                frame_id: parent.clone(),
                child_frame_id: child.clone(),
                stamp_nanos: s.stamp_ns,
                pose: pose_of(&s.pose),
            },
        );
    }
    Ok(d)
}

fn pose_of(iso: &tf_tree::Iso3) -> [f64; 7] {
    [
        iso.q.w, iso.q.x, iso.q.y, iso.q.z, iso.t.x, iso.t.y, iso.t.z,
    ]
}

/// `tf_tree topology --discover <file.tfstream>`.
///
/// # Errors
///
/// If the stream cannot be read, or the config it produces cannot be written.
pub fn cmd_discover(
    source: &Path,
    out: Option<&Path>,
    history_secs: f64,
    tf_prefix: Option<&str>,
    interp: Option<InterpPolicy>,
) -> Result<()> {
    let d = discover_from_tfstream(source, history_secs, tf_prefix, interp)?;
    let config = d.to_config();
    let text = config.to_toml();

    // Re-read what is about to be written. `Discovery` and the parser now share
    // `frame_name_ok`, so this should never fire — which is exactly why it is
    // here: the "a discovered config reparses" contract was asserted by three
    // tests and enforced at no boundary, and the boundary is the one place an
    // operator would otherwise meet the failure, on the robot.
    TopologyConfig::parse(&text)
        .map_err(|e| anyhow!("the discovered config does not reparse: {e}"))?;

    // The findings go to **stderr**, always, so `--discover > topology.toml`
    // produces a usable file and still tells the operator what it could not
    // represent. Putting them in the file as comments would be worse: a config
    // is edited and re-emitted, and the warning would survive the fix.
    for (child, rejected) in d.multi_parent() {
        eprintln!(
            "warning: frame {child:?} has more than one parent in this recording; \
             {rejected:?} was dropped. tf_tree gives a frame exactly one parent \
             (docs/PROJECT.md §5 D4), so this is a defect in the observed system."
        );
    }
    if d.dropped_multi_parent() > 0 {
        eprintln!(
            "warning: {} transforms discarded for a second parent",
            d.dropped_multi_parent()
        );
    }
    for (parent, child) in d.kind_clashes() {
        eprintln!(
            "warning: edge {parent:?} -> {child:?} appears on both /tf and /tf_static; \
             the edge kind cannot change (docs/PHASE4.md §5.7). Declared as first seen."
        );
    }
    if d.dropped_bad_name() > 0 {
        eprintln!(
            "warning: {} transforms discarded for an unusable frame name (§5.6)",
            d.dropped_bad_name()
        );
    }
    // The per-edge sample count is what tells an operator whether a ring size
    // is worth trusting: an edge sized from four samples got a rate measured
    // over three intervals, and the number in the file looks exactly as
    // confident as one measured over ten thousand.
    for (parent, child, n) in d.sample_counts() {
        eprintln!("  {parent} -> {child}: {n} samples");
    }
    eprintln!(
        "discovered {} edges from {}",
        config.edges.len(),
        source.display()
    );

    match out {
        Some(p) => {
            std::fs::write(p, &text).with_context(|| format!("writing {}", p.display()))?;
            eprintln!("wrote {}", p.display());
        }
        None => print!("{text}"),
    }
    Ok(())
}

/// `tf_tree topology --config <file.toml>` — parse, build, and describe.
///
/// # Errors
///
/// If the file cannot be read, does not parse, or describes a topology the
/// engine refuses (two edges on one child, a cycle, an arena that does not fit).
pub fn cmd_check(path: &Path, domain: Option<u8>) -> Result<()> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    // `ConfigError` borrows from `text`, so it is rendered here rather than
    // returned — that is the trade `Copy`, allocation-free errors make, and it
    // is the right one for a type the bridge's hot path also holds.
    let config = match TopologyConfig::parse(&text) {
        Ok(c) => c,
        Err(e) => bail!("{}: {e}", path.display()),
    };
    // §5.5's NORMATIVE startup refusal, run here rather than only at bridge
    // startup — the whole point of this command is that a file which fails on
    // the robot should have failed on a laptop first.
    if let Some(d) = domain {
        if let Err(e) = config.check_domain(d) {
            bail!("{}: {e}", path.display());
        }
    }
    // Ask the config before asking the builder. The builder finds the same
    // cycle and names it `FrameId(1)` — a number that indexes an arena which
    // was never built, and which an operator holding a text file cannot map
    // back to anything.
    if let Some(child) = config.cycle_child() {
        bail!(
            "{}: the declared topology has a cycle through frame {child:?} — \
             following its parent links returns to it",
            path.display()
        );
    }
    let tree = config.builder().build().map_err(|e| {
        anyhow!(
            "{}: the declared topology does not build: {e}",
            path.display()
        )
    })?;

    println!("{}: {} edges", path.display(), config.edges.len());
    println!("  arena: {} bytes", tree.arena_size_bytes());
    for e in &config.edges {
        match e.shape {
            EdgeShape::Static { pose } => println!(
                "  static  {} -> {}  t=[{:.4}, {:.4}, {:.4}]",
                e.parent, e.child, pose[4], pose[5], pose[6]
            ),
            EdgeShape::Dynamic { ring } => println!(
                "  dynamic {} -> {}  {} slots",
                e.parent,
                e.child,
                ring.capacity().get()
            ),
        }
    }
    for f in &config.frames {
        println!("  frame   {f} (no edge)");
    }
    Ok(())
}
