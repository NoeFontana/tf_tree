//! The ROS-independent half of the `tf_tree` ingest bridge — `docs/PHASE4.md` §5.
//!
//! The decisions — authority (§5.4), clock guard (§5.5), names (§5.6), statics
//! (§5.7) — as pure functions. Topology comes from a [`TopologyConfig`] file
//! (§5.8, `docs/decisions/0004`), which [`Discovery`] produces.
//!
//! **It does not publish** (§5.1, NORMATIVE): ingress only.

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

/// A `geometry_msgs/TransformStamped`-shaped plain struct (§0.0).
#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    /// The parent frame, as it arrived (not normalized).
    pub frame_id: String,
    /// The child frame, likewise raw.
    pub child_frame_id: String,
    /// Stamp, nanoseconds, in the publisher's domain. §5.5 judges it against
    /// [`Sample::received`].
    pub stamp_nanos: i64,
    /// Local steady-clock arrival time; one reading per message.
    pub received: SteadyNanos,
    /// `[qw qx qy qz tx ty tz]`, the canonical order (`docs/PHASE1.md` §3.1).
    pub pose: [f64; 7],
}

impl Sample {
    /// An identity-rotation sample; [`Sample::received`] is [`SteadyNanos::UNKNOWN`].
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

    /// The same sample with its arrival reading.
    #[must_use]
    pub fn received_at(mut self, received: SteadyNanos) -> Sample {
        self.received = received;
        self
    }

    /// The `(parent, child)` pair this sample addresses.
    #[must_use]
    pub fn edge(&self) -> (&str, &str) {
        (self.frame_id.as_str(), self.child_frame_id.as_str())
    }
}

/// Who published a sample, as far as the middleware could tell (§5.3).
///
/// Attribution is diagnostic, never a correctness dependency. **The identity is
/// the GID; the name is decoration**: `PartialEq`, `Ord` and `Hash` read `id`
/// alone, because a name can change under [`AuthorityPolicy::FirstWriterWins`].
///
/// [`Publisher::Unattributed`] collapses to one identity by design: less
/// attribution means less detection, never more stopping
/// ([`0012`](../../../docs/decisions/0012-the-authoritative-clock-jump-signal-and-the-degradation-ladder.md)).
#[derive(Clone, Debug)]
pub enum Publisher {
    /// A middleware publisher, identified by its GID.
    Gid {
        /// The GID rendered as `<gid:…>` once, so `owner_key` can borrow it.
        id: Box<str>,
        /// The node name once the graph resolved one. Presentation only.
        name: Option<String>,
    },
    /// Identified by topic (offline ingest has no GID). Cannot collide with a
    /// bracketed key: a ROS topic name cannot contain `<`.
    Topic(String),
    /// The middleware reported no GID at all.
    Unattributed,
    /// The topology config file, incumbent owner of every static edge's value
    /// (§5.8); not a publisher.
    Declared,
}

impl Publisher {
    /// A publisher known by its GID.
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

    /// The stable identity.
    #[must_use]
    pub fn key(&self) -> &str {
        match self {
            Publisher::Gid { id, .. } => id,
            Publisher::Topic(t) => t,
            Publisher::Unattributed => "<unattributed>",
            Publisher::Declared => "<declared>",
        }
    }
}

/// A deterministic GID derived from a name — test scaffolding only.
#[doc(hidden)]
#[must_use]
pub fn gid_for_name(name: &str) -> [u8; 16] {
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

// Identity only; hand-written so a derive cannot include `name`.
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
            Publisher::Gid { id, .. } => write!(f, "{id}"),
            Publisher::Topic(t) => write!(f, "{t}"),
            Publisher::Unattributed => write!(f, "<unattributed>"),
            Publisher::Declared => write!(f, "<topology config>"),
        }
    }
}
