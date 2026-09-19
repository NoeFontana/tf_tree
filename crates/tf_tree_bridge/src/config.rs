//! The topology config file (`docs/PHASE4.md` §5.8's amendment).
//!
//! The engine has no runtime edge declaration (`docs/decisions/0004`, D4), so a
//! bridge cannot learn its topology from `/tf`: it is told before the arena
//! exists, in this format (`docs/PHASE2.md` §9's `tf_treed --config`).
//!
//! The parser is hand-written: the workspace has no TOML dependency, and a
//! general parser plus `serde` silently drops unknown keys (a `capaciy = 4096`
//! typo would size an edge 1). Unsupported constructs (dotted keys, inline
//! tables, literal and multi-line strings, datetimes, multi-line arrays, any
//! table but `[topology]` / `[[edge]]`) are a [`ConfigErrorKind::Unsupported`]
//! naming the line, never a silent skip.
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
//! [`TopologyConfig::check_domain`] refuses, at startup, any *dynamic* edge whose
//! resolved domain differs from the bridge's own tag (`time_domain` parameter,
//! `tft_bridge_options::domain`); static edges are exempt.
//!
//! `rate_hz` both sizes the ring and is recorded in the arena as the edge's
//! declared nominal rate (`EdgeRecord::nominal_rate_mhz`), the evidence
//! `TFT007` judges against. An edge sized by `capacity` declares no rate.
//!
//! [`ConfigError`] borrows `&str` from the config text, so validation runs in
//! [`TopologyConfig::parse`] while the source is in hand.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use tf_tree::{
    Capacity, Domain, EdgeCfg, InterpPolicy, Iso3, Quat, SensorDomain, SimDomain, SteadyDomain,
    SystemDomain, TreeBuilder, Vec3,
};

use crate::names::NameNormalizer;

/// How a dynamic edge's ring is sized: a slot count or a rate plus history.
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

/// What an edge declaration describes. Not `EdgeKind`: `tf_tree::EdgeKind`
/// is the arena's record of the same distinction.
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
    /// Spare frame-name slots for `Tree::frame()` at runtime. There is no
    /// `edge_headroom`: those slots would have zero capacity (§5.8).
    pub frame_headroom: u32,
    /// Default interpolation for dynamic edges that do not override it.
    pub default_interp: InterpPolicy,
    /// Default time-domain tag for edges that do not override it. A `u8` because
    /// [`Domain`] is an open trait (user tags start at 4, `docs/API.md` §2.5).
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

    /// Check every dynamic edge against the domain the bridge stamps in.
    ///
    /// `docs/PHASE4.md` §5.5, **NORMATIVE**: the bridge fails at startup, not at
    /// first message, on a declared domain differing from its own.
    ///
    /// # Errors
    ///
    /// [`DomainMismatch`] naming the first offending edge in file order.
    pub fn check_domain(&self, bridge_domain: u8) -> Result<(), DomainMismatch<'_>> {
        for e in &self.edges {
            // Static edges are exempt: no stamp to be wrong about, and
            // `robot_state_publisher` stamps them zero under `use_sim_time`.
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

    /// The child frame that closes a parent cycle, if any, by name (`build()`
    /// reports only an index into an arena never constructed).
    ///
    /// Not folded into [`TopologyConfig::parse`]: `ConfigError` is `Copy` and
    /// borrows from the text, and a cycle is a property of the owned edge set.
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

    /// This topology with every declared frame name put through `names` (§5.6,
    /// `tf_prefix` included).
    ///
    /// The config is the sole source of declared edges, so it must be rewritten
    /// with the wire: otherwise a prefixed bridge misses every declared edge.
    /// The same `NameNormalizer` the wire uses is passed in, which also
    /// populates its remap table before the first message (§5.6's startup log).
    /// A name that does not normalize (a bare `"/"`) is kept verbatim.
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

    /// A [`TreeBuilder`] carrying exactly this topology, declared before
    /// `build()` (§5.8's amendment).
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
                    // `rate_hz` is also the arena's nominal for `TFT007`; a
                    // `capacity` edge declares none (0).
                    if let RingSize::History { rate_hz, .. } = ring {
                        cfg = cfg.nominal_rate_hz(rate_hz);
                    }
                    b.dynamic_edge(&e.parent, &e.child, cfg)
                }
            };
        }
        b
    }

    /// Render back to the file format; parsing the result yields an equal
    /// [`TopologyConfig`].
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

