//! The workload catalogue: one definition of *what load is being run*.
//!
//! A [`Workload`] is the whole description of a load (tree, query pairs, stamp
//! window, live-publisher edges) and [`Workload::build`] the only way to turn it
//! into a running tree, so no two harnesses label different loads alike.
//!
//! `docs/PHASE1.md` §11.3 is normative: every latency row must state its
//! dynamic-step count, not only nominal depth. [`Shape`] carries it.
//!
//! Limits a scale sweep finds:
//!
//! * Two depth bounds (`0034`): `tf_tree_core::MAX_DEPTH` (**32**) caps a
//!   *compiled* plan (after static folding), `MAX_PATH_EDGES` (**64**) the raw
//!   walk. The raw one is checked in [`Workload::estimate`]; the compiled one
//!   surfaces from [`Workload::build`]'s `Built::plans` and `tests/workload.rs`
//!   asserts it over the catalogue.
//! * `BuildError::TooManyFrames` / `TooManyEdges`: both counts are `u32`.
//! * `LayoutError::ArenaTooLarge`: 4 GiB is a hard ceiling (largest entry 85
//!   MiB); `scale_sweep` finds it deliberately.

use anyhow::{anyhow, bail, Context, Result};

use tf_tree::{Capacity, EdgeCfg, InterpPolicy, Iso3, Plan, Step, Tree, TreeBuilder};

use crate::fixture;
use crate::replay::{Sample, TfStream};

/// Where a workload's arena lives.
///
/// A parameter rather than two build functions, so both paths populate identically.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backing {
    /// An ordinary heap arena.
    Heap,
    /// A shared `memfd` arena under the given rendezvous name. Requires the
    /// `shm` feature and Linux.
    ///
    /// Readers only: since `docs/decisions/0028` step 0b a peer holding the fd can
    /// attach `ReadOnly` only. Harnesses whose children *publish* want [`Backing::Served`].
    Shared(&'static str),
    /// A shared arena created **through the rendezvous**, so peers can join it
    /// read-write: this process owns the ownership byte, lock file and serving
    /// thread. `just contended-scaling`'s writers need it. Costs a runtime
    /// directory (`$TF_TREE_RUNTIME_DIR`, else `$XDG_RUNTIME_DIR/tf_tree`, else
    /// `/run/tf_tree`, else `/tmp/tf_tree-<uid>`) holding a lock file and socket.
    ///
    /// # `require_create`
    ///
    /// A benchmark that silently *joined* a live arena would publish that arena's
    /// numbers as its own, and `layout_hash` cannot catch a topology mismatch. So
    /// this uses [`tf_tree::Open::require_create`]: `OpenOutcome::Joined` becomes
    /// `OpenError::ArenaAlreadyLive`. Pinned by
    /// `a_served_workload_refuses_an_arena_it_did_not_create`
    /// (`crates/tf_tree_bench/tests/multiprocess.rs`).
    ///
    /// `IfAbsent` rather than `Always`: creating over a live but unreachable
    /// arena leaves survivors publishing into a segment this process cannot see,
    /// inside the measurement. `IfAbsent` reports `ArenaHeldButUnreachable` with
    /// the holding pids instead.
    Served(&'static str),
}

/// How a workload's frame tree is shaped.
#[derive(Clone, Copy, Debug)]
pub enum Topology {
    /// The 24-frame mobile robot of [`crate::fixture`], built by the same code as the existing benches.
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
    ///
    /// Many independent publishers on one wide tree, which no other fixture has.
    Fleet {
        /// Number of robot subtrees.
        robots: usize,
        /// Seconds of history each dynamic ring retains, separate from
        /// [`fixture::HISTORY_SECS`] because fleet sample counts meet the arena ceiling.
        history_secs: f64,
    },
}

/// Which pairs a workload is queried on.
#[derive(Clone, Copy, Debug)]
pub enum QuerySpec {
    /// One named pair, where a published number must stay comparable (`imu_link <- map`, `docs/PHASE1.md` §11.3).
    Fixed {
        /// Target frame.
        target: &'static str,
        /// Source frame.
        source: &'static str,
    },
    /// The deepest resolvable pair: a leaf under the last spine link, back to
    /// the root.
    Deepest,
    /// Seeded random pairs, for the recorded stream where no one pair is representative.
    Drawn {
        /// How many pairs.
        count: usize,
        /// Seed, so a run is byte-reproducible.
        seed: u64,
    },
    /// A leaf on robot 0 against the same leaf on the last robot, crossing the fleet root.
    CrossFleet,
}

/// One named load: a topology, the pairs it is queried on, and why it is here.
#[derive(Clone, Copy, Debug)]
pub struct Workload {
    /// Stable key, verbatim in every JSON row; renaming breaks A/B against older runs.
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
    /// Total ring slots over every dynamic edge (powers of two, so >= `samples`).
    pub slots: usize,
    /// The arena's size in bytes.
    pub arena_bytes: usize,
    /// Post-folding `Step::Dyn` count of the deepest plan (`docs/PHASE1.md` §11.3);
    /// `None` until the tree is built.
    pub dyn_steps: Option<usize>,
}

/// The edges a live publisher writes to while a harness measures.
///
/// A reader benchmark on a quiescent tree exercises neither the seqlock retry
/// path nor `tf2::BufferCore`'s writer/reader exclusion (see `crate::mp`).
#[derive(Clone, Debug)]
pub struct PubEdge {
    /// Parent frame name.
    pub parent: String,
    /// Child frame name.
    pub child: String,
    /// The rate this edge is published at.
    pub rate_hz: f64,
    /// Seed for [`fixture::dynamic_pose`]; same edge, same trajectory.
    pub seed: f64,
    /// The stamp population stopped at; a publisher continues from it (restarting at zero would be rejected).
    pub next_stamp_ns: i64,
}

/// One declared edge of a workload, as a consumer outside this crate sees it.
///
/// For the DDS comparison, whose publisher and bridge config are generated from
/// a workload so both describe the same tree by construction.
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
    /// longer than `MAX_PATH_EDGES` raw edges, or more than `MAX_DEPTH` steps
    /// once folded).
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
/// Free-standing: the scale sweep uses it on plans it compiled itself.
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
/// `tests/workload.rs` builds every entry and asserts its query pairs resolve.
pub const CATALOGUE: &[Workload] = &[
    Workload {
        name: "robot",
        topology: Topology::Fixture,
        // `imu_link <- map`: three dynamic steps, the chain `docs/PHASE1.md` §11.3 pins the gate to.
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
            // 1 s, not 10: 10 s is 6.4M samples and 460 MB, and ring depth is `scale_sweep`'s axis.
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
/// If no entry has that name; the message lists those that do.
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
    /// Arithmetic, except `Recorded`, which parses the file.
    ///
    /// # Errors
    ///
    /// If a recording cannot be read, or if the implied arena exceeds the `u32`
    /// offset model (`LayoutError::ArenaTooLarge`).
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

        // The row is labelled with the deepest query: a max, not an average.
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
                Some(rate_hz),
            ),
            Topology::Fleet {
                robots,
                history_secs,
            } => fleet_plan(robots, history_secs, self.queries),
        }
    }
}

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
    /// Seconds of history the ring is sized for; carried because `Capacity` rounds to a power of two.
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

    // `ArenaLayout` decides whether this fits; asking here turns a 4 GiB overrun into a message.
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
        #[cfg(all(feature = "shm", target_os = "linux"))]
        Backing::Served(name) => tf_tree::Open::new()
            .name(name)
            .and_then(|o| {
                o.mode(tf_tree::AttachMode::ReadWrite)
                    .create(tf_tree::CreatePolicy::IfAbsent)
                    // The refusal; see `Backing::Served`.
                    .require_create(true)
                    .layout_if_creating(b)
                    .open()
            })
            .map_err(|e| anyhow!("creating and serving the arena {name:?}: {e}")),
        #[cfg(not(all(feature = "shm", target_os = "linux")))]
        Backing::Shared(_) | Backing::Served(_) => {
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
    // Dropping the writers releases every claim; a live publisher re-claims its edges.
    drop(writers);
    Ok(())
}

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
                // Must match `fixture::spin_up`'s seed (index in `DYNAMIC_EDGES`).
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
    // The measured rate only paces a live publisher and fills `nominal_rate_hz`.
    stream_plan(&stream, queries, None)
}

