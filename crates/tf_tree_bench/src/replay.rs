//! Replay of a **real** recorded `/tf` stream through the engine.
//!
//! The synthetic [`crate::fixture`] is a good correctness oracle but it is not a
//! real load: its rates are exact, its motion is a smooth analytic function, and
//! nothing ever arrives late or out of order. A recorded bag has none of those
//! conveniences — irregular periods, duplicated stamps, frames that appear
//! partway through, and quaternions that are only approximately normalised.
//!
//! # The `.tfstream` format
//!
//! Bags are converted **once**, offline, by `scripts/bag_to_tfstream.py` (which
//! runs in the ROS container) into a line-oriented ASCII format:
//!
//! ```text
//! S <parent> <child> <qw> <qx> <qy> <qz> <tx> <ty> <tz>
//! D <parent> <child> <stamp_ns> <qw> <qx> <qy> <qz> <tx> <ty> <tz>
//! ```
//!
//! That indirection is deliberate: this module — and therefore the whole replay
//! harness — has **no ROS dependency**, so it builds and runs on any host, and
//! any bag from any distro reduces to one format we control. Quaternions are
//! stored w-first (matching [`Iso3`]), so the ROS w-last transposition happens
//! once, in the converter.
//!
//! Stamps are rebased so the earliest sample is 0. Real bags carry wall-clock
//! epochs around 1.8e18 ns; rebasing keeps dumps readable and keeps every stamp
//! comfortably inside the unsigned time ROS requires.

use std::collections::BTreeMap;

use anyhow::{anyhow, bail, Context, Result};

use tf_tree::{Capacity, EdgeCfg, InterpPolicy, Iso3, Stamp, Tree, TreeBuilder};

/// One recorded dynamic sample.
#[derive(Clone, Debug)]
pub struct Sample {
    /// Index into [`TfStream::dynamic_edges`].
    pub edge: usize,
    /// Rebased stamp, nanoseconds from the start of the recording.
    pub stamp_ns: i64,
    /// `T_parent_child`.
    pub pose: Iso3,
}

/// A parsed recording: its topology and its full sample stream.
#[derive(Clone, Debug, Default)]
pub struct TfStream {
    /// Static edges as `(parent, child, T_parent_child)`.
    pub static_edges: Vec<(String, String, Iso3)>,
    /// Dynamic edges as `(parent, child)`, in first-seen order.
    pub dynamic_edges: Vec<(String, String)>,
    /// Every dynamic sample, sorted by stamp.
    pub samples: Vec<Sample>,
    /// Header comments from the file, retained so a report can cite provenance.
    pub provenance: Vec<String>,
}

impl TfStream {
    /// Parse a `.tfstream`.
    ///
    /// # Errors
    ///
    /// On a malformed line, naming the line number — a truncated or
    /// hand-edited stream should fail loudly, not silently replay less data.
    pub fn parse(text: &str) -> Result<TfStream> {
        let mut out = TfStream::default();
        let mut dyn_index: BTreeMap<(String, String), usize> = BTreeMap::new();

        for (lineno, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix('#') {
                out.provenance.push(rest.trim().to_owned());
                continue;
            }
            let f: Vec<&str> = line.split_whitespace().collect();
            let n = lineno + 1;
            match f.first().copied() {
                Some("S") => {
                    if f.len() != 10 {
                        bail!("line {n}: `S` needs 10 fields, got {}", f.len());
                    }
                    let pose = parse_pose(&f[3..10], n)?;
                    out.static_edges
                        .push((f[1].to_owned(), f[2].to_owned(), pose));
                }
                Some("D") => {
                    if f.len() != 11 {
                        bail!("line {n}: `D` needs 11 fields, got {}", f.len());
                    }
                    let stamp_ns: i64 = f[3]
                        .parse()
                        .with_context(|| format!("line {n}: bad stamp {:?}", f[3]))?;
                    let pose = parse_pose(&f[4..11], n)?;
                    let key = (f[1].to_owned(), f[2].to_owned());
                    let edge = *dyn_index.entry(key.clone()).or_insert_with(|| {
                        out.dynamic_edges.push(key);
                        out.dynamic_edges.len() - 1
                    });
                    out.samples.push(Sample {
                        edge,
                        stamp_ns,
                        pose,
                    });
                }
                _ => bail!("line {n}: expected `S`, `D` or `#`, got {line:?}"),
            }
        }

        if out.samples.is_empty() && out.static_edges.is_empty() {
            bail!("stream contains no transforms");
        }
        // The converter sorts, but a hand-assembled stream might not; the engine
        // rejects a regressing stamp, so normalise rather than fail late.
        out.samples.sort_by_key(|s| s.stamp_ns);
        Ok(out)
    }

