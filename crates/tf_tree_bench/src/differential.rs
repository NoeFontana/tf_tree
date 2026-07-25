//! The differential harness (`docs/PHASE1.md` §10.5 *Differential against tf2*).
//!
//! Drives an identical tree and an identical sample stream through two
//! independent lookup pipelines and compares the results across many random
//! queries with [`InterpPolicy::LerpSlerp`] (tf2's policy).
//!
//! The reference is **pluggable**:
//!
//! * [`Reference::NaiveRust`] (the default, runs now): an independent Rust
//!   lookup — its own LCA walk, its own linear-scan bracket search, its own
//!   `LerpSlerp` composition over the *same* sample values the engine holds. It
//!   shares only the input data, never the engine's code path, so agreement to
//!   ~`1e-12` is a real cross-check of the seqlock ring + plan evaluation.
//! * [`Reference::Tf2`] (behind `--features tf2`): drives ROS 2's real
//!   `tf2::BufferCore` through the `tf_tree_tf2_sys` FFI bridge. This is the
//!   migration-credibility test — if it fails, code moving from tf2 to tf_tree
//!   would observe a different transform. It needs a ROS 2 install; run it with
//!   `just tf2-differential`, which containerises the toolchain.
//!
//! Both references implement one private `Oracle` trait and go through the
//! *same* query loop, so a disagreement is attributable to the engine under test
//! rather than to two subtly different harnesses.
//!
//! Queries an oracle declines are skipped and counted, never scored as
//! agreement — see [`DiffReport::compared`].

use std::collections::HashMap;

use anyhow::{anyhow, Result};

use tf_tree::{InterpPolicy, Iso3, Stamp, Tree};
use tf_tree_math::{log_so3, Interp, LerpSlerp};

use crate::fixture::{self, EdgeDefKind, EDGES};

/// Which reference oracle to compare tf_tree against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reference {
    /// The independent naive Rust lookup — always available.
    NaiveRust,
    /// ROS 2 `tf2::BufferCore` — requires `--features tf2` and a ROS 2 install.
    Tf2,
}

/// The query that produced the worst disagreement, so a failure is reproducible
/// rather than just a number.
#[derive(Clone, Copy, Debug)]
pub struct WorstQuery {
    /// Target frame name.
    pub target: &'static str,
    /// Source frame name.
    pub source: &'static str,
    /// The stamp queried, in nanoseconds.
    pub stamp_ns: i64,
}

/// The outcome of a differential run.
#[derive(Clone, Copy, Debug)]
pub struct DiffReport {
    /// Which reference was used.
    pub reference: Reference,
    /// How many random queries were drawn.
    pub queries: usize,
    /// How many were actually scored. Lower than `queries` because identical
    /// target/source pairs are skipped, and because an oracle may decline a
    /// query (tf2 does, past its cache horizon). A run with a low `compared`
    /// proved little, so this is reported rather than hidden.
    pub compared: usize,
    /// The worst observed disagreement (max of rotation-angle error in radians
    /// and translation error in metres) across all scored queries.
    pub max_error: f64,
    /// The agreement tolerance the run was checked against.
    pub tolerance: f64,
    /// Which query was worst, for reproduction.
    pub worst_query: Option<WorstQuery>,
}

impl DiffReport {
    /// Whether the run stayed within [`Self::tolerance`].
    ///
    /// A run that scored **nothing** does not pass: an oracle that declined every
    /// query would otherwise report a `max_error` of `0.0` and look like perfect
    /// agreement.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.compared > 0 && self.max_error <= self.tolerance
    }
}

impl core::fmt::Display for DiffReport {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{:?}: max_error={:e} tol={:e} ({}/{} queries scored)",
            self.reference, self.max_error, self.tolerance, self.compared, self.queries
        )?;
        if let Some(w) = self.worst_query {
            write!(f, " worst: {}<-{} @ {} ns", w.target, w.source, w.stamp_ns)?;
        }
        Ok(())
    }
}

/// A frame plus the pieces the reference lookup needs about its incoming edge.
struct RefFrame {
    parent: Option<usize>,
    depth: u32,
    /// `Some(pose)` for a static edge (`T_parent_child`); `None` for dynamic.
    static_pose: Option<Iso3>,
    /// For a dynamic edge, its `(stamp, pose)` sample stream in publish order.
    samples: Vec<(i64, Iso3)>,
}

/// An in-memory tree the reference lookup walks — built from the *same*
/// declarations and the *same* synthetic sample stream as the engine tree, but
/// entirely independent of the engine's arena, seqlock, and plan machinery.
struct RefModel {
    index: HashMap<&'static str, usize>,
    frames: Vec<RefFrame>,
}

