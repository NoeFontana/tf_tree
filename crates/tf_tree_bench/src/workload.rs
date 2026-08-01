//! The workload catalogue: one definition of *what load is being run*.
//!
//! # Why this exists
//!
//! [`crate::fixture`] holds one 24-frame mobile robot, and it exists so that the
//! criterion benches, the CLI demo, the tf2 differential and the doctor's
//! healthy-tree tests "never drift apart". That worked, and it is exactly the
//! reason to do it again one level up: the performance suite now spans several
//! harnesses (contended scaling, the scale sweep, the soak, the DDS
//! comparison), and each of them needs a tree, a set of query pairs, a stamp
//! window those queries are valid in, and a list of edges a live publisher
//! should write to. Four harnesses each inventing those four things is four
//! chances for two rows to be labelled the same and measure something
//! different.
//!
//! So a [`Workload`] is the whole description, and [`Workload::build`] is the
//! only way to turn one into a running tree.
//!
//! # What a row must state, and why the shape is returned rather than logged
//!
//! `docs/PHASE1.md` §11.3 is normative about this: **"Every reported latency row
//! must state its dynamic-step count, not just its nominal depth. A row labelled
//! only 'depth 3' is not interpretable."** A static edge folds to one multiply;
//! a chain that folds to a single dynamic step measures almost nothing. The same
//! argument applies to a row labelled only `fleet_64` — it is a name, not a
//! measurement.
//!
//! [`Shape`] therefore carries the frame, edge, sample and byte counts *and* the
//! post-folding dynamic-step count of the deepest query, and every harness in
//! this crate prints it next to its numbers rather than treating it as
//! commentary.
//!
//! # The three limits a scale sweep will find, named here rather than hit
//!
//! * `tf_tree_core::MAX_DEPTH` (**16**) caps a *compiled* plan. A 24-deep spine
//!   is refused outright — `docs/benchmarks/tf2.md` records that this was found
//!   while building a scaling row, and it is checked in [`Workload::estimate`]
//!   so a catalogue entry cannot be added that no harness can query.
//! * `BuildError::TooManyFrames` / `TooManyEdges`: both counts are `u32`.
//! * **`LayoutError::ArenaTooLarge`: the whole arena must fit a `u32` offset
//!   model**, so 4 GiB is a hard ceiling regardless of frame and edge counts.
//!   No catalogue entry is near it — the largest is 85 MiB — and that is worth
//!   stating rather than implying: what bounds the entries below is population
//!   *time*, and the byte ceiling is found deliberately by `scale_sweep`.
//!
//! [`Workload::estimate`] exists as a separate, *cheap* call so a harness can
//! ask what a workload would cost — and be refused by any of the three limits
//! above — before spending a minute populating it.

use anyhow::{anyhow, bail, Context, Result};

use tf_tree::{Capacity, EdgeCfg, InterpPolicy, Iso3, Plan, Step, Tree, TreeBuilder};

use crate::fixture;
use crate::replay::{Sample, TfStream};

/// Where a workload's arena lives.
///
/// `Shared` is what the multi-process harnesses need and `Heap` is what the
/// in-process ones need; keeping it a parameter rather than two build functions
/// is what stops the two paths from being populated differently, which is a
/// difference that would show up as a benchmark result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backing {
    /// An ordinary heap arena.
    Heap,
    /// A shared `memfd` arena under the given rendezvous name. Requires the
    /// `shm` feature and Linux.
    Shared(&'static str),
}

/// How a workload's frame tree is shaped.
#[derive(Clone, Copy, Debug)]
pub enum Topology {
    /// The 24-frame mobile robot of [`crate::fixture`], built by the same code
    /// the existing benches use so a new harness's numbers are comparable with
    /// the committed ones.
    Fixture,
    /// A recorded `.tfstream`, relative to the repository root.
    Recorded {
        /// Path relative to the repository root.
        path: &'static str,
    },
    /// [`crate::replay::synth_robot`]: a dynamic spine with static sensor
    /// subtrees, the shape a real URDF produces.
    Synth {
        /// Spine length. Every spine edge is dynamic, so this *is* the
        /// dynamic-step count.
        depth: usize,
        /// Static children per spine link.
        branches: usize,
        /// Samples published per dynamic edge.
        samples: usize,
        /// Publish rate of every spine edge.
        rate_hz: f64,
    },
    /// `robots` copies of [`Topology::Fixture`] under one `world` root, each
    /// under an `r<i>/` name prefix.
    ///
    /// This is the shape the argument in `docs/PROJECT.md` is actually about,
    /// and no existing fixture has it: many *independent publishers* on one
    /// tree, wide rather than deep. `tf2` pays for it twice — once per consumer
    /// in memory, and once per lookup in a walk over a tree that got wider for
    /// reasons the lookup does not care about.
    Fleet {
        /// Number of robot subtrees.
        robots: usize,
        /// Seconds of history each dynamic ring retains. Separate from
        /// [`fixture::HISTORY_SECS`] because at fleet scale the sample count is
        /// what meets the 4 GiB arena ceiling, and a wide-but-shallow arena is
        /// a legitimate thing to measure.
        history_secs: f64,
    },
}

