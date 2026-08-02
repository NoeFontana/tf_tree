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
    ///
    /// **The publisher's number, in the domain under suspicion.** Everything
    /// §5.5 judges about the clock is judged against
    /// [`Sample::received`], never against another publisher's stamp.
    pub stamp_nanos: i64,
    /// When the *local steady clock* said this message arrived.
    ///
    /// A different type from [`Sample::stamp_nanos`] because confusing the two
    /// is the whole bug class `docs/PHASE4.md` §5.5's detector kept falling into
    /// — see [`SteadyNanos`], which also documents where a caller gets one and
    /// what [`SteadyNanos::UNKNOWN`] costs.
    ///
    /// One reading per **message**, shared by every transform the message
    /// expands into. [`Sample::identity`] leaves it unknown;
    /// [`Sample::received_at`] sets it.
    pub received: SteadyNanos,
    /// `[qw qx qy qz tx ty tz]`, the canonical order (`docs/PHASE1.md` §3.1).
    pub pose: [f64; 7],
}

impl Sample {
    /// A sample with an identity rotation at `t`, for tests and for callers
    /// building one by hand.
    ///
    /// [`Sample::received`] is left at [`SteadyNanos::UNKNOWN`], which is the
    /// honest answer for a caller that has not said otherwise: the common-mode
    /// layer is then simply absent for this sample rather than fed a fiction.
    /// Chain [`Sample::received_at`] to supply one.
    ///
    /// The three-argument shape is deliberate. Fifty-odd construction sites in
    /// this repository route through it, and every one of them would have had to
    /// invent a receipt time — which is exactly the pressure that ends with
    /// somebody passing `stamp_nanos` and silently re-enabling inference over
    /// the signal under suspicion.
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

    /// The same sample, with the local steady clock's reading of when its
    /// message arrived.
    ///
    /// Take one reading per `TFMessage` and apply it to every transform the
    /// message carries — see [`SteadyNanos`] for why per-message and not
    /// per-transform.
    #[must_use]
    pub fn received_at(mut self, received: SteadyNanos) -> Sample {
        self.received = received;
        self
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
///
/// # The identity is the GID, and the name is decoration
///
/// This is the whole point of the shape below, and getting it wrong cost a real
/// deployment. §5.3 says to *match* the message's GID against the graph's, so
/// the GID is what identifies a publisher; the node name is what a diagnostic
/// prints. An earlier revision made `Publisher::Node`'s `String` the identity,
/// and a name is not stable:
///
/// * `rmw_fastrtps` reports `_NODE_NAME_UNKNOWN_` for an endpoint discovered
///   before its participant's node information arrives, and fills in the real
///   name on a later graph walk. Same publisher, two names.
/// * Under [`AuthorityPolicy::FirstWriterWins`] the first name seen became the
///   edge's owner, and the corrected name was then a *different* publisher and
///   was rejected — permanently, since that policy never re-inserts.
///
/// Measured, on one correctly configured publisher with a declared topology:
/// **10 070 transforms received, 187 applied, 9 864 dropped as authority
/// conflicts, and 100 % of consumer lookups failing.** `crates/tf_tree_bench`'s
/// DDS comparison is what found it, because its aggregator flags a row whose
/// lookups mostly failed instead of reporting its latencies.
///
/// So [`Publisher::Gid`] carries the identity in `id` and the name in `name`,
/// and **`PartialEq`, `Ord` and `Hash` read `id` alone**. Renaming a publisher
/// is not a change of publisher.
///
/// # What this fixes in the other direction, and what it must not
///
/// The old `Publisher::UnknownGid` was a *unit* variant, so on a walk that
/// resolved no names every publisher compared **equal** and §5.4's conflict
/// detection was silently off — `docs/PHASE4.md` §5.3's amendment names that
/// blend. A GID we have no name for is still a distinct publisher, and now
/// compares as one.
///
/// [`Publisher::Unattributed`] stays a unit variant and that is deliberate, not
/// an oversight. It means the middleware reported **no GID at all**, and
/// collapsing those to one identity is the degradation
/// [`0012`](../../../docs/decisions/0012-the-authoritative-clock-jump-signal-and-the-degradation-ladder.md)
/// requires: fewer identifiable publishers must mean *less detection*, never
/// more stopping.
#[derive(Clone, Debug)]
pub enum Publisher {
    /// A middleware publisher, identified by the GID the RMW reported for it.
    Gid {
        /// The stable identity: the 16-byte GID rendered as `<gid:…>`.
        ///
        /// Rendered once, at construction, rather than held as bytes plus a
        /// derived key — `crate::ingest::owner_key` runs on **every** dynamic
        /// sample and must return a borrow, so the key has to be the stored
        /// form rather than something computed per call.
        id: Box<str>,
        /// The node name, once the graph resolved one. Presentation only: it
        /// may change, and a change is not a change of publisher.
        name: Option<String>,
    },
    /// A publisher identified by the **topic** it published on.
    ///
    /// Offline ingest (`tf_tree_ingest`) reads a recording, where there is no
    /// middleware and therefore no GID — what a record carries is its topic.
    /// A topic is a stable identity for the length of a recording, which is the
    /// property [`Publisher::Gid`] exists to guarantee for the online case, so
    /// this is a peer of it and not a degradation of it.
    ///
    /// It cannot collide with the bracketed keys below or with a GID key: a ROS
    /// topic name cannot contain `<`.
    Topic(String),
    /// No GID at all — the middleware could not tell us.
    ///
    /// Distinct from an unnamed [`Publisher::Gid`] on purpose: one says the
    /// middleware told us nothing, the other says it told us something we have
    /// no name for, and only the first is a reason to stop distinguishing
    /// publishers.
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
    ///
    /// This is what a later graph walk does when an RMW upgrades
    /// `_NODE_NAME_UNKNOWN_` to the real name. It is deliberately **not** a way
    /// to change who a publisher is.
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
            // Bracketed because a ROS node name cannot contain `<`, and a GID
            // key is bracketed too, so a real publisher can never collide with
            // one of these.
            Publisher::Unattributed => "<unattributed>",
            Publisher::Declared => "<declared>",
        }
    }
}