impl RefModel {
    /// Build the reference model from the fixture, replaying the identical
    /// synthetic history the engine received.
    fn build() -> RefModel {
        let names = fixture::frame_names();
        let mut index = HashMap::new();
        for (i, &n) in names.iter().enumerate() {
            index.insert(n, i);
        }
        let mut frames: Vec<RefFrame> = names
            .iter()
            .map(|_| RefFrame {
                parent: None,
                depth: 0,
                static_pose: None,
                samples: Vec::new(),
            })
            .collect();

        // Wire parents and edge payloads.
        let mut dyn_seed = 0.0f64;
        for e in EDGES {
            let ci = index[e.child];
            let pi = index[e.parent];
            frames[ci].parent = Some(pi);
            match e.kind {
                EdgeDefKind::Static { xi } => {
                    frames[ci].static_pose = Some(tf_tree_math::exp_se3(xi));
                }
                EdgeDefKind::Dynamic { rate_hz } => {
                    let period_ns = (1e9 / rate_hz) as i64;
                    let count = (fixture::HISTORY_SECS * rate_hz) as i64;
                    let mut stream = Vec::with_capacity(count as usize);
                    for k in 0..count {
                        let stamp = k * period_ns;
                        stream.push((stamp, fixture::dynamic_pose(dyn_seed, stamp)));
                    }
                    frames[ci].samples = stream;
                    dyn_seed += 1.0;
                }
            }
        }

        // Compute depths by walking to the root.
        for i in 0..frames.len() {
            let mut d = 0u32;
            let mut cur = frames[i].parent;
            while let Some(p) = cur {
                d += 1;
                cur = frames[p].parent;
            }
            frames[i].depth = d;
        }

        RefModel { index, frames }
    }

    /// `T_parent_child` for the edge whose child is `frame`, sampled at `t`.
    fn edge_pose(&self, frame: usize, t: i64) -> Iso3 {
        let f = &self.frames[frame];
        if let Some(p) = f.static_pose {
            return p;
        }
        ref_sample(&f.samples, t)
    }

    /// Independent `lookup(target, source)` at stamp `t`, returning
    /// `T_target_source`. Mirrors the engine's LCA walk and inverted/forward step
    /// emission, but composes freshly sampled poses with no shared code.
    fn lookup(&self, target: usize, source: usize, t: i64) -> Iso3 {
        let mut a = target;
        let mut b = source;
        let mut up_t: Vec<usize> = Vec::new();
        let mut up_s: Vec<usize> = Vec::new();

        while self.frames[a].depth > self.frames[b].depth {
            up_t.push(a);
            a = self.frames[a].parent.unwrap_or(a);
        }
        while self.frames[b].depth > self.frames[a].depth {
            up_s.push(b);
            b = self.frames[b].parent.unwrap_or(b);
        }
        while a != b {
            up_t.push(a);
            up_s.push(b);
            a = self.frames[a].parent.unwrap_or(a);
            b = self.frames[b].parent.unwrap_or(b);
        }

        // T_target_source = (T_lca_target)^-1 * T_lca_source.
        // Target side: inverted, in walk order. Source side: forward, reversed.
        let mut acc = Iso3::IDENTITY;
        for &f in &up_t {
            acc = acc.mul_inv(&self.edge_pose(f, t));
        }
        for &f in up_s.iter().rev() {
            acc = acc * self.edge_pose(f, t);
        }
        acc
    }
}

/// Independent bracket-search + `LerpSlerp` sample of a `(stamp, pose)` stream at
/// `t`. Assumes `t` lies inside `[first, last]` (the harness only draws in-window
/// stamps); clamps to the endpoints otherwise.
fn ref_sample(stream: &[(i64, Iso3)], t: i64) -> Iso3 {
    if stream.is_empty() {
        return Iso3::IDENTITY;
    }
    if t <= stream[0].0 {
        return stream[0].1;
    }
    let last = stream.len() - 1;
    if t >= stream[last].0 {
        return stream[last].1;
    }
    // Linear scan for the bracket [i, i+1] with stamps[i] <= t < stamps[i+1].
    let mut i = 0usize;
    while i + 1 < stream.len() && stream[i + 1].0 <= t {
        i += 1;
    }
    let (ta, a) = stream[i];
    let (tb, b) = stream[i + 1];
    if t == ta {
        return a;
    }
    let s = (t - ta) as f64 / (tb - ta) as f64;
    <LerpSlerp as Interp>::eval(&a, &b, s)
}

/// The disagreement between two poses: `max(rotation-angle error, translation
/// error)`.
fn pose_error(x: &Iso3, y: &Iso3) -> f64 {
    let dq = x.q.conjugate() * y.q;
    let rot = log_so3(dq).norm();
    let trans = x.t.sub(y.t).norm();
    rot.max(trans)
}

/// A tiny SplitMix64 PRNG so the harness needs no `rand` dependency.
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

/// The agreement bound both references are held to.
///
/// `1e-12` is `docs/PHASE1.md` §10.5's number. It is far above `f64` epsilon and far
/// below anything a robot cares about, so it catches a genuine algorithmic
/// divergence (a wrong branch, a transposed quaternion, an off-by-one bracket)
/// without tripping on the last bits of a different-but-equivalent operation
/// order.
pub const TOLERANCE: f64 = 1e-12;

