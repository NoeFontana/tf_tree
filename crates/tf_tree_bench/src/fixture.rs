//! The shared mobile-robot fixture tree (decision `0003`, *Benchmarks*).
//!
//! A single 24-frame tree, max depth 6, shaped like a real mobile robot: four
//! dynamic edges (`map→odom` @ 50 Hz, `odom→base_link` @ 200 Hz,
//! `base_link→imu_link` @ 1 kHz, `lidar_mount→lidar` @ 10 Hz) and 19 static edges.
//! Every dynamic ring is sized (via [`Capacity::history`]) to retain
//! [`HISTORY_SECS`] seconds at its own rate.
//!
//! The same [`EDGES`] table drives the criterion benches, the CLI demo, the tf2
//! differential harness, and the doctor's healthy-tree tests, so they never drift
//! apart. All builders return `Result`; the crate proper holds no `unwrap`/`panic`
//! (the workspace lints forbid them), and callers that genuinely cannot fail
//! (benches, tests) surface the error at their own boundary.

use anyhow::{anyhow, Result};

use tf_tree::{Capacity, EdgeCfg, InterpPolicy, Iso3, Publisher, Tree, TreeBuilder};
use tf_tree_math::exp_se3;

/// Seconds of history every dynamic ring is sized to retain.
pub const HISTORY_SECS: f64 = 10.0;

/// A query stamp that lies inside *every* dynamic edge's retained window.
///
/// The lidar edge (10 Hz, 100 samples over 10 s) has the shortest reach; its
/// newest stamp is `9.9 s`. Picking `9.9 s` as "now" keeps `At(now)` and
/// `At(now − 100 ms)` valid on all four edges without extrapolation.
pub const NOW_NS: i64 = 9_900_000_000;

/// What an [`EdgeDef`] describes.
#[derive(Clone, Copy, Debug)]
pub enum EdgeDefKind {
    /// A static edge whose constant pose is `exp_se3(xi)` (`T_parent_child`).
    Static {
        /// The SE(3) twist whose exponential is the constant pose.
        xi: [f64; 6],
    },
    /// A dynamic edge published at `rate_hz`; its ring holds [`HISTORY_SECS`] of it.
    Dynamic {
        /// Nominal publish rate, in hertz.
        rate_hz: f64,
    },
}

/// One declared edge of the fixture: `parent → child` of a given kind.
#[derive(Clone, Copy, Debug)]
pub struct EdgeDef {
    /// Parent frame name.
    pub parent: &'static str,
    /// Child frame name (the edge stores `T_parent_child`).
    pub child: &'static str,
    /// Static or dynamic, with its parameters.
    pub kind: EdgeDefKind,
}

/// A small static translation-only pose (a mount offset), as an SE(3) twist.
const fn offset(x: f64, y: f64, z: f64) -> EdgeDefKind {
    EdgeDefKind::Static {
        xi: [0.0, 0.0, 0.0, x, y, z],
    }
}

/// A static pose with a rotation (`rz` about z) and a translation.
const fn mount(rz: f64, x: f64, y: f64, z: f64) -> EdgeDefKind {
    EdgeDefKind::Static {
        xi: [0.0, 0.0, rz, x, y, z],
    }
}

