//! The ROS-independent half of the `tf_tree` ingest bridge — `docs/PHASE4.md` §5.
//!
//! The `rclcpp` half (subscriptions, QoS, GIDs) builds only where ROS 2 is
//! installed and lives outside the workspace. This crate holds the **decisions**
//! — publisher authority (§5.4), clock guard (§5.5), name normalization (§5.6),
//! static verification (§5.7) — as pure functions testable on every host
//! (§0.0 sanctions the `TransformStamped`-shaped [`Sample`]).
//!
//! Topology comes from a file, not the wire (§5.8's amendment): the engine has
//! no runtime edge declaration (`docs/decisions/0004`), so [`Ingest`] takes a
//! [`TopologyConfig`]; an undeclared edge is dropped and diagnosed once
//! ([`Action::UndeclaredEdge`]) and `/tf_static` is verified against the
//! declared constant ([`Action::StaticVerified`]). [`Discovery`] produces the file.
//!
//! **It does not publish.** §5.1 is NORMATIVE: the bridge is ingress only.

#![forbid(unsafe_code)]

pub mod authority;
pub mod clock;
pub mod config;
pub mod discover;
mod edgeindex;
mod edgemap;
pub mod ingest;
mod interner;
pub mod names;
pub mod statics;
pub mod stats;

pub use authority::{Authority, AuthorityPolicy, Verdict};
pub use clock::{
    ClockEvidence, ClockGuard, ClockPolicy, ClockVerdict, CommonMode, JumpKind, OffsetTable,
    OnClockReset, SteadyNanos,
};
pub use config::{
    ConfigError, ConfigErrorKind, DomainMismatch, EdgeConfig, EdgeShape, RingSize, TopologyConfig,
};
pub use discover::Discovery;
pub use ingest::{Action, DropReason, HaltReason, Ingest, Topic};
pub use names::{NameError, NameNormalizer, Normalized};
pub use statics::{StaticKind, StaticStore, StaticVerdict};
pub use stats::BridgeStats;

/// A `geometry_msgs/TransformStamped`-shaped plain struct owing nothing to ROS
/// (§0.0). The ROS half converts.
#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    /// The parent frame, exactly as it arrived — **not** normalized, so
    /// [`names::NameNormalizer`] can be tested on raw input.
    pub frame_id: String,
    /// The child frame, likewise raw.
    pub child_frame_id: String,
    /// Stamp, nanoseconds: the publisher's number, in the domain under
    /// suspicion. §5.5 judges it against [`Sample::received`], never against
    /// another publisher's stamp.
    pub stamp_nanos: i64,
    /// When the *local steady clock* said this message arrived. A distinct type
    /// from [`Sample::stamp_nanos`] (see [`SteadyNanos`]); one reading per
    /// **message**, shared by every transform it expands into.
    pub received: SteadyNanos,
    /// `[qw qx qy qz tx ty tz]`, the canonical order (`docs/PHASE1.md` §3.1).
    pub pose: [f64; 7],
}

impl Sample {
    /// A sample with an identity rotation at `t`. [`Sample::received`] is left
    /// at [`SteadyNanos::UNKNOWN`] (the common-mode layer is then absent for it);
    /// chain [`Sample::received_at`] to supply one.
    #[must_use]
    pub fn identity(frame_id: &str, child_frame_id: &str, stamp_nanos: i64) -> Sample {
        Sample {
            frame_id: frame_id.to_string(),
            child_frame_id: child_frame_id.to_string(),
            stamp_nanos,
            received: SteadyNanos::UNKNOWN,
            pose: [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        }
    }

    /// The same sample, with the steady-clock reading of its message's arrival.
    #[must_use]
    pub fn received_at(mut self, received: SteadyNanos) -> Sample {
        self.received = received;
        self
    }

    /// The `(parent, child)` pair this sample addresses; the bridge keys every
    /// table on it.
    #[must_use]
    pub fn edge(&self) -> (&str, &str) {
        (self.frame_id.as_str(), self.child_frame_id.as_str())
    }
}

/// Who published a sample, as far as the middleware could tell (§5.3).
///
/// Attribution degrades across RMWs and is diagnostic, never a correctness
/// dependency. **The identity is the GID; the name is decoration**:
/// [`Publisher::Gid`] carries both, and `PartialEq`, `Ord` and `Hash` read `id`
/// alone. `rmw_fastrtps` reports `_NODE_NAME_UNKNOWN_` and later the real name;
/// keying on the name made one publisher two and, under
/// [`AuthorityPolicy::FirstWriterWins`], rejected the corrected name forever.
///
/// A GID with no name is still a distinct publisher. [`Publisher::Unattributed`]
/// (no GID reported) collapses to one identity by design: fewer identifiable
/// publishers must mean less detection, never more stopping
/// ([`0012`](../../../docs/decisions/0012-the-authoritative-clock-jump-signal-and-the-degradation-ladder.md)).
#[derive(Clone, Debug)]
pub enum Publisher {
    /// A middleware publisher, identified by the GID the RMW reported for it.
    Gid {
        /// The stable identity: the GID rendered as `<gid:…>` once, so
        /// `crate::ingest::owner_key` can return a borrow per sample.
        id: Box<str>,
        /// The node name once the graph resolved one. Presentation only.
        name: Option<String>,
    },
    /// A publisher identified by the **topic** it published on: offline ingest
    /// has no GID, and a topic is stable for a recording. Cannot collide with a
    /// bracketed key: a ROS topic name cannot contain `<`.
    Topic(String),
    /// The middleware reported no GID at all.
    Unattributed,
    /// **The topology config file**, the incumbent owner of every static edge's
    /// value (§5.8's amendment); not a publisher.
    Declared,
}

impl Publisher {
    /// A publisher known by its GID and nothing else.
    #[must_use]
    pub fn from_gid(gid: &[u8; 16]) -> Publisher {
        Publisher::Gid {
            id: render_gid(gid),
            name: None,
        }
    }