/// Which pairs a workload is queried on.
#[derive(Clone, Copy, Debug)]
pub enum QuerySpec {
    /// One named pair. Used where an existing published number must stay
    /// comparable — `imu_link <- map` is the depth-3-*dynamic* chain
    /// `docs/PHASE1.md` §11.3 pins the gate to.
    Fixed {
        /// Target frame.
        target: &'static str,
        /// Source frame.
        source: &'static str,
    },
    /// The deepest resolvable pair: a leaf under the last spine link, back to
    /// the root.
    Deepest,
    /// Random pairs drawn from the tree's frames, seeded — the recorded
    /// stream's shape, where no single pair is representative.
    Drawn {
        /// How many pairs.
        count: usize,
        /// Seed, so a run is byte-reproducible.
        seed: u64,
    },
    /// A leaf on robot 0 against the same leaf on the last robot: the query
    /// that crosses the fleet root and therefore composes two robots' dynamic
    /// spines.
    CrossFleet,
}

/// One named load: a topology, the pairs it is queried on, and why it is here.
#[derive(Clone, Copy, Debug)]
pub struct Workload {
    /// Stable key. Appears verbatim in every emitted JSON row, so renaming one
    /// breaks an A/B comparison against an older run — which is the correct
    /// behaviour, and why these are `&'static str` rather than derived.
    pub name: &'static str,
    /// What the tree looks like.
    pub topology: Topology,
    /// What is asked of it.
    pub queries: QuerySpec,
    /// One line on what this entry is for. Printed by the harnesses.
    pub note: &'static str,
}

/// What a workload actually costs, in the units a reader needs to interpret a
/// latency row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shape {
    /// Frames in the tree.
    pub frames: usize,
    /// Edges, static and dynamic.
    pub edges: usize,
    /// Of those, the ones with a ring.
    pub dynamic_edges: usize,
    /// Samples published during population.
    pub samples: usize,
    /// Total ring slots, summed over every dynamic edge (each is a power of
    /// two, so this exceeds `samples`).
    pub slots: usize,
    /// The arena's size in bytes.
    pub arena_bytes: usize,
    /// **Post-folding `Step::Dyn` count of the deepest query plan** — the
    /// number `docs/PHASE1.md` §11.3 says a latency row is uninterpretable
    /// without. `None` before the tree is built, since folding is a property of
    /// the compiled plan and not of the topology.
    pub dyn_steps: Option<usize>,
}

/// The edges a live publisher writes to while a harness measures.
///
/// A reader benchmark against a quiescent tree exercises neither tf_tree's
/// seqlock retry path nor `tf2::BufferCore`'s writer/reader exclusion, which is
/// the flaw `crate::mp`'s header names and which every harness in this suite
/// avoids by driving these.
#[derive(Clone, Debug)]
pub struct PubEdge {
    /// Parent frame name.
    pub parent: String,
    /// Child frame name.
    pub child: String,
    /// The rate this edge is published at.
    pub rate_hz: f64,
    /// Seed for [`fixture::dynamic_pose`], so two processes publishing the same
    /// edge produce the same trajectory.
    pub seed: f64,
    /// The stamp population stopped at. A live publisher continues from here
    /// rather than restarting at zero, which would be an out-of-order push and
    /// would be rejected.
    pub next_stamp_ns: i64,
}

/// One declared edge of a workload, as a consumer outside this crate sees it.
///
/// This exists for the DDS comparison, whose publisher and whose bridge config
/// are both *generated* from a workload rather than hand-written. Generating
/// them is what makes "a `dds_bench` row and a `contended_scaling` row on the
/// same workload name describe the same tree" true by construction instead of
/// by somebody keeping two files in step.
#[derive(Clone, Debug)]
pub enum EdgeDecl {
    /// A static edge and its constant pose.
    Static {
        /// Parent frame name.
        parent: String,
        /// Child frame name.
        child: String,
        /// `T_parent_child`.
        pose: Iso3,
    },
    /// A dynamic edge, its rate, and how much history its ring holds.
    Dynamic {
        /// Parent frame name.
        parent: String,
        /// Child frame name.
        child: String,
        /// Nominal publish rate.
        rate_hz: f64,
        /// Seconds of history the ring is sized for.
        history_secs: f64,
    },
}

/// A built, populated workload.
pub struct Built {
    /// The tree, with its history already published.
    pub tree: Tree,
    /// Counts and bytes, with `dyn_steps` filled in from the compiled plan.
    pub shape: Shape,
    /// `(target, source)` pairs to query.
    pub pairs: Vec<(String, String)>,
    /// A stamp window every pair can be answered in, `[lo, hi]`.
    pub window: (i64, i64),
    /// Edges a live publisher may write to.
    pub publishers: Vec<PubEdge>,
}

