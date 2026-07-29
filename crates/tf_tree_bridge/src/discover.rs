//! `--discover`: watch a `/tf` stream, print the config file it implies.
//!
//! `docs/PHASE4.md` §5.8's amendment: *"A `--discover` mode that subscribes,
//! collects and prints a config file is how an operator obtains that file."*
//!
//! It is the answer to the obvious objection to config-driven topology — that
//! nobody wants to hand-write forty edges for a robot whose URDF already knows
//! them. Run the bridge in discover mode against the running system (or, with
//! no ROS at all, against a recorded `.tfstream`), read the file it prints,
//! edit the ring sizes, ship it.
//!
//! # What it is *not*
//!
//! It is not a substitute for reading the file. Two things it finds are
//! **defects in the observed system**, not topology to encode, and it reports
//! them rather than silently resolving them:
//!
//! * **A child with two parents.** `tf_tree` gives a frame exactly one parent
//!   (D4/`0004`); `tf2` lets two publishers give it two and re-parents on every
//!   message. First parent seen wins here, the rest are counted and named — and
//!   because [`TopologyConfig::parse`] refuses a duplicate child outright, a
//!   config that kept both would not even reparse.
//! * **An edge on both `/tf` and `/tf_static`.** §5.7 calls a kind change a
//!   hard error. First topic seen wins, the clash is counted.
//!
//! # Rates
//!
//! A dynamic edge's ring is sized from the rate actually observed —
//! `(samples − 1) / span`, which is the mean interval and not the nominal rate
//! the launch file claims. It is rounded **up** to two decimals so a recording
//! that saw 4.29 Hz never produces a ring sized for 4.28, and then
//! [`Capacity::history`] rounds up to a power of two on top of that. An edge
//! with fewer than two samples has no measurable rate at all and gets an
//! explicit slot count instead of a fabricated one.

use std::collections::{BTreeMap, BTreeSet};

use tf_tree::InterpPolicy;

use crate::config::{frame_name_ok, EdgeConfig, EdgeShape, RingSize, TopologyConfig};
use crate::ingest::Topic;
use crate::names::NameNormalizer;
use crate::statics::StaticKind;
use crate::Sample;

/// What was seen for one `(parent, child)` edge.
#[derive(Clone, Debug)]
struct Seen {
    kind: StaticKind,
    count: u64,
    first_ns: i64,
    last_ns: i64,
    /// The first pose observed, kept only for static edges. **First**, not
    /// last, so discovery agrees with `FirstWriterWins` (§5.4): if two
    /// publishers disagree about a static, the config records the one the
    /// bridge would have kept.
    pose: [f64; 7],
    /// Set when a later sample for this edge arrived on the other topic.
    kind_clash: bool,
}

/// Collects the topology a stream actually contains.
#[derive(Debug)]
pub struct Discovery {
    names: NameNormalizer,
    edges: BTreeMap<(String, String), Seen>,
    /// child -> the parent that owns it, so a second parent is detectable.
    parent_of: BTreeMap<String, String>,
    /// child -> **every** parent rejected for it, not just the last one. A map
    /// to a single `String` reported one parent for a child that had three,
    /// which understates exactly the defect this collector exists to surface.
    rejected_parents: BTreeMap<String, BTreeSet<String>>,
    dropped_multi_parent: u64,
    dropped_bad_name: u64,
    history_secs: f64,
    default_interp: InterpPolicy,
}

/// The fallback ring for an edge whose rate could not be measured.
///
/// Reached only when an edge produced fewer than two samples in the whole
/// recording, so any rate would be invented. 64 slots is a placeholder loud
/// enough to be noticed in the printed file and small enough that shipping it
/// unedited wastes nothing.
pub const UNMEASURABLE_RATE_SLOTS: u32 = 64;

impl Discovery {
    /// A collector that will size rings to hold `history_secs` of samples.
    #[must_use]
    pub fn new(history_secs: f64) -> Discovery {
        Discovery {
            names: NameNormalizer::new(),
            edges: BTreeMap::new(),
            parent_of: BTreeMap::new(),
            rejected_parents: BTreeMap::new(),
            dropped_multi_parent: 0,
            dropped_bad_name: 0,
            history_secs,
            default_interp: InterpPolicy::ScLerp,
        }
    }

    /// Apply a `tf_prefix` while collecting (§5.6), so the printed config uses
    /// the names the bridge will key on rather than the ones on the wire.
    #[must_use]
    pub fn with_prefix(mut self, prefix: &str) -> Discovery {
        self.names = NameNormalizer::with_prefix(prefix);
        self
    }

