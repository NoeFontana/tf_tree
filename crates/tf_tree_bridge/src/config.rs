//! The topology config file — `docs/PHASE4.md` §5.8's amendment.
//!
//! # Why this exists
//!
//! §5.8's amendment established, by reading the engine, that **there is no
//! runtime edge declaration**: `ArenaBuilder::declare_edge` takes `&mut dyn
//! Arena`, `TreeBuilder::{static_edge, dynamic_edge}` consume the builder, and
//! `Tree::claim` returns `NoEdge` whenever the child's topology record has
//! `edge == 0` — which nothing can change after `build()`. `edge_headroom`
//! reserves zero-capacity slots no API can fill. `docs/decisions/0004` is
//! authoritative and D4 (fixed capacity, no growth) is why.
//!
//! So a bridge cannot learn its topology from `/tf`. It has to be told, before
//! the arena exists, and this module is the format it is told in — the file
//! `docs/PHASE2.md` §9 calls `tf_treed --config <file.toml|urdf>`.
//!
//! # Why the parser is hand-written
//!
//! **The workspace has no TOML dependency and this does not add one.** `toml`
//! pulls `serde`, `serde_spanned`, `toml_datetime` and `toml_edit`/`winnow` —
//! five crates through `cargo deny` — to read a file with one table, one
//! array-of-tables and five scalar key kinds.
//!
//! The decisive argument is not the crate count, though. A general TOML parser
//! plus `serde` **silently ignores keys it does not know** unless every struct
//! carries `deny_unknown_fields`, and a topology file whose `capaciy = 4096`
//! typo is dropped on the floor gives an operator an edge sized 1 with no
//! message. This parser's error set is mostly *refusals*: an unknown key, an
//! unknown table, a duplicate key, and every TOML construct outside the schema
//! (dotted keys, inline tables, literal strings, multi-line strings,
//! datetimes) are errors that name the line. A config file is read once, at
//! startup, by an operator who is already unsure whether they got it right;
//! being told exactly what was not understood is worth more here than
//! accepting the whole language.
//!
//! What it does **not** accept, deliberately: dotted keys (`a.b = 1`), inline
//! tables, literal (`'…'`) and multi-line strings, datetimes, multi-line
//! arrays, and any table other than `[topology]` / `[[edge]]`. Each is a
//! [`ConfigErrorKind::Unsupported`] naming the line, never a silent skip.
//!
//! # The schema
//!
//! ```toml
//! [topology]
//! interp = "sclerp"          # default for dynamic edges: sclerp | lerpslerp
//! domain = "system"          # default: system | sensor | sim | steady | 0..=255
//! frames = ["map"]           # frames with no edge yet (lookup endpoints)
//! frame_headroom = 8         # spare name slots for `Tree::frame()`
//!
//! [[edge]]
//! parent = "base_footprint"
//! child = "base_link"
//! kind = "static"
//! pose = [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]   # qw qx qy qz tx ty tz
//!
//! [[edge]]
//! parent = "odom"
//! child = "base_footprint"
//! kind = "dynamic"
//! rate_hz = 50.0             # with history_secs, sizes the ring…
//! history_secs = 10.0
//! # capacity = 512           # …or say it outright. Not both.
//! interp = "lerpslerp"       # optional per-edge overrides
//! domain = "sensor"          # …or a bare tag, for a user-declared domain
//! ```
//!
//! **The `domain` lines above are not free-standing, and that example is not a
//! config every bridge starts with.** [`TopologyConfig::check_domain`] refuses,
//! at startup, any *dynamic* edge whose resolved domain differs from the tag
//! the bridge itself stamps in — `ros/tf_tree_ros`'s `time_domain` parameter,
//! reaching this crate as `tft_bridge_options::domain`. The one dynamic edge
//! above overrides the file default to `"sensor"`, so that file starts a bridge
//! only when that tag is `1`. Static edges are exempt: a constant carries no
//! stamp for a domain to be wrong about.
//!
//! `rate_hz` does **two** things, and the second is why writing `capacity`
//! instead is not a free simplification: it sizes the ring, and it is recorded
//! in the arena as the edge's *declared nominal rate*
//! (`EdgeRecord::nominal_rate_mhz`), which is the only evidence `tf_tree
//! doctor`'s `TFT007` has that an observed rate is wrong rather than merely
//! what it is. An edge sized by `capacity` declares no rate and `TFT007` says
//! so for it, rather than comparing against a zero it would have to invent a
//! meaning for.
//!
//! # Errors name the offending frame, and cost nothing to carry
//!
//! [`ConfigError`] is `Copy` and holds a `&str` **borrowed from the config
//! text**, so "edge `base_link` -> `laser` declares both `capacity` and
//! `rate_hz`" is reported without a single allocation and without a `String` in
//! an error type (`CLAUDE.md`'s hard rules). That is only possible because
//! validation runs while the source is still in hand — which is why the
//! semantic checks live in [`TopologyConfig::parse`] rather than in a later
//! pass over the owned struct.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use tf_tree::{
    Capacity, Domain, EdgeCfg, InterpPolicy, Iso3, Quat, SensorDomain, SimDomain, SteadyDomain,
    SystemDomain, TreeBuilder, Vec3,
};

use crate::names::NameNormalizer;

// ---------------------------------------------------------------------------
// The parsed config
// ---------------------------------------------------------------------------

/// How a dynamic edge's ring is sized.
///
/// Two spellings because operators arrive with two different pieces of
/// knowledge. Someone reading `ros2 topic hz /tf` knows a rate and how much
/// history their pipeline needs; someone tuning memory knows a slot count.
/// Both resolve through [`Capacity`], which rounds up to a power of two.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RingSize {
    /// `capacity = N` — at least `N` slots.
    Slots(u32),
    /// `rate_hz` + `history_secs` — enough slots to retain that much history.
    History {
        /// Publication rate.
        rate_hz: f64,
        /// Seconds of history to retain.
        secs: f64,
    },
}

impl RingSize {
    /// The resolved (power-of-two) ring capacity.
    #[must_use]
    pub fn capacity(self) -> Capacity {
        match self {
            RingSize::Slots(n) => Capacity::slots(n),
            RingSize::History { rate_hz, secs } => Capacity::history(rate_hz, secs),
        }
    }
}

/// What an edge declaration describes.
///
/// Named `EdgeShape` rather than `EdgeKind` because `tf_tree::EdgeKind` already
/// exists and means the *arena's* record of the same distinction; two types
/// with one name in one file is how a conversion gets written backwards.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EdgeShape {
    /// A constant `T_parent_child`, `[qw qx qy qz tx ty tz]`. Zero ring slots.
    Static {
        /// The constant.
        pose: [f64; 7],
    },
    /// A ring of samples.
    Dynamic {
        /// How the ring is sized.
        ring: RingSize,
    },
}

/// One declared edge.
#[derive(Clone, Debug, PartialEq)]
pub struct EdgeConfig {
    /// Parent frame name.
    pub parent: String,
    /// Child frame name. Unique across the config: a frame has one parent.
    pub child: String,
    /// Static constant or dynamic ring.
    pub shape: EdgeShape,
    /// Per-edge interpolation policy; `None` takes the file default.
    pub interp: Option<InterpPolicy>,
    /// Per-edge time-domain tag; `None` takes the file default.
    pub domain: Option<u8>,
}

impl EdgeConfig {
    /// The `(parent, child)` key every bridge table is keyed on.
    #[must_use]
    pub fn key(&self) -> (&str, &str) {
        (self.parent.as_str(), self.child.as_str())
    }
}

/// A whole topology file.
#[derive(Clone, Debug, PartialEq)]
pub struct TopologyConfig {
    /// Frames with no edge — lookup endpoints that nothing publishes yet.
    pub frames: Vec<String>,
    /// Spare frame-name slots for `Tree::frame()` at runtime.
    ///
    /// There is deliberately **no `edge_headroom`**: §5.8's amendment is that
    /// those slots have zero capacity and no API can fill them, so offering the
    /// knob here would re-advertise the thing this file exists to replace.
    pub frame_headroom: u32,
    /// Default interpolation for dynamic edges that do not override it.
    pub default_interp: InterpPolicy,
    /// Default time-domain tag for edges that do not override it.
    ///
    /// A `u8` and not one of the built-in domain *types*, because [`Domain`] is
    /// an open trait: a user-declared domain picks a free tag from 4 upwards
    /// (`docs/API.md` §2.5) and there is no type here to name it with. The four
    /// built-ins are spellable by name in the file — `system`, `sensor`, `sim`,
    /// `steady` — and resolve to their [`Domain::TAG`] at parse time.
    pub default_domain: u8,
    /// The edges, in file order.
    pub edges: Vec<EdgeConfig>,
}

impl Default for TopologyConfig {
    fn default() -> TopologyConfig {
        TopologyConfig {
            frames: Vec::new(),
            frame_headroom: 0,
            default_interp: InterpPolicy::ScLerp,
            default_domain: SystemDomain::TAG,
            edges: Vec::new(),
        }
    }
}

impl TopologyConfig {
    /// The declaration for `(parent, child)`, if this file declares it.
    #[must_use]
    pub fn edge(&self, parent: &str, child: &str) -> Option<&EdgeConfig> {
        self.edges
            .iter()
            .find(|e| e.parent == parent && e.child == child)
    }