impl Built {
    /// Compile every pair into a [`Plan`].
    ///
    /// # Errors
    ///
    /// If a frame name is unknown or the path does not compile (disconnected,
    /// or deeper than `MAX_DEPTH`).
    pub fn plans(&self) -> Result<Vec<Plan>> {
        let mut out = Vec::with_capacity(self.pairs.len());
        for (target, source) in &self.pairs {
            let t = self
                .tree
                .frame(target)
                .map_err(|e| anyhow!("frame {target}: {e:?}"))?;
            let s = self
                .tree
                .frame(source)
                .map_err(|e| anyhow!("frame {source}: {e:?}"))?;
            out.push(
                self.tree
                    .plan(t, s)
                    .map_err(|e| anyhow!("plan {target} <- {source}: {e:?}"))?,
            );
        }
        Ok(out)
    }

    /// A stamp `frac` of the way through [`Self::window`], for `frac` in
    /// `[0, 1]`.
    #[must_use]
    pub fn stamp_at(&self, frac: f64) -> i64 {
        let (lo, hi) = self.window;
        lo + ((hi - lo) as f64 * frac.clamp(0.0, 1.0)) as i64
    }
}

/// The number of `Step::Dyn` entries in a compiled plan.
///
/// Free-standing rather than a method on [`Built`] because the scale sweep
/// computes it for plans it compiled itself.
#[must_use]
pub fn dyn_steps(plan: &Plan) -> usize {
    plan.steps()
        .iter()
        .filter(|s| matches!(s, Step::Dyn { .. }))
        .count()
}

/// Seconds of history a synthetic or fleet workload retains by default.
pub const DEFAULT_HISTORY_SECS: f64 = fixture::HISTORY_SECS;

/// The named catalogue, ordered realistic → extreme.
///
/// Adding an entry is cheap; `tests/workload.rs` builds every one of them and
/// asserts its query pairs resolve, which is the guard against a sweep that
/// runs green while measuring nothing.
pub const CATALOGUE: &[Workload] = &[
    Workload {
        name: "robot",
        topology: Topology::Fixture,
        // `imu_link <- map` is three *dynamic* steps (map→odom, odom→base_link,
        // base_link→imu_link), which is the chain `docs/PHASE1.md` §11.3 pins
        // the gate to and the one `read_scaling` and `mp_bench` already use.
        // Every new harness must reproduce the committed numbers here or it is
        // measuring something else.
        queries: QuerySpec::Fixed {
            target: "imu_link",
            source: "map",
        },
        note: "the 24-frame mobile robot; the continuity anchor for every other row",
    },
    Workload {
        name: "recorded",
        topology: Topology::Recorded {
            path: "testdata/tfstream/indoor_atelier.tfstream",
        },
        queries: QuerySpec::Drawn {
            count: 256,
            seed: 0x5EED,
        },
        note: "a real recording: irregular periods, duplicate stamps, late frames",
    },
    Workload {
        name: "humanoid",
        topology: Topology::Synth {
            depth: 12,
            branches: 8,
            samples: 512,
            rate_hz: 100.0,
        },
        queries: QuerySpec::Deepest,
        note: "~117 frames, 12 dynamic steps: a dual-arm or humanoid description",
    },
    Workload {
        name: "av",
        topology: Topology::Synth {
            depth: 14,
            branches: 24,
            samples: 512,
            rate_hz: 100.0,
        },
        queries: QuerySpec::Deepest,
        note: "~375 frames, 14 dynamic steps: a large multi-sensor platform",
    },
    Workload {
        name: "fleet_16",
        topology: Topology::Fleet {
            robots: 16,
            history_secs: DEFAULT_HISTORY_SECS,
        },
        queries: QuerySpec::CrossFleet,
        note: "16 robots on one tree: many independent publishers, wide not deep",
    },
    Workload {
        name: "fleet_64",
        topology: Topology::Fleet {
            robots: 64,
            history_secs: DEFAULT_HISTORY_SECS,
        },
        queries: QuerySpec::CrossFleet,
        note: "64 robots: 1.5k frames, 256 publishing edges",
    },
    Workload {
        name: "extreme_wide",
        topology: Topology::Fleet {
            robots: 512,
            // 1 s, not 10. At 10 s this is 6.4M samples and 460 MB of arena,
            // which builds for minutes to measure a property — lookup cost
            // against a very wide tree — that does not depend on ring depth.
            // The ring-depth axis is swept separately by `scale_sweep`.
            history_secs: 1.0,
        },
        queries: QuerySpec::CrossFleet,
        note: "512 robots: 12k frames, 2k publishing edges — the width extreme",
    },
];