    /// Set the interpolation policy the printed config will default to.
    #[must_use]
    pub fn with_interp(mut self, interp: InterpPolicy) -> Discovery {
        self.default_interp = interp;
        self
    }

    /// Record one transform.
    ///
    /// Names are normalized exactly as [`crate::Ingest`] normalizes them, so
    /// the config this prints is keyed the way the bridge will look it up. A
    /// discovered config keyed on `/base_link` while the running bridge keys on
    /// `base_link` would declare every edge and match none.
    pub fn observe(&mut self, topic: Topic, sample: &Sample) {
        let (Ok(parent), Ok(child)) = (
            self.names.normalize(&sample.frame_id),
            self.names.normalize(&sample.child_frame_id),
        ) else {
            self.dropped_bad_name += 1;
            return;
        };
        let (parent, child) = (parent.name, child.name);
        if parent == child {
            self.dropped_bad_name += 1;
            return;
        }
        // `NameNormalizer` refuses only the empty name and a bare `/` — it is
        // §5.6's wire rule, not a file rule. This collector's output is a
        // *config file*, so a name has to survive being written and read back,
        // and only `frame_name_ok` decides that. Without this a robot
        // publishing `odo"m` produced `parent = "odo"m"`, which this crate's
        // own parser refuses — so `--discover > topology.toml` emitted a file
        // that `--config` could not read, and the operator found out on the
        // robot rather than on the laptop.
        if !frame_name_ok(&parent) || !frame_name_ok(&child) {
            self.dropped_bad_name += 1;
            return;
        }
        match self.parent_of.get(&child) {
            Some(p) if *p != parent => {
                // A second parent for one child. Recorded, not encoded: see the
                // module docs.
                self.dropped_multi_parent += 1;
                self.rejected_parents
                    .entry(child)
                    .or_default()
                    .insert(parent);
                return;
            }
            Some(_) => {}
            None => {
                self.parent_of.insert(child.clone(), parent.clone());
            }
        }
        let kind = match topic {
            Topic::Tf => StaticKind::Dynamic,
            Topic::TfStatic => StaticKind::Static,
        };
        self.edges
            .entry((parent, child))
            .and_modify(|s| {
                s.count += 1;
                s.first_ns = s.first_ns.min(sample.stamp_nanos);
                s.last_ns = s.last_ns.max(sample.stamp_nanos);
                if s.kind != kind {
                    s.kind_clash = true;
                }
            })
            .or_insert(Seen {
                kind,
                count: 1,
                first_ns: sample.stamp_nanos,
                last_ns: sample.stamp_nanos,
                pose: sample.pose,
                kind_clash: false,
            });
    }

    /// The config this stream implies.
    #[must_use]
    pub fn to_config(&self) -> TopologyConfig {
        let mut out = TopologyConfig {
            default_interp: self.default_interp,
            ..TopologyConfig::default()
        };
        for ((parent, child), seen) in &self.edges {
            let shape = match seen.kind {
                StaticKind::Static => EdgeShape::Static { pose: seen.pose },
                StaticKind::Dynamic => EdgeShape::Dynamic {
                    ring: match measured_rate(seen) {
                        Some(rate_hz) => RingSize::History {
                            rate_hz,
                            secs: self.history_secs,
                        },
                        None => RingSize::Slots(UNMEASURABLE_RATE_SLOTS),
                    },
                },
            };
            out.edges.push(EdgeConfig {
                parent: parent.clone(),
                child: child.clone(),
                shape,
                interp: None,
                domain: None,
            });
        }
        out
    }

    /// Children seen with more than one parent, as `(child, rejected parent)`.
    ///
    /// Non-empty means the observed system is doing something `tf_tree` cannot
    /// represent and `tf2` was hiding — the §5.4 class of finding. The caller
    /// must surface it; the printed config keeps the **first** parent.
    #[must_use]
    pub fn multi_parent(&self) -> Vec<(&str, &str)> {
        self.rejected_parents
            .iter()
            .flat_map(|(c, ps)| ps.iter().map(move |p| (c.as_str(), p.as_str())))
            .collect()
    }

    /// Transforms discarded because their child already had a different parent.
    ///
    /// The module docs promise a second parent's samples are *"counted and
    /// named"*; [`Discovery::multi_parent`] names them and this counts them.
    #[must_use]
    pub fn dropped_multi_parent(&self) -> u64 {
        self.dropped_multi_parent
    }

    /// Edges that arrived on both `/tf` and `/tf_static` — §5.7's hard error,
    /// found before the bridge is ever started.
    #[must_use]
    pub fn kind_clashes(&self) -> Vec<(&str, &str)> {
        self.edges
            .iter()
            .filter(|(_, s)| s.kind_clash)
            .map(|((p, c), _)| (p.as_str(), c.as_str()))
            .collect()
    }