/// Turn a [`TfStream`] into a [`BuildPlan`].
///
/// `rate_hz` is `None` for a recording (rate *measured*, [`median_rate_hz`]) and
/// `Some` for a synthetic stream.
fn stream_plan(stream: &TfStream, queries: QuerySpec, rate_hz: Option<f64>) -> Result<BuildPlan> {
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
        let rate = rate_hz.unwrap_or_else(|| median_rate_hz(&samples));
        // Plus the one slot a ring cannot hand back (`SampleRing::retained`), or the oldest sample is lost.
        let want = u32::try_from(samples.len())
            .unwrap_or(u32::MAX)
            .saturating_add(1);
        let history_secs = if rate > 0.0 {
            samples.len() as f64 / rate
        } else {
            0.0
        };
        dynamics.push(DynEdge {
            parent: p.clone(),
            child: c.clone(),
            rate_hz: rate,
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
        // Static: dynamic would add a step to every cross-fleet query for no modelling gain.
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
                    // Per-robot seed, so robots do not share identical trajectories.
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
/// The intersection, so no query is partly declined (an error path).
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
/// Median, since real recordings contain gaps (`docs/PHASE5.md` §3.2).
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
            // Within one robot would just measure the `robot` row.
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
/// Walked over the parent map, not assumed from naming.
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
        // Acyclic by construction; the bound guards a malformed hand-built plan.
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
    // Raw bound only (`0034`): `depth` counts edges, and `MAX_DEPTH` bounds the
    // plan after static folding, which surfaces in `Built::plans`.
    if depth > tf_tree::MAX_PATH_EDGES {
        bail!(
            "deepest chain is {depth} edges ({leaf} <- {root}) and MAX_PATH_EDGES is {}; \
             no plan can be compiled for it",
            tf_tree::MAX_PATH_EDGES
        );
    }
    Ok((root.to_owned(), leaf.to_owned()))
}

/// SplitMix64, as in `crate::replay`: no `rand` dependency, reproducible draws.
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
    /// Printed next to every harness's numbers (`docs/PHASE1.md` §11.3).
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
