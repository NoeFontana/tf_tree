//! A **synthetic** MCAP writer for tests — not a recording.
//!
//! # Say it plainly
//!
//! Nothing this module produces came off a robot. There is no rosbag2 bag in
//! this repository, and vendoring one would add tens of megabytes and a
//! licensing question for a test that needs a few kilobytes. So the fixtures
//! here are *fabricated*: real MCAP framing (written by the `mcap` crate's own
//! writer) around real CDR payloads (written by [`crate::cdr::encode_tf_message`],
//! whose byte-level agreement with the wire is proved separately against
//! hand-assembled bytes in `cdr::tests::wire_bytes_decode_w_last`).
//!
//! What that buys is a hermetic test of *this crate's* logic — schema-based
//! discovery, the two passes, every §3.2 anomaly — with no container and no
//! network. What it does **not** buy is any evidence about real recordings'
//! quirks. `docs/PHASE5.md` §0.0 records that ROS 2 is available in a container
//! and can produce a real recording; that is the test this one does not replace.
//!
//! # Uncompressed, deliberately
//!
//! [`WriteOptions::compression(None)`](mcap::WriteOptions::compression), because
//! this build of `mcap` has no codecs (see the crate docs). A fixture written
//! with zstd would be unreadable by the very code it is testing.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use crate::cdr::{encode_tf_message, TransformStamped};

/// The schema name a real `rosbag2` writes for `/tf`.
pub const TF_SCHEMA: &str = "tf2_msgs/msg/TFMessage";

/// One fabricated message: a topic, a log time, and the transforms in it.
#[derive(Clone, Debug)]
pub struct FixtureMessage {
    /// Topic to publish on. `/tf_static` (or anything ending in it) becomes a
    /// static channel by the same rule the reader uses.
    pub topic: String,
    /// MCAP log time in nanoseconds — when the recorder wrote it.
    pub log_time_ns: i64,
    /// The transforms this `TFMessage` carries.
    pub transforms: Vec<TransformStamped>,
}

impl FixtureMessage {
    /// One transform on `/tf` whose log time equals its stamp, which is the
    /// ordinary, un-anomalous case.
    #[must_use]
    pub fn dynamic(parent: &str, child: &str, stamp_ns: i64, pose: [f64; 7]) -> FixtureMessage {
        FixtureMessage {
            topic: "/tf".into(),
            log_time_ns: stamp_ns,
            transforms: vec![TransformStamped {
                stamp_ns,
                frame_id: parent.into(),
                child_frame_id: child.into(),
                pose,
            }],
        }
    }

    /// One transform on `/tf_static`.
    #[must_use]
    pub fn static_edge(parent: &str, child: &str, pose: [f64; 7]) -> FixtureMessage {
        FixtureMessage {
            topic: "/tf_static".into(),
            log_time_ns: 0,
            transforms: vec![TransformStamped {
                stamp_ns: 0,
                frame_id: parent.into(),
                child_frame_id: child.into(),
                pose,
            }],
        }
    }

    /// Move this message's log time away from its stamps, which is what §3.2's
    /// "stamps far in the future" row needs in order to be detectable at all.
    #[must_use]
    pub fn logged_at(mut self, log_time_ns: i64) -> FixtureMessage {
        self.log_time_ns = log_time_ns;
        self
    }
}

/// Write `messages` to `path` as an uncompressed MCAP.
///
/// Channels are created on first use, so the order of `messages` is the order
/// the reader will see, including out-of-order stamps.
///
/// # Errors
///
/// Any I/O or `mcap` failure, as a boxed error — this is test scaffolding and
/// its caller is a test, so a `Copy` error would buy nothing.
pub fn write_mcap(
    path: &Path,
    messages: &[FixtureMessage],
) -> Result<(), Box<dyn std::error::Error>> {
    let out = BufWriter::new(File::create(path)?);
    let mut w = mcap::WriteOptions::new()
        .compression(None)
        .profile("ros2")
        .library("tf_tree_ingest fixture (synthetic, not a recording)")
        .create(out)?;
    // An empty schema payload: MCAP requires the schema *record* to exist so
    // discovery works, and nothing in this crate parses the IDL text. A real
    // rosbag2 puts the `.msg` definition here.
    let schema = w.add_schema(TF_SCHEMA, "ros2msg", b"")?;
    let mut channels: BTreeMap<String, u16> = BTreeMap::new();
    for (sequence, m) in messages.iter().enumerate() {
        let id = match channels.get(&m.topic) {
            Some(&id) => id,
            None => {
                let id = w.add_channel(schema, &m.topic, "cdr", &BTreeMap::new())?;
                channels.insert(m.topic.clone(), id);
                id
            }
        };
        let log_time = u64::try_from(m.log_time_ns).unwrap_or(0);
        w.write_to_known_channel(
            &mcap::records::MessageHeader {
                channel_id: id,
                sequence: sequence as u32,
                log_time,
                publish_time: log_time,
            },
            &encode_tf_message(&m.transforms),
        )?;
    }
    w.finish()?;
    Ok(())
}