/// Look a workload up by name.
///
/// # Errors
///
/// If no entry has that name; the message lists the ones that do, because a
/// typo at the command line should not read as an empty result set.
pub fn by_name(name: &str) -> Result<&'static Workload> {
    CATALOGUE.iter().find(|w| w.name == name).ok_or_else(|| {
        let known: Vec<&str> = CATALOGUE.iter().map(|w| w.name).collect();
        anyhow!("unknown workload {name:?}; known: {}", known.join(", "))
    })
}

/// Every catalogue name, for a `--help` line.
#[must_use]
pub fn names() -> Vec<&'static str> {
    CATALOGUE.iter().map(|w| w.name).collect()
}

impl Workload {
    /// What this workload would cost, without building it.
    ///
    /// Cheap for `Fixture`, `Synth` and `Fleet` (arithmetic over the topology);
    /// for `Recorded` it parses the file, which is the only way to know how many
    /// samples it holds.
    ///
    /// `dyn_steps` is always `None` here: folding is a property of the compiled
    /// plan, so it can only be filled in once a tree exists.
    ///
    /// # Errors
    ///
    /// If a recording cannot be read, or if the implied arena exceeds the `u32`
    /// offset model (`LayoutError::ArenaTooLarge`) — refused here rather than
    /// after a minute of population.
    pub fn estimate(&self) -> Result<Shape> {
        let plan = self.plan()?;
        shape_of(&plan)
    }

    /// Build and populate the workload.
    ///
    /// # Errors
    ///
    /// If the topology cannot be built, a push is rejected, or a query pair
    /// does not resolve.
    pub fn build(&self, interp: InterpPolicy, backing: Backing) -> Result<Built> {
        let plan = self.plan()?;
        let mut shape = shape_of(&plan)?;

        let tree = build_tree(&plan, interp, backing)
            .with_context(|| format!("building workload {}", self.name))?;
        populate(&tree, &plan).with_context(|| format!("populating workload {}", self.name))?;

        let built = Built {
            tree,
            shape,
            pairs: plan.pairs,
            window: plan.window,
            publishers: plan.publishers,
        };

        // Fill in the one number that needed a compiled plan. The *deepest*
        // query is the one the row is labelled with, so this is a max and not
        // an average: a mixed set whose worst plan is 4 dynamic steps is not
        // interpretable as "1 step" because most of its pairs are shallow.
        let compiled = built.plans()?;
        shape.dyn_steps = compiled.iter().map(dyn_steps).max();

        Ok(Built { shape, ..built })
    }

    /// The workload's declared edges, without building or populating it.
    ///
    /// # Errors
    ///
    /// As [`Workload::estimate`].
    pub fn edges(&self) -> Result<Vec<EdgeDecl>> {
        let plan = self.plan()?;
        let mut out = Vec::with_capacity(plan.statics.len() + plan.dynamics.len());
        for e in &plan.statics {
            out.push(EdgeDecl::Static {
                parent: e.parent.clone(),
                child: e.child.clone(),
                pose: e.pose,
            });
        }
        for e in &plan.dynamics {
            out.push(EdgeDecl::Dynamic {
                parent: e.parent.clone(),
                child: e.child.clone(),
                rate_hz: e.rate_hz,
                history_secs: e.history_secs,
            });
        }
        Ok(out)
    }

    /// The `(target, source)` pairs this workload is queried on, without
    /// building it.
    ///
    /// # Errors
    ///
    /// As [`Workload::estimate`].
    pub fn query_pairs(&self) -> Result<Vec<(String, String)>> {
        Ok(self.plan()?.pairs)
    }

    /// The topology, expanded into the intermediate form both `estimate` and
    /// `build` work from.
    fn plan(&self) -> Result<BuildPlan> {
        match self.topology {
            Topology::Fixture => fixture_plan(self.queries),
            Topology::Recorded { path } => recorded_plan(path, self.queries),
            Topology::Synth {
                depth,
                branches,
                samples,
                rate_hz,
            } => stream_plan(
                &crate::replay::synth_robot(depth, branches, samples, rate_hz),
                self.queries,
                rate_hz,
            ),
            Topology::Fleet {
                robots,
                history_secs,
            } => fleet_plan(robots, history_secs, self.queries),
        }
    }
}

// ---------------------------------------------------------------------------
// The intermediate form
// ---------------------------------------------------------------------------

/// A static edge, ready to declare.
struct StaticEdge {
    parent: String,
    child: String,
    pose: Iso3,
}

/// A dynamic edge, its ring size, and the samples to publish into it.
struct DynEdge {
    parent: String,
    child: String,
    rate_hz: f64,
    /// Seconds of history the ring is sized for. Carried rather than derived
    /// from `capacity / rate_hz`: `Capacity` rounds up to a power of two, so
    /// the derivation would emit a different number from the one that was
    /// asked for — and that number goes into a bridge config an operator reads.
    history_secs: f64,
    capacity: Capacity,
    /// `(stamp_ns, pose)` in stamp order.
    samples: Vec<(i64, Iso3)>,
    /// [`fixture::dynamic_pose`] seed, carried into [`PubEdge`] so a live
    /// publisher continues the same trajectory.
    seed: f64,
}