    /// Transforms discarded because a frame name was unusable (§5.6), because
    /// parent and child were the same frame, or because the name could not be
    /// written to a config file and read back (`"`, `\`, a control character).
    #[must_use]
    pub fn dropped_bad_name(&self) -> u64 {
        self.dropped_bad_name
    }

    /// How many samples each discovered edge contributed, for the report.
    #[must_use]
    pub fn sample_counts(&self) -> Vec<(&str, &str, u64)> {
        self.edges
            .iter()
            .map(|((p, c), s)| (p.as_str(), c.as_str(), s.count))
            .collect()
    }
}

/// The mean rate over the observed span, rounded **up** to two decimals.
///
/// `None` when fewer than two samples arrived or they all carried the same
/// stamp: there is no rate to measure, and inventing one puts a number in the
/// operator's file that no observation supports.
///
/// **This is an observation, and `rate_hz` in the emitted file is read back as
/// an intention.** Since `docs/PHASE5.md` §6's amendment, `rate_hz` is also the
/// arena's declared nominal, which `tf_tree doctor`'s `TFT007` judges the robot
/// against — so a recording captured while a publisher was degraded discovers
/// the fault as the declaration, and `doctor` would then certify the fault and
/// fire once the publisher is repaired. Nothing here can tell the two apart;
/// the mitigation is that `--discover` prints each edge's sample count and the
/// amendment states that a discovered rate is a starting point to review.
fn measured_rate(s: &Seen) -> Option<f64> {
    let span_ns = s.last_ns.checked_sub(s.first_ns)?;
    if s.count < 2 || span_ns <= 0 {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    let secs = span_ns as f64 / 1e9;
    #[allow(clippy::cast_precision_loss)]
    let rate = (s.count - 1) as f64 / secs;
    let rounded = (rate * 100.0).ceil() / 100.0;
    rounded.is_finite().then_some(rounded.max(0.01))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::config::ConfigErrorKind;

    fn dyn_sample(p: &str, c: &str, t: i64) -> Sample {
        Sample::identity(p, c, t)
    }

    /// **A discovered config parses back, and the round trip is what makes the
    /// mode usable at all**: the operator edits what was printed.
    ///
    /// The fixture is deliberately irregular — two edges at different rates,
    /// jittered stamps, a static, and a `/`-prefixed spelling of a frame that
    /// also appears bare — so the emitted rates are not round numbers and the
    /// normalization is actually exercised.
    ///
    /// Mutant: emit `rate_hz` without [`crate::config::float`]'s `.0` guard for
    /// a whole-number rate and the reparse still works (`as_f64` accepts an
    /// integer); emit the two spellings as two edges instead of normalizing and
    /// this fails with `DuplicateChild`, which is the real property.
    #[test]
    fn a_discovered_config_reparses_to_itself() {
        let mut d = Discovery::new(10.0);
        d.observe(
            Topic::TfStatic,
            &Sample {
                pose: [1.0, 0.0, 0.0, 0.0, 0.58, 0.0, 0.32],
                ..Sample::identity("base_footprint", "camera_link", 0)
            },
        );
        for k in 0..100i64 {
            // 20 Hz nominal with ±1 ms of jitter, so the measured rate is not
            // exactly 20.
            d.observe(
                Topic::Tf,
                &dyn_sample(
                    "odom",
                    "base_footprint",
                    k * 50_000_000 + (k % 3) * 1_000_000,
                ),
            );
            if k % 4 == 0 {
                // 5 Hz, and spelled with a leading slash half the time.
                let (p, c) = if k % 8 == 0 {
                    ("/base_footprint", "/base_link")
                } else {
                    ("base_footprint", "base_link")
                };
                d.observe(Topic::Tf, &dyn_sample(p, c, k * 50_000_000));
            }
        }
        assert!(d.multi_parent().is_empty());
        assert!(d.kind_clashes().is_empty());

        let cfg = d.to_config();
        assert_eq!(cfg.edges.len(), 3, "one static, two dynamic");
        let text = cfg.to_toml();
        let back = TopologyConfig::parse(&text).unwrap_or_else(|e| panic!("{e} in:\n{text}"));
        assert_eq!(cfg, back);

        // The slash-prefixed spelling collapsed into the bare one rather than
        // becoming a second edge.
        assert!(cfg.edge("base_footprint", "base_link").is_some());
        assert!(cfg.edge("/base_footprint", "/base_link").is_none());

        // …and it builds a tree.
        let tree = cfg.builder().build().unwrap();
        let odom = tree.frame("odom").unwrap();
        let foot = tree.frame("base_footprint").unwrap();
        assert!(tree.claim(foot, odom).is_ok());
    }

    /// **The measured rate is the observed one, not the nominal one**, and it
    /// is rounded **up** so a ring is never sized for a rate slower than what
    /// arrived.
    ///
    /// The interval is chosen so `ceil` and `round` disagree — 19.783001… Hz
    /// gives 19.79 one way and 19.78 the other. A fixture landing on a round
    /// number (99 samples over exactly 5 s ⇒ 19.8) would assert nothing about
    /// the direction, which is the only interesting half.
    ///
    /// Mutant: `.round()` instead of `.ceil()` in `measured_rate` ⇒ 19.78, and
    /// this fails.
    #[test]
    fn the_rate_is_measured_and_rounded_up() {
        let mut d = Discovery::new(10.0);
        // 100 samples, 99 intervals of 50.548446 ms ⇒ 19.783001835… Hz.
        for k in 0..100i64 {
            d.observe(Topic::Tf, &dyn_sample("a", "b", k * 50_548_446));
        }
        let cfg = d.to_config();
        match cfg.edges[0].shape {
            EdgeShape::Dynamic {
                ring: RingSize::History { rate_hz, secs },
            } => {
                assert!(
                    (rate_hz - 19.79).abs() < 1e-12,
                    "19.783001… rounded up to two decimals, got {rate_hz}"
                );
                assert!((secs - 10.0).abs() < 1e-12);
            }
            ref other => panic!("{other:?}"),
        }
    }

    /// **An edge with one sample has no rate**, so it gets an explicit slot
    /// count rather than a fabricated frequency.
    ///
    /// Mutant: return `Some(1.0)` from `measured_rate` on the degenerate case ⇒
    /// the file says `rate_hz = 1.0` about an edge nothing measured, and an
    /// operator has no way to tell it apart from a real 1 Hz edge.
    #[test]
    fn an_unmeasurable_rate_is_not_invented() {
        let mut d = Discovery::new(10.0);
        d.observe(Topic::Tf, &dyn_sample("a", "b", 7));
        // …and neither is one from two samples that share a stamp.
        d.observe(Topic::Tf, &dyn_sample("c", "e", 7));
        d.observe(Topic::Tf, &dyn_sample("c", "e", 7));
        let cfg = d.to_config();
        for e in &cfg.edges {
            assert_eq!(
                e.shape,
                EdgeShape::Dynamic {
                    ring: RingSize::Slots(UNMEASURABLE_RATE_SLOTS)
                },
                "{}: {:?}",
                e.child,
                e.shape
            );
        }
    }

    /// **A child with two parents is reported, not encoded** — and it must be,
    /// because a config carrying both does not even reparse.
    ///
    /// This is the §5.4 class of finding: `tf2` re-parents on every message and
    /// says nothing, so a system can run for months like this.
    ///
    /// Mutant: drop the `parent_of` check and let both edges in ⇒ `to_config`
    /// emits two `[[edge]]` blocks with `child = "base_link"` and the reparse
    /// fails with `DuplicateChild`, which this asserts cannot happen.
    #[test]
    fn a_second_parent_is_reported_and_the_config_still_reparses() {
        let mut d = Discovery::new(10.0);
        for k in 0..10i64 {
            d.observe(Topic::Tf, &dyn_sample("odom", "base_link", k * 10_000_000));
            d.observe(Topic::Tf, &dyn_sample("map", "base_link", k * 10_000_000));
        }
        assert_eq!(d.multi_parent(), [("base_link", "map")]);
        let cfg = d.to_config();
        assert_eq!(cfg.edges.len(), 1);
        assert_eq!(cfg.edges[0].parent, "odom", "first parent seen wins");
        let text = cfg.to_toml();
        assert!(
            TopologyConfig::parse(&text).is_ok(),
            "a config that kept both parents would fail with {:?}",
            ConfigErrorKind::DuplicateChild
        );
    }

    /// **An edge on both topics is a §5.7 hard error, found before startup.**
    ///
    /// Mutant: never set `kind_clash` ⇒ this reports nothing and the operator
    /// meets the failure at run time, one dropped transform at a time.
    #[test]
    fn an_edge_on_both_topics_is_reported() {
        let mut d = Discovery::new(10.0);
        d.observe(Topic::TfStatic, &dyn_sample("base", "lidar", 0));
        d.observe(Topic::Tf, &dyn_sample("base", "lidar", 1_000_000));
        assert_eq!(d.kind_clashes(), [("base", "lidar")]);
        // The first topic wins, so the edge is still declared — as static.
        let cfg = d.to_config();
        assert!(matches!(cfg.edges[0].shape, EdgeShape::Static { .. }));
    }

    /// **An unusable frame name is dropped and counted**, not turned into an
    /// edge named `""` that the config parser then rejects.
    ///
    /// Mutant: drop the `parent == child` guard ⇒ the self-edge is counted as
    /// an edge (`dropped_bad_name` falls to 1) and `to_config` emits a block
    /// whose parent and child are the same frame — a file the parser refuses
    /// with `SelfEdge`.
    #[test]
    fn a_bad_name_never_reaches_the_config() {
        let mut d = Discovery::new(10.0);
        d.observe(Topic::Tf, &dyn_sample("/", "base", 0));
        d.observe(Topic::Tf, &dyn_sample("base", "base", 0));
        assert_eq!(d.dropped_bad_name(), 2);
        assert!(d.to_config().edges.is_empty());
    }

    /// **A frame name that cannot survive the config file is dropped here, not
    /// discovered into an unparseable file.**
    ///
    /// `NameNormalizer` is §5.6's *wire* rule and refuses only the empty name
    /// and a bare `/`. A robot publishing `odo"m` therefore used to be written
    /// out as `parent = "odo"m"`, which this crate's own parser refuses — so
    /// `--discover > topology.toml` produced a file `--config` could not read,
    /// and the operator met the failure on the robot. A name holding a newline
    /// was worse: it emitted a structurally broken block.
    ///
    /// The good edge in the fixture is non-degenerate on purpose: without it
    /// the config would be empty and `parse` would trivially succeed, so this
    /// would pass even if `observe` dropped *everything*.
    ///
    /// Mutant: delete the `frame_name_ok` guard from `Discovery::observe` ⇒ the
    /// emitted text no longer reparses and the `unwrap` fails.
    #[test]
    fn a_name_that_cannot_be_written_to_a_config_is_not_discovered() {
        let mut d = Discovery::new(10.0);
        for (p, c) in [
            ("base", "odo\"m"),
            ("ba\\se", "wheel"),
            ("base", "line\nbreak"),
            ("base", "bell\u{7}"),
        ] {
            d.observe(Topic::Tf, &dyn_sample(p, c, 0));
            d.observe(Topic::Tf, &dyn_sample(p, c, 50_000_000));
        }
        // …and one edge that is perfectly fine, so the config is not empty.
        d.observe(Topic::Tf, &dyn_sample("base", "wheel", 0));
        d.observe(Topic::Tf, &dyn_sample("base", "wheel", 50_000_000));

        assert_eq!(d.dropped_bad_name(), 8, "two samples each, all refused");
        let config = d.to_config();
        assert_eq!(config.edges.len(), 1, "only the usable edge survives");
        assert_eq!(config.edges[0].child, "wheel");

        let text = config.to_toml();
        assert!(
            TopologyConfig::parse(&text).is_ok(),
            "a discovered config must reparse; got {text}"
        );
    }

    /// **Every rejected parent is counted and named**, which is what the module
    /// docs promise and what a `BTreeMap<String, String>` could not deliver: it
    /// overwrote, so a child with three parents reported only the last one, and
    /// no counter moved at all.
    ///
    /// Three parents, not two: with two, "reports only the last" and "reports
    /// all of them" are indistinguishable once the first is the incumbent.
    ///
    /// Mutant: change `rejected_parents` back to `insert(child, parent)` over a
    /// `BTreeMap<String, String>` ⇒ `multi_parent()` has one entry, not two.
    /// Mutant: drop `self.dropped_multi_parent += 1` ⇒ the count assertion
    /// fails.
    #[test]
    fn every_rejected_parent_is_counted_and_named() {
        let mut d = Discovery::new(10.0);
        d.observe(Topic::Tf, &dyn_sample("odom", "base", 0));
        d.observe(Topic::Tf, &dyn_sample("map", "base", 10_000_000));
        d.observe(Topic::Tf, &dyn_sample("world", "base", 20_000_000));
        d.observe(Topic::Tf, &dyn_sample("map", "base", 30_000_000));

        let mut rejected: Vec<&str> = d.multi_parent().iter().map(|(_, p)| *p).collect();
        rejected.sort_unstable();
        assert_eq!(rejected, ["map", "world"], "both losers are named");
        assert_eq!(
            d.dropped_multi_parent(),
            3,
            "every sample for a second parent is counted, repeats included"
        );
        // The first parent seen still wins, per §5.4 / the module docs.
        let config = d.to_config();
        assert_eq!(config.edges.len(), 1);
        assert_eq!(config.edges[0].parent, "odom");
    }
}
