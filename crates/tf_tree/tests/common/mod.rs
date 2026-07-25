//! Shared helpers for the `tf_tree` facade integration tests.
//!
//! Included via `mod common;` in each test binary; not every helper is used by
//! every binary, and the `pub` surface is only "reachable" within a given test
//! crate, so both `dead_code` and `unreachable_pub` are expected here.
#![allow(dead_code, unreachable_pub)]

use tf_tree::{
    exp_se3, log_so3, Capacity, EdgeCfg, FrameId, Iso3, Publisher, Stamp, SystemDomain, Tree,
    TreeBuilder,
};

/// A `SystemDomain` stamp — a terse constructor whose return type pins the domain
/// (the bare `Stamp::from_nanos` cannot infer the default when passed to a
/// generic function).
#[must_use]
pub fn ns(nanos: i64) -> Stamp<SystemDomain> {
    Stamp::from_nanos(nanos)
}

/// A deterministic, well-conditioned pose from an integer seed.
#[must_use]
pub fn pose(seed: u64) -> Iso3 {
    let f = seed as f64;
    exp_se3([0.03 * f, -0.02 * f, 0.017 * f, 0.5 * f, -0.25 * f, 0.1 * f])
}

/// Scale-aware closeness metric: max of absolute rotation error (radians) and
/// translation error relative to the poses' scale.
#[must_use]
pub fn max_err(a: Iso3, b: Iso3) -> f64 {
    let rot = log_so3(a.q.conjugate() * b.q).norm();
    let scale = 1.0 + a.t.norm().max(b.t.norm());
    let trans = a.t.sub(b.t).norm() / scale;
    rot.max(trans)
}

/// Assert two poses agree within `tol` under [`max_err`].
pub fn assert_close(a: Iso3, b: Iso3, tol: f64, ctx: &str) {
    let e = max_err(a, b);
    assert!(e <= tol, "{ctx}: max_err {e:e} > {tol:e}");
}

/// A three-frame chain `map -> odom -> base` with two dynamic edges, each with a
/// series of samples published at `dt`-spaced stamps. Returns the tree plus the
/// three frame ids.
pub struct Chain {
    pub tree: Tree,
    pub map: FrameId,
    pub odom: FrameId,
    pub base: FrameId,
    /// Poses of the `map->odom` edge, indexed by sample.
    pub map_odom: Vec<Iso3>,
    /// Poses of the `odom->base` edge, indexed by sample.
    pub odom_base: Vec<Iso3>,
    /// Stamp of sample `i` (nanoseconds), shared by both edges.
    pub dt: i64,
    /// Number of samples per edge.
    pub n: usize,
}

impl Chain {
    #[must_use]
    pub fn new(n: usize, dt: i64) -> Chain {
        let cfg = EdgeCfg::new(Capacity::slots(1024));
        let tree = TreeBuilder::new()
            .dynamic_edge("map", "odom", cfg)
            .dynamic_edge("odom", "base", cfg)
            .build()
            .expect("build");
        let map = tree.frame("map").unwrap();
        let odom = tree.frame("odom").unwrap();
        let base = tree.frame("base").unwrap();

        let map_odom: Vec<Iso3> = (0..n).map(|i| pose(i as u64 + 1)).collect();
        let odom_base: Vec<Iso3> = (0..n).map(|i| pose(i as u64 + 100)).collect();
        {
            let w_mo = tree.claim(odom, map).unwrap();
            for (i, p) in map_odom.iter().enumerate() {
                w_mo.push(i as i64 * dt, p).unwrap();
            }
            let w_ob = tree.claim(base, odom).unwrap();
            for (i, p) in odom_base.iter().enumerate() {
                w_ob.push(i as i64 * dt, p).unwrap();
            }
        }
        Chain {
            tree,
            map,
            odom,
            base,
            map_odom,
            odom_base,
            dt,
            n,
        }
    }
}

/// Publish a full sample stream onto a claimed edge (helper for ad-hoc trees).
pub fn publish(w: &Publisher, dt: i64, poses: &[Iso3]) {
    for (i, p) in poses.iter().enumerate() {
        w.push(i as i64 * dt, p).unwrap();
    }
}

/// Ground-truth record of one edge for the [`Robot`] fixture.
struct EdgeGt {
    child: FrameId,
    parent: FrameId,
    dynamic: bool,
    /// For a static edge, one constant pose; for a dynamic edge, one per sample.
    poses: Vec<Iso3>,
}