/// Everything needed to build and populate a tree, plus what to ask it.
struct BuildPlan {
    statics: Vec<StaticEdge>,
    dynamics: Vec<DynEdge>,
    pairs: Vec<(String, String)>,
    window: (i64, i64),
    publishers: Vec<PubEdge>,
}

fn shape_of(plan: &BuildPlan) -> Result<Shape> {
    let frames = distinct_frames(plan);
    let edges = plan.statics.len() + plan.dynamics.len();
    let samples: usize = plan.dynamics.iter().map(|d| d.samples.len()).sum();
    let slots: usize = plan
        .dynamics
        .iter()
        .map(|d| d.capacity.get() as usize)
        .sum();

    // `TreeBuilder` adds one frame and one edge of slack plus any headroom, and
    // `ArenaLayout` is what actually decides whether this fits. Asking it here —
    // with the totals rather than the split, which is exactly what `from_totals`
    // is for — is what turns a 4 GiB overrun into a message instead of a build
    // that runs for a minute and then fails.
    let max_frames = u32::try_from(frames + 1).map_err(|_| anyhow!("too many frames: {frames}"))?;
    let max_edges = u32::try_from(edges + 1).map_err(|_| anyhow!("too many edges: {edges}"))?;
    let total_slots = u32::try_from(slots).map_err(|_| anyhow!("too many ring slots: {slots}"))?;
    let layout = tf_tree_arena::ArenaLayout::from_totals(max_frames, max_edges, total_slots)
        .map_err(|e| {
            anyhow!(
                "this workload does not fit an arena ({frames} frames, {edges} edges, \
                 {slots} slots): {e:?}. The arena's offsets are u32, so 4 GiB is the \
                 ceiling; reduce history_secs or robot count."
            )
        })?;

    Ok(Shape {
        frames,
        edges,
        dynamic_edges: plan.dynamics.len(),
        samples,
        slots,
        arena_bytes: layout.total_size(),
        dyn_steps: None,
    })
}

fn distinct_frames(plan: &BuildPlan) -> usize {
    let mut names: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for e in &plan.statics {
        names.insert(&e.parent);
        names.insert(&e.child);
    }
    for e in &plan.dynamics {
        names.insert(&e.parent);
        names.insert(&e.child);
    }
    names.len()
}

fn build_tree(plan: &BuildPlan, interp: InterpPolicy, backing: Backing) -> Result<Tree> {
    let mut b = TreeBuilder::new().default_interp(interp);
    for e in &plan.statics {
        b = b.static_edge(&e.parent, &e.child, &e.pose);
    }
    for e in &plan.dynamics {
        b = b.dynamic_edge(
            &e.parent,
            &e.child,
            EdgeCfg::new(e.capacity).nominal_rate_hz(e.rate_hz),
        );
    }
    match backing {
        Backing::Heap => b.build().map_err(|e| anyhow!("build: {e}")),
        #[cfg(all(feature = "shm", target_os = "linux"))]
        Backing::Shared(name) => b
            .build_shared(name)
            .map_err(|e| anyhow!("build_shared: {e}")),
        #[cfg(not(all(feature = "shm", target_os = "linux")))]
        Backing::Shared(_) => {
            bail!("a shared arena needs `--features shm` on Linux; this build has neither")
        }
    }
}

fn populate(tree: &Tree, plan: &BuildPlan) -> Result<()> {
    let mut writers = Vec::with_capacity(plan.dynamics.len());
    for e in &plan.dynamics {
        let parent = tree
            .frame(&e.parent)
            .map_err(|err| anyhow!("frame {}: {err:?}", e.parent))?;
        let child = tree
            .frame(&e.child)
            .map_err(|err| anyhow!("frame {}: {err:?}", e.child))?;
        writers.push(
            tree.claim(child, parent)
                .map_err(|err| anyhow!("claim {}->{}: {err}", e.parent, e.child))?,
        );
    }
    for (w, e) in writers.iter().zip(&plan.dynamics) {
        for (stamp, pose) in &e.samples {
            w.push(*stamp, pose)
                .map_err(|err| anyhow!("push {}->{} @{stamp}: {err:?}", e.parent, e.child))?;
        }
    }
    // Dropping the writers releases every claim, which is what the reader
    // harnesses want: a live publisher re-claims the edges it is going to write.
    drop(writers);
    Ok(())
}

// ---------------------------------------------------------------------------
// Topology expansion
// ---------------------------------------------------------------------------

