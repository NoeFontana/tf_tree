//! `doctor`'s recording sources — `docs/PHASE5.md` §6's third [`crate::Source`].
//!
//! # Why this module exists
//!
//! Before it, `doctor` had exactly two sources: the built-in reference fixture
//! and a live `--attach`. That is a diagnostic that can be pointed at a demo or
//! at a robot that already runs `tf_tree`, and at nothing a stranger owns. §2.2
//! calls the frozen arena the wedge precisely because *a user changes nothing
//! about their robot*; this is that argument applied to the catalogue.
//!
//! # It is wiring, not a reader
//!
//! §4.1 is NORMATIVE that there is no separate offline API — the same objects,
//! the same semantics — and both sources here obey it literally:
//!
//! * `--from-bag` calls [`tf_tree_ingest::run`], which is the same two passes
//!   `tf_tree ingest` and `tf_tree freeze --from-bag` run, and hands back the
//!   ordinary [`Tree`] they build.
//! * `--from-file` calls `Tree::open_frozen`, which §2.1 already requires to
//!   return an ordinary [`Tree`] read by the identical `Plan::at` code.
//!
//! Every check below then runs against the same [`crate::doctor::Snapshot`] it
//! runs against for the fixture. Nothing in `checks.rs` learned a new input
//! shape.
//!
//! # The one thing that is *not* free: the arrival stream
//!
//! `TFT018` needs the stamps **in the order they arrived**, including the ones
//! invariant 6 rejected. An arena cannot supply that from either source, and the
//! reason is structural rather than incidental: `SampleRing::push` refuses a
//! stamp older than the ring's last one, so a ring only ever holds *accepted*
//! pushes and [`crate::doctor::Observations::from_arena`] can only ever
//! reconstruct a non-decreasing sequence. That is true of a frozen `.tft` and of
//! a bag-built arena exactly as it is true of a live one — §3.1 additionally
//! *sorts* by stamp before pushing, so a bag-built arena is monotone twice over.
//!
//! The recording itself does carry it, because a recording is written in log
//! order. [`arrival_observations`] replays that order with the same reader and
//! the same filter `ingest::fill` uses, which is what makes `TFT018` and
//! `TFT019` reach a verdict on a file a stranger already has.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};

use tf_tree::{EdgeKind, Tree};
use tf_tree_bench::fixture::PushSample;
use tf_tree_bridge::names::NameNormalizer;
use tf_tree_ingest::source::read_tf;
use tf_tree_ingest::{IngestOptions, Ingested};

use crate::doctor::{Observations, Snapshot};

/// Ingest `bag` in-process and return the tree it built alongside the §3.2
/// report.
///
/// The report is returned rather than printed so the caller decides which
/// stream it lands on: `doctor --json` must keep stdout parseable.
///
/// # Errors
///
/// Any [`tf_tree_ingest::IngestError`], already rendered through
/// [`crate::ingest_err`] so a `.db3` gets the `ros2 bag convert` remedy and a
/// compressed chunk gets the flag rather than "corrupt file".
pub fn open_bag(bag: &Path, opts: &IngestOptions) -> Result<Ingested> {
    let mut frames = tf_tree_ingest::Frames::default();
    tf_tree_ingest::run(bag, opts, &mut frames).map_err(|e| crate::ingest_err(e, &frames))
}

/// Replay a recording's transforms **in its own log order** as an observed push
/// stream.
///
/// This is the evidence `TFT018` cannot get from an arena. Every transform the
/// recording holds is offered in the order the recorder wrote it, so a stamp
/// that went backwards is visible as the backwards arrival it was — which is
/// exactly what a publisher's consumers saw and what invariant 6 would have
/// rejected.
///
/// # It is a third pass over the file, and that is deliberate
///
/// `survey` and `fill` have already read it once each. Neither retains the
/// arrival order — `fill` sorts it away by construction (§3.1) and `survey`
/// reduces it to `Anomalies::out_of_order`, a single count for the whole
/// recording with no edge attached — so the order has to be re-read or carried,
/// and carrying it would mean a new field on a `tf_tree_ingest` type. A
/// diagnostic that a user runs once is the right place to pay a sequential
/// re-read rather than to widen a library's output.
///
/// # What it filters, and why it matches `fill`
///
/// The closure is the sibling of the one inside [`tf_tree_ingest::fill`]: skip
/// statics, skip `stamp == 0`, normalize both names, resolve the pair. Written
/// against the **arena's** frame ids via [`Tree::frame`] rather than against the
/// ingest's own interning table, so a name the arena stored differently — it is
/// a fixed-size field — resolves the way every other check resolves it.
///
/// * **Statics** are skipped because a static edge has no ring and no ordering
///   rule; `robot_state_publisher` stamps them zero, and §3.2 already says so.
/// * **`stamp == 0`** is skipped because §3.2 drops it in pass one. Keeping it
///   would make one misconfigured publisher's zero read as a jump back to the
///   epoch, and report as a clock step something `TFT006` already names better.
/// * **An unresolvable pair** is skipped: it is an edge the ingest dropped, so
///   the arena has no id to attribute it to.
///
/// `writer_pid` is `0` and `arrival_delay_ns` is `0` for every sample, because a
/// recording carries neither. A `/tf` message has no publisher identity in it —
/// which is why `TFT001` skips on this source — and the recorder's log time is a
/// different clock from the publisher's stamp, so differencing them would report
/// clock offset as publish latency.
///
/// # Errors
///
/// Any [`tf_tree_ingest::IngestError`] from re-reading the recording. It has
/// already been read twice by this point, so a failure here is a file that
/// changed underneath the process.
pub fn arrival_observations(
    bag: &Path,
    opts: &IngestOptions,
    tree: &Tree,
    snap: &Snapshot,
) -> Result<Observations> {
    // The arena's dynamic edges, keyed by the frame-id pair. Static edges are
    // absent on purpose: they are the one kind with no arrival order to judge.
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
    // Memoized on the **raw** pair, so `normalize` and two arena name lookups
    // run once per distinct edge rather than once per transform. `None` is
    // cached too: an unresolvable pair is unresolvable every time, and a
    // recording whose transforms are mostly one dropped edge would otherwise pay
    // the lookup on all of them.
    let mut resolved: BTreeMap<(String, String), Option<u32>> = BTreeMap::new();
    let mut obs = Observations::new();

    read_tf(bag, &opts.roles, opts.chunk_policy(), |rec| {
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