    /// The time-domain tag `(parent, child)` will be declared with — its own
    /// override, or the file default.
    #[must_use]
    pub fn domain_of(&self, edge: &EdgeConfig) -> u8 {
        edge.domain.unwrap_or(self.default_domain)
    }

    /// Check every declared edge against the domain the bridge will stamp in.
    ///
    /// `docs/PHASE4.md` §5.5, **NORMATIVE**: *"the bridge refuses to write to
    /// an edge whose declared domain differs from its own, and fails at startup
    /// rather than at first message. Sim and real transforms in one arena is a
    /// class of bug worth making impossible."*
    ///
    /// It could not be checked before §5.8's amendment, because before a config
    /// file there was nothing that declared a domain for the bridge to disagree
    /// with. It is a startup check and not a per-sample one on purpose: the
    /// answer is the same for every message on an edge, and finding out at the
    /// first message means finding out after the arena has been built and
    /// twenty nodes have attached to it.
    ///
    /// # Errors
    ///
    /// [`DomainMismatch`] naming the **first** offending edge in file order.
    /// First rather than all of them: an operator who set `use_sim_time` on one
    /// side of a launch file gets every edge listed, and the list is not more
    /// actionable than the first line of it.
    pub fn check_domain(&self, bridge_domain: u8) -> Result<(), DomainMismatch<'_>> {
        for e in &self.edges {
            // Static edges are exempt. Their pose is a constant folded into the
            // plan with no stamp of its own, so there is no clock for a domain
            // to be wrong about — and `robot_state_publisher` stamps them zero
            // regardless of `use_sim_time`, which would make every sim
            // deployment fail this check for no reason.
            if matches!(e.shape, EdgeShape::Static { .. }) {
                continue;
            }
            let declared = self.domain_of(e);
            if declared != bridge_domain {
                return Err(DomainMismatch {
                    parent: e.parent.as_str(),
                    child: e.child.as_str(),
                    declared,
                    bridge: bridge_domain,
                });
            }
        }
        Ok(())
    }

    /// The child frame that closes a parent cycle, if the declared edges have
    /// one.
    ///
    /// `build()` finds this too, and reports it as `WouldCreateCycle { child:
    /// FrameId(1) }` — an index into an arena that was never constructed, which
    /// is the one thing an operator holding a text file cannot resolve. The
    /// config still has the names, so the preflight answers with one.
    ///
    /// It is deliberately *not* folded into [`TopologyConfig::parse`]:
    /// [`ConfigError`] borrows its `at` from the input text, and a cycle is a
    /// property of the assembled edge set whose frame names are owned `String`s
    /// by the time it can be detected. Reporting it would mean giving
    /// `ConfigError` an owned field, and `ConfigError` is `Copy` on purpose.
    ///
    /// Each child has at most one parent (`DuplicateChild` is refused at parse
    /// time), so the edges form a functional graph and walking parent links
    /// terminates at a root or repeats.
    #[must_use]
    pub fn cycle_child(&self) -> Option<&str> {
        let parent_of: BTreeMap<&str, &str> = self
            .edges
            .iter()
            .map(|e| (e.child.as_str(), e.parent.as_str()))
            .collect();
        for e in &self.edges {
            let mut seen = BTreeSet::new();
            let mut cur = e.child.as_str();
            while let Some(p) = parent_of.get(cur) {
                if !seen.insert(cur) {
                    return Some(cur);
                }
                cur = p;
            }
        }
        None
    }

    /// This topology with every declared frame name put through `names` —
    /// §5.6's normalization, `tf_prefix` included.
    ///
    /// # Why the config is normalized and not only the wire
    ///
    /// §5.6's `tf_prefix` rewrites the names arriving on `/tf`. Everything
    /// downstream keys on `(parent, child)`, and §5.8's amendment makes the
    /// *config* the sole source of declared edges — so if only the wire side is
    /// rewritten, a bridge with a prefix looks up `robot1/odom -> robot1/base`
    /// in a table seeded with `odom -> base`, misses every time, and reports
    /// every transform on the robot as an undeclared edge. The arena is built
    /// from this same rewritten config, so the frame names a consumer looks up
    /// are the prefixed ones too — there is no second set of names anywhere.
    ///
    /// The operator workflow is what settles the direction: `tf_tree topology
    /// --discover` writes the names as they appear on the wire, and adding
    /// `tf_prefix` for a second robot must not require hand-editing every name
    /// in the file it just produced.
    ///
    /// **The same [`NameNormalizer`] instance the wire will use**, not a second
    /// one configured the same way: passing it in is what makes the two sides
    /// provably identical rather than merely similar, and it leaves the
    /// normalizer's remap table populated with the declared frames before the
    /// first message arrives — which is what §5.6 means by *"log the resulting
    /// mapping table at startup"*.
    ///
    /// A name that does not normalize is kept verbatim. The only such name is a
    /// bare `"/"`, which the parser accepts and which no wire name can ever
    /// match afterwards, so the edge is simply one nothing can write — the same
    /// outcome as dropping the declaration, without inventing a second failure
    /// mode at create time.
    #[must_use]
    pub fn rewritten(&self, names: &mut NameNormalizer) -> TopologyConfig {
        let mut rename = |s: &String| names.normalize(s).map_or_else(|_| s.clone(), |n| n.name);
        TopologyConfig {
            frames: self.frames.iter().map(&mut rename).collect(),
            frame_headroom: self.frame_headroom,
            default_interp: self.default_interp,
            default_domain: self.default_domain,
            edges: self
                .edges
                .iter()
                .map(|e| EdgeConfig {
                    parent: rename(&e.parent),
                    child: rename(&e.child),
                    shape: e.shape,
                    interp: e.interp,
                    domain: e.domain,
                })
                .collect(),
        }
    }

    /// A [`TreeBuilder`] carrying exactly this topology.
    ///
    /// This is the operation §5.8's amendment says the engine was missing: the
    /// whole tree — frames, both edge kinds, ring capacities, interp policies,
    /// domains and every static edge's constant — declared before `build()`,
    /// which is the only moment the arena can be sized.
    #[must_use]
    pub fn builder(&self) -> TreeBuilder {
        let mut b = TreeBuilder::new()
            .default_interp(self.default_interp)
            .default_domain(self.default_domain)
            .frame_headroom(self.frame_headroom);
        for f in &self.frames {
            b = b.frame(f);
        }
        for e in &self.edges {
            b = match e.shape {
                EdgeShape::Static { pose } => b.static_edge(&e.parent, &e.child, &iso_of(pose)),
                EdgeShape::Dynamic { ring } => {
                    let mut cfg = EdgeCfg::new(ring.capacity());
                    cfg.interp = e.interp;
                    cfg.domain = e.domain;
                    // `rate_hz` is carried into the arena as well as consumed by
                    // `ring.capacity()`. It is the operator's statement of what
                    // this edge *should* publish at, so it is exactly the
                    // nominal `TFT007` needs; an edge written as `capacity = N`
                    // states no rate and leaves the field 0 (undeclared), which
                    // is why this is a `match` and not an `unwrap_or(0.0)`.
                    if let RingSize::History { rate_hz, .. } = ring {
                        cfg = cfg.nominal_rate_hz(rate_hz);
                    }
                    b.dynamic_edge(&e.parent, &e.child, cfg)
                }
            };
        }
        b
    }

    /// Render back to the file format.
    ///
    /// Round-trips: parsing this string yields an equal [`TopologyConfig`].
    /// That is what makes `--discover` usable — the operator edits what it
    /// printed and hands it straight back.
    #[must_use]
    pub fn to_toml(&self) -> String {
        let mut s = String::new();
        s.push_str("# tf_tree topology — docs/PHASE4.md §5.8\n");
        s.push_str("[topology]\n");
        s.push_str(&format!(
            "interp = \"{}\"\n",
            interp_name(self.default_interp)
        ));
        s.push_str(&format!("domain = {}\n", self.default_domain));
        s.push_str("frames = [");
        for (i, f) in self.frames.iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            s.push_str(&quote(f));
        }
        s.push_str("]\n");
        s.push_str(&format!("frame_headroom = {}\n", self.frame_headroom));

        for e in &self.edges {
            s.push_str("\n[[edge]]\n");
            s.push_str(&format!("parent = {}\n", quote(&e.parent)));
            s.push_str(&format!("child = {}\n", quote(&e.child)));
            match e.shape {
                EdgeShape::Static { pose } => {
                    s.push_str("kind = \"static\"\n");
                    s.push_str("pose = [");
                    for (i, v) in pose.iter().enumerate() {
                        if i > 0 {
                            s.push_str(", ");
                        }
                        s.push_str(&float(*v));
                    }
                    s.push_str("]\n");
                }
                EdgeShape::Dynamic { ring } => {
                    s.push_str("kind = \"dynamic\"\n");
                    match ring {
                        RingSize::Slots(n) => s.push_str(&format!("capacity = {n}\n")),
                        RingSize::History { rate_hz, secs } => {
                            s.push_str(&format!("rate_hz = {}\n", float(rate_hz)));
                            s.push_str(&format!("history_secs = {}\n", float(secs)));
                        }
                    }
                }
            }
            if let Some(i) = e.interp {
                s.push_str(&format!("interp = \"{}\"\n", interp_name(i)));
            }
            if let Some(d) = e.domain {
                s.push_str(&format!("domain = {d}\n"));
            }
        }
        s
    }
}