fn fixture_plan(queries: QuerySpec) -> Result<BuildPlan> {
    let mut statics = Vec::new();
    let mut dynamics = Vec::new();

    for e in fixture::EDGES {
        match e.kind {
            fixture::EdgeDefKind::Static { xi } => statics.push(StaticEdge {
                parent: e.parent.to_owned(),
                child: e.child.to_owned(),
                pose: tf_tree::exp_se3(xi),
            }),
            fixture::EdgeDefKind::Dynamic { rate_hz } => {
                // The seed must match `fixture::spin_up`'s, which is the
                // dynamic edge's index in `DYNAMIC_EDGES` — otherwise this tree
                // holds a different trajectory from the one every committed
                // number was taken against.
                let seed = fixture::DYNAMIC_EDGES
                    .iter()
                    .position(|(p, c, _)| *p == e.parent && *c == e.child)
                    .ok_or_else(|| {
                        anyhow!(
                            "fixture edge {}->{} is dynamic in EDGES but absent from \
                             DYNAMIC_EDGES; the two tables have drifted",
                            e.parent,
                            e.child
                        )
                    })? as f64;
                dynamics.push(dyn_edge(
                    e.parent.to_owned(),
                    e.child.to_owned(),
                    rate_hz,
                    fixture::HISTORY_SECS,
                    seed,
                ));
            }
        }
    }

    let pairs = resolve_pairs(queries, &statics, &dynamics)?;
    finish(statics, dynamics, pairs)
}

fn recorded_plan(rel_path: &str, queries: QuerySpec) -> Result<BuildPlan> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel_path);
    let stream = TfStream::load(&path).with_context(|| format!("loading {}", path.display()))?;
    // A recording's rate is not declared, so it is measured: the median
    // interval over the samples of each edge. A *mean* would be dragged by the
    // gaps §3.2 says every real recording has, and a rate is only used here to
    // pace a live publisher and to fill `nominal_rate_hz`.
    stream_plan_recorded(&stream, queries)
}

fn stream_plan_recorded(stream: &TfStream, queries: QuerySpec) -> Result<BuildPlan> {
    let counts = stream.samples_per_edge();
    let window = stream
        .common_window()
        .ok_or_else(|| anyhow!("recording has no window covered by every dynamic edge"))?;

    let mut per_edge: Vec<Vec<(i64, Iso3)>> = vec![Vec::new(); stream.dynamic_edges.len()];
    for Sample {
        edge,
        stamp_ns,
        pose,
    } in &stream.samples
    {
        per_edge[*edge].push((*stamp_ns, *pose));
    }

    let statics: Vec<StaticEdge> = stream
        .static_edges
        .iter()
        .map(|(p, c, pose)| StaticEdge {
            parent: p.clone(),
            child: c.clone(),
            pose: *pose,
        })
        .collect();

    let mut dynamics = Vec::with_capacity(stream.dynamic_edges.len());
    for (i, (p, c)) in stream.dynamic_edges.iter().enumerate() {
        let samples = std::mem::take(&mut per_edge[i]);
        let rate_hz = median_rate_hz(&samples);
        // Size for what the recording holds, plus the one slot a ring cannot
        // hand back (`SampleRing::retained`) — the same reasoning
        // `TfStream::build_tree` documents. Sizing for `count` alone loses the
        // oldest sample when `count` is a power of two, and the oldest sample is
        // exactly what `common_window`'s lower bound points at.
        let want = u32::try_from(counts[i])
            .unwrap_or(u32::MAX)
            .saturating_add(1);
        let history_secs = if rate_hz > 0.0 {
            samples.len() as f64 / rate_hz
        } else {
            0.0
        };
        dynamics.push(DynEdge {
            parent: p.clone(),
            child: c.clone(),
            rate_hz,
            history_secs,
            capacity: Capacity::slots(want),
            samples,
            seed: i as f64,
        });
    }

    let pairs = resolve_pairs(queries, &statics, &dynamics)?;
    let publishers = publishers_of(&dynamics);
    Ok(BuildPlan {
        statics,
        dynamics,
        pairs,
        window,
        publishers,
    })
}

fn stream_plan(stream: &TfStream, queries: QuerySpec, rate_hz: f64) -> Result<BuildPlan> {
    let mut per_edge: Vec<Vec<(i64, Iso3)>> = vec![Vec::new(); stream.dynamic_edges.len()];
    for Sample {
        edge,
        stamp_ns,
        pose,
    } in &stream.samples
    {
        per_edge[*edge].push((*stamp_ns, *pose));
    }

    let statics: Vec<StaticEdge> = stream
        .static_edges
        .iter()
        .map(|(p, c, pose)| StaticEdge {
            parent: p.clone(),
            child: c.clone(),
            pose: *pose,
        })
        .collect();

    let mut dynamics = Vec::with_capacity(stream.dynamic_edges.len());
    for (i, (p, c)) in stream.dynamic_edges.iter().enumerate() {
        let samples = std::mem::take(&mut per_edge[i]);
        let want = u32::try_from(samples.len())
            .unwrap_or(u32::MAX)
            .saturating_add(1);
        let history_secs = if rate_hz > 0.0 {
            samples.len() as f64 / rate_hz
        } else {
            0.0
        };
        dynamics.push(DynEdge {
            parent: p.clone(),
            child: c.clone(),
            rate_hz,
            history_secs,
            capacity: Capacity::slots(want),
            samples,
            seed: i as f64,
        });
    }

    let pairs = resolve_pairs(queries, &statics, &dynamics)?;
    finish(statics, dynamics, pairs)
}