/// The 23 edges of the fixture (4 dynamic + 19 static), over 24 frames.
///
/// The longest chain — `map → odom → base_link → sensor_arch → camera_mount →
/// camera_link → camera_optical` — is six edges deep, matching the spec's "max
/// depth 6".
pub const EDGES: &[EdgeDef] = &[
    // --- kinematic spine (dynamic) --------------------------------------
    EdgeDef {
        parent: "map",
        child: "odom",
        kind: EdgeDefKind::Dynamic { rate_hz: 50.0 },
    },
    EdgeDef {
        parent: "odom",
        child: "base_link",
        kind: EdgeDefKind::Dynamic { rate_hz: 200.0 },
    },
    EdgeDef {
        parent: "base_link",
        child: "imu_link",
        kind: EdgeDefKind::Dynamic { rate_hz: 1000.0 },
    },
    EdgeDef {
        parent: "lidar_mount",
        child: "lidar",
        kind: EdgeDefKind::Dynamic { rate_hz: 10.0 },
    },
    // --- chassis (static) -----------------------------------------------
    EdgeDef {
        parent: "base_link",
        child: "base_footprint",
        kind: offset(0.0, 0.0, -0.15),
    },
    EdgeDef {
        parent: "base_link",
        child: "sensor_arch",
        kind: offset(0.10, 0.0, 0.40),
    },
    EdgeDef {
        parent: "base_footprint",
        child: "caster_wheel",
        kind: offset(-0.25, 0.0, -0.05),
    },
    EdgeDef {
        parent: "base_link",
        child: "left_wheel",
        kind: offset(0.0, 0.20, -0.10),
    },
    EdgeDef {
        parent: "base_link",
        child: "right_wheel",
        kind: offset(0.0, -0.20, -0.10),
    },
    EdgeDef {
        parent: "base_link",
        child: "gps_link",
        kind: offset(-0.10, 0.0, 0.30),
    },
    EdgeDef {
        parent: "base_link",
        child: "battery_link",
        kind: offset(-0.05, 0.0, 0.02),
    },
    EdgeDef {
        parent: "base_link",
        child: "compute_box",
        kind: offset(0.05, 0.0, 0.10),
    },
    EdgeDef {
        parent: "base_link",
        child: "sonar_front",
        kind: offset(0.30, 0.0, 0.05),
    },
    EdgeDef {
        parent: "base_link",
        child: "sonar_rear",
        kind: mount(core::f64::consts::PI, -0.30, 0.0, 0.05),
    },
    EdgeDef {
        parent: "base_link",
        child: "rear_camera_link",
        kind: mount(core::f64::consts::PI, -0.28, 0.0, 0.20),
    },
    // --- sensor arch (static) -------------------------------------------
    EdgeDef {
        parent: "sensor_arch",
        child: "lidar_mount",
        kind: offset(0.0, 0.0, 0.08),
    },
    EdgeDef {
        parent: "sensor_arch",
        child: "camera_mount",
        kind: offset(0.06, 0.0, 0.02),
    },
    EdgeDef {
        parent: "camera_mount",
        child: "camera_link",
        kind: offset(0.02, 0.0, 0.01),
    },
    EdgeDef {
        parent: "camera_link",
        child: "camera_optical",
        kind: mount(-core::f64::consts::FRAC_PI_2, 0.0, 0.0, 0.0),
    },
    EdgeDef {
        parent: "sensor_arch",
        child: "antenna_mount",
        kind: offset(-0.04, 0.0, 0.06),
    },
    EdgeDef {
        parent: "antenna_mount",
        child: "antenna",
        kind: offset(0.0, 0.0, 0.15),
    },
    EdgeDef {
        parent: "sensor_arch",
        child: "thermal_camera_mount",
        kind: offset(0.06, 0.05, 0.02),
    },
    EdgeDef {
        parent: "thermal_camera_mount",
        child: "thermal_camera",
        kind: offset(0.01, 0.0, 0.0),
    },
];

/// The four dynamic edges as `(parent, child, rate_hz)`, in declaration order.
pub const DYNAMIC_EDGES: &[(&str, &str, f64)] = &[
    ("map", "odom", 50.0),
    ("odom", "base_link", 200.0),
    ("base_link", "imu_link", 1000.0),
    ("lidar_mount", "lidar", 10.0),
];

/// Every frame name in the fixture (first-seen order over [`EDGES`]).
#[must_use]
pub fn frame_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = Vec::new();
    for e in EDGES {
        if !names.contains(&e.parent) {
            names.push(e.parent);
        }
        if !names.contains(&e.child) {
            names.push(e.child);
        }
    }
    names
}

/// Build the fixture tree with the default `ScLerp` interpolation policy.
///
/// # Errors
///
/// Propagates any [`tf_tree::BuildError`] (unreachable for this fixed topology,
/// but surfaced rather than unwrapped).
pub fn build_tree() -> Result<Tree> {
    build_tree_with(InterpPolicy::ScLerp)
}

/// Build the fixture tree with a chosen default interpolation policy.
///
/// The tf2 differential harness uses [`InterpPolicy::LerpSlerp`] (tf2's policy);
/// the benches use the `ScLerp` default.
///
/// # Errors
///
/// Propagates any [`tf_tree::BuildError`].
pub fn build_tree_with(interp: InterpPolicy) -> Result<Tree> {
    let mut b = TreeBuilder::new().default_interp(interp);
    for e in EDGES {
        b = match e.kind {
            EdgeDefKind::Static { xi } => b.static_edge(e.parent, e.child, &exp_se3(xi)),
            EdgeDefKind::Dynamic { rate_hz } => b.dynamic_edge(
                e.parent,
                e.child,
                EdgeCfg::new(Capacity::history(rate_hz, HISTORY_SECS)),
            ),
        };
    }
    b.build().map_err(|e| anyhow!("build fixture tree: {e}"))
}

