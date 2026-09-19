//! `tf_tree topology` — obtain, validate and explain a bridge topology file.
//! `docs/PHASE4.md` §5.8: `--discover <source>` prints the bridge config a
//! recorded `.tfstream` implies and what a config cannot express; `--config
//! <file.toml>` parses and builds it as a pre-flight for bridge startup.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};

use tf_tree::InterpPolicy;
use tf_tree_bench::replay::TfStream;
use tf_tree_bridge::{Discovery, EdgeShape, Sample, Topic, TopologyConfig};

/// Read a `.tfstream`, collect its topology, and return the config it implies.
/// Receipt times are [`tf_tree_bridge::SteadyNanos::UNKNOWN`]: a `.tfstream` has
/// no log time, and `stamp_nanos` would zero every offset (§5.5).
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
    if let Some(p) = tf_prefix {
        d = d.with_prefix(p);
    }
    if let Some(i) = interp {
        d = d.with_interp(i);
    }
    // Statics first, matching the wire (`/tf_static` is latched).
    for (parent, child, iso) in &stream.static_edges {
        let mut sample = Sample::identity(parent, child, 0);
        sample.pose = pose_of(iso);
        d.observe(Topic::TfStatic, &sample);
    }
    for s in &stream.samples {
        let (parent, child) = stream
            .dynamic_edges
            .get(s.edge)
            .ok_or_else(|| anyhow!("sample references edge {} which does not exist", s.edge))?;
        let mut sample = Sample::identity(parent, child, s.stamp_ns);
        sample.pose = pose_of(&s.pose);
        d.observe(Topic::Tf, &sample);
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

    TopologyConfig::parse(&text)
        .map_err(|e| anyhow!("the discovered config does not reparse: {e}"))?;

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
    let config = match TopologyConfig::parse(&text) {
        Ok(c) => c,
        Err(e) => bail!("{}: {e}", path.display()),
    };
    // §5.5's NORMATIVE startup refusal, run here so it fails before the robot.
    if let Some(d) = domain {
        if let Err(e) = config.check_domain(d) {
            bail!("{}: {e}", path.display());
        }
    }
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