/// `robots` copies of the fixture under one `world` root.
fn fleet_plan(robots: usize, history_secs: f64, queries: QuerySpec) -> Result<BuildPlan> {
    if robots == 0 {
        bail!("a fleet needs at least one robot");
    }
    let mut statics = Vec::new();
    let mut dynamics = Vec::new();

    for i in 0..robots {
        let pfx = format!("r{i}/");
        // Attach this robot's root to the fleet root. Static, because a fleet
        // frame is normally established by a localisation output that is itself
        // one of the dynamic edges below — making it dynamic would add a step
        // to every cross-fleet query for no modelling gain.
        statics.push(StaticEdge {
            parent: FLEET_ROOT.to_owned(),
            child: format!("{pfx}map"),
            pose: fixture::dynamic_pose(i as f64 * 0.37, 0),
        });

        for e in fixture::EDGES {
            let parent = format!("{pfx}{}", e.parent);
            let child = format!("{pfx}{}", e.child);
            match e.kind {
                fixture::EdgeDefKind::Static { xi } => statics.push(StaticEdge {
                    parent,
                    child,
                    pose: tf_tree::exp_se3(xi),
                }),
                fixture::EdgeDefKind::Dynamic { rate_hz } => {
                    // Per-robot seed, so two robots do not publish an identical
                    // trajectory — which would let a shared cache line carry
                    // both and make the fleet look cheaper than it is.
                    let seed = (i * fixture::DYNAMIC_EDGES.len()) as f64 + dynamics.len() as f64;
                    dynamics.push(dyn_edge(parent, child, rate_hz, history_secs, seed));
                }
            }
        }
    }

    let pairs = resolve_pairs(queries, &statics, &dynamics)?;
    finish(statics, dynamics, pairs)
}

/// The fleet's root frame name.
pub const FLEET_ROOT: &str = "world";

/// A dynamic edge holding `history_secs` of [`fixture::dynamic_pose`] at
/// `rate_hz`.
fn dyn_edge(parent: String, child: String, rate_hz: f64, history_secs: f64, seed: f64) -> DynEdge {
    let capacity = Capacity::history(rate_hz, history_secs);
    let period_ns = (1e9 / rate_hz) as i64;
    let count = (history_secs * rate_hz) as i64;
    let samples = (0..count)
        .map(|k| {
            let stamp = k * period_ns;
            (stamp, fixture::dynamic_pose(seed, stamp))
        })
        .collect();
    DynEdge {
        parent,
        child,
        rate_hz,
        history_secs,
        capacity,
        samples,
        seed,
    }
}

/// Assemble a plan whose stamp window is the intersection of its edges'.
fn finish(
    statics: Vec<StaticEdge>,
    dynamics: Vec<DynEdge>,
    pairs: Vec<(String, String)>,
) -> Result<BuildPlan> {
    let window = common_window(&dynamics).ok_or_else(|| {
        anyhow!("no stamp window is covered by every dynamic edge; nothing could be queried")
    })?;
    let publishers = publishers_of(&dynamics);
    Ok(BuildPlan {
        statics,
        dynamics,
        pairs,
        window,
        publishers,
    })
}

/// The widest `[lo, hi]` every dynamic edge has data for.
///
/// The *intersection*, not the union: a query outside it is answered by some
/// edges and declined by others, and a benchmark whose queries are partly
/// declined is timing an error path.
fn common_window(dynamics: &[DynEdge]) -> Option<(i64, i64)> {
    let mut lo = i64::MIN;
    let mut hi = i64::MAX;
    for d in dynamics {
        let first = d.samples.first()?.0;
        let last = d.samples.last()?.0;
        lo = lo.max(first);
        hi = hi.min(last);
    }
    (lo < hi).then_some((lo, hi))
}

fn publishers_of(dynamics: &[DynEdge]) -> Vec<PubEdge> {
    dynamics
        .iter()
        .map(|d| PubEdge {
            parent: d.parent.clone(),
            child: d.child.clone(),
            rate_hz: d.rate_hz,
            seed: d.seed,
            next_stamp_ns: d.samples.last().map_or(0, |s| s.0) + (1e9 / d.rate_hz) as i64,
        })
        .collect()
}

/// The median inter-sample interval, as a rate.
///
/// Median rather than mean: `docs/PHASE5.md` §3.2 lists the gaps and duplicate
/// stamps every real recording contains, and one 5-second dropout drags a mean
/// far enough to misdescribe a 100 Hz publisher.
fn median_rate_hz(samples: &[(i64, Iso3)]) -> f64 {
    if samples.len() < 2 {
        return 0.0;
    }
    let mut deltas: Vec<i64> = samples.windows(2).map(|w| w[1].0 - w[0].0).collect();
    deltas.sort_unstable();
    let mid = deltas[deltas.len() / 2];
    if mid <= 0 {
        return 0.0;
    }
    1e9 / mid as f64
}