/// `[qw qx qy qz tx ty tz]` as an [`Iso3`], without renormalizing:
/// [`TopologyConfig::parse`] already refused a non-unit quaternion.
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

/// A TOML basic string. No escaping: [`check_frame_name`] refuses `"`, `\` and
/// control characters, so the emitter and parser agree.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    out.push_str(s);
    out.push('"');
    out
}

/// An `f64` as a TOML float, round-tripping exactly (`.0` appended to a whole
/// number). Cosmetic: [`as_f64`] accepts an integer, so no test pins the `.0`.
fn float(v: f64) -> String {
    let s = format!("{v:?}");
    if s.contains('.') || s.contains('e') || s.contains('E') {
        s
    } else {
        format!("{s}.0")
    }
}

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
    /// A frame listed in `frames` is already an edge endpoint.
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
/// `Copy`; `at` borrows from the config text.
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

/// A declared edge whose time domain is not the bridge's (§5.5). `Copy`, borrowing from the config.
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

/// How far a config quaternion may be from unit norm. `1e-9`: URDF-derived
/// files carry finite digits; looser lets a mis-scaled rotation through.
pub const POSE_UNIT_EPS: f64 = 1e-9;

/// One TOML value, in the four scalar kinds this schema uses plus arrays.
#[derive(Clone, Debug, PartialEq)]
enum Value<'a> {
    Str(&'a str),
    Int(i64),
    Float(f64),
    Array(Vec<Value<'a>>),
}

/// A table's key/value pairs with their lines, so a combination error can name one.
type Table<'a> = Vec<(&'a str, Value<'a>, u32)>;

impl TopologyConfig {
    /// Parse a topology file.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] naming the line and the offending frame, edge or key;
    /// every construct outside the schema is an error, not a skip.
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
/// A trailing comment is accepted (`[[edge]] # left wheel`); anything else after
/// the bracket is refused.
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

/// `key = value`; only a comment may follow the value.
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
    // A dotted key is valid TOML this schema has no place for; refuse it.
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
            // `"""` (multi-line) is refused, not parsed as an empty string plus junk.
            if s.starts_with("\"\"\"") {
                return Err(err(ConfigErrorKind::Unsupported, s));
            }
            for (i, c) in s.char_indices().skip(1) {
                match c {
                    // No escapes: `check_frame_name` rejects `"` and `\`, so
                    // `quote` never emits one and the parser never decodes one.
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
            // Commas are required (`[1.0 0.0]` is not TOML and `to_toml` never
            // writes it); a trailing comma stays legal.
            let mut need_comma = false;
            loop {
                if let Some(r) = rest.strip_prefix(']') {
                    return Ok((Value::Array(items), r));
                }
                if rest.is_empty() {
                    // Multi-line arrays: the parser is line-oriented.
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
                // No key in this schema is a boolean.
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
        // A hand-written `history_secs = 10` is an integer; accept it.
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
/// Names resolve through each built-in's [`Domain::TAG`] (permanent, `docs/API.md`
/// §2.5). The integer form stays: [`Domain`] is an open trait and user tags
/// start at 4.
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

/// A frame name usable as both an arena key and a TOML basic string (see
/// [`quote`]).
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

/// Whether a frame name can be written to a config file and read back; shared
/// by the parser and [`crate::Discovery`] so a discovered config reparses.
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

    // Edges, then cross-edge checks.
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
                        // The product must be finite too: an overflow makes
                        // `Capacity::history` fall back to a one-slot ring.
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
                    // Under- or over-specified: a half-edit, not a guess.
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

    /// The documented schema parses to what it says, with per-edge overrides
    /// that differ from the defaults.
    ///
    /// Mutant: `build_config` ignores an edge's `interp`.
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

    /// `to_toml` round-trips.
    ///
    /// Mutant: drop the `domain = ...` line from `to_toml`'s `[topology]` block.
    #[test]
    fn a_config_round_trips_through_its_own_emitter() {
        let c = TopologyConfig::parse(SAMPLE).unwrap();
        let text = c.to_toml();
        let c2 = TopologyConfig::parse(&text).unwrap_or_else(|e| panic!("{e} in:\n{text}"));
        assert_eq!(c, c2);
    }

    /// An unknown key is an error, not a shrug.
    ///
    /// Mutant: delete the edge `reject_unknown` call ⇒ a `MissingKey` on the
    /// wrong key.
    #[test]
    fn a_typo_is_named_not_ignored() {
        let text = "[[edge]]\nparent=\"a\"\nchild=\"b\"\nkind=\"dynamic\"\ncapaciy = 4096\n";
        let e = TopologyConfig::parse(text).unwrap_err();
        assert_eq!(e.kind, ConfigErrorKind::UnknownKey);
        assert_eq!(e.at, "capaciy");
        assert_eq!(e.line, 5);
    }

    /// Every error names the offending frame or edge.
    ///
    /// Mutant: report a constant `at` instead of `child` in any arm.
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

    /// TOML this schema does not implement is refused, never half-read.
    ///
    /// Mutant: fall through `'{'` to the number branch ⇒ `BadValue`, not
    /// `Unsupported`.
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

    /// The config builds a real tree: the static edge is constant-folded and
    /// the dynamic one is claimable.
    ///
    /// Mutant: build the dynamic edge with `static_edge` ⇒ `claim` fails.
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
        // A static edge has no ring, so claiming it is refused.
        assert!(tree.claim(base, foot).is_err());
    }

    /// `rate_hz` reaches the arena as the edge's declared nominal
    /// (`docs/PHASE5.md` §6, `TFT007`); a `capacity` edge declares nothing. 5.4
    /// Hz is fractional so an integer-hertz field would fail.
    ///
    /// Mutant: drop the `RingSize::History` arm from `TopologyConfig::builder`.
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
        // Non-vacuity: both are dynamic rings.
        assert_eq!(view.edge(tf_tree::EdgeId(1)).unwrap().capacity, 64);
        assert_eq!(view.edge(tf_tree::EdgeId(2)).unwrap().capacity, 512);
    }

    /// A quaternion a few ulps off unit is accepted; a scaled one is refused.
    ///
    /// Mutant: tighten the bound to `1e-13`.
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

    /// A listed frame that is already an edge endpoint is an error.
    ///
    /// Mutant: push the listed frame without consulting `endpoints`.
    #[test]
    fn a_frame_that_is_already_an_endpoint_is_rejected() {
        let text = "[topology]\nframes = [\"b\"]\n[[edge]]\nparent=\"a\"\nchild=\"b\"\nkind=\"dynamic\"\ncapacity=8\n";
        let e = TopologyConfig::parse(text).unwrap_err();
        assert_eq!(e.kind, ConfigErrorKind::RedundantFrame);
        assert_eq!(e.at, "b");
    }

    /// §5.5's NORMATIVE startup domain check: an edge declared in another domain
    /// is refused before the arena is built; static edges are exempt.
    ///
    /// Mutants: drop the `declared != bridge_domain` return; drop the static
    /// `continue`.
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

        let statics_only = "[topology]\ndomain = 1\n[[edge]]\nparent=\"a\"\nchild=\"b\"\nkind=\"static\"\npose=[1.0,0.0,0.0,0.0,0.0,0.0,0.0]\n";
        let c = TopologyConfig::parse(statics_only).unwrap();
        assert_eq!(c.check_domain(0), Ok(()), "a static edge has no clock");
    }

    /// All four built-in domains are spellable by name (file default and
    /// per-edge), resolving to the engine's tags; the integer form stays for
    /// user tags from 4 (`docs/API.md` §2.5).
    ///
    /// Mutant: drop the `Value::Str("sim")` arm.
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

    /// An unknown domain spelling (`"sim_time"`, `256`) is refused by name, not
    /// taken as the default.
    ///
    /// Mutant: make the `Value::Str(s)` arm return `Ok(SystemDomain::TAG)`.
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

    /// Ring sizing resolves the way `Capacity` documents.
    ///
    /// Mutant: `Capacity::slots(rate_hz as u32)` in `RingSize::capacity`.
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

    /// A trailing comment after a table header is a comment.
    ///
    /// Mutant: `header_name` back to `rest.strip_suffix(close)`.
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

    /// Junk after a table header is still refused (`[[edge]] [[edge]]`).
    ///
    /// Mutant: drop the `!tail.starts_with('#')` check from `header_name`.
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

    /// A ring whose `rate_hz * history_secs` overflows to infinity is refused,
    /// naming the child; `1e10 * 1.0` pins that only overflow is refused.
    ///
    /// Mutant: remove `&& (rate_hz * secs).is_finite()`.
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

    /// An array needs its separators; a trailing comma stays legal.
    ///
    /// Mutant: make the comma optional in the `need_comma` branch.
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

    /// A cycle is reported by frame name; the acyclic half is a two-edge chain
    /// so "any child with a parent" cannot pass.
    ///
    /// Mutant: return `Some(cur)` unconditionally on the first iteration.
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