/// `[qw qx qy qz tx ty tz]` as an [`Iso3`].
///
/// No renormalization: [`TopologyConfig::parse`] already refused a quaternion
/// outside `POSE_UNIT_EPS` of unit, and silently fixing one here would mean the
/// arena held a rotation the file does not describe.
fn iso_of(p: [f64; 7]) -> Iso3 {
    Iso3::new(
        Quat::new(p[0], p[1], p[2], p[3]),
        Vec3::new(p[4], p[5], p[6]),
    )
}

fn interp_name(i: InterpPolicy) -> &'static str {
    match i {
        InterpPolicy::ScLerp => "sclerp",
        InterpPolicy::LerpSlerp => "lerpslerp",
    }
}

/// A TOML basic string.
///
/// No escaping, because none can be needed: [`check_frame_name`] refuses a name
/// holding `"`, `\` or a control character, and those are exactly the
/// characters a basic string would have to escape. The two halves have to agree
/// — an emitter that escaped and a parser that refuses escapes would produce
/// files the tool cannot read back.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    out.push_str(s);
    out.push('"');
    out
}

/// An `f64` as a TOML **float**, round-tripping exactly.
///
/// `{:?}` gives Rust's shortest round-tripping form, but for a whole number
/// that is `1` — a TOML *integer* — so a `.0` is appended when the rendering
/// carries neither point nor exponent, and the emitted file matches the schema
/// as documented.
///
/// **This is cosmetic, and deliberately so**: [`as_f64`] also accepts an
/// integer, because `history_secs = 10` is what a person writes and refusing it
/// teaches nothing. The consequence is that the round-trip test does *not* pin
/// this `.0` — do not read it as covered.
fn float(v: f64) -> String {
    let s = format!("{v:?}");
    if s.contains('.') || s.contains('e') || s.contains('E') {
        s
    } else {
        format!("{s}.0")
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// What was wrong with a config file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigErrorKind {
    /// A TOML construct outside this schema's subset (dotted key, inline
    /// table, literal or multi-line string, datetime, multi-line array).
    Unsupported,
    /// A table other than `[topology]` or `[[edge]]`.
    UnknownTable,
    /// A key this schema does not define — a typo, not something to ignore.
    UnknownKey,
    /// The same key twice in one table.
    DuplicateKey,
    /// A key/value line that is not `key = value`.
    NotAKeyValue,
    /// A key before any table header.
    NoTable,
    /// The value has the wrong type or is out of range.
    BadValue,
    /// A required key is absent.
    MissingKey,
    /// `kind` was neither `"static"` nor `"dynamic"`.
    BadKind,
    /// `interp` was neither `"sclerp"` nor `"lerpslerp"`.
    BadInterp,
    /// `domain` was not a known name or an integer in `0..=255`.
    BadDomain,
    /// A frame name is empty or holds a control character, `"` or `\`.
    BadFrameName,
    /// `parent == child`.
    SelfEdge,
    /// Two edges declare the same child: a frame has exactly one parent.
    DuplicateChild,
    /// `pose` is not seven finite floats.
    BadPose,
    /// `pose`'s quaternion is not unit within [`POSE_UNIT_EPS`].
    NonUnitQuaternion,
    /// A static edge carries ring-sizing keys, or a dynamic one carries `pose`.
    KeyWrongForKind,
    /// A dynamic edge gave both `capacity` and `rate_hz`/`history_secs`.
    ConflictingRingSize,
    /// A frame listed in `frames` is already an edge endpoint, so listing it
    /// says nothing and hides the real question of which edge owns it.
    RedundantFrame,
}

impl ConfigErrorKind {
    fn message(self) -> &'static str {
        match self {
            ConfigErrorKind::Unsupported => "TOML construct outside this schema",
            ConfigErrorKind::UnknownTable => "unknown table (expected [topology] or [[edge]])",
            ConfigErrorKind::UnknownKey => "unknown key",
            ConfigErrorKind::DuplicateKey => "duplicate key",
            ConfigErrorKind::NotAKeyValue => "expected `key = value`",
            ConfigErrorKind::NoTable => "key outside any table",
            ConfigErrorKind::BadValue => "bad value",
            ConfigErrorKind::MissingKey => "missing required key",
            ConfigErrorKind::BadKind => "kind must be \"static\" or \"dynamic\"",
            ConfigErrorKind::BadInterp => "interp must be \"sclerp\" or \"lerpslerp\"",
            ConfigErrorKind::BadDomain => {
                "domain must be \"system\", \"sensor\", \"sim\", \"steady\" or 0..=255"
            }
            ConfigErrorKind::BadFrameName => {
                "frame name is empty or holds a control character, quote or backslash"
            }
            ConfigErrorKind::SelfEdge => "an edge's parent and child are the same frame",
            ConfigErrorKind::DuplicateChild => "two edges declare the same child",
            ConfigErrorKind::BadPose => "pose must be seven finite floats [qw qx qy qz tx ty tz]",
            ConfigErrorKind::NonUnitQuaternion => "pose's quaternion is not unit",
            ConfigErrorKind::KeyWrongForKind => "key does not belong to this edge kind",
            ConfigErrorKind::ConflictingRingSize => {
                "give capacity or rate_hz/history_secs, not both"
            }
            ConfigErrorKind::RedundantFrame => "frame is already an edge endpoint",
        }
    }
}

/// A config error, naming the line and the offending frame, edge or key.
///
/// `Copy`, and `at` borrows from the config text — no `String` in an error type
/// (`CLAUDE.md`), and no allocation on the failure path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfigError<'a> {
    /// 1-based line number.
    pub line: u32,
    /// What went wrong.
    pub kind: ConfigErrorKind,
    /// The offending frame, edge child, key or literal, borrowed from the text.
    pub at: &'a str,
}

impl fmt::Display for ConfigError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "line {}: {}: {:?}",
            self.line,
            self.kind.message(),
            self.at
        )
    }
}

impl std::error::Error for ConfigError<'_> {}

/// A declared edge whose time domain is not the bridge's (§5.5).
///
/// `Copy` and borrowing the frame names from the config, like [`ConfigError`]
/// and for the same reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DomainMismatch<'a> {
    /// Parent frame of the offending edge.
    pub parent: &'a str,
    /// Child frame of the offending edge.
    pub child: &'a str,
    /// The tag the config declares.
    pub declared: u8,
    /// The tag the bridge stamps in.
    pub bridge: u8,
}

impl fmt::Display for DomainMismatch<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "edge {:?} -> {:?} is declared in time domain {} but the bridge stamps in domain {}",
            self.parent, self.child, self.declared, self.bridge
        )
    }
}

impl std::error::Error for DomainMismatch<'_> {}

/// How far a config quaternion may be from unit norm.
///
/// `1e-9`, not `1e-12`: a URDF's RPY is converted to a quaternion by somebody
/// else's code and printed with finite digits, so a hand-written or
/// tool-generated file lands a few ulps of a *float* away, not of a double.
/// Tighter than this rejects correct files; looser lets a genuinely
/// mis-scaled rotation through, and `Quat::rotate` mis-scales by `‖q‖²`.
pub const POSE_UNIT_EPS: f64 = 1e-9;

// ---------------------------------------------------------------------------
// The parser
// ---------------------------------------------------------------------------

/// One TOML value, in the four scalar kinds this schema uses plus arrays.
#[derive(Clone, Debug, PartialEq)]
enum Value<'a> {
    Str(&'a str),
    Int(i64),
    Float(f64),
    Array(Vec<Value<'a>>),
}

/// A table's key/value pairs with the line each was on, so an error about a
/// *combination* of keys can still name a line.
type Table<'a> = Vec<(&'a str, Value<'a>, u32)>;

impl TopologyConfig {
    /// Parse a topology file.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] naming the line and the offending frame, edge or key.
    /// Every construct outside the schema is an error rather than a silent
    /// skip — see the module docs for why that is the point.
    pub fn parse(text: &str) -> Result<TopologyConfig, ConfigError<'_>> {
        let mut topology: Option<Table<'_>> = None;
        let mut edges: Vec<Table<'_>> = Vec::new();
        let mut cur: Option<&mut Table<'_>> = None;

        for (i, raw) in text.lines().enumerate() {
            let line = u32::try_from(i + 1).unwrap_or(u32::MAX);
            let s = raw.trim();
            if s.is_empty() || s.starts_with('#') {
                continue;
            }
            if let Some(rest) = s.strip_prefix("[[") {
                let name = header_name(rest, "]]", s, line)?;
                if name != "edge" {
                    return Err(ConfigError {
                        line,
                        kind: ConfigErrorKind::UnknownTable,
                        at: name,
                    });
                }
                edges.push(Table::new());
                cur = edges.last_mut();
                continue;
            }
            if let Some(rest) = s.strip_prefix('[') {
                let name = header_name(rest, "]", s, line)?;
                if name != "topology" {
                    return Err(ConfigError {
                        line,
                        kind: ConfigErrorKind::UnknownTable,
                        at: name,
                    });
                }
                if topology.is_some() {
                    return Err(ConfigError {
                        line,
                        kind: ConfigErrorKind::DuplicateKey,
                        at: "topology",
                    });
                }
                topology = Some(Table::new());
                cur = topology.as_mut();
                continue;
            }

            let (key, value) = parse_key_value(s, line)?;
            let Some(table) = cur.as_deref_mut() else {
                return Err(ConfigError {
                    line,
                    kind: ConfigErrorKind::NoTable,
                    at: key,
                });
            };
            if table.iter().any(|(k, _, _)| *k == key) {
                return Err(ConfigError {
                    line,
                    kind: ConfigErrorKind::DuplicateKey,
                    at: key,
                });
            }
            table.push((key, value, line));
        }

        build_config(&topology.unwrap_or_default(), &edges)
    }
}