    /// A publisher known by its GID, with the name the graph resolved.
    #[must_use]
    pub fn named(gid: &[u8; 16], name: &str) -> Publisher {
        Publisher::Gid {
            id: render_gid(gid),
            name: Some(name.to_owned()),
        }
    }

    /// Attach or replace the presentation name, leaving the identity alone.
    pub fn set_name(&mut self, new_name: &str) {
        if let Publisher::Gid { name, .. } = self {
            *name = Some(new_name.to_owned());
        }
    }

    /// The stable identity, as the string `crate::ingest::owner_key` returns.
    #[must_use]
    pub fn key(&self) -> &str {
        match self {
            Publisher::Gid { id, .. } => id,
            Publisher::Topic(t) => t,
            // Bracketed: a ROS name cannot contain `<`.
            Publisher::Unattributed => "<unattributed>",
            Publisher::Declared => "<declared>",
        }
    }
}

/// A deterministic GID derived from a name — **test scaffolding**, `pub` because
/// integration tests and examples are separate crates. Real GIDs come from
/// `rmw_message_info_t::publisher_gid`.
///
/// Real code never calls this. A real GID comes from
/// `rmw_message_info_t::publisher_gid`, and the point of [`Publisher`]'s shape
/// is that the GID is what the middleware said, not something derived from a
/// name that can change.
#[doc(hidden)]
#[must_use]
pub fn gid_for_name(name: &str) -> [u8; 16] {
    // FNV-1a, splatted across 16 bytes.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let mut gid = [0u8; 16];
    gid[..8].copy_from_slice(&h.to_le_bytes());
    gid[8..].copy_from_slice(&h.rotate_left(32).to_be_bytes());
    gid
}

/// `<gid:` + 32 lowercase hex digits + `>`.
fn render_gid(gid: &[u8; 16]) -> Box<str> {
    let mut s = String::with_capacity(38);
    s.push_str("<gid:");
    for b in gid {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s.push('>');
    s.into_boxed_str()
}

// By identity ONLY, hand-written so a derive cannot silently include `name`.
impl PartialEq for Publisher {
    fn eq(&self, other: &Publisher) -> bool {
        self.key() == other.key()
    }
}

impl Eq for Publisher {}

impl PartialOrd for Publisher {
    fn partial_cmp(&self, other: &Publisher) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Publisher {
    fn cmp(&self, other: &Publisher) -> core::cmp::Ordering {
        self.key().cmp(other.key())
    }
}

impl core::hash::Hash for Publisher {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.key().hash(state);
    }
}

impl core::fmt::Display for Publisher {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Publisher::Gid {
                name: Some(name), ..
            } => write!(f, "{name}"),
            // The full key: endpoint GIDs from one process share a prefix.
            Publisher::Gid { id, .. } => write!(f, "{id}"),
            Publisher::Topic(t) => write!(f, "{t}"),
            Publisher::Unattributed => write!(f, "<unattributed>"),
            Publisher::Declared => write!(f, "<topology config>"),
        }
    }
}
