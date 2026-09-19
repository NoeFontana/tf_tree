//! `doctor`'s recording sources — `docs/PHASE5.md` §6's third `Source`.
//!
//! Wiring, not a reader (§4.1 NORMATIVE): `--from-bag` runs [`tf_tree_ingest::run`]
//! and `--from-file` runs `Tree::open_frozen`; both yield an ordinary [`Tree`]
//! checked through the same [`crate::doctor::Snapshot`] as the fixture.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};

use tf_tree::unstable::EdgeKind;
use tf_tree::Tree;
use tf_tree_bench::fixture::PushSample;
use tf_tree_bridge::names::NameNormalizer;
use tf_tree_ingest::source::read_tf;
use tf_tree_ingest::{IngestOptions, Ingested};

use crate::doctor::{Observations, Snapshot};

/// Ingest `bag` in-process; returns the tree and the §3.2 report.
///
/// # Errors
///
/// Any [`tf_tree_ingest::IngestError`], rendered through `ingest_err`.
pub fn open_bag(bag: &Path, opts: &IngestOptions) -> Result<Ingested> {
    let mut frames = tf_tree_ingest::Frames::default();
    tf_tree_ingest::run(bag, opts, &mut frames).map_err(|e| crate::ingest_err(e, &frames))
}

/// Replay a recording's transforms in log order as the push stream `TFT018`
/// cannot get from an arena.
///
/// Skips statics, `stamp == 0` and unresolvable pairs (§3.2); `writer_pid` and
/// `arrival_delay_ns` are `0`. Exceeding `max_bytes` is an error, not a truncation:
/// a prefix would let `TFT018`/`TFT019` pass about the part they saw.
///
/// # Errors
///
/// Any [`tf_tree_ingest::IngestError`], or the arrival stream exceeding `max_bytes`.
pub fn arrival_observations(
    bag: &Path,
    opts: &IngestOptions,
    tree: &Tree,
    snap: &Snapshot,
    max_bytes: u64,
) -> Result<Observations> {
    let by_pair: BTreeMap<(u32, u32), u32> = snap
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Dynamic)
        .map(|e| ((e.parent, e.child), e.id))
        .collect();

    let mut normalizer = match &opts.tf_prefix {
        Some(p) => NameNormalizer::with_prefix(p),
        None => NameNormalizer::new(),
    };
    let mut resolved: BTreeMap<(String, String), Option<u32>> = BTreeMap::new();
    let mut obs = Observations::new();
    let cap = (max_bytes / core::mem::size_of::<PushSample>() as u64).max(1);
    let mut overflowed = false;

    read_tf(bag, &opts.roles, opts.read_policy(), |rec| {
        if rec.is_static || rec.stamp_ns == 0 {
            return Ok(());
        }
        let key = (rec.parent.to_owned(), rec.child.to_owned());
        let edge = match resolved.get(&key) {
            Some(&e) => e,
            None => {
                let e = resolve(&mut normalizer, tree, &by_pair, rec.parent, rec.child);
                resolved.insert(key, e);
                e
            }
        };
        if let Some(edge) = edge {
            // Flagged, not returned: `IngestError` has no variant for a `doctor` limit.
            if obs.events.len() as u64 >= cap {
                overflowed = true;
                return Ok(());
            }
            obs.record(PushSample {
                edge,
                writer_pid: 0,
                stamp_ns: rec.stamp_ns,
                arrival_delay_ns: 0,
            });
        }
        Ok(())
    })
    .map_err(|e| crate::ingest_err(e, &tf_tree_ingest::Frames::default()))
    .with_context(|| format!("re-reading {} for its arrival order", bag.display()))?;

    anyhow::ensure!(
        !overflowed,
        "{} holds more dynamic transforms than --max-memory allows doctor to replay in arrival \
         order: the cap is {cap} sample(s) at {} bytes each ({} MiB).\n\x20 Raise --max-memory. \
         Reporting on the first {cap} would let TFT018 and TFT019 pass about a prefix of your \
         recording, which is the all-clear --from-bag exists to remove.",
        bag.display(),
        core::mem::size_of::<PushSample>(),
        max_bytes / (1024 * 1024),
    );
    Ok(obs)
}

/// One raw `(parent, child)` pair to an arena edge id, or `None` if this arena
/// has no such dynamic edge.
fn resolve(
    normalizer: &mut NameNormalizer,
    tree: &Tree,
    by_pair: &BTreeMap<(u32, u32), u32>,
    parent: &str,
    child: &str,
) -> Option<u32> {
    let p = normalizer.normalize(parent).ok()?;
    let c = normalizer.normalize(child).ok()?;
    let pid = tree.frame(&p.name).ok()?;
    let cid = tree.frame(&c.name).ok()?;
    by_pair.get(&(pid.get(), cid.get())).copied()
}