/// The name inside a table header, given the text after its opening bracket.
///
/// `close` is `"]]"` for `[[edge]]` and `"]"` for `[topology]`. A **trailing
/// comment is accepted**, for exactly the reason one is accepted after a value:
/// `[[edge]] # left wheel` is an operator annotating the file this format
/// exists to be hand-edited as. Refusing it produced the worst kind of
/// diagnostic — `unknown table (expected [topology] or [[edge]])` pointing at a
/// line that *does* say `[[edge]]` — which is why the check is a suffix match
/// on the bracket rather than on the whole line.
///
/// Anything else after the bracket is still refused: this schema has no place
/// for it, and `[topology] junk` silently ignored is how a typo'd second table
/// header becomes a config that parses and means something else.
fn header_name<'a>(
    rest: &'a str,
    close: &str,
    whole: &'a str,
    line: u32,
) -> Result<&'a str, ConfigError<'a>> {
    let bad = ConfigError {
        line,
        kind: ConfigErrorKind::UnknownTable,
        at: whole,
    };
    let end = rest.find(close).ok_or(bad)?;
    let tail = rest[end + close.len()..].trim();
    if !tail.is_empty() && !tail.starts_with('#') {
        return Err(bad);
    }
    Ok(rest[..end].trim())
}

/// `key = value`, with the trailing comment (if any) already required to be all
/// that follows the value.
fn parse_key_value(s: &str, line: u32) -> Result<(&str, Value<'_>), ConfigError<'_>> {
    let eq = s.find('=').ok_or(ConfigError {
        line,
        kind: ConfigErrorKind::NotAKeyValue,
        at: s,
    })?;
    let key = s[..eq].trim();
    if key.is_empty() {
        return Err(ConfigError {
            line,
            kind: ConfigErrorKind::NotAKeyValue,
            at: s,
        });
    }
    // A dotted key (`edge.parent = …`) is valid TOML and means something this
    // schema has no place for. Accepting the line and ignoring the prefix would
    // put the value in the wrong table silently.
    if !key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(ConfigError {
            line,
            kind: ConfigErrorKind::Unsupported,
            at: key,
        });
    }
    let (value, rest) = parse_value(s[eq + 1..].trim_start(), line)?;
    let rest = rest.trim();
    if !rest.is_empty() && !rest.starts_with('#') {
        return Err(ConfigError {
            line,
            kind: ConfigErrorKind::BadValue,
            at: rest,
        });
    }
    Ok((key, value))
}

/// One value, returning it and whatever follows on the line.
fn parse_value(s: &str, line: u32) -> Result<(Value<'_>, &str), ConfigError<'_>> {
    let err = |kind, at| ConfigError { line, kind, at };
    let mut chars = s.char_indices();
    let Some((_, first)) = chars.next() else {
        return Err(err(ConfigErrorKind::BadValue, s));
    };
    match first {
        '"' => {
            // Basic string. `"""` (multi-line) is refused rather than parsed as
            // an empty string followed by junk, which is what a naive scan does.
            if s.starts_with("\"\"\"") {
                return Err(err(ConfigErrorKind::Unsupported, s));
            }
            for (i, c) in s.char_indices().skip(1) {
                match c {
                    // **No escape sequences**, and refusing them is what keeps
                    // `to_toml` honest: `check_frame_name` rejects `"` and `\`
                    // in a frame name, so `quote` never has to emit an escape,
                    // so the emitter can never produce a string this parser
                    // would have to decode. A parser that accepted `\"` while
                    // returning the raw slice would hand back a name with a
                    // stray backslash in it — silently, and only for the names
                    // nobody tests with.
                    '\\' => return Err(err(ConfigErrorKind::Unsupported, s)),
                    '"' => return Ok((Value::Str(&s[1..i]), &s[i + 1..])),
                    _ => {}
                }
            }
            Err(err(ConfigErrorKind::BadValue, s))
        }
        // Literal strings, inline tables: valid TOML, outside this schema.
        '\'' | '{' => Err(err(ConfigErrorKind::Unsupported, s)),
        '[' => {
            let mut items = Vec::new();
            let mut rest = s[1..].trim_start();
            // The separator is **required**, not optional. `[1.0 0.0 0.0]` is
            // not TOML, and a parser that accepted it would be the one piece of
            // leniency in a module whose whole argument is that a construct it
            // does not understand is an error rather than a silent skip. It
            // also cannot round-trip: `to_toml` always emits commas, so
            // accepting their absence means reading files this tool can never
            // write. A trailing comma before `]` stays legal — TOML allows it.
            let mut need_comma = false;
            loop {
                if let Some(r) = rest.strip_prefix(']') {
                    return Ok((Value::Array(items), r));
                }
                if rest.is_empty() {
                    // A multi-line array is valid TOML; this parser is
                    // line-oriented, so it says so instead of truncating.
                    return Err(err(ConfigErrorKind::Unsupported, s));
                }
                if need_comma {
                    let Some(r) = rest.strip_prefix(',') else {
                        return Err(err(ConfigErrorKind::BadValue, rest));
                    };
                    rest = r.trim_start();
                    need_comma = false;
                    continue;
                }
                let (v, r) = parse_value(rest, line)?;
                items.push(v);
                rest = r.trim_start();
                need_comma = true;
            }
        }
        _ => {
            let end = s.find([',', ']', '#', ' ', '\t']).unwrap_or(s.len());
            let (tok, rest) = s.split_at(end);
            if tok.is_empty() {
                return Err(err(ConfigErrorKind::BadValue, s));
            }
            if tok == "true" || tok == "false" {
                // No key in this schema is a boolean; saying so beats parsing
                // one and reporting a type error two layers up.
                return Err(err(ConfigErrorKind::BadValue, tok));
            }
            if tok.contains('.') || tok.contains('e') || tok.contains('E') {
                let v: f64 = tok
                    .parse()
                    .map_err(|_| err(ConfigErrorKind::BadValue, tok))?;
                if !v.is_finite() {
                    return Err(err(ConfigErrorKind::BadValue, tok));
                }
                return Ok((Value::Float(v), rest));
            }
            let v: i64 = tok
                .parse()
                .map_err(|_| err(ConfigErrorKind::BadValue, tok))?;
            Ok((Value::Int(v), rest))
        }
    }
}

// --- schema application ----------------------------------------------------

fn get<'a, 'b>(t: &'b Table<'a>, key: &str) -> Option<(&'b Value<'a>, u32)> {
    t.iter()
        .find(|(k, _, _)| *k == key)
        .map(|(_, v, l)| (v, *l))
}

fn reject_unknown<'a>(t: &Table<'a>, allowed: &[&str]) -> Result<(), ConfigError<'a>> {
    for (k, _, line) in t {
        if !allowed.contains(k) {
            return Err(ConfigError {
                line: *line,
                kind: ConfigErrorKind::UnknownKey,
                at: k,
            });
        }
    }
    Ok(())
}

fn as_str<'a>(v: &Value<'a>, line: u32, at: &'a str) -> Result<&'a str, ConfigError<'a>> {
    match v {
        Value::Str(s) => Ok(s),
        _ => Err(ConfigError {
            line,
            kind: ConfigErrorKind::BadValue,
            at,
        }),
    }
}

fn as_f64<'a>(v: &Value<'a>, line: u32, at: &'a str) -> Result<f64, ConfigError<'a>> {
    match v {
        Value::Float(f) => Ok(*f),
        // An integer where a float belongs is what a hand-written
        // `history_secs = 10` is, and refusing it teaches nothing.
        Value::Int(i) => Ok(*i as f64),
        _ => Err(ConfigError {
            line,
            kind: ConfigErrorKind::BadValue,
            at,
        }),
    }
}

fn as_u32<'a>(v: &Value<'a>, line: u32, at: &'a str) -> Result<u32, ConfigError<'a>> {
    match v {
        Value::Int(i) => u32::try_from(*i).map_err(|_| ConfigError {
            line,
            kind: ConfigErrorKind::BadValue,
            at,
        }),
        _ => Err(ConfigError {
            line,
            kind: ConfigErrorKind::BadValue,
            at,
        }),
    }
}

fn parse_interp<'a>(v: &Value<'a>, line: u32) -> Result<InterpPolicy, ConfigError<'a>> {
    let s = as_str(v, line, "interp")?;
    match s {
        "sclerp" => Ok(InterpPolicy::ScLerp),
        "lerpslerp" => Ok(InterpPolicy::LerpSlerp),
        _ => Err(ConfigError {
            line,
            kind: ConfigErrorKind::BadInterp,
            at: s,
        }),
    }
}