/// A robot-shaped fixture (24-ish frames, mixed static + dynamic edges) with a
/// naive ground-truth reference that composes edge poses without plan folding.
/// This is the reference the plan-evaluation and folding proptests check against.
pub struct Robot {
    pub tree: Tree,
    frames: Vec<(&'static str, FrameId)>,
    edges: Vec<EdgeGt>,
    /// Number of dynamic samples per edge.
    pub n: usize,
    /// Nanoseconds between samples.
    pub dt: i64,
}

impl Robot {
    /// Build the fixture with `n` samples per dynamic edge spaced by `dt` ns.
    #[must_use]
    pub fn new(n: usize, dt: i64) -> Robot {
        let names = [
            "map",
            "odom",
            "base",
            "imu",
            "lidar_mount",
            "lidar",
            "cam_mount",
            "cam",
            "cam_opt",
        ];

        // (child, parent, dynamic, pose-seed). Depth up to map->odom->base->cam_mount->cam->cam_opt.
        let spec: &[(&str, &str, bool, u64)] = &[
            ("odom", "map", true, 1),
            ("base", "odom", true, 2),
            ("imu", "base", false, 3),
            ("lidar_mount", "base", false, 4),
            ("lidar", "lidar_mount", true, 5),
            ("cam_mount", "base", false, 6),
            ("cam", "cam_mount", false, 7),
            ("cam_opt", "cam", false, 8),
        ];

        // Declare the whole topology on the builder; the arena is sized from it.
        let mut builder = TreeBuilder::new();
        for &(child, parent, dynamic, seed) in spec {
            builder = if dynamic {
                builder.dynamic_edge(parent, child, EdgeCfg::new(Capacity::slots(1024)))
            } else {
                builder.static_edge(parent, child, &pose(seed * 10))
            };
        }
        let tree = builder.build().expect("build");

        let frames: Vec<(&'static str, FrameId)> = names
            .iter()
            .map(|&nm| (nm, tree.frame(nm).unwrap()))
            .collect();
        let id = |nm: &str| frames.iter().find(|(n, _)| *n == nm).unwrap().1;

        // Record ground truth and publish samples onto the dynamic edges.
        let mut edges = Vec::new();
        for &(child, parent, dynamic, seed) in spec {
            let (c, p) = (id(child), id(parent));
            if !dynamic {
                edges.push(EdgeGt {
                    child: c,
                    parent: p,
                    dynamic: false,
                    poses: std::vec![pose(seed * 10)],
                });
                continue;
            }
            let poses: Vec<Iso3> = (0..n).map(|i| pose(seed * 1000 + i as u64)).collect();
            {
                let w = tree.claim(c, p).unwrap();
                publish(&w, dt, &poses);
            }
            edges.push(EdgeGt {
                child: c,
                parent: p,
                dynamic: true,
                poses,
            });
        }

        Robot {
            tree,
            frames,
            edges,
            n,
            dt,
        }
    }

    /// All frame ids in the fixture.
    #[must_use]
    pub fn frame_ids(&self) -> Vec<FrameId> {
        self.frames.iter().map(|(_, f)| *f).collect()
    }

    /// Ground-truth `T_root_frame` at sample index `i`, composed naively up the
    /// parent chain (no plan folding).
    #[must_use]
    pub fn root_transform(&self, frame: FrameId, i: usize) -> Iso3 {
        let mut chain: Vec<&EdgeGt> = Vec::new();
        let mut cur = frame;
        while let Some(e) = self.edges.iter().find(|e| e.child == cur) {
            chain.push(e);
            cur = e.parent;
        }
        // chain[0] is `frame`'s own edge, chain.last() is nearest the root.
        // T_root_frame = T_root_top * ... * T_parent_frame -> multiply root-first.
        let mut acc = Iso3::IDENTITY;
        for e in chain.iter().rev() {
            let p = if e.dynamic { e.poses[i] } else { e.poses[0] };
            acc = acc * p;
        }
        acc
    }

    /// Ground-truth `T_target_source` at sample index `i`:
    /// `T_root_target⁻¹ · T_root_source`.
    #[must_use]
    pub fn transform(&self, target: FrameId, source: FrameId, i: usize) -> Iso3 {
        self.root_transform(target, i).inverse() * self.root_transform(source, i)
    }
}