/// A deterministic synthetic pose for dynamic edge `seed` at nanosecond `stamp`.
///
/// A smooth, bounded screw motion (`exp_se3` of a slowly time-varying twist), so
/// interpolation between adjacent samples is well conditioned and the reference /
/// engine agreement in the differential harness is meaningful.
#[must_use]
pub fn dynamic_pose(seed: f64, stamp_ns: i64) -> Iso3 {
    let t = stamp_ns as f64 * 1e-9;
    let xi = [
        0.20 * (0.7 * t + seed).sin(),
        0.10 * (0.5 * t + 0.3 * seed).cos(),
        0.05 * (1.1 * t + 0.7 * seed).sin(),
        (0.5 + 0.1 * seed) * (0.9 * t + seed).sin(),
        0.30 * (0.6 * t + 0.5 * seed).cos(),
        0.20 * (0.4 * t + 0.2 * seed).sin(),
    ];
    exp_se3(xi)
}

/// One recorded publish, in arrival order — the observed-history input the
/// `doctor` diagnostics consume (multi-writer, inconsistent-rate, out-of-order,
/// short-buffer checks all read this stream).
#[derive(Clone, Copy, Debug)]
pub struct PushSample {
    /// The edge id the sample was published to.
    pub edge: u32,
    /// The writer's process id (single-writer under the claim protocol).
    pub writer_pid: u32,
    /// The sample's timestamp, in nanoseconds.
    pub stamp_ns: i64,
    /// How late this sample arrived relative to its stamp (publish latency), in
    /// nanoseconds. The fixture models one nominal period of lateness.
    pub arrival_delay_ns: i64,
}

/// Claim every dynamic edge and publish [`HISTORY_SECS`] of synthetic history,
/// returning the live [`Publisher`]s (claims stay held while they live) and the
/// recorded push stream.
///
/// Keeping the publishers alive is what lets the CLI `tree`/`doctor` demo show
/// *claimed* edges with a live writer PID; drop them to release the claims.
///
/// # Errors
///
/// If a frame is unknown, an edge cannot be claimed, or a push is rejected — none
/// of which can happen for the fixed fixture, but all surfaced rather than
/// unwrapped.
pub fn spin_up(tree: &Tree) -> Result<(Vec<Publisher<'_>>, Vec<PushSample>)> {
    let pid = std::process::id();
    let mut writers = Vec::new();
    let mut samples = Vec::new();
    for (seed, &(parent_name, child_name, rate_hz)) in DYNAMIC_EDGES.iter().enumerate() {
        let parent = tree
            .frame(parent_name)
            .map_err(|e| anyhow!("frame {parent_name}: {e:?}"))?;
        let child = tree
            .frame(child_name)
            .map_err(|e| anyhow!("frame {child_name}: {e:?}"))?;
        let publisher = tree
            .claim(child, parent)
            .map_err(|e| anyhow!("claim {parent_name}->{child_name}: {e}"))?;
        let edge = publisher.edge().get();
        let period_ns = (1e9 / rate_hz) as i64;
        let count = (HISTORY_SECS * rate_hz) as i64;
        for k in 0..count {
            let stamp = k * period_ns;
            let iso = dynamic_pose(seed as f64, stamp);
            publisher
                .push(stamp, &iso)
                .map_err(|e| anyhow!("push {child_name}@{stamp}: {e:?}"))?;
            samples.push(PushSample {
                edge,
                writer_pid: pid,
                stamp_ns: stamp,
                arrival_delay_ns: period_ns,
            });
        }
        writers.push(publisher);
    }
    Ok((writers, samples))
}

/// Build the fixture and pre-populate its history, dropping the publishers (the
/// benches read against the populated rings and do not need the claims held).
///
/// # Errors
///
/// Propagates [`build_tree`] / [`spin_up`] errors.
pub fn populated_tree() -> Result<(Tree, Vec<PushSample>)> {
    let tree = build_tree()?;
    let samples = {
        let (writers, samples) = spin_up(&tree)?;
        drop(writers);
        samples
    };
    Ok((tree, samples))
}
