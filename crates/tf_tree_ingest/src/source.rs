//! Reading `tf2_msgs/msg/TFMessage` out of an MCAP recording — `docs/PHASE5.md`
//! §3.3.
//!
//! A channel is a TF channel when its **schema** is `tf2_msgs/msg/TFMessage`, so
//! a remapped `/robot1/tf` is read; the topic name is consulted only in
//! [`TopicRoles`]. The reader streams (`sans_io`) because `mcap::MessageStream`
//! needs a `&[u8]`, which would defeat `--max-memory` (`docs/decisions/0007`).

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use crate::cdr::{decode_tf_message, CdrError};
use crate::decompress;
use crate::IngestError;

/// The schema names this reader accepts, newest first.
/// The ROS 1 spelling survives `rosbags-convert`.
const TF_SCHEMAS: [&str; 2] = ["tf2_msgs/msg/TFMessage", "tf2_msgs/TFMessage"];

/// Length of MCAP's file magic, at both ends of a complete recording.
const MAGIC_LEN: usize = 8;

/// A record's framing: `opcode: u8` then `len: u64` little-endian.
/// `mcap::parse_record` owns every record body.
const RECORD_HEADER_LEN: usize = 1 + 8;

/// How a topic name decides whether its edges are static.
/// `/tf` and `/tf_static` share a type and MCAP does not record QoS durability.
/// The default rule is the last path segment (`/robot1/tf_static` matches,
/// `/tf_static_debug` does not); `--static-topic` overrides it.
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
    /// Only [`dynamic_topics`](TopicRoles::dynamic_topics) narrows the read; with
    /// none, every TF-schema channel is read (§3.3).
    #[must_use]
    pub fn selects(&self, topic: &str) -> bool {
        if self.dynamic_topics.is_empty() {
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
    /// The MCAP log time: §3.2's reference clock for "stamps far in the future".
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

/// What to do about a chunk that does not decompress or does not check out.
///
/// The default is to skip because the skip is exact: the framing gave the
/// chunk's declared length. The report carries the skip and the lost time span.
/// When nothing survives, [`IngestError::NoTransforms`] carries no
/// [`Anomalies`](crate::Anomalies)
/// (`tests/ingest.rs::a_skipped_chunk_that_carried_the_only_channel_drops_the_rest`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OnBadChunk {
    /// Skip it, count it, and report the span it covered. **The default.**
    #[default]
    Skip,
    /// Fail, naming the chunk — for a user who must know the recording is whole
    /// before trusting a number derived from it.
    Halt,
}

/// What this reader will do before it believes a length read off disk: the
/// record ceiling, the chunk skip policy, and the decompression bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadPolicy {
    /// What to do about a chunk that does not decompress or does not check out.
    pub on_bad: OnBadChunk,
    /// What this reader will decompress a chunk into, before it believes a header.
    pub limits: decompress::ChunkLimits,
    /// Largest top-level record body this reader will allocate for, in bytes.
    ///
    /// A record header is a length off disk; this bounds the allocation it can
    /// cause (`docs/decisions/0010`; [`crate::IngestOptions::max_record_bytes`]).
    pub max_record_bytes: u64,
}

/// Counts of what the reader declined to decode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SkipCounts {
    /// **Channels** carrying the TF schema whose `message_encoding` was not
    /// `cdr`, once per channel id.
    pub non_cdr: u64,
    /// Top-level records over [`ReadPolicy::max_record_bytes`] that
    /// `reader_needs` answers `false` for, stepped over instead of refusing the
    /// file.
    ///
    /// Not a promise that the stream is whole: a skipped length landing on a later
    /// record boundary passes `resyncs_here`.
    pub oversized_records_skipped: u64,
    /// TF-schema channels excluded by the operator's `--tf-topic` narrowing, once
    /// per channel id.
    pub filtered_channels: u64,
    /// The recording ended mid-record; everything up to that point was read (see
    /// [`read_tf`]). A truncated *compressed* chunk is lost and not counted in
    /// [`bad_chunks`](SkipCounts::bad_chunks).
    pub truncated: bool,
    /// Chunks that did not decompress or did not check out, and were skipped
    /// under [`OnBadChunk::Skip`].
    pub bad_chunks: u64,
    /// The span the skipped chunks covered, from their declared message times.
    pub bad_chunk_span_ns: Option<(u64, u64)>,
    /// Of [`bad_chunks`](SkipCounts::bad_chunks), how many were refused by this
    /// reader's own limits (a flag fixes it: [`IngestError::AllChunksOverLimit`]).
    /// A subset, still skippable.
    pub chunks_over_limit: u64,
}