    /// Read and parse a `.tfstream` file.
    ///
    /// # Errors
    ///
    /// If the file cannot be read, or its contents do not parse.
    pub fn load(path: &std::path::Path) -> Result<TfStream> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        TfStream::parse(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// Every frame name in the recording, sorted for determinism.
    #[must_use]
    pub fn frame_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = Vec::new();
        for (p, c, _) in &self.static_edges {
            names.push(p);
            names.push(c);
        }
        for (p, c) in &self.dynamic_edges {
            names.push(p);
            names.push(c);
        }
        names.sort_unstable();
        names.dedup();
        names
    }

    /// How many samples each dynamic edge carries.
    #[must_use]
    pub fn samples_per_edge(&self) -> Vec<usize> {
        let mut counts = std::vec![0usize; self.dynamic_edges.len()];
        for s in &self.samples {
            counts[s.edge] += 1;
        }
        counts
    }

    /// The stamp window in which **every** dynamic edge has data.
    ///
    /// Queries outside it are legitimately extrapolation for at least one edge,
    /// so a differential must stay inside it or it is comparing error paths
    /// rather than transforms. `None` if some edge has no samples at all.
    #[must_use]
    pub fn common_window(&self) -> Option<(i64, i64)> {
        if self.dynamic_edges.is_empty() {
            return None;
        }
        let mut first = std::vec![i64::MAX; self.dynamic_edges.len()];
        let mut last = std::vec![i64::MIN; self.dynamic_edges.len()];
        for s in &self.samples {
            first[s.edge] = first[s.edge].min(s.stamp_ns);
            last[s.edge] = last[s.edge].max(s.stamp_ns);
        }
        if first.contains(&i64::MAX) {
            return None; // an edge with no samples
        }
        let lo = *first.iter().max()?;
        let hi = *last.iter().min()?;
        (lo < hi).then_some((lo, hi))
    }

    /// Build a [`Tree`] with this recording's topology and replay its history
    /// into it.
    ///
    /// Ring capacities are sized to each edge's actual sample count (plus the
    /// one slot a ring cannot hand back), which is what the recording demands
    /// rather than a guess: every sample stays readable, so a query anywhere in
    /// [`Self::common_window`] is answered rather than declined.
    ///
    /// # Errors
    ///
    /// If the topology cannot be built (a frame with two parents, a cycle), or a
    /// push is rejected.
    pub fn build_tree(&self, interp: InterpPolicy) -> Result<Tree> {
        let mut b = TreeBuilder::new().default_interp(interp);
        for (p, c, pose) in &self.static_edges {
            b = b.static_edge(p, c, pose);
        }
        let counts = self.samples_per_edge();
        for (i, (p, c)) in self.dynamic_edges.iter().enumerate() {
            // `slots` rounds up to a power of two, but a ring of `cap` slots
            // *retains* only `cap - 1` samples (`SampleRing::retained`): the slot
            // the writer is about to overwrite is not readable. Sizing for
            // `count` alone therefore loses the oldest sample whenever `count` is
            // an exact power of two — and the oldest sample is precisely the one
            // `common_window`'s lower bound points at, so every query at `lo`
            // would be declined. Ask for one more than the recording holds.
            let want = u32::try_from(counts[i])
                .unwrap_or(u32::MAX)
                .saturating_add(1);
            let cap = Capacity::slots(want);
            b = b.dynamic_edge(p, c, EdgeCfg::new(cap));
        }
        let tree = b.build().map_err(|e| anyhow!("build replay tree: {e}"))?;

        // Claim every dynamic edge, replay in stamp order, then release.
        let mut writers = Vec::with_capacity(self.dynamic_edges.len());
        for (p, c) in &self.dynamic_edges {
            let parent = tree.frame(p).map_err(|e| anyhow!("frame {p}: {e:?}"))?;
            let child = tree.frame(c).map_err(|e| anyhow!("frame {c}: {e:?}"))?;
            writers.push(
                tree.claim(child, parent)
                    .map_err(|e| anyhow!("claim {p}->{c}: {e}"))?,
            );
        }
        for s in &self.samples {
            writers[s.edge]
                .push(s.stamp_ns, &s.pose)
                .map_err(|e| anyhow!("push on {:?}: {e:?}", self.dynamic_edges[s.edge]))?;
        }
        drop(writers);
        Ok(tree)
    }
}

/// Parse seven whitespace-separated `f64`s as `{qw,qx,qy,qz,tx,ty,tz}`.
fn parse_pose(f: &[&str], lineno: usize) -> Result<Iso3> {
    let mut v = [0.0f64; 7];
    for (i, s) in f.iter().enumerate() {
        v[i] = s
            .parse()
            .with_context(|| format!("line {lineno}: bad float {s:?}"))?;
    }
    let bits: [u64; 7] = core::array::from_fn(|i| v[i].to_bits());
    Ok(Iso3::from_bits(&bits))
}

