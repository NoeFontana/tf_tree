//! Reading `tf2_msgs/msg/TFMessage` out of an MCAP recording — `docs/PHASE5.md`
//! §3.3.
//!
//! # Discovery is by schema, not by topic name — §3.3 is explicit
//!
//! A channel is a TF channel when its **schema** is `tf2_msgs/msg/TFMessage`,
//! so a remapped `/robot1/tf` is read and a `/tf` carrying something else is
//! not. The topic name is consulted for exactly one thing — see
//! [`TopicRoles`] — and that one thing is unavoidable.
//!
//! # Why the reader streams instead of mapping the file
//!
//! `mcap::MessageStream` needs the whole file as a `&[u8]`, which for §3's
//! motivating case (a 4-hour recording) means either loading it or mapping it.
//! Loading defeats the `--max-memory` cap this module exists to respect, and
//! mapping would put an `unsafe` block in a crate that has no business owning
//! one (`docs/decisions/0007` — the OS boundary is `tf_tree_ipc`'s). The
//! `sans_io` reader takes bytes as they are handed to it, so the whole file is
//! never resident and the crate keeps `#![forbid(unsafe_code)]`.
//!
//! Streaming also makes §3.1's *two* passes cheap to express: [`read_tf`] is
//! called twice on the same path, and nothing has to be retained between them.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use mcap::sans_io::{LinearReadEvent, LinearReader};

use crate::cdr::{decode_tf_message, CdrError};
use crate::IngestError;

/// The schema names this reader accepts, newest first.
///
/// The ROS 1 spelling is here because a bag converted from ROS 1 by
/// `rosbags-convert` keeps it, and the payload is CDR either way once
/// `rosbag2` has written it.
const TF_SCHEMAS: [&str; 2] = ["tf2_msgs/msg/TFMessage", "tf2_msgs/TFMessage"];

/// Largest single MCAP record this reader will allocate for: **256 MiB**.
///
/// See the option's comment in [`read_tf`]. This is a bound on a corrupt length
/// field, not a format limit.
const MAX_RECORD_BYTES: usize = 256 * 1024 * 1024;

/// How a topic name decides whether its edges are static.
///
/// **This is the one thing that cannot come from the schema**, and it is worth
/// being explicit about why: `/tf` and `/tf_static` carry the *identical*
/// message type. What separates them is the QoS durability of the publisher,
/// which MCAP does not record, so the topic name is the only evidence in the
/// file. The default rule is the last path segment — `/robot1/tf_static`
/// matches, `/tf_static_debug` does not — and `--static-topic` overrides it for
/// a deployment that renamed the topic outright.
#[derive(Clone, Debug, Default)]
pub struct TopicRoles {
    /// Topics to treat as static, in full. Empty means "use the suffix rule".
    pub static_topics: Vec<String>,
    /// Topics to treat as dynamic, in full. Empty means "everything else".
    pub dynamic_topics: Vec<String>,
}

impl TopicRoles {
    /// Whether `topic` carries static transforms.
    #[must_use]
    pub fn is_static(&self, topic: &str) -> bool {
        if self.static_topics.iter().any(|t| t == topic) {
            return true;
        }
        if self.dynamic_topics.iter().any(|t| t == topic) {
            return false;
        }
        if !self.static_topics.is_empty() {
            return false;
        }
        topic.rsplit('/').next() == Some("tf_static")
    }

    /// Whether a TF-schema channel on `topic` should be read at all.
    ///
    /// An explicit `--dynamic-topic` list narrows the read; without one, every
    /// TF-schema channel is read, which is what §3.3's "and remapped
    /// equivalents" asks for.
    #[must_use]
    pub fn selects(&self, topic: &str) -> bool {
        if self.static_topics.is_empty() && self.dynamic_topics.is_empty() {
            return true;
        }
        self.static_topics.iter().any(|t| t == topic)
            || self.dynamic_topics.iter().any(|t| t == topic)
    }
}

/// One transform as it was found in the recording, before any normalization.
#[derive(Clone, Debug)]
pub struct RawRecord<'a> {
    /// The topic it arrived on.
    pub topic: &'a str,
    /// Whether that topic is a static one (see [`TopicRoles`]).
    pub is_static: bool,
    /// The MCAP log time — when the recorder wrote it, not when it was stamped.
    ///
    /// §3.2's "stamps far in the future" row needs a reference clock that is
    /// *not* the header stamp, or the check is circular. This is it, and it is
    /// the only reference a bag actually contains.
    pub log_time_ns: i64,
    /// `header.stamp`, flattened.
    pub stamp_ns: i64,
    /// `header.frame_id`, raw.
    pub parent: &'a str,
    /// `child_frame_id`, raw.
    pub child: &'a str,
    /// `[qw qx qy qz tx ty tz]`.
    pub pose: [f64; 7],
}