/// The first sixteen bytes of every SQLite database file, including a rosbag2
/// `.db3` (<https://sqlite.org/fileformat2.html> §1.3).
const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// A detection, not a reader (`docs/PHASE5.md` §3.3); it beats "not a
/// well-formed MCAP". The peek consumes nothing.
fn is_sqlite(input: &mut BufReader<File>) -> Result<bool, IngestError> {
    let head = input.fill_buf().map_err(|e| IngestError::Io {
        raw_os_error: e.raw_os_error().unwrap_or(0),
    })?;
    Ok(head.starts_with(SQLITE_MAGIC))
}

/// A `Chunk` plus the three [`handle_record`] acts on: `Schema`, `Channel`,
/// `Message`. Written as the set the reader *needs* so a newly used record kind
/// is a change here, not a silent skip.
const fn reader_needs(opcode: u8) -> bool {
    matches!(
        opcode,
        mcap::records::op::CHUNK
            | mcap::records::op::SCHEMA
            | mcap::records::op::CHANNEL
            | mcap::records::op::MESSAGE
    )
}

/// Returns whether the body was entirely present; without the `file_len`
/// comparison a record declaring more than the file holds would read as a clean
/// end.
fn skip_body(input: &mut BufReader<File>, len: u64, file_len: u64) -> Result<bool, IngestError> {
    let io = |e: &std::io::Error| IngestError::Io {
        raw_os_error: e.raw_os_error().unwrap_or(0),
    };
    let at = input.stream_position().map_err(|e| io(&e))?;
    let end = at.saturating_add(len);
    let complete = end <= file_len;
    input
        .seek(SeekFrom::Start(if complete { end } else { file_len }))
        .map_err(|e| io(&e))?;
    Ok(complete)
}

