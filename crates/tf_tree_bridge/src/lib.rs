//! The ROS-independent half of the `tf_tree` ingest bridge — `docs/PHASE4.md`
//! §5.
//!
//! # Why this is a crate and not a module of the ROS node
//!
//! §5 has two halves and they have opposite build requirements.
//!
//! The half that needs `rclcpp` is subscriptions, QoS, executors and publisher
//! GIDs (§5.2, §5.3, §5.8, §5.9's plumbing). It builds only where ROS 2 is
//! installed, so — like `tf_tree_tf2_sys` — it must sit outside the workspace,
//! which is exactly why `tf_tree_tf2_sys` is *"the one crate in the repo
//! carrying `unsafe` with no lint coverage"* by its own admission.
//!
//! The other half is **decisions**: which publisher owns an edge (§5.4), whether
//! the clock went backwards (§5.5), what a frame name normalizes to (§5.6),
//! whether a repeated static transform is the same one (§5.7). None of it needs
//! a middleware. All of it is where the interesting bugs are, because all of it
//! encodes judgments about somebody else's misconfigured robot.
//!
//! §0.0 sanctions this split explicitly: a `TransformStamped`-shaped plain
//! struct *is* legitimate for these sections. So they live here, under `just
//! test` and `just lint`, on every host.
//!
//! # Topology comes from a file, not from the wire (§5.8's amendment)
//!
//! The engine has **no runtime edge declaration**: `declare_edge` is build-time
//! only and `edge_headroom` reserves zero-capacity slots no API can fill
//! (`docs/decisions/0004`, D4). So [`Ingest`] takes a [`TopologyConfig`] up
//! front and everything else follows from it — a transform for an undeclared
//! edge is dropped, counted and diagnosed once naming both frames
//! ([`Action::UndeclaredEdge`]), and `/tf_static` is *verified against the
//! declared constant* ([`Action::StaticVerified`]) rather than declaring
//! anything. [`Discovery`] is how an operator obtains the file in the first
//! place.
//!
//! # What this deliberately does not do
//!
//! **It does not publish.** §5.1 is NORMATIVE: the bridge is ingress only. One
//! direction removes every loopback, echo and authority-cycle question from the
//! phase, and it is all dogfooding needs — new nodes read from `tf_tree`, the
//! existing ones keep publishing to `/tf` unchanged.

#![forbid(unsafe_code)]

pub mod authority;
pub mod clock;
pub mod config;
pub mod discover;
pub mod ingest;
pub mod names;
pub mod statics;
pub mod stats;

pub use authority::{Authority, AuthorityPolicy, Verdict};
pub use clock::{ClockGuard, ClockVerdict, OnClockReset};
pub use config::{
    ConfigError, ConfigErrorKind, DomainMismatch, EdgeConfig, EdgeShape, RingSize, TopologyConfig,
};
pub use discover::Discovery;
pub use ingest::{Action, DropReason, HaltReason, Ingest, Topic};
pub use names::{NameError, NameNormalizer, Normalized};
pub use statics::{StaticKind, StaticStore, StaticVerdict};
pub use stats::BridgeStats;

/// A `geometry_msgs/TransformStamped`, shaped like the real one but owing
/// nothing to ROS.
///
/// §0.0: *"a `TransformStamped`-shaped plain struct is legitimate for §5.4,
/// §5.5, §5.6 and §5.7, which are pure functions"*. The point is not to avoid
/// the dependency for its own sake — it is that a decision function tested
/// against a struct is tested on every host, and one tested against a
/// subscription is tested in a container that the normal loop does not run.
///
/// The bridge's ROS half converts; nothing here knows how.
#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    /// The parent frame, exactly as it arrived — **not** normalized. Passing
    /// raw names in is deliberate: [`names::NameNormalizer`] is the thing under
    /// test, and a struct that could only hold normalized names would make it
    /// impossible to test what happens to the others.
    pub frame_id: String,
    /// The child frame, likewise raw.
    pub child_frame_id: String,
    /// Stamp, nanoseconds in whichever domain the bridge is running.
    pub stamp_nanos: i64,
    /// `[qw qx qy qz tx ty tz]`, the canonical order (`docs/PHASE1.md` §3.1).
    pub pose: [f64; 7],
}

impl Sample {
    /// A sample with an identity rotation at `t`, for tests and for callers
    /// building one by hand.
    #[must_use]
    pub fn identity(frame_id: &str, child_frame_id: &str, stamp_nanos: i64) -> Sample {
        Sample {
            frame_id: frame_id.to_string(),
            child_frame_id: child_frame_id.to_string(),
            stamp_nanos,
            pose: [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        }
    }

    /// The `(parent, child)` pair this sample addresses, after normalization is
    /// assumed to have happened. The bridge keys every table on this.
    #[must_use]
    pub fn edge(&self) -> (&str, &str) {
        (self.frame_id.as_str(), self.child_frame_id.as_str())
    }
}

/// Who published a sample, as far as the middleware could tell.
///
/// §5.3: `TFMessage` carries no publisher identity, but the middleware does —
/// `rmw_message_info_t::publisher_gid` matched against
/// `get_publishers_info_by_topic`. **And it degrades:** GID reporting varies
/// across RMW implementations, so a failure to attribute must not be a failure
/// to run. Attribution is diagnostic value, never a correctness dependency.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Publisher {
    /// A node name resolved from the graph, e.g. `/ekf`.
    Node(String),
    /// The GID was reported but matched no node — the graph moved, or the RMW
    /// reports GIDs that its own graph API does not.
    UnknownGid,
    /// No GID at all. Distinct from [`Publisher::UnknownGid`] on purpose: one
    /// says the middleware could not tell us, the other says it told us
    /// something we could not use, and only the second is a graph-cache bug
    /// worth chasing.
    Unattributed,
    /// **The topology config file**, which is not a publisher at all.
    ///
    /// It is here because `docs/PHASE4.md` §5.8's amendment makes the config
    /// the incumbent owner of every static edge's value: `/tf_static` is
    /// verified against it, and a disagreement is §5.7's conflict with the file
    /// on one side. A diagnostic that named `<unattributed>` there would send
    /// an operator looking for a node.
    Declared,
}

impl core::fmt::Display for Publisher {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Publisher::Node(n) => write!(f, "{n}"),
            Publisher::UnknownGid => write!(f, "<unknown publisher>"),
            Publisher::Unattributed => write!(f, "<unattributed>"),
            Publisher::Declared => write!(f, "<topology config>"),
        }
    }
}