/// A deterministic GID derived from a name — **test scaffolding**.
///
/// `pub` and `#[doc(hidden)]` rather than `#[cfg(test)]` because the callers are
/// separate compilation units: `tests/steady_state_alloc.rs` and
/// `examples/offer_cost.rs` are their own crates and cannot see a test-only
/// item. Five copies of four lines was the alternative.
///
/// Real code never calls this. A real GID comes from
/// `rmw_message_info_t::publisher_gid`, and the point of [`Publisher`]'s shape
/// is that the GID is what the middleware said, not something derived from a
/// name that can change.
#[doc(hidden)]
#[must_use]
pub fn gid_for_name(name: &str) -> [u8; 16] {
    // FNV-1a, splatted across the 16 bytes. Any injective-enough function does;
    // what matters for a test is that two names differ and one name is stable.
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
        // `write!` would pull `core::fmt`'s machinery onto a path that runs once
        // per publisher; two table lookups do not.
        const HEX: &[u8; 16] = b"0123456789abcdef";
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s.push('>');
    s.into_boxed_str()
}

// `PartialEq`/`Eq`/`Ord`/`Hash` by identity ONLY — never by name. Hand-written
// rather than derived precisely because a derive would include `name`, which is
// the defect this shape exists to prevent and which a derive would silently
// reintroduce the next time someone adds a field.
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
            // The full key, not a prefix and not `<unknown publisher>`. Two
            // unnamed publishers must be distinguishable in a diagnostic whose
            // entire job is to say that two publishers are fighting over an
            // edge — and endpoint GIDs from one process share a participant
            // prefix, so a truncation would collide exactly where it matters.
            Publisher::Gid { id, .. } => write!(f, "{id}"),
            Publisher::Topic(t) => write!(f, "{t}"),
            Publisher::Unattributed => write!(f, "<unattributed>"),
            Publisher::Declared => write!(f, "<topology config>"),
        }
    }
}