/// A deterministic query set over a recording: random frame pairs at random
/// stamps inside the common window.
///
/// Shared by the replay differential and the replay benchmarks so both measure
/// the same thing.
pub struct QuerySet {
    /// `(target, source, stamp_ns)` triples.
    pub queries: Vec<(String, String, i64)>,
}

impl QuerySet {
    /// Draw `n` queries from `stream`, seeded for reproducibility.
    ///
    /// # Errors
    ///
    /// If the recording has no window in which every dynamic edge has data.
    pub fn draw(stream: &TfStream, n: usize, seed: u64) -> Result<QuerySet> {
        let (lo, hi) = stream
            .common_window()
            .ok_or_else(|| anyhow!("recording has no window covered by every dynamic edge"))?;
        let names = stream.frame_names();
        if names.len() < 2 {
            bail!("recording has fewer than two frames");
        }
        let mut rng = Rng(seed ^ 0x9E37_79B9_7F4A_7C15);
        let mut queries = Vec::with_capacity(n);
        while queries.len() < n {
            let a = rng.below(names.len());
            let b = rng.below(names.len());
            if a == b {
                continue;
            }
            let span = (hi - lo) as u64;
            let stamp = lo + (rng.next_u64() % span.max(1)) as i64;
            queries.push((names[a].to_owned(), names[b].to_owned(), stamp));
        }
        Ok(QuerySet { queries })
    }
}

/// Synthesise a [`TfStream`] shaped like a real robot description, at a chosen
/// scale.
///
/// Real URDFs produce a characteristic frame tree: one kinematic spine of
/// dynamic joints, with mostly-static sensor and link subtrees hanging off it.
/// `chain_depth` sets the spine length (the thing lookup cost is sensitive to)
/// and `branches_per_link` the fan-out (the thing tree *size* is sensitive to),
/// so the two can be varied independently — which is exactly what a scaling
/// comparison needs and what a single fixed robot cannot give.
///
/// Only the spine is dynamic, matching how real robots publish: a handful of
/// moving joints and a large static skeleton from `robot_state_publisher`.
#[must_use]
pub fn synth_robot(
    chain_depth: usize,
    branches_per_link: usize,
    samples_per_edge: usize,
    rate_hz: f64,
) -> TfStream {
    let mut s = TfStream::default();
    s.provenance.push(format!(
        "synthetic robot: chain_depth={chain_depth} branches={branches_per_link} \
         samples={samples_per_edge} rate={rate_hz}Hz"
    ));

    // Dynamic spine: link_0 -> link_1 -> ... -> link_n.
    for d in 0..chain_depth {
        s.dynamic_edges
            .push((format!("link_{d}"), format!("link_{}", d + 1)));
    }
    // Static sensor/appendage subtrees hanging off every spine link.
    for d in 0..=chain_depth {
        for b in 0..branches_per_link {
            let pose = crate::fixture::dynamic_pose((d * 31 + b) as f64 * 0.017, 0);
            s.static_edges
                .push((format!("link_{d}"), format!("s_{d}_{b}"), pose));
        }
    }

    let period_ns = (1e9 / rate_hz) as i64;
    for k in 0..samples_per_edge {
        let stamp = k as i64 * period_ns;
        for edge in 0..s.dynamic_edges.len() {
            s.samples.push(Sample {
                edge,
                stamp_ns: stamp,
                pose: crate::fixture::dynamic_pose(edge as f64, stamp),
            });
        }
    }
    s.samples.sort_by_key(|x| x.stamp_ns);
    s
}

/// SplitMix64, so the harness needs no `rand` dependency and every run is
/// byte-reproducible from its seed.
struct Rng(u64);
impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

/// Evaluate a query set against a replayed [`Tree`], returning the poses it
/// resolved and how many it declined.
///
/// Declines are expected and legitimate: a recorded tree is often disconnected
/// (a frame published on `/tf` whose parent chain never reaches the queried
/// root), and a stamp can fall in a gap on one edge. The count is returned so a
/// caller can tell "engines agree" from "nothing was compared".
///
/// # Errors
///
/// If a frame name in the query set is unknown to the tree.
pub fn evaluate(tree: &Tree, queries: &QuerySet) -> Result<(Vec<Option<Iso3>>, usize)> {
    let guard = tree.guard();
    let mut out = Vec::with_capacity(queries.queries.len());
    let mut declined = 0usize;
    for (target, source, stamp_ns) in &queries.queries {
        let t = tree
            .frame(target)
            .map_err(|e| anyhow!("frame {target}: {e:?}"))?;
        let s = tree
            .frame(source)
            .map_err(|e| anyhow!("frame {source}: {e:?}"))?;
        let pose = match tree.plan(t, s) {
            Ok(plan) => {
                let stamp: Stamp = Stamp::from_nanos(*stamp_ns);
                plan.at(&guard, stamp).ok()
            }
            Err(_) => None,
        };
        if pose.is_none() {
            declined += 1;
        }
        out.push(pose);
    }
    Ok((out, declined))
}