// ---------------------------------------------------------------------------
// Query selection
// ---------------------------------------------------------------------------

fn resolve_pairs(
    queries: QuerySpec,
    statics: &[StaticEdge],
    dynamics: &[DynEdge],
) -> Result<Vec<(String, String)>> {
    match queries {
        QuerySpec::Fixed { target, source } => Ok(vec![(target.to_owned(), source.to_owned())]),
        QuerySpec::Deepest => {
            let (root, leaf) = deepest_chain(statics, dynamics)?;
            Ok(vec![(leaf, root)])
        }
        QuerySpec::CrossFleet => {
            // The whole point of this spec: a pair whose path leaves one
            // robot's subtree, crosses the fleet root and descends into
            // another's, so the plan composes two spines' worth of dynamic
            // steps. Querying within one robot would measure the `robot` row
            // with extra frames in the arena.
            let robots = fleet_robot_count(statics);
            if robots < 2 {
                bail!("CrossFleet needs at least two robots; found {robots}");
            }
            Ok(vec![(
                "r0/camera_optical".to_owned(),
                format!("r{}/camera_optical", robots - 1),
            )])
        }
        QuerySpec::Drawn { count, seed } => {
            let names = frame_names(statics, dynamics);
            if names.len() < 2 {
                bail!("fewer than two frames; nothing to query");
            }
            let mut rng = SplitMix(seed ^ 0x9E37_79B9_7F4A_7C15);
            let mut out = Vec::with_capacity(count);
            while out.len() < count {
                let a = rng.below(names.len());
                let b = rng.below(names.len());
                if a != b {
                    out.push((names[a].clone(), names[b].clone()));
                }
            }
            Ok(out)
        }
    }
}

fn frame_names(statics: &[StaticEdge], dynamics: &[DynEdge]) -> Vec<String> {
    let mut set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for e in statics {
        set.insert(e.parent.clone());
        set.insert(e.child.clone());
    }
    for e in dynamics {
        set.insert(e.parent.clone());
        set.insert(e.child.clone());
    }
    set.into_iter().collect()
}

fn fleet_robot_count(statics: &[StaticEdge]) -> usize {
    statics.iter().filter(|e| e.parent == FLEET_ROOT).count()
}

/// The `(root, leaf)` of the longest parent chain in the topology.
///
/// Walked over the parent map rather than assumed from a naming convention, so
/// it stays correct if `synth_robot`'s shape changes.
fn deepest_chain(statics: &[StaticEdge], dynamics: &[DynEdge]) -> Result<(String, String)> {
    let mut parent: std::collections::BTreeMap<&str, &str> = std::collections::BTreeMap::new();
    for e in statics {
        parent.insert(&e.child, &e.parent);
    }
    for e in dynamics {
        parent.insert(&e.child, &e.parent);
    }
    let mut best: Option<(usize, &str, &str)> = None;
    for &child in parent.keys() {
        let mut depth = 0usize;
        let mut cur = child;
        // `parent` is acyclic by construction (a `TreeBuilder` would refuse a
        // cycle), but the bound keeps a malformed hand-built plan from hanging
        // the harness instead of failing it.
        while let Some(&p) = parent.get(cur) {
            depth += 1;
            cur = p;
            if depth > parent.len() {
                bail!("cycle in the topology while looking for the deepest chain");
            }
        }
        if best.is_none_or(|(d, _, _)| depth > d) {
            best = Some((depth, cur, child));
        }
    }
    let (depth, root, leaf) = best.ok_or_else(|| anyhow!("topology has no edges"))?;
    if depth > tf_tree::MAX_DEPTH {
        bail!(
            "deepest chain is {depth} edges ({leaf} <- {root}) and MAX_DEPTH is {}; \
             no plan can be compiled for it",
            tf_tree::MAX_DEPTH
        );
    }
    Ok((root.to_owned(), leaf.to_owned()))
}

/// SplitMix64 — the same generator `crate::replay` uses, so the suite needs no
/// `rand` dependency and every draw is reproducible from its seed.
struct SplitMix(u64);

impl SplitMix {
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

impl Shape {
    /// A one-line description for a harness header.
    ///
    /// Every harness prints this next to its numbers. `docs/PHASE1.md` §11.3:
    /// a row that does not state its dynamic-step count is not interpretable.
    #[must_use]
    pub fn describe(&self) -> String {
        let steps = self
            .dyn_steps
            .map_or_else(|| "?".to_owned(), |n| n.to_string());
        format!(
            "{} frames, {} edges ({} dynamic), {} samples in {} slots, \
             {:.1} MiB arena, {steps} dynamic steps",
            self.frames,
            self.edges,
            self.dynamic_edges,
            self.samples,
            self.slots,
            self.arena_bytes as f64 / (1024.0 * 1024.0),
        )
    }
}