/// What a channel turned out to be, decided once when its record is read.
struct ChannelRole {
    topic: String,
    is_static: bool,
}

/// Counts of what the reader declined to decode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SkipCounts {
    /// Messages on a TF-schema channel whose `message_encoding` was not `cdr`.
    pub non_cdr: u64,
    /// Channels carrying the TF schema that the topic filter excluded.
    pub filtered_channels: u64,
    /// The recording ended mid-record or without its end magic: everything up to
    /// that point was read and the rest does not exist. See [`read_tf`].
    pub truncated: bool,
}

/// Read every TF transform in `path`, calling `f` once per transform.
///
/// The callback returns a `Result` so a caller can stop on the first anomaly it
/// treats as fatal (§3.2's edge-kind change, and the clock-reset `halt` policy)
/// without this module knowing what those are.
///
/// # Errors
///
/// [`IngestError::Io`] for a failing read, [`IngestError::CompressedChunk`] for
/// a chunk this build cannot decompress (see the crate docs), [`IngestError::
/// Mcap`] for a malformed file, [`IngestError::Cdr`] for a payload that is not
/// a decodable `TFMessage`, or whatever the callback returned.
pub fn read_tf<F>(path: &Path, roles: &TopicRoles, mut f: F) -> Result<SkipCounts, IngestError>
where
    F: FnMut(RawRecord<'_>) -> Result<(), IngestError>,
{
    let file = File::open(path).map_err(|e| IngestError::Io {
        raw_os_error: e.raw_os_error().unwrap_or(0),
    })?;
    let mut input = BufReader::new(file);
    let mut reader = LinearReader::new_with_options(
        mcap::sans_io::LinearReaderOptions::default()
            // **A truncated recording is read up to the truncation point.**
            //
            // The default requires the end magic, so a file whose recorder was
            // SIGKILLed, whose disk filled, or whose copy was interrupted fails
            // wholesale with `Mcap` — zero transforms out of a recording that is
            // 90 % intact, and a message ("not a well-formed MCAP recording")
            // that is wrong: the file is well-formed, it is incomplete. MCAP is
            // designed to be readable up to the point it stops, and an *offline
            // forensic* tool that discards a whole recording because the tail is
            // missing inverts its own use case — §3.3's `freeze --from-live`
            // exists to "capture a fault in the field", and a fault in the field
            // is how recordings get truncated.
            //
            // The cost is that genuine tail corruption is no longer detected
            // here. It was never detected *well*: the end magic says nothing
            // about the records before it.
            .with_skip_end_magic(true)
            // A record header is a caller-supplied `u64` length. Without a limit
            // a corrupt one asks for a multi-gigabyte allocation before anything
            // validates it — the same failure `cdr::ImplausibleCount` exists to
            // stop one layer down, and the guard was available here and switched
            // off. 256 MiB is far above any real MCAP record (a chunk is
            // typically 1–8 MiB) and far below a length that can exhaust memory.
            .with_record_length_limit(MAX_RECORD_BYTES),
    );

    // Our own accumulators. The `sans_io` reader emits raw records and leaves
    // schema/channel bookkeeping to the caller, which suits us: we only need
    // two fields out of each and never the schema payload.
    let mut tf_schema_ids: HashMap<u16, ()> = HashMap::new();
    let mut channels: HashMap<u16, ChannelRole> = HashMap::new();
    let mut skips = SkipCounts::default();

    while let Some(event) = reader.next_event() {
        // **A truncated recording is a short recording, not a broken one.**
        //
        // `UnexpectedEof` is what the reader returns when the file stops in the
        // middle of a record — a recorder that was SIGKILLed, a disk that
        // filled, an interrupted copy. Every record before the cut is intact and
        // has already been handed to `f`. Propagating the error would throw all
        // of it away, which for an *offline forensic* tool is the wrong trade:
        // §3.3's `freeze --from-live` exists to "capture a fault in the field",
        // and a fault in the field is how recordings get truncated.
        //
        // The prefix is kept and the fact is recorded, so the report can say so
        // loudly rather than the tool pretending the file was whole. What this
        // costs is that tail corruption now reads as truncation; it was never
        // distinguished well anyway, because the end magic says nothing about
        // the records before it. A file that is not an MCAP at all still fails,
        // on the *start* magic, before any of this.
        let event = match event {
            Ok(e) => e,
            Err(mcap::McapError::UnexpectedEof) => {
                skips.truncated = true;
                break;
            }
            Err(e) => return Err(map_mcap(&e)),
        };
        match event {
            LinearReadEvent::ReadRequest(want) => {
                let n = input
                    .read(reader.insert(want))
                    .map_err(|e| IngestError::Io {
                        raw_os_error: e.raw_os_error().unwrap_or(0),
                    })?;
                reader.notify_read(n);
            }
            LinearReadEvent::Record { data, opcode } => {
                match mcap::parse_record(opcode, data).map_err(|e| map_mcap(&e))? {
                    mcap::records::Record::Schema { header, .. }
                        if TF_SCHEMAS.contains(&header.name.as_str()) =>
                    {
                        tf_schema_ids.insert(header.id, ());
                    }
                    mcap::records::Record::Channel(ch) => {
                        if !tf_schema_ids.contains_key(&ch.schema_id) {
                            continue;
                        }
                        if !roles.selects(&ch.topic) {
                            skips.filtered_channels += 1;
                            continue;
                        }
                        // ROS 2 writes `cdr`; a `json`/`protobuf` channel with
                        // the TF schema name is possible and is not ours to
                        // decode. Counted, not silently dropped.
                        if ch.message_encoding != "cdr" {
                            skips.non_cdr += 1;
                            continue;
                        }
                        let is_static = roles.is_static(&ch.topic);
                        channels.insert(
                            ch.id,
                            ChannelRole {
                                topic: ch.topic,
                                is_static,
                            },
                        );
                    }
                    mcap::records::Record::Message { header, data } => {
                        let Some(role) = channels.get(&header.channel_id) else {
                            continue;
                        };
                        // `log_time` is a `u64` of nanoseconds since the epoch;
                        // a value past `i64::MAX` is 2262 and is corrupt, so it
                        // saturates rather than wrapping into the past — which
                        // would make the future-stamp check fire on every
                        // message instead of none.
                        let log_time_ns = i64::try_from(header.log_time).unwrap_or(i64::MAX);
                        for t in decode_tf_message(&data).map_err(IngestError::Cdr)? {
                            f(RawRecord {
                                topic: &role.topic,
                                is_static: role.is_static,
                                log_time_ns,
                                stamp_ns: t.stamp_ns,
                                parent: &t.frame_id,
                                child: &t.child_frame_id,
                                pose: t.pose,
                            })?;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(skips)
}

/// Classify an `mcap` failure into this crate's `Copy` error type.
///
/// The distinction that matters to a user is compression: it is the one failure
/// with an action attached, and §0.0's `default-features = false` makes it
/// reachable on ordinary recordings.
fn map_mcap(e: &mcap::McapError) -> IngestError {
    match e {
        mcap::McapError::UnsupportedCompression(_) => IngestError::CompressedChunk,
        _ => IngestError::Mcap,
    }
}

/// A `CdrError` is carried through unchanged; this exists so the conversion is
/// named at the one place it happens.
impl From<CdrError> for IngestError {
    fn from(e: CdrError) -> IngestError {
        IngestError::Cdr(e)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    /// The suffix rule matches a remapped `/robot1/tf_static` and does **not**
    /// match a topic that merely starts with the same characters.
    ///
    /// Mutant: change `rsplit('/').next() == Some("tf_static")` to
    /// `topic.contains("tf_static")` — applied, and the
    /// `/tf_static_debug` assertion failed.
    #[test]
    fn static_role_is_the_last_path_segment() {
        let r = TopicRoles::default();
        assert!(r.is_static("/tf_static"));
        assert!(r.is_static("/robot1/tf_static"));
        assert!(!r.is_static("/tf"));
        assert!(!r.is_static("/tf_static_debug"));
        assert!(!r.is_static("/robot1/tf"));
    }

    /// An explicit `--static-topic` makes the suffix rule stop applying, so a
    /// deployment whose static topic is called something else does not get both
    /// its own topic *and* everything ending in `tf_static`.
    ///
    /// Mutant: delete the `if !self.static_topics.is_empty() { return false; }`
    /// arm — applied, and the `/tf_static` assertion failed (it fell through to
    /// the suffix rule and came back `true`).
    #[test]
    fn explicit_static_topics_replace_the_suffix_rule() {
        let r = TopicRoles {
            static_topics: vec!["/fixed_frames".into()],
            dynamic_topics: vec!["/tf".into()],
        };
        assert!(r.is_static("/fixed_frames"));
        assert!(!r.is_static("/tf_static"));
        assert!(r.selects("/tf"));
        assert!(!r.selects("/tf_static"));
    }
}
