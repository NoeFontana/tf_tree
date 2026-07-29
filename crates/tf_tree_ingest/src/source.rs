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

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use crate::cdr::{decode_tf_message, CdrError};
use crate::decompress;
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

/// Length of MCAP's file magic, at both ends of a complete recording.
const MAGIC_LEN: usize = 8;

/// A record's framing: `opcode: u8` then `len: u64` little-endian.
///
/// This module owns the framing and `mcap::parse_record` owns every record
/// *body*, which is the split that matters: nine bytes of length prefix are
/// trivial and we already walk them inside chunks, whereas a `Schema`,
/// `Channel` or `Message` body is not ours to re-derive.
const RECORD_HEADER_LEN: usize = 1 + 8;

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
    /// **Only [`dynamic_topics`](TopicRoles::dynamic_topics) narrows the read.**
    /// `--static-topic` answers "which topics are static", not "which topics
    /// exist": naming a renamed static topic must not silently stop `/tf` from
    /// being read, which is what an earlier revision did — it keyed the narrowing
    /// on *either* list being non-empty, so `--static-topic /fixed_frames` alone
    /// ingested the statics and zero dynamic samples, and said nothing about it.
    ///
    /// With no `--tf-topic`, every TF-schema channel is read, which is what
    /// §3.3's "and remapped equivalents" asks for. With one, the read is the
    /// named dynamic topics plus any named static ones — a user who narrows to
    /// `/robot1/tf` still wants `/robot1/tf_static` if they named it.
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

/// What to do about a chunk that does not decompress or does not check out.
///
/// # Why the default is to skip
///
/// One bad chunk in four hundred thousand must not lose the recording, and here
/// the skip is **exact rather than heuristic**: the framing gave us the chunk's
/// declared length, so we resume on the next record boundary with no
/// resynchronisation guess. That is a stronger position than the general
/// "truncated bags are how the field works" argument, and it is why this is a
/// default rather than an opt-in.
///
/// # What a skip costs, which the report must say
///
/// Chunks carry `Schema` and `Channel` records as well as messages. If the
/// skipped chunk held the only `Channel` record for `/tf`, every later message on
/// that channel is dropped as belonging to an unknown channel — and that drop has
/// no counter of its own. So the report puts the skip beside `truncated`, with the
/// same "the counts below cover only part of the recording" framing, and carries
/// the *time span* that was lost.
///
/// Both passes read the same file, so a deterministic bad chunk is skipped
/// identically in the survey and the fill, and the two stay consistent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OnBadChunk {
    /// Skip it, count it, and report the span it covered. **The default.**
    #[default]
    Skip,
    /// Fail, naming the chunk — for a user who must know the recording is whole
    /// before trusting a number derived from it.
    Halt,
}

/// Counts of what the reader declined to decode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SkipCounts {
    /// Messages on a TF-schema channel whose `message_encoding` was not `cdr`.
    pub non_cdr: u64,
    /// Channels carrying the TF schema that the topic filter excluded.
    pub filtered_channels: u64,
    /// The recording ended mid-record: everything up to that point was read and
    /// the rest does not exist. See [`read_tf`].
    ///
    /// **Recovery is record-granular, including inside a chunk.** A recording cut
    /// mid-chunk still yields every whole record in that chunk's prefix, which is
    /// why this module owns the record framing rather than asking a reader for
    /// complete records only — see [`RECORD_HEADER_LEN`].
    ///
    /// The one case that cannot be recovered is a truncated *compressed* chunk: a
    /// partial codec frame is not decodable by a one-shot decoder, so that chunk's
    /// records are lost even though its prefix is on disk. The bound is one chunk,
    /// and [`bad_chunks`](SkipCounts::bad_chunks) does not count it — nothing is
    /// wrong with the chunk, the file simply stops inside it.
    pub truncated: bool,
    /// Chunks that did not decompress or did not check out, and were skipped
    /// under [`OnBadChunk::Skip`](crate::ingest::OnBadChunk::Skip).
    pub bad_chunks: u64,
    /// The span the skipped chunks covered, from their own declared message
    /// times.
    ///
    /// A count alone is not actionable. "Three chunks were unreadable" tells an
    /// operator nothing they can do; "the transforms between 14:22:07 and
    /// 14:22:19 are missing" tells them which part of the run to distrust.
    pub bad_chunk_span_ns: Option<(u64, u64)>,
}