/// The callback returns a `Result` so a caller can stop on a fatal anomaly.
/// # Errors
///
/// [`IngestError::Io`] for a failing read, [`IngestError::Rosbag2Sqlite`] for a
/// rosbag2 sqlite3 bag, [`IngestError::CompressedChunk`] for a codec this build
/// has no decoder for (an unrecognised name, or zstd/lz4 without the
/// `compression` feature), [`IngestError::Mcap`] for a malformed file,
/// [`IngestError::Cdr`] for a payload that is not a decodable `TFMessage`, or
/// whatever the callback returned.
pub fn read_tf<F>(
    path: &Path,
    roles: &TopicRoles,
    policy: ReadPolicy,
    mut f: F,
) -> Result<SkipCounts, IngestError>
where
    F: FnMut(RawRecord<'_>) -> Result<(), IngestError>,
{
    let file = File::open(path).map_err(|e| IngestError::Io {
        raw_os_error: e.raw_os_error().unwrap_or(0),
    })?;
    let file_len = file
        .metadata()
        .map_err(|e| IngestError::Io {
            raw_os_error: e.raw_os_error().unwrap_or(0),
        })?
        .len();
    let mut input = BufReader::new(file);
    if is_sqlite(&mut input)? {
        return Err(IngestError::Rosbag2Sqlite);
    }
    let mut magic = [0u8; MAGIC_LEN];
    read_exact_or_eof(&mut input, &mut magic)?;
    if magic != *mcap::MAGIC {
        return Err(IngestError::Mcap);
    }

    let mut book = Bookkeeping::default();
    let mut skips = SkipCounts::default();
    // `body` never grows past `policy.max_record_bytes`; `scratch` is bounded by
    // `policy.limits.max_uncompressed_bytes`.
    let mut body: Vec<u8> = Vec::new();
    let mut scratch: Vec<u8> = Vec::new();
    let mut chunk_ordinal: u64 = 0;

    loop {
        // A truncated recording is a short one: the prefix is kept and the fact
        // recorded.
        let mut header = [0u8; RECORD_HEADER_LEN];
        match read_full(&mut input, &mut header)? {
            0 => break,
            MAGIC_LEN if header[..MAGIC_LEN] == *mcap::MAGIC => break,
            n if n < RECORD_HEADER_LEN => {
                skips.truncated = true;
                break;
            }
            _ => {}
        }
        let opcode = header[0];
        let len_bytes: [u8; 8] = match header[1..RECORD_HEADER_LEN].try_into() {
            Ok(b) => b,
            Err(_) => return Err(IngestError::Mcap),
        };
        let declared = u64::from_le_bytes(len_bytes);
        // Compared in `u64` before the `usize` narrowing (32-bit targets).
        let fits = declared <= policy.max_record_bytes && usize::try_from(declared).is_ok();
        // The opcode is consulted first: a record never read costs no allocation;
        // needed records still refuse ([`reader_needs`]).
        if !fits {
            if reader_needs(opcode) {
                return Err(IngestError::RecordTooLarge {
                    declared,
                    ceiling: policy.max_record_bytes,
                });
            }
            if !skip_body(&mut input, declared, file_len)? {
                skips.truncated = true;
                break;
            }
            // A skipped length is unvalidated, so it must land on a record boundary
            // (`resyncs_here`) or the ceiling refusal is returned.
            if !resyncs_here(&mut input, file_len)? {
                return Err(IngestError::RecordTooLarge {
                    declared,
                    ceiling: policy.max_record_bytes,
                });
            }
            skips.oversized_records_skipped += 1;
            continue;
        }
        // Sized against the file as well as the ceiling: `--max-record-size` can be
        // `u64::MAX` and a tiny file declaring a huge length must not allocate.
        let Ok(want) = usize::try_from(declared.min(file_len)) else {
            return Err(IngestError::RecordTooLarge {
                declared,
                ceiling: policy.max_record_bytes,
            });
        };
        // No `clear()` (`decompress::decode_zstd`): `resize` zero-fills only the
        // shortfall; `reserve_exact` keeps the `max_record_bytes` bound.
        body.reserve_exact(want.saturating_sub(body.len()));
        body.resize(want, 0);
        let got = read_full(&mut input, &mut body)?;
        // Record-granular recovery ([`SkipCounts::truncated`]). Compared against
        // `declared`, not the clamped `want`, so an appended file cannot pass.
        let complete = got as u64 == declared;
        if !complete {
            skips.truncated = true;
            body.truncate(got);
        }

        if opcode == mcap::records::op::CHUNK {
            chunk_ordinal += 1;
            read_chunk(
                &body,
                complete,
                chunk_ordinal - 1,
                policy,
                &mut scratch,
                &mut book,
                roles,
                &mut skips,
                &mut f,
            )?;
        } else if complete {
            let rec = mcap::parse_record(opcode, &body).map_err(|e| map_mcap(&e))?;
            handle_record(rec, &mut book, roles, &mut skips, &mut f)?;
        }
        if !complete {
            break;
        }
    }
    Ok(skips)
}

/// Read exactly `buf.len()` bytes, or as many as the file has left.
/// A short `Read::read` is not EOF; returns how many bytes were read.
fn read_full(input: &mut BufReader<File>, buf: &mut [u8]) -> Result<usize, IngestError> {
    let mut at = 0;
    while at < buf.len() {
        let n = input.read(&mut buf[at..]).map_err(|e| IngestError::Io {
            raw_os_error: e.raw_os_error().unwrap_or(0),
        })?;
        if n == 0 {
            break;
        }
        at += n;
    }
    Ok(at)
}

/// [`read_full`], but a short read is an error, for the magic.
fn read_exact_or_eof(input: &mut BufReader<File>, buf: &mut [u8]) -> Result<(), IngestError> {
    if read_full(input, buf)? == buf.len() {
        Ok(())
    } else {
        Err(IngestError::Mcap)
    }
}

/// The only check between a corrupt skipped length and a walk that parses
/// whatever it lands in. Accepted: end of file, the end magic, or a header with
/// an assigned opcode whose length is no larger than the file (loose: a
/// truncated final record overshoots). The peek is undone before returning.
fn resyncs_here(input: &mut BufReader<File>, file_len: u64) -> Result<bool, IngestError> {
    let io = |e: &std::io::Error| IngestError::Io {
        raw_os_error: e.raw_os_error().unwrap_or(0),
    };
    let at = input.stream_position().map_err(|e| io(&e))?;
    let mut head = [0u8; RECORD_HEADER_LEN];
    let n = read_full(input, &mut head)?;
    input.seek(SeekFrom::Start(at)).map_err(|e| io(&e))?;
    Ok(match n {
        0 => true,
        MAGIC_LEN if head[..MAGIC_LEN] == *mcap::MAGIC => true,
        RECORD_HEADER_LEN => {
            let Ok(len_bytes) = <[u8; 8]>::try_from(&head[1..RECORD_HEADER_LEN]) else {
                return Ok(false);
            };
            let assigned = matches!(
                head[0],
                mcap::records::op::HEADER..=mcap::records::op::DATA_END
            );
            assigned && u64::from_le_bytes(len_bytes) <= file_len
        }
        _ => false,
    })
}

/// `complete` is false when the file ended inside this chunk: an uncompressed
/// chunk yields its whole-record prefix; a compressed one yields none and lands
/// in [`SkipCounts::truncated`], never [`SkipCounts::bad_chunks`].
#[allow(clippy::too_many_arguments)]
fn read_chunk<F>(
    body: &[u8],
    complete: bool,
    ordinal: u64,
    policy: ReadPolicy,
    scratch: &mut Vec<u8>,
    book: &mut Bookkeeping,
    roles: &TopicRoles,
    skips: &mut SkipCounts,
    f: &mut F,
) -> Result<(), IngestError>
where
    F: FnMut(RawRecord<'_>) -> Result<(), IngestError>,
{
    // One handler serves top level and chunks; message times let a skipped chunk
    // report its span.
    let span = decompress::chunk_span(body);
    let records = match decompress::chunk_records(body, complete, policy.limits, scratch) {
        Ok(r) => r,
        Err(fault) => return note_or_fail(fault, ordinal, policy.on_bad, span, skips),
    };
    let tolerate_tail = !complete;
    match decompress::for_each_record(records, tolerate_tail, |op, inner| {
        let rec = mcap::parse_record(op, inner).map_err(|e| map_mcap(&e))?;
        handle_record(rec, book, roles, skips, f)
    }) {
        Ok(()) => Ok(()),
        Err(fault) => note_or_fail(fault, ordinal, policy.on_bad, span, skips),
    }
}

/// Never skippable: [`ChunkFault::Unsupported`] (every chunk shares the codec)
/// and [`ChunkFault::Callback`] (the caller's own verdict). A chunk refused by
/// this reader's own limits is counted twice
/// ([`SkipCounts::chunks_over_limit`]).
fn note_or_fail(
    fault: decompress::ChunkFault,
    ordinal: u64,
    on_bad_chunk: OnBadChunk,
    span: Option<(u64, u64)>,
    skips: &mut SkipCounts,
) -> Result<(), IngestError> {
    let skippable = matches!(fault, decompress::ChunkFault::Bad(_));
    if !skippable || on_bad_chunk == OnBadChunk::Halt {
        return Err(chunk_error(fault, ordinal));
    }
    skips.bad_chunks += 1;
    if matches!(
        fault,
        decompress::ChunkFault::Bad(
            decompress::BadChunkKind::ImplausibleSize { .. }
                | decompress::BadChunkKind::ImplausibleWindow { .. }
        )
    ) {
        skips.chunks_over_limit += 1;
    }
    if let Some((lo, hi)) = span {
        skips.bad_chunk_span_ns = Some(match skips.bad_chunk_span_ns {
            Some((a, b)) => (a.min(lo), b.max(hi)),
            None => (lo, hi),
        });
    }
    Ok(())
}

/// Join a chunk fault to the ordinal of the chunk it came from; a callback
/// failure passes through.
fn chunk_error(fault: decompress::ChunkFault, ordinal: u64) -> IngestError {
    match fault {
        decompress::ChunkFault::Unsupported(codec) => IngestError::CompressedChunk { codec },
        decompress::ChunkFault::Bad(kind) => IngestError::BadChunk {
            chunk: ordinal,
            kind,
        },
        decompress::ChunkFault::Callback(e) => e,
    }
}

/// Schema and channel state accumulated as the recording is read.
/// Lets [`handle_record`] serve both the top-level stream and chunk contents.
#[derive(Default)]
struct Bookkeeping {
    /// Ids of schemas whose name is one of [`TF_SCHEMAS`].
    tf_schema_ids: HashSet<u16>,
    /// Channels carrying such a schema with a `cdr` encoding, and their role.
    channels: HashMap<u16, ChannelRole>,
    /// Channel ids already classified (the summary section repeats them).
    seen_channels: HashSet<u16>,
}

/// Fold one record into the bookkeeping, emitting transforms for a TF message.
/// Other records are ignored.
fn handle_record<F>(
    rec: mcap::records::Record<'_>,
    book: &mut Bookkeeping,
    roles: &TopicRoles,
    skips: &mut SkipCounts,
    f: &mut F,
) -> Result<(), IngestError>
where
    F: FnMut(RawRecord<'_>) -> Result<(), IngestError>,
{
    match rec {
        mcap::records::Record::Schema { header, .. }
            if TF_SCHEMAS.contains(&header.name.as_str()) =>
        {
            book.tf_schema_ids.insert(header.id);
        }
        mcap::records::Record::Channel(ch) => {
            if !book.tf_schema_ids.contains(&ch.schema_id) {
                return Ok(());
            }
            if !book.seen_channels.insert(ch.id) {
                return Ok(());
            }
            if !roles.selects(&ch.topic) {
                skips.filtered_channels += 1;
                return Ok(());
            }
            if ch.message_encoding != "cdr" {
                skips.non_cdr += 1;
                return Ok(());
            }
            let is_static = roles.is_static(&ch.topic);
            book.channels.insert(
                ch.id,
                ChannelRole {
                    topic: ch.topic,
                    is_static,
                },
            );
        }
        mcap::records::Record::Message { header, data } => {
            let Some(role) = book.channels.get(&header.channel_id) else {
                return Ok(());
            };
            // Past `i64::MAX` (year 2262) is corrupt: saturate, never wrap into the past.
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
    Ok(())
}

/// Codec classification happens in `crate::decompress`, so what remains is a
/// malformed record body, [`IngestError::Mcap`].
fn map_mcap(e: &mcap::McapError) -> IngestError {
    match e {
        mcap::McapError::UnsupportedCompression(_) => IngestError::CompressedChunk {
            codec: decompress::ChunkCodec::Other,
        },
        _ => IngestError::Mcap,
    }
}

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
    #[test]
    fn static_role_is_the_last_path_segment() {
        let r = TopicRoles::default();
        assert!(r.is_static("/tf_static"));
        assert!(r.is_static("/robot1/tf_static"));
        assert!(!r.is_static("/tf"));
        assert!(!r.is_static("/tf_static_debug"));
        assert!(!r.is_static("/robot1/tf"));
    }

    /// An explicit `--static-topic` disables the suffix rule.
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

    /// `--static-topic` alone does not narrow the read.
    #[test]
    fn a_renamed_static_topic_does_not_exclude_the_dynamic_ones() {
        let r = TopicRoles {
            static_topics: vec!["/fixed_frames".into()],
            dynamic_topics: Vec::new(),
        };
        assert!(r.selects("/fixed_frames"));
        assert!(r.selects("/tf"), "naming a static topic hid /tf");
        assert!(r.selects("/robot1/tf"));
        assert!(r.is_static("/fixed_frames"));
        assert!(!r.is_static("/tf"));
    }

    /// `--tf-topic` narrows; a static topic named alongside it survives.
    #[test]
    fn dynamic_topics_narrow_the_read_and_keep_named_statics() {
        let r = TopicRoles {
            static_topics: vec!["/robot1/fixed".into()],
            dynamic_topics: vec!["/robot1/tf".into()],
        };
        assert!(r.selects("/robot1/tf"));
        assert!(r.selects("/robot1/fixed"));
        assert!(!r.selects("/robot2/tf"));
        assert!(!r.selects("/tf"));
    }
}