/// `domain = "system" | "sensor" | "sim" | "steady" | 0..=255`.
///
/// The four names are the four built-ins, and each resolves through the
/// engine's own [`Domain::TAG`] rather than through a literal repeated here:
/// [`SystemDomain`] 0, [`SensorDomain`] 1, [`SimDomain`] 2, [`SteadyDomain`] 3.
/// `docs/API.md` §2.5 is why the numbering is permanent — a tag is written into
/// `EdgeRecord::domain` and into every recording already on disk — and going
/// through the constants is what stops this file from being a second place it
/// could be renumbered.
///
/// **The integer form stays, and it is not a legacy escape.** [`Domain`] is an
/// open trait: a driver with a PTP-disciplined clock declares its own unit
/// struct and picks a free tag from 4 upwards (`docs/API.md` §2.5), and this
/// parser must not refuse a number just because it has no name for it. Naming
/// the built-ins removes the case where an operator wanting *sim* time had to
/// write `2`; it does not close the tag space.
fn parse_domain<'a>(v: &Value<'a>, line: u32) -> Result<u8, ConfigError<'a>> {
    match v {
        Value::Str("system") => Ok(SystemDomain::TAG),
        Value::Str("sensor") => Ok(SensorDomain::TAG),
        Value::Str("sim") => Ok(SimDomain::TAG),
        Value::Str("steady") => Ok(SteadyDomain::TAG),
        Value::Str(s) => Err(ConfigError {
            line,
            kind: ConfigErrorKind::BadDomain,
            at: s,
        }),
        Value::Int(i) => u8::try_from(*i).map_err(|_| ConfigError {
            line,
            kind: ConfigErrorKind::BadDomain,
            at: "domain",
        }),
        _ => Err(ConfigError {
            line,
            kind: ConfigErrorKind::BadDomain,
            at: "domain",
        }),
    }
}

/// A frame name usable as both an arena key and a TOML basic string.
///
/// `"` and `\` are refused alongside control characters so [`quote`] never has
/// to escape and the parser never has to unescape — see [`quote`]. ROS frame
/// names are identifiers; none of the three has ever been one.
fn check_frame_name<'a>(name: &'a str, line: u32) -> Result<&'a str, ConfigError<'a>> {
    if !frame_name_ok(name) {
        return Err(ConfigError {
            line,
            kind: ConfigErrorKind::BadFrameName,
            at: name,
        });
    }
    Ok(name)
}

/// Whether a frame name can be written to a config file and read back.
///
/// The predicate behind [`check_frame_name`], separated so **every producer of
/// a config uses the same one as the parser**. [`crate::Discovery`] is the
/// other producer, and it takes names off the wire: a robot publishing a frame
/// called `odo"m` used to yield `parent = "odo"m"` in a discovered file, which
/// this crate's own parser then refused. The contract that a discovered config
/// reparses is only real if the two halves share this function.
pub(crate) fn frame_name_ok(name: &str) -> bool {
    !name.is_empty()
        && !name
            .chars()
            .any(|c| c.is_control() || c == '"' || c == '\\')
}

const TOPOLOGY_KEYS: &[&str] = &["interp", "domain", "frames", "frame_headroom"];
const EDGE_KEYS: &[&str] = &[
    "parent",
    "child",
    "kind",
    "pose",
    "capacity",
    "rate_hz",
    "history_secs",
    "interp",
    "domain",
];

#[allow(clippy::too_many_lines)]
fn build_config<'a>(
    topology: &Table<'a>,
    edges: &[Table<'a>],
) -> Result<TopologyConfig, ConfigError<'a>> {
    reject_unknown(topology, TOPOLOGY_KEYS)?;

    let mut out = TopologyConfig::default();
    if let Some((v, line)) = get(topology, "interp") {
        out.default_interp = parse_interp(v, line)?;
    }
    if let Some((v, line)) = get(topology, "domain") {
        out.default_domain = parse_domain(v, line)?;
    }
    if let Some((v, line)) = get(topology, "frame_headroom") {
        out.frame_headroom = as_u32(v, line, "frame_headroom")?;
    }
    let mut listed_frames: Vec<(&str, u32)> = Vec::new();
    if let Some((v, line)) = get(topology, "frames") {
        let Value::Array(items) = v else {
            return Err(ConfigError {
                line,
                kind: ConfigErrorKind::BadValue,
                at: "frames",
            });
        };
        for item in items {
            let name = check_frame_name(as_str(item, line, "frames")?, line)?;
            listed_frames.push((name, line));
        }
    }

    // Edges, then the cross-edge checks. `children` keeps the borrowed name so
    // a duplicate can be reported as `"base_link"` rather than as an index.
    let mut children: BTreeMap<&str, u32> = BTreeMap::new();
    let mut endpoints: BTreeMap<&str, ()> = BTreeMap::new();
    for t in edges {
        reject_unknown(t, EDGE_KEYS)?;
        let (pv, pl) = get(t, "parent").ok_or(ConfigError {
            line: t.first().map_or(0, |(_, _, l)| *l),
            kind: ConfigErrorKind::MissingKey,
            at: "parent",
        })?;
        let parent = check_frame_name(as_str(pv, pl, "parent")?, pl)?;
        let (cv, cl) = get(t, "child").ok_or(ConfigError {
            line: pl,
            kind: ConfigErrorKind::MissingKey,
            at: "child",
        })?;
        let child = check_frame_name(as_str(cv, cl, "child")?, cl)?;
        if parent == child {
            return Err(ConfigError {
                line: cl,
                kind: ConfigErrorKind::SelfEdge,
                at: child,
            });
        }
        if children.insert(child, cl).is_some() {
            return Err(ConfigError {
                line: cl,
                kind: ConfigErrorKind::DuplicateChild,
                at: child,
            });
        }
        endpoints.insert(parent, ());
        endpoints.insert(child, ());

        let (kv, kl) = get(t, "kind").ok_or(ConfigError {
            line: cl,
            kind: ConfigErrorKind::MissingKey,
            at: "kind",
        })?;
        let kind = as_str(kv, kl, "kind")?;
        let ring_keys = ["capacity", "rate_hz", "history_secs"];
        let shape = match kind {
            "static" => {
                for k in ring_keys {
                    if let Some((_, l)) = get(t, k) {
                        return Err(ConfigError {
                            line: l,
                            kind: ConfigErrorKind::KeyWrongForKind,
                            at: child,
                        });
                    }
                }
                let (pv, pl) = get(t, "pose").ok_or(ConfigError {
                    line: kl,
                    kind: ConfigErrorKind::MissingKey,
                    at: "pose",
                })?;
                EdgeShape::Static {
                    pose: parse_pose(pv, pl, child)?,
                }
            }
            "dynamic" => {
                if let Some((_, l)) = get(t, "pose") {
                    return Err(ConfigError {
                        line: l,
                        kind: ConfigErrorKind::KeyWrongForKind,
                        at: child,
                    });
                }
                let cap = get(t, "capacity");
                let rate = get(t, "rate_hz");
                let secs = get(t, "history_secs");
                match (cap, rate, secs) {
                    (Some((v, l)), None, None) => {
                        let n = as_u32(v, l, child)?;
                        if n == 0 {
                            return Err(ConfigError {
                                line: l,
                                kind: ConfigErrorKind::BadValue,
                                at: child,
                            });
                        }
                        EdgeShape::Dynamic {
                            ring: RingSize::Slots(n),
                        }
                    }
                    (None, Some((rv, rl)), Some((sv, sl))) => {
                        let rate_hz = as_f64(rv, rl, child)?;
                        let secs = as_f64(sv, sl, child)?;
                        // The **product** has to be finite too, not just the
                        // factors. `rate_hz = 1e300` with `history_secs = 1e300`
                        // passes both individual guards and overflows to `inf`
                        // in `Capacity::history`, whose non-finite fallback is
                        // the *minimum* — so the edge silently gets a one-slot
                        // ring, the worst ring this sizing code can produce and
                        // the only one it used to produce without a word.
                        if !(rate_hz.is_finite()
                            && rate_hz > 0.0
                            && secs.is_finite()
                            && secs > 0.0
                            && (rate_hz * secs).is_finite())
                        {
                            return Err(ConfigError {
                                line: rl,
                                kind: ConfigErrorKind::BadValue,
                                at: child,
                            });
                        }
                        EdgeShape::Dynamic {
                            ring: RingSize::History { rate_hz, secs },
                        }
                    }
                    (None, None, None) => {
                        return Err(ConfigError {
                            line: kl,
                            kind: ConfigErrorKind::MissingKey,
                            at: child,
                        })
                    }
                    // Everything else is under-specified (`rate_hz` alone) or
                    // over-specified (both spellings), and both are the kind of
                    // half-edit that a permissive reader turns into a ring of
                    // the wrong size with no message.
                    _ => {
                        return Err(ConfigError {
                            line: kl,
                            kind: ConfigErrorKind::ConflictingRingSize,
                            at: child,
                        })
                    }
                }
            }
            _ => {
                return Err(ConfigError {
                    line: kl,
                    kind: ConfigErrorKind::BadKind,
                    at: kind,
                })
            }
        };

        let interp = match get(t, "interp") {
            Some((v, l)) => Some(parse_interp(v, l)?),
            None => None,
        };
        let domain = match get(t, "domain") {
            Some((v, l)) => Some(parse_domain(v, l)?),
            None => None,
        };
        out.edges.push(EdgeConfig {
            parent: parent.to_owned(),
            child: child.to_owned(),
            shape,
            interp,
            domain,
        });
    }

    for (name, line) in listed_frames {
        if endpoints.contains_key(name) {
            return Err(ConfigError {
                line,
                kind: ConfigErrorKind::RedundantFrame,
                at: name,
            });
        }
        out.frames.push(name.to_owned());
    }
    Ok(out)
}