/// The first sixteen bytes of every SQLite database file, including a rosbag2
/// `.db3` (<https://sqlite.org/fileformat2.html> §1.3).
const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// Whether this file is a SQLite database rather than an MCAP.
///
/// **A detection, not a reader.** `docs/PHASE5.md` §3.3's rosbag2 sqlite3 source
/// is not implemented — the amendment there records why, and it is a dependency
/// finding rather than a schedule — so the whole value of looking is the error
/// message: without this, the most likely wrong file to hand `tf_tree ingest` is
/// reported as "not a well-formed MCAP recording", which is true, useless, and
/// sends the user looking for corruption in a file that is fine.
///
/// The peek is a `fill_buf` on the same `BufReader` the MCAP reader then uses,
/// so it consumes nothing and costs no second `open`.
fn is_sqlite(input: &mut BufReader<File>) -> Result<bool, IngestError> {
    let head = input.fill_buf().map_err(|e| IngestError::Io {
        raw_os_error: e.raw_os_error().unwrap_or(0),
    })?;
    Ok(head.starts_with(SQLITE_MAGIC))
}

/// Read every TF transform in `path`, calling `f` once per transform.
///
/// The callback returns a `Result` so a caller can stop on the first anomaly it
/// treats as fatal (§3.2's edge-kind change, and the clock-reset `halt` policy)
/// without this module knowing what those are.
///
/// # Errors
///
/// [`IngestError::Io`] for a failing read, [`IngestError::Rosbag2Sqlite`] for a
/// rosbag2 sqlite3 bag, [`IngestError::CompressedChunk`] for a chunk this build
/// cannot decompress (see the crate docs), [`IngestError::Mcap`] for a malformed
/// file, [`IngestError::Cdr`] for a payload that is not a decodable `TFMessage`,
/// or whatever the callback returned.
pub fn read_tf<F>(
    path: &Path,
    roles: &TopicRoles,
    on_bad_chunk: OnBadChunk,
    mut f: F,
) -> Result<SkipCounts, IngestError>
where
    F: FnMut(RawRecord<'_>) -> Result<(), IngestError>,
{
    let file = File::open(path).map_err(|e| IngestError::Io {
        raw_os_error: e.raw_os_error().unwrap_or(0),
    })?;
    let mut input = BufReader::new(file);
    if is_sqlite(&mut input)? {
        return Err(IngestError::Rosbag2Sqlite);
    }
    // **The start magic is the one thing that separates "incomplete" from "not an
    // MCAP at all".** Everything below tolerates a short file; this does not, so a
    // JPEG handed to `tf_tree ingest` fails here with a clear reason rather than
    // being read as a recording containing nothing.
    let mut magic = [0u8; MAGIC_LEN];
    read_exact_or_eof(&mut input, &mut magic)?;
    if magic != *mcap::MAGIC {
        return Err(IngestError::Mcap);
    }

    // Our own accumulators. Records are parsed by `mcap::parse_record`; what this
    // module owns is which bytes are a record, and the bookkeeping across them.
    let mut book = Bookkeeping::default();
    let mut skips = SkipCounts::default();
    // One buffer for the whole file, reused for every record body, and a second
    // for decompression. Neither grows past `MAX_RECORD_BYTES`.
    let mut body: Vec<u8> = Vec::new();
    let mut scratch: Vec<u8> = Vec::new();
    let mut chunk_ordinal: u64 = 0;

    loop {
        // **A truncated recording is a short recording, not a broken one, and
        // the cut is honoured at *record* granularity — including inside a
        // chunk.**
        //
        // A recorder that was SIGKILLed, a disk that filled, an interrupted copy:
        // every record before the cut is intact and belongs to the caller.
        // Discarding them would invert this tool's own use case, since §3.3's
        // `freeze --from-live` exists to "capture a fault in the field" and a
        // fault in the field is how recordings get truncated.
        //
        // The prefix is kept and the fact recorded, so the report says so loudly
        // rather than pretending the file was whole. What this costs is that
        // genuine tail corruption reads as truncation — it was never distinguished
        // well anyway, because the end magic says nothing about the records before
        // it.
        let mut header = [0u8; RECORD_HEADER_LEN];
        match read_full(&mut input, &mut header)? {
            // A clean end: either the footer's magic was consumed as a record
            // above, or the file simply stops on a record boundary.
            0 => break,
            // **The end magic, which is eight bytes and therefore one short of a
            // record header.** A complete recording ends `Footer` then MAGIC, so
            // without this a healthy file reports itself truncated on its very
            // last eight bytes.
            MAGIC_LEN if header[..MAGIC_LEN] == *mcap::MAGIC => break,
            n if n < RECORD_HEADER_LEN => {
                skips.truncated = true;
                break;
            }
            _ => {}
        }
        let opcode = header[0];
        // Infallible: `header[1..9]` is exactly eight bytes.
        let len_bytes: [u8; 8] = match header[1..RECORD_HEADER_LEN].try_into() {
            Ok(b) => b,
            Err(_) => return Err(IngestError::Mcap),
        };
        let declared = u64::from_le_bytes(len_bytes);
        // A record header is a length straight off disk. Without this bound a
        // corrupt one asks for a multi-gigabyte allocation before anything
        // validates it — the same failure `cdr::ImplausibleCount` stops one layer
        // down. 256 MiB is far above any real record (a chunk is typically
        // 1–8 MiB) and far below a length that can exhaust memory.
        let Ok(want) = usize::try_from(declared) else {
            return Err(IngestError::Mcap);
        };
        if want > MAX_RECORD_BYTES {
            return Err(IngestError::Mcap);
        }
        body.clear();
        body.resize(want, 0);
        let got = read_full(&mut input, &mut body)?;
        // **This is the branch the whole rewrite exists for.** A record cut short
        // by the end of the file is not simply dropped: if it is a chunk, its
        // prefix still holds complete records, and those are recovered. Asking a
        // reader for whole records only would lose every transform in the final
        // chunk — up to a few megabytes of a real recording, and all of a small
        // one.
        let complete = got == want;
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
                on_bad_chunk,
                &mut scratch,
                &mut book,
                roles,
                &mut skips,
                &mut f,
            )?;
        } else if complete {
            // A truncated non-chunk record has no recoverable interior — a
            // partial `Message` body is a partial CDR payload, and decoding one
            // would invent a transform.
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
///
/// Returns how many were read. `Read::read` is allowed to return short without
/// being at EOF, so a single call cannot distinguish "the file ends here" from
/// "the pipe had less ready" — which is exactly the distinction truncation
/// handling turns on.
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

/// [`read_full`], but a short read is an error rather than a count. For the magic,
/// where a short file is not a recording at all.
fn read_exact_or_eof(input: &mut BufReader<File>, buf: &mut [u8]) -> Result<(), IngestError> {
    if read_full(input, buf)? == buf.len() {
        Ok(())
    } else {
        Err(IngestError::Mcap)
    }
}

/// Read the records inside one chunk record's body.
///
/// `complete` is false when the file ended inside this chunk. A truncated
/// *uncompressed* chunk still yields every whole record in its prefix; the
/// trailing fragment is expected in that case and is not reported as corruption.
#[allow(clippy::too_many_arguments)]
fn read_chunk<F>(
    body: &[u8],
    complete: bool,
    ordinal: u64,
    on_bad_chunk: OnBadChunk,
    scratch: &mut Vec<u8>,
    book: &mut Bookkeeping,
    roles: &TopicRoles,
    skips: &mut SkipCounts,
    f: &mut F,
) -> Result<(), IngestError>
where
    F: FnMut(RawRecord<'_>) -> Result<(), IngestError>,
{
    // **A chunk is the one record that contains other records.**
    //
    // Everything a recording says about schemas, channels and messages can appear
    // either at the top level or inside a chunk: a file written with a summary
    // section has its schemas and channels in both places, one written without has
    // them only inside. So the same handler serves both.
    // The chunk header's message times, kept before the body is consumed so a
    // skipped chunk can report the span it took with it.
    let span = decompress::chunk_span(body);
    let records = match decompress::chunk_records(body, complete, scratch) {
        Ok(r) => r,
        Err(fault) => return note_or_fail(fault, ordinal, on_bad_chunk, span, skips),
    };
    // A truncated chunk's records field ends mid-record by construction, so the
    // fragment the walk would otherwise report is the truncation we already know
    // about.
    let tolerate_tail = !complete;
    match decompress::for_each_record(records, tolerate_tail, |op, inner| {
        let rec = mcap::parse_record(op, inner).map_err(|e| map_mcap(&e))?;
        handle_record(rec, book, roles, skips, f)
    }) {
        Ok(()) => Ok(()),
        Err(fault) => note_or_fail(fault, ordinal, on_bad_chunk, span, skips),
    }
}

/// Skip a bad chunk and count it, or fail naming it, per the policy.
///
/// Two faults are **never** skippable, and both for the same reason: skipping
/// them would answer a question the user did not ask.
///
/// * [`ChunkFault::Unsupported`] — every chunk in a recording uses the same codec,
///   so skipping them all yields "no transforms" about a file that is perfectly
///   intact.
/// * [`ChunkFault::Callback`] — the caller's own verdict on a transform (an
///   edge-kind change, a clock reset under `halt`). Swallowing it would convert a
///   hard error into silent data loss, which is the exact inversion this policy
///   exists to avoid.
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
    if let Some((lo, hi)) = span {
        skips.bad_chunk_span_ns = Some(match skips.bad_chunk_span_ns {
            Some((a, b)) => (a.min(lo), b.max(hi)),
            None => (lo, hi),
        });
    }
    Ok(())
}

/// Join a chunk fault to the ordinal of the chunk it came from.
///
/// A callback failure passes straight through: it is the caller's own verdict on
/// a transform, not a fact about the chunk, and dressing it as one would let a
/// skip policy swallow a hard error.
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
///
/// Extracted so [`handle_record`] can serve both the top-level record stream and
/// the records inside a chunk. Before `emit_chunks` these lived as three locals
/// in the event loop; a chunk's contents would now need a second copy of every
/// rule, and two copies of "is this channel one of ours" is exactly the drift
/// worth preventing.
#[derive(Default)]
struct Bookkeeping {
    /// Ids of schemas whose name is one of [`TF_SCHEMAS`].
    tf_schema_ids: HashMap<u16, ()>,
    /// Channels carrying such a schema with a `cdr` encoding, and their role.
    channels: HashMap<u16, ChannelRole>,
    /// Channel ids already classified, so the summary section's repeat of every
    /// Schema and Channel record is not counted twice.
    seen_channels: HashSet<u16>,
}

/// Fold one record into the bookkeeping, emitting transforms for a TF message.
///
/// Records other than Schema, Channel and Message are ignored, which includes
/// the `MessageIndex` records that follow every chunk and become visible under
/// `emit_chunks`.
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
            book.tf_schema_ids.insert(header.id, ());
        }
        mcap::records::Record::Channel(ch) => {
            if !book.tf_schema_ids.contains_key(&ch.schema_id) {
                return Ok(());
            }
            // **A channel is counted once, not once per record.** MCAP repeats
            // every Schema and Channel record in the summary section at the end
            // of the file, so a linear read sees each of them twice. Without
            // this, the report's "TF channels were skipped" line says two for one
            // channel — a number a user cannot reconcile with their recording.
            if !book.seen_channels.insert(ch.id) {
                return Ok(());
            }
            if !roles.selects(&ch.topic) {
                skips.filtered_channels += 1;
                return Ok(());
            }
            // ROS 2 writes `cdr`; a `json`/`protobuf` channel with the TF schema
            // name is possible and is not ours to decode. Counted, not silently
            // dropped.
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
            // `log_time` is a `u64` of nanoseconds since the epoch; a value past
            // `i64::MAX` is 2262 and is corrupt, so it saturates rather than
            // wrapping into the past — which would make the future-stamp check
            // fire on every message instead of none.
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

/// Classify an `mcap` failure into this crate's `Copy` error type.
///
/// The distinction that matters to a user is compression: it is the one failure
/// with an action attached, and §0.0's `default-features = false` makes it
/// reachable on ordinary recordings.
fn map_mcap(e: &mcap::McapError) -> IngestError {
    match e {
        // Unreachable in practice now that this crate decides about codecs
        // itself — `crate::decompress` classifies the chunk before `mcap` ever
        // sees a compression field. Mapped rather than dropped so the arm cannot
        // rot into a wrong one if that ever changes.
        mcap::McapError::UnsupportedCompression(_) => IngestError::CompressedChunk {
            codec: decompress::ChunkCodec::Other,
        },
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

    /// **`--static-topic` alone does not narrow the read.** It answers "which
    /// topics are static", and an earlier revision let it silently exclude every
    /// dynamic channel — a user who renamed their static topic got the statics
    /// and none of the motion, with no error and no anomaly line.
    ///
    /// Mutant: restore `if self.static_topics.is_empty() && ...` as the early
    /// return in `selects` — applied, and the `/tf` assertion failed.
    #[test]
    fn a_renamed_static_topic_does_not_exclude_the_dynamic_ones() {
        let r = TopicRoles {
            static_topics: vec!["/fixed_frames".into()],
            dynamic_topics: Vec::new(),
        };
        assert!(r.selects("/fixed_frames"));
        assert!(r.selects("/tf"), "naming a static topic hid /tf");
        assert!(r.selects("/robot1/tf"));
        // …and it is still the thing that classifies.
        assert!(r.is_static("/fixed_frames"));
        assert!(!r.is_static("/tf"));
    }

    /// `--tf-topic` **is** the flag that narrows, and a static topic named
    /// alongside it survives the narrowing.
    ///
    /// Mutant: return `self.dynamic_topics.iter().any(...)` alone, dropping the
    /// static term — applied, and the `/fixed_frames` assertion failed.
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