/// The oracle a differential run compares the engine against.
///
/// Both references answer the same question — `T_target_source` at a stamp — so
/// [`run`] drives them through one identical query loop.
trait Oracle {
    /// `T_target_source` at `stamp_ns`, or `None` if this oracle cannot answer
    /// (only tf2 declines, at its cache horizon).
    fn lookup(&self, target: &str, source: &str, stamp_ns: i64) -> Option<Iso3>;
}

impl Oracle for RefModel {
    fn lookup(&self, target: &str, source: &str, stamp_ns: i64) -> Option<Iso3> {
        Some(RefModel::lookup(
            self,
            self.index[target],
            self.index[source],
            stamp_ns,
        ))
    }
}

/// Run the differential query loop against an arbitrary oracle.
///
/// Builds a `LerpSlerp` engine tree with pre-populated history, then compares
/// `queries` random `lookup(target, source)` results at random in-window stamps.
/// Returns the worst disagreement; the caller asserts it is within [`TOLERANCE`].
///
/// Queries the oracle declines are **skipped, not scored**, and counted in
/// [`DiffReport::compared`] — so a tf2 cache-horizon miss never masquerades as
/// agreement, and a run where the oracle answered almost nothing is visible
/// rather than a silent pass.
fn run(reference: Reference, oracle: &dyn Oracle, queries: usize, seed: u64) -> Result<DiffReport> {
    let tree: Tree = fixture::build_tree_with(InterpPolicy::LerpSlerp)?;
    let (_writers, _samples) = fixture::spin_up(&tree)?;
    let names = fixture::frame_names();

    // Query stamps stay inside every edge's window: [NOW - 100 ms, NOW].
    let now = fixture::NOW_NS;
    let lo = now - 100_000_000;

    let mut rng = Rng(seed ^ 0xD1B5_4A32_D192_ED03);
    let mut max_error = 0.0f64;
    let mut compared = 0usize;
    let mut worst: Option<(&'static str, &'static str, i64)> = None;

    let guard = tree.guard();
    for _ in 0..queries {
        let ti = rng.below(names.len());
        let si = rng.below(names.len());
        if ti == si {
            continue;
        }
        let stamp_ns = lo + (rng.next_u64() % (now - lo) as u64) as i64;

        let Some(reference_pose) = oracle.lookup(names[ti], names[si], stamp_ns) else {
            continue; // the oracle cannot answer this one; do not score it
        };

        let target = tree
            .frame(names[ti])
            .map_err(|e| anyhow!("frame {}: {e:?}", names[ti]))?;
        let source = tree
            .frame(names[si])
            .map_err(|e| anyhow!("frame {}: {e:?}", names[si]))?;
        let plan = tree
            .plan(target, source)
            .map_err(|e| anyhow!("plan {}<-{}: {e:?}", names[ti], names[si]))?;
        let stamp: Stamp = Stamp::from_nanos(stamp_ns);
        let engine = plan
            .at(&guard, stamp)
            .map_err(|e| anyhow!("eval {}<-{}: {e:?}", names[ti], names[si]))?;

        let err = pose_error(&engine, &reference_pose);
        compared += 1;
        if err > max_error {
            max_error = err;
            worst = Some((names[ti], names[si], stamp_ns));
        }
    }

    Ok(DiffReport {
        reference,
        queries,
        compared,
        max_error,
        tolerance: TOLERANCE,
        worst_query: worst.map(|(t, s, n)| WorstQuery {
            target: t,
            source: s,
            stamp_ns: n,
        }),
    })
}

/// Run the differential harness against the naive Rust reference.
///
/// The reference is an independent Rust lookup over the *same* sample values,
/// sharing no code with the engine, so agreement is a real cross-check of the
/// seqlock ring and plan evaluation. Always available.
///
/// # Errors
///
/// Propagates fixture build / spin-up failures, or an engine lookup error (a
/// disconnected pair, which the connected fixture never produces).
pub fn run_naive_rust(queries: usize, seed: u64) -> Result<DiffReport> {
    let model = RefModel::build();
    run(Reference::NaiveRust, &model, queries, seed)
}

/// Run the differential harness against ROS 2's `tf2::BufferCore`.
///
/// This is the migration-credibility test from `docs/PHASE1.md` §10.5: identical tree,
/// identical sample stream, `LerpSlerp` on both sides (tf2's policy), compared
/// across `queries` random lookups.
///
/// # Errors
///
/// Propagates fixture failures, or a failure loading the tf2 buffer.
#[cfg(feature = "tf2")]
pub fn run_tf2(queries: usize, seed: u64) -> Result<DiffReport> {
    let fixture = crate::tf2::Tf2Fixture::load()?;
    run(Reference::Tf2, &fixture, queries, seed)
}

#[cfg(feature = "tf2")]
impl Oracle for crate::tf2::Tf2Fixture {
    fn lookup(&self, target: &str, source: &str, stamp_ns: i64) -> Option<Iso3> {
        crate::tf2::Tf2Fixture::lookup(self, target, source, stamp_ns)
    }
}