fn parse_pose<'a>(v: &Value<'a>, line: u32, at: &'a str) -> Result<[f64; 7], ConfigError<'a>> {
    let Value::Array(items) = v else {
        return Err(ConfigError {
            line,
            kind: ConfigErrorKind::BadPose,
            at,
        });
    };
    if items.len() != 7 {
        return Err(ConfigError {
            line,
            kind: ConfigErrorKind::BadPose,
            at,
        });
    }
    let mut pose = [0.0f64; 7];
    for (slot, item) in pose.iter_mut().zip(items) {
        let x = as_f64(item, line, at)?;
        if !x.is_finite() {
            return Err(ConfigError {
                line,
                kind: ConfigErrorKind::BadPose,
                at,
            });
        }
        *slot = x;
    }
    let n2 = pose[0].mul_add(
        pose[0],
        pose[1].mul_add(pose[1], pose[2].mul_add(pose[2], pose[3] * pose[3])),
    );
    if (n2 - 1.0).abs() > 2.0 * POSE_UNIT_EPS {
        return Err(ConfigError {
            line,
            kind: ConfigErrorKind::NonUnitQuaternion,
            at,
        });
    }
    Ok(pose)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
# a comment
[topology]
interp = "lerpslerp"
domain = "sensor"
frames = ["map"]
frame_headroom = 4

[[edge]]
parent = "base_footprint"
child = "base_link"
kind = "static"
pose = [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]

[[edge]]
parent = "odom"
child = "base_footprint"
kind = "dynamic"
rate_hz = 5.4
history_secs = 10.0
interp = "sclerp"
domain = 0
"#;

    /// **The documented schema parses to exactly what it says**, including the
    /// per-edge overrides that differ from the file defaults — a fixture where
    /// they agreed would assert nothing about whether overrides are read.
    ///
    /// Mutant: have `build_config` ignore an edge's `interp` key and inherit
    /// the default ⇒ `edges[1].interp` is `None` and this fails.
    #[test]
    fn the_schema_parses_to_what_it_says() {
        let c = TopologyConfig::parse(SAMPLE).unwrap();
        assert_eq!(c.default_interp, InterpPolicy::LerpSlerp);
        assert_eq!(c.default_domain, 1, "\"sensor\" is tag 1");
        assert_eq!(c.frames, ["map"]);
        assert_eq!(c.frame_headroom, 4);
        assert_eq!(c.edges.len(), 2);
        assert_eq!(
            c.edges[0].shape,
            EdgeShape::Static {
                pose: [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]
            }
        );
        assert_eq!(c.edges[0].interp, None);
        assert_eq!(
            c.edges[1].shape,
            EdgeShape::Dynamic {
                ring: RingSize::History {
                    rate_hz: 5.4,
                    secs: 10.0
                }
            }
        );
        assert_eq!(c.edges[1].interp, Some(InterpPolicy::ScLerp));
        assert_eq!(c.edges[1].domain, Some(0));
    }

    /// **`to_toml` round-trips**, which is what makes `--discover` an operator
    /// workflow rather than a demo: they edit what it printed and hand it back.
    ///
    /// Mutant: drop the `domain = …` line from `to_toml`'s `[topology]` block ⇒
    /// the reparse falls back to tag 0 while the fixture says `"sensor"` (1),
    /// and this fails. (The `.0` in [`float`] is *not* pinned here — `as_f64`
    /// accepts an integer on purpose; see that function.)
    #[test]
    fn a_config_round_trips_through_its_own_emitter() {
        let c = TopologyConfig::parse(SAMPLE).unwrap();
        let text = c.to_toml();
        let c2 = TopologyConfig::parse(&text).unwrap_or_else(|e| panic!("{e} in:\n{text}"));
        assert_eq!(c, c2);
    }

    /// **An unknown key is an error, not a shrug.** This is the whole argument
    /// for the hand-written parser: `serde` without `deny_unknown_fields` reads
    /// `capaciy = 4096` as nothing at all, and the operator gets an edge sized
    /// by the *other* branch with no message.
    ///
    /// Mutant: delete the `reject_unknown` call for edges ⇒ this parses, and
    /// the typo'd edge falls into the `MissingKey` arm — a message about the
    /// key they did not write instead of the one they did.
    #[test]
    fn a_typo_is_named_not_ignored() {
        let text = "[[edge]]\nparent=\"a\"\nchild=\"b\"\nkind=\"dynamic\"\ncapaciy = 4096\n";
        let e = TopologyConfig::parse(text).unwrap_err();
        assert_eq!(e.kind, ConfigErrorKind::UnknownKey);
        assert_eq!(e.at, "capaciy");
        assert_eq!(e.line, 5);
    }

    /// **Every error names the offending frame or edge**, and does it while
    /// borrowing from the text — `ConfigError` is `Copy` and carries no
    /// `String`.
    ///
    /// Mutant: report `at: "edge"` (a constant) instead of `child` in any one
    /// of the six arms ⇒ that row fails on `e.at`, which is the half an
    /// operator with forty edges actually needs.
    #[test]
    fn errors_name_the_offending_frame() {
        let cases: [(&str, ConfigErrorKind, &str); 6] = [
            (
                "[[edge]]\nparent=\"a\"\nchild=\"a\"\nkind=\"static\"\npose=[1.0,0.0,0.0,0.0,0.0,0.0,0.0]\n",
                ConfigErrorKind::SelfEdge,
                "a",
            ),
            (
                "[[edge]]\nparent=\"a\"\nchild=\"b\"\nkind=\"dynamic\"\ncapacity=8\n[[edge]]\nparent=\"c\"\nchild=\"b\"\nkind=\"dynamic\"\ncapacity=8\n",
                ConfigErrorKind::DuplicateChild,
                "b",
            ),
            (
                "[[edge]]\nparent=\"a\"\nchild=\"b\"\nkind=\"static\"\npose=[2.0,0.0,0.0,0.0,0.0,0.0,0.0]\n",
                ConfigErrorKind::NonUnitQuaternion,
                "b",
            ),
            (
                "[[edge]]\nparent=\"a\"\nchild=\"b\"\nkind=\"dynamic\"\ncapacity=8\nrate_hz=10.0\nhistory_secs=1.0\n",
                ConfigErrorKind::ConflictingRingSize,
                "b",
            ),
            (
                "[[edge]]\nparent=\"a\"\nchild=\"b\"\nkind=\"static\"\npose=[1.0,0.0,0.0,0.0,0.0,0.0,0.0]\ncapacity=8\n",
                ConfigErrorKind::KeyWrongForKind,
                "b",
            ),
            (
                "[[edge]]\nparent=\"\"\nchild=\"b\"\nkind=\"dynamic\"\ncapacity=8\n",
                ConfigErrorKind::BadFrameName,
                "",
            ),
        ];
        for (text, kind, at) in cases {
            let e = TopologyConfig::parse(text).unwrap_err();
            assert_eq!(e.kind, kind, "for {text:?}");
            assert_eq!(e.at, at, "for {text:?}");
        }
    }

    /// **TOML this schema does not implement is refused, never half-read.**
    ///
    /// A general parser would accept all four of these and hand back something;
    /// the danger is the *inline table*, which a naive line reader silently
    /// truncates into a valid-looking string.
    ///
    /// Mutant: fall through `'{'` to the number branch ⇒ the inline-table case
    /// reports `BadValue` on a token rather than `Unsupported`, and a reader
    /// cannot tell "I mistyped" from "you do not support this".
    #[test]
    fn unsupported_toml_is_refused_by_name() {
        for text in [
            "[topology]\nfoo.bar = 1\n",
            "[topology]\nframes = { a = 1 }\n",
            "[topology]\nframes = ['map']\n",
            "[topology]\nframes = [\n",
        ] {
            let e = TopologyConfig::parse(text).unwrap_err();
            assert_eq!(e.kind, ConfigErrorKind::Unsupported, "for {text:?}");
        }
        // …and a table nobody defined.
        let e = TopologyConfig::parse("[edges]\n").unwrap_err();
        assert_eq!(e.kind, ConfigErrorKind::UnknownTable);
        assert_eq!(e.at, "edges");
    }

    /// **The config builds a real tree with the topology it declares** — the
    /// operation §5.8's amendment says the engine was missing.
    ///
    /// The static edge must be constant-folded (capacity 0) and the dynamic one
    /// must be claimable, which is the property `Tree::claim`'s `NoEdge` denies
    /// to an `edge_headroom` slot.
    ///
    /// Mutant: build the dynamic edge with `static_edge` and an identity pose ⇒
    /// `claim` returns `NotDynamic` and this fails.
    #[test]
    fn a_config_builds_a_tree_whose_dynamic_edges_are_claimable() {
        let c = TopologyConfig::parse(SAMPLE).unwrap();
        let tree = c.builder().build().unwrap();
        let odom = tree.frame("odom").unwrap();
        let foot = tree.frame("base_footprint").unwrap();
        let base = tree.frame("base_link").unwrap();
        let map = tree.frame("map").unwrap();
        assert_ne!(map, odom, "an isolated frame is interned too");

        let w = tree
            .claim(foot, odom)
            .unwrap_or_else(|e| panic!("declared dynamic edge must be claimable: {e:?}"));
        w.push(1, &Iso3::IDENTITY).unwrap();
        // The static edge has no ring, so claiming it is refused — that is what
        // "static" means in the arena, and it is why the file has to say which.
        assert!(tree.claim(base, foot).is_err());
    }

    /// **`rate_hz` is carried into the arena as the edge's declared nominal
    /// rate, and an edge sized by `capacity` declares nothing.**
    ///
    /// This is the whole evidence path for `docs/PHASE5.md` §6's `TFT007`: the
    /// operator writes a rate in the topology file, the bridge builds the arena
    /// from that file, and `doctor` — a different process, attaching later —
    /// finds the declaration in `EdgeRecord::nominal_rate_mhz`. Break this link
    /// and `TFT007` goes back to having nothing to compare against, silently,
    /// because a `0` there means "not declared" and is a legal state.
    ///
    /// 5.4 Hz is deliberately not a whole number: an integer-hertz field would
    /// store 5, and the check would then report a correct publisher as 8% fast
    /// forever.
    ///
    /// Mutant: drop the `if let RingSize::History { rate_hz, .. }` arm from
    /// `TopologyConfig::builder`. Applied: the first assertion fails with
    /// `left: 0, right: 5400`.
    #[test]
    fn a_declared_rate_hz_reaches_the_arena_and_capacity_declares_nothing() {
        let text = "\
[[edge]]
parent = \"odom\"
child = \"base_footprint\"
kind = \"dynamic\"
rate_hz = 5.4
history_secs = 10.0

[[edge]]
parent = \"base_footprint\"
child = \"base_link\"
kind = \"dynamic\"
capacity = 512
";
        let c = TopologyConfig::parse(text).unwrap();
        let tree = c.builder().build().unwrap();
        let view = tree.arena_view();
        assert_eq!(
            view.edge(tf_tree::EdgeId(1)).unwrap().nominal_rate_mhz,
            5400,
            "rate_hz = 5.4 must reach the arena as 5400 mHz"
        );
        assert_eq!(
            view.edge(tf_tree::EdgeId(2)).unwrap().nominal_rate_mhz,
            0,
            "an edge sized by `capacity` states no rate, and 0 means undeclared"
        );
        // Non-vacuity: both edges really were built as dynamic rings, so the
        // difference above is the declaration and not the edge kind.
        assert_eq!(view.edge(tf_tree::EdgeId(1)).unwrap().capacity, 64);
        assert_eq!(view.edge(tf_tree::EdgeId(2)).unwrap().capacity, 512);
    }

    /// **A quaternion a few ulps off unit is a correct file, not a bad one.**
    ///
    /// A URDF's RPY is converted and printed by somebody else's tool. Rejecting
    /// at `1e-12` would refuse files that are right; accepting at `1e-3` would
    /// admit a mis-scaled rotation, and `Quat::rotate` mis-scales by `‖q‖²`.
    ///
    /// Mutant: tighten the bound to `1e-13` ⇒ the perturbed case is rejected.
    #[test]
    fn the_unit_quaternion_tolerance_admits_rounding_and_refuses_scaling() {
        let ok = format!(
            "[[edge]]\nparent=\"a\"\nchild=\"b\"\nkind=\"static\"\npose=[{}, 0.0, 0.0, 0.0, 0.0,0.0,0.0]\n",
            1.0 + 1e-10
        );
        assert!(TopologyConfig::parse(&ok).is_ok(), "{ok}");
        let bad = "[[edge]]\nparent=\"a\"\nchild=\"b\"\nkind=\"static\"\npose=[1.001, 0.0, 0.0, 0.0, 0.0,0.0,0.0]\n";
        assert_eq!(
            TopologyConfig::parse(bad).unwrap_err().kind,
            ConfigErrorKind::NonUnitQuaternion
        );
    }

    /// **A listed frame that is already an edge endpoint is an error.**
    ///
    /// Not pedantry: `frames` exists for lookup endpoints nothing publishes,
    /// and a name in both places usually means the operator meant to declare an
    /// edge and did not. Accepting it silently is how a frame ends up interned
    /// with no parent and every lookup through it returns `NoPath`.
    ///
    /// Mutant: push the listed frame without consulting `endpoints` ⇒ this
    /// parses and the check asserts nothing.
    #[test]
    fn a_frame_that_is_already_an_endpoint_is_rejected() {
        let text = "[topology]\nframes = [\"b\"]\n[[edge]]\nparent=\"a\"\nchild=\"b\"\nkind=\"dynamic\"\ncapacity=8\n";
        let e = TopologyConfig::parse(text).unwrap_err();
        assert_eq!(e.kind, ConfigErrorKind::RedundantFrame);
        assert_eq!(e.at, "b");
    }

    /// **§5.5's NORMATIVE startup domain check**: a bridge stamping system time
    /// refuses an edge declared in another domain, *before* the arena is built.
    ///
    /// Sim and real transforms in one arena is the bug §5.5 calls "worth making
    /// impossible", and the engine's typed domains only reject the *query* —
    /// by then the wrong stamps are already in the ring. Static edges are
    /// exempt because their constant carries no stamp, and `robot_state_publisher`
    /// stamps `/tf_static` at zero whatever `use_sim_time` says.
    ///
    /// Two mutants, both confirmed dead: drop the `declared != bridge_domain`
    /// return ⇒ the check never fires and the second assertion fails; drop the
    /// `matches!(.., Static)` continue ⇒ the statics-only file is refused and
    /// the third fails.
    #[test]
    fn a_bridge_refuses_an_edge_declared_in_another_time_domain() {
        // SAMPLE's file default is "sensor" (1); its one dynamic edge overrides
        // to 0. A bridge in domain 0 is therefore fine…
        let c = TopologyConfig::parse(SAMPLE).unwrap();
        assert_eq!(c.check_domain(0), Ok(()));
        // …and one in domain 1 is not, and is told which edge.
        let e = c.check_domain(1).unwrap_err();
        assert_eq!((e.parent, e.child), ("odom", "base_footprint"));
        assert_eq!((e.declared, e.bridge), (0, 1));

        // A static edge inheriting the mismatching default is exempt: its
        // constant has no stamp. Without the exemption every sim deployment
        // fails this check because of `/tf_static`.
        let statics_only = "[topology]\ndomain = 1\n[[edge]]\nparent=\"a\"\nchild=\"b\"\nkind=\"static\"\npose=[1.0,0.0,0.0,0.0,0.0,0.0,0.0]\n";
        let c = TopologyConfig::parse(statics_only).unwrap();
        assert_eq!(c.check_domain(0), Ok(()), "a static edge has no clock");
    }

    /// **All four built-in domains are spellable by name**, at the file default
    /// and as a per-edge override, and each resolves to the tag the *engine*
    /// defines rather than to a literal this parser repeats.
    ///
    /// `docs/PHASE4.md` §5.5 opens by saying the bridge tags every edge it
    /// declares `SimDomain` under `use_sim_time`. Until `"sim"` parsed, a
    /// deployment that wanted tag 2 had to write `2` — the state that section's
    /// amendment recorded as its own text being true of a number and not of a
    /// name. The *derivation* from `use_sim_time` is still not implemented and
    /// this test does not claim otherwise; §5.5's amendment tracks it.
    ///
    /// The last two rows are the reason the integer form is not a legacy
    /// escape: [`Domain`] is an open trait and a user-declared domain picks a
    /// free tag from 4 upwards (`docs/API.md` §2.5), so a parser that accepted
    /// only the four names would refuse the case the trait is open *for*.
    ///
    /// Mutant: drop the `Value::Str("sim")` arm ⇒ `"sim"` falls into the
    /// `Value::Str(s)` refusal and the row panics in the `unwrap_or_else`
    /// (`domain must be "system", "sensor", "sim", "steady" or 0..=255: "sim"
    /// for domain = "sim"`).
    #[test]
    fn every_built_in_domain_is_spellable_by_name() {
        let cases: [(&str, u8); 6] = [
            ("\"system\"", SystemDomain::TAG),
            ("\"sensor\"", SensorDomain::TAG),
            ("\"sim\"", SimDomain::TAG),
            ("\"steady\"", SteadyDomain::TAG),
            // A user-declared domain, which has no name to be spelled with.
            ("4", 4),
            ("255", 255),
        ];
        for (spelling, tag) in cases {
            let text = format!(
                "[topology]\ndomain = {spelling}\n\
                 [[edge]]\nparent=\"a\"\nchild=\"b\"\nkind=\"dynamic\"\ncapacity=8\ndomain = {spelling}\n"
            );
            let c = TopologyConfig::parse(&text)
                .unwrap_or_else(|e| panic!("{e} for domain = {spelling}"));
            assert_eq!(c.default_domain, tag, "[topology] domain = {spelling}");
            assert_eq!(c.edges[0].domain, Some(tag), "[[edge]] domain = {spelling}");
            // …and the check §5.5 exists for reads the same tag.
            assert_eq!(c.check_domain(tag), Ok(()), "domain = {spelling}");
        }
    }

    /// **A domain spelling that is not one of the four is refused by name, not
    /// silently taken as the default.** `"sim_time"` and `"wall"` are what an
    /// operator reaches for; a parser that shrugged would tag their edges 0 and
    /// hand the whole deployment to §5.5's bug class with no message.
    ///
    /// `256` is here because the numeric escape has a boundary: the tag space
    /// is a `u8` and one past it is a typo, not a domain.
    ///
    /// Mutant: make the `Value::Str(s)` arm return `Ok(SystemDomain::TAG)` ⇒
    /// `"sim_time"` parses and the first row panics on the `Ok(c)` arm of the
    /// `match` below (`domain = "sim_time" parsed, as tag 0`).
    #[test]
    fn a_domain_that_is_not_a_built_in_name_is_refused_by_name() {
        let cases = [
            ("\"sim_time\"", "sim_time"),
            ("\"wall\"", "wall"),
            ("256", "domain"),
        ];
        for (spelling, at) in cases {
            let text = format!("[topology]\ndomain = {spelling}\n");
            let e = match TopologyConfig::parse(&text) {
                Ok(c) => panic!("domain = {spelling} parsed, as tag {}", c.default_domain),
                Err(e) => e,
            };
            assert_eq!(e.kind, ConfigErrorKind::BadDomain, "domain = {spelling}");
            assert_eq!(e.at, at, "domain = {spelling}");
        }
    }

    /// **Ring sizing resolves the way `Capacity` documents**, so a file that
    /// says "50 Hz for 10 s" gets a ring that actually holds it.
    ///
    /// Mutant: swap `Capacity::history(rate_hz, secs)` for
    /// `Capacity::slots(rate_hz as u32)` in `RingSize::capacity` — the shape a
    /// "just use the rate" simplification takes ⇒ 64 instead of 512, and ten
    /// seconds of history become 1.28.
    #[test]
    fn ring_sizes_round_up_to_a_power_of_two() {
        assert_eq!(RingSize::Slots(5000).capacity().get(), 8192);
        assert_eq!(
            RingSize::History {
                rate_hz: 50.0,
                secs: 10.0
            }
            .capacity()
            .get(),
            512
        );
    }

    /// **A trailing comment after a table header is a comment, not an unknown
    /// table.** Comments are accepted at line start and after a value, so an
    /// operator annotating `[[edge]] # left wheel` has every reason to expect
    /// this to work — and the refusal it used to get named the wrong thing
    /// entirely: `unknown table (expected [topology] or [[edge]])` pointing at
    /// a line that says `[[edge]]`.
    ///
    /// Mutant: in `header_name`, go back to `rest.strip_suffix(close)` ⇒
    /// neither header matches and this fails on the `unwrap`.
    #[test]
    fn a_table_header_may_carry_a_trailing_comment() {
        let c = TopologyConfig::parse(
            "[topology] # main\n\
             domain = 1\n\
             [[edge]]  # left wheel\n\
             parent = \"base\"\n\
             child = \"wheel\"\n\
             kind = \"dynamic\"\n\
             capacity = 8\n",
        )
        .unwrap();
        assert_eq!(c.default_domain, 1);
        assert_eq!(c.edges.len(), 1);
        assert_eq!(c.edges[0].child, "wheel");
    }

    /// **Junk after a table header is still refused.** The comment rule must
    /// not become "ignore whatever follows the bracket": `[[edge]] [[edge]]` on
    /// one line would then parse as a single edge header and silently swallow
    /// the second.
    ///
    /// Mutant: drop the `!tail.is_empty() && !tail.starts_with('#')` check from
    /// `header_name` ⇒ both of these parse and this fails.
    #[test]
    fn junk_after_a_table_header_is_still_refused() {
        for text in ["[topology] junk\n", "[[edge]] [[edge]]\n"] {
            assert_eq!(
                TopologyConfig::parse(text).unwrap_err().kind,
                ConfigErrorKind::UnknownTable,
                "{text:?}"
            );
        }
    }

    /// **A ring whose `rate_hz * history_secs` overflows to infinity is
    /// refused, naming the child.** Both factors pass `is_finite() && > 0`
    /// individually; their product does not, and `Capacity::history`'s
    /// non-finite fallback is the *minimum*. So such an edge used to be given a
    /// **one-slot ring** — the worst ring this sizing code can produce — with
    /// no message at all.
    ///
    /// The `1e10 * 1.0` half pins that the guard rejects overflow and not
    /// merely large numbers, so this cannot pass by refusing everything big.
    ///
    /// Mutant: remove `&& (rate_hz * secs).is_finite()` ⇒ the first case parses
    /// and yields a 1-slot ring, and this fails.
    #[test]
    fn a_ring_size_that_overflows_to_infinity_is_refused() {
        let overflowing = "[[edge]]\n\
                           parent = \"a\"\n\
                           child = \"b\"\n\
                           kind = \"dynamic\"\n\
                           rate_hz = 1e300\n\
                           history_secs = 1e300\n";
        let e = TopologyConfig::parse(overflowing).unwrap_err();
        assert_eq!(e.kind, ConfigErrorKind::BadValue);
        assert_eq!(e.at, "b", "the error names the offending child");

        let big = "[[edge]]\n\
                   parent = \"a\"\n\
                   child = \"b\"\n\
                   kind = \"dynamic\"\n\
                   rate_hz = 1e10\n\
                   history_secs = 1.0\n";
        let c = TopologyConfig::parse(big).unwrap();
        assert!(
            matches!(c.edges[0].shape, EdgeShape::Dynamic { ring }
                     if ring.capacity().get() > 1),
            "a finite product must not hit the 1-slot fallback"
        );
    }

    /// **An array needs its separators.** `[1.0 0.0 …]` is not TOML, and
    /// `to_toml` always emits commas — accepting their absence means reading
    /// files this tool can never write, in the one module whose whole argument
    /// is that it refuses what it does not understand.
    ///
    /// A trailing comma stays legal, because TOML says so; without that second
    /// case the mutant below could be "fixed" by demanding a comma everywhere,
    /// which would reject a legal file.
    ///
    /// Mutant: replace the `need_comma` branch with the old optional
    /// `if let Some(r) = rest.strip_prefix(',') { rest = r.trim_start(); }` ⇒
    /// the first assertion fails.
    #[test]
    fn an_array_requires_commas_between_its_items() {
        let no_commas = "[[edge]]\n\
                         parent = \"a\"\n\
                         child = \"b\"\n\
                         kind = \"static\"\n\
                         pose = [1.0 0.0 0.0 0.0 0.0 0.0 0.0]\n";
        assert_eq!(
            TopologyConfig::parse(no_commas).unwrap_err().kind,
            ConfigErrorKind::BadValue
        );

        let trailing_comma = "[topology]\n\
                              frames = [\"map\", \"odom\",]\n";
        assert_eq!(
            TopologyConfig::parse(trailing_comma).unwrap().frames,
            ["map", "odom"],
            "TOML allows a trailing comma"
        );
    }

    /// **A cycle is reported by frame name, not by `FrameId`.** `build()` finds
    /// the same cycle and calls it `WouldCreateCycle { child: FrameId(1) }` — an
    /// index into an arena that was never constructed. This preflight exists to
    /// fail on a laptop with something an operator can act on.
    ///
    /// The acyclic half is a two-edge *chain*, not a single edge: a one-edge
    /// fixture would pass even if `cycle_child` reported any child that merely
    /// has a parent.
    ///
    /// Mutant: return `Some(cur)` unconditionally on the first loop iteration
    /// instead of on `!seen.insert(cur)` ⇒ the chain reports a cycle and this
    /// fails.
    #[test]
    fn a_cycle_is_named_by_frame_and_an_acyclic_chain_is_not() {
        let chain = "[[edge]]\n\
                     parent = \"map\"\n\
                     child = \"odom\"\n\
                     kind = \"dynamic\"\n\
                     capacity = 8\n\
                     [[edge]]\n\
                     parent = \"odom\"\n\
                     child = \"base\"\n\
                     kind = \"dynamic\"\n\
                     capacity = 8\n";
        assert_eq!(TopologyConfig::parse(chain).unwrap().cycle_child(), None);

        let cyclic = "[[edge]]\n\
                      parent = \"base\"\n\
                      child = \"odom\"\n\
                      kind = \"dynamic\"\n\
                      capacity = 8\n\
                      [[edge]]\n\
                      parent = \"odom\"\n\
                      child = \"base\"\n\
                      kind = \"dynamic\"\n\
                      capacity = 8\n";
        let c = TopologyConfig::parse(cyclic).unwrap();
        let child = c.cycle_child().unwrap();
        assert!(
            child == "base" || child == "odom",
            "names a frame on the cycle, got {child:?}"
        );
        // And the builder does refuse it, so the preflight is not inventing a
        // rule the engine does not have.
        assert!(c.builder().build().is_err());
    }
}