/// A small, non-degenerate recording: two static edges and three dynamic ones
/// at different rates, with a rotation that actually turns.
///
/// **Non-degenerate on purpose.** An earlier generation of fixtures in this
/// repository used identity poses everywhere, which makes a transposed
/// quaternion, a dropped sample and a mis-sorted ring all invisible. Every pose
/// here has a distinct quaternion and a distinct translation, and the three
/// dynamic edges publish at 100 Hz, 50 Hz and 10 Hz so a per-edge rate is a
/// real number rather than a shared one.
#[must_use]
pub fn small_recording() -> Vec<FixtureMessage> {
    let mut out = vec![
        FixtureMessage::static_edge(
            "base_link",
            "laser",
            [
                0.9238795325112867,
                0.0,
                0.0,
                0.3826834323650898,
                0.2,
                0.0,
                0.31,
            ],
        ),
        FixtureMessage::static_edge(
            "base_link",
            "imu_link",
            [
                core::f64::consts::FRAC_1_SQRT_2,
                core::f64::consts::FRAC_1_SQRT_2,
                0.0,
                0.0,
                -0.05,
                0.02,
                0.11,
            ],
        ),
    ];
    // 1 second of data. Interleave the three edges the way several publishers
    // do, so pass two's per-edge grouping is exercised rather than assumed.
    for i in 0..100i64 {
        let t = 1_000_000_000 + i * 10_000_000;
        let a = i as f64 * 0.01;
        out.push(FixtureMessage::dynamic(
            "odom",
            "base_link",
            t,
            [a.cos(), 0.0, 0.0, a.sin(), a, a * 2.0, 0.0],
        ));
        if i % 2 == 0 {
            out.push(FixtureMessage::dynamic(
                "map",
                "odom",
                t,
                [1.0, 0.0, 0.0, 0.0, 0.5 + a, -0.25, 1.0],
            ));
        }
        if i % 10 == 0 {
            out.push(FixtureMessage::dynamic(
                "base_link",
                "arm_link",
                t,
                [
                    (a * 3.0).cos(),
                    (a * 3.0).sin(),
                    0.0,
                    0.0,
                    0.0,
                    0.0,
                    0.4 + a,
                ],
            ));
        }
    }
    out
}

/// The shape of a **real** `/tf`: several publishers, each stamping at a
/// different point in its own pipeline, interleaved into one topic.
///
/// [`small_recording`] gives every edge the identical stamp at each tick, which
/// makes the merged stamp stream monotone and hides an entire class of bug — the
/// merged stream of a real recording is *not* monotone, and never was. Here
/// `odom -> base_link` is stamped as it is published (100 Hz, zero latency),
/// while `map -> odom` comes from a localization node that stamps at the scan it
/// processed and publishes `latency_ns` later (10 Hz). Nothing about this
/// recording is anomalous; it is what a navigation stack writes.
///
/// `latency_ns` is a parameter because the interesting values straddle the
/// reset threshold: at 200 ms the skew is above the 100 ms default and a
/// per-stream clock guard halts on it.
#[must_use]
pub fn two_publishers_with_latency(latency_ns: i64) -> Vec<FixtureMessage> {
    let mut out = Vec::new();
    for i in 0..100i64 {
        let t = 10_000_000_000 + i * 10_000_000;
        let a = i as f64 * 0.01;
        // Published now, stamped now.
        out.push(
            FixtureMessage::dynamic(
                "odom",
                "base_link",
                t,
                [a.cos(), 0.0, 0.0, a.sin(), a, a * 2.0, 0.5],
            )
            .logged_at(t),
        );
        if i % 10 == 0 {
            // Published now, stamped `latency_ns` ago.
            out.push(
                FixtureMessage::dynamic(
                    "map",
                    "odom",
                    t - latency_ns,
                    [1.0, 0.0, 0.0, 0.0, 0.25 + a, -0.5, 1.5],
                )
                .logged_at(t),
            );
        }
    }
    out
}
