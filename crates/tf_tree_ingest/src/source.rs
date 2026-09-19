//! Reading `tf2_msgs/msg/TFMessage` out of an MCAP recording — `docs/PHASE5.md`
//! §3.3.
//!
//! A channel is a TF channel when its **schema** is `tf2_msgs/msg/TFMessage`, so
//! a remapped `/robot1/tf` is read; the topic name is consulted only in
//! [`TopicRoles`].
//!
//! The reader streams (`sans_io`) rather than mapping the file: `mcap::MessageStream`
//! needs a `&[u8]`, which would defeat `--max-memory` or need an `unsafe` map
//! (`docs/decisions/0007`), and streaming makes §3.1's two passes cheap.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use crate::cdr::{decode_tf_message, CdrError};
use crate::decompress;
use crate::IngestError;

/// The schema names this reader accepts, newest first.
///
/// The ROS 1 spelling survives `rosbags-convert`.
const TF_SCHEMAS: [&str; 2] = ["tf2_msgs/msg/TFMessage", "tf2_msgs/TFMessage"];

/// Length of MCAP's file magic, at both ends of a complete recording.
const MAGIC_LEN: usize = 8;

/// A record's framing: `opcode: u8` then `len: u64` little-endian.
///
/// This module owns the framing; `mcap::parse_record` owns every record body.
const RECORD_HEADER_LEN: usize = 1 + 8;

/// How a topic name decides whether its edges are static.
///
/// The one thing the schema cannot decide: `/tf` and `/tf_static` share a type
/// and MCAP does not record QoS durability. The default rule is the last path
/// segment (`/robot1/tf_static` matches, `/tf_static_debug` does not);
/// `--static-topic` overrides it.
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
    /// Only [`dynamic_topics`](TopicRoles::dynamic_topics) narrows the read
    /// (`--static-topic` alone must not stop `/tf` being read). With none, every
    /// TF-schema channel is read (§3.3); with some, those plus any named static ones.
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
    /// The MCAP log time (when the recorder wrote it): §3.2's "stamps far in the
    /// future" reference clock, which must not be the header stamp.
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
/// chunk's declared length, so the next record boundary needs no guess.
///
/// A skip costs: if the chunk held the only `Channel` record for `/tf`, every
/// later message is dropped as unknown-channel, with no counter. The report
/// therefore carries the skip and the lost *time span* beside `truncated`.
/// When nothing survives, [`survey`](crate::survey) fails with
/// [`IngestError::NoTransforms`], which carries no [`Anomalies`](crate::Anomalies),
/// so the skip is lost too; `tests/ingest.rs`'s
/// `a_skipped_chunk_that_carried_the_only_channel_drops_the_rest` pins it. Owed:
/// an unknown-channel drop counter and a distinct error variant.
///
/// Both passes skip a deterministic bad chunk identically.
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
/// record ceiling, the chunk skip policy, and the bounds it will decompress a
/// chunk within.
///
/// One argument rather than three, so `read_chunk` (already
/// `too_many_arguments`) does not grow: every field answers what the reader does
/// when the file is not what its headers say.
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
    /// `cdr`, once per channel id (`Bookkeeping::seen_channels` deduplicates the
    /// summary section's repeat).
    pub non_cdr: u64,
    /// Top-level records over [`ReadPolicy::max_record_bytes`] that
    /// `reader_needs` answers `false` for, stepped over instead of refusing the
    /// file. An oversized `Chunk`, `Schema`, `Channel` or `Message` still fails
    /// with [`crate::IngestError::RecordTooLarge`].
    ///
    /// **Not a promise that the transform stream is whole**: the skipped length is
    /// unvalidated, and one landing on a later record boundary passes
    /// `resyncs_here` while everything between is lost.
    pub oversized_records_skipped: u64,
    /// TF-schema channels excluded by the operator's `--tf-topic` narrowing, once
    /// per channel id.
    pub filtered_channels: u64,
    /// The recording ended mid-record; everything up to that point was read. See
    /// [`read_tf`].
    ///
    /// Recovery is record-granular, including inside a chunk. The one loss is a
    /// truncated *compressed* chunk (a partial codec frame is undecodable); it is
    /// not counted in [`bad_chunks`](SkipCounts::bad_chunks).
    pub truncated: bool,
    /// Chunks that did not decompress or did not check out, and were skipped
    /// under [`OnBadChunk::Skip`].
    pub bad_chunks: u64,
    /// The span the skipped chunks covered, from their declared message times, so
    /// an operator knows which part of the run to distrust.
    pub bad_chunk_span_ns: Option<(u64, u64)>,
    /// Of [`bad_chunks`](SkipCounts::bad_chunks), how many were refused by one of
    /// **this reader's own limits** rather than found damaged.
    ///
    /// A subset, not a separate tally: the remedy differs. Damage has none, while
    /// [`BadChunkKind::ImplausibleSize`](crate::BadChunkKind::ImplausibleSize) and
    /// [`BadChunkKind::ImplausibleWindow`](crate::BadChunkKind::ImplausibleWindow)
    /// mean a sound chunk exceeded `--max-chunk-size` (or its zstd window) and a
    /// flag fixes it.
    ///
    /// Still **skippable**: a sector-corrupted `uncompressed_size` lands here too.
    /// The fix is the diagnosis, [`IngestError::AllChunksOverLimit`].
    pub chunks_over_limit: u64,
}

/// The first sixteen bytes of every SQLite database file, including a rosbag2
/// `.db3` (<https://sqlite.org/fileformat2.html> §1.3).
const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// Whether this file is a SQLite database rather than an MCAP.
///
/// A detection, not a reader: `docs/PHASE5.md` §3.3's rosbag2 sqlite3 source is
/// not implemented (see its amendment), but naming the file beats "not a
/// well-formed MCAP". The peek is a `fill_buf` and consumes nothing.
fn is_sqlite(input: &mut BufReader<File>) -> Result<bool, IngestError> {
    let head = input.fill_buf().map_err(|e| IngestError::Io {
        raw_os_error: e.raw_os_error().unwrap_or(0),
    })?;
    Ok(head.starts_with(SQLITE_MAGIC))
}

/// Whether this reader has to look inside a top-level record of this opcode.
///
/// A `Chunk` (contains records) plus the three [`handle_record`] acts on:
/// `Schema`, `Channel`, `Message`. Written as the set the reader *needs* so a
/// newly used record kind is a change here, not a silent skip (a needed record
/// stepped over reads like a recording that lacked it). The complement is not
/// enumerated: the prose would drift from the `matches!`.
const fn reader_needs(opcode: u8) -> bool {
    matches!(
        opcode,
        mcap::records::op::CHUNK
            | mcap::records::op::SCHEMA
            | mcap::records::op::CHANNEL
            | mcap::records::op::MESSAGE
    )
}

/// Step over `len` bytes of a record body without reading them in.
///
/// Returns whether the body was entirely present. `file_len` is read once at
/// open; seeking past the end is allowed, so without the comparison a record
/// declaring more than the file holds would read as a clean end.
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

/// Read every TF transform in `path`, calling `f` once per transform.
///
/// The callback returns a `Result` so a caller can stop on a fatal anomaly
/// (§3.2's edge-kind change, the clock-reset `halt` policy).
///
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
    // Snapshotted at open, not the length now; see `complete` below.
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
    // The start magic separates "incomplete" from "not an MCAP at all".
    let mut magic = [0u8; MAGIC_LEN];
    read_exact_or_eof(&mut input, &mut magic)?;
    if magic != *mcap::MAGIC {
        return Err(IngestError::Mcap);
    }

    let mut book = Bookkeeping::default();
    let mut skips = SkipCounts::default();
    // `body` never grows past `policy.max_record_bytes`; `scratch` is bounded by
    // `policy.limits.max_uncompressed_bytes` (`--max-chunk-size`), an unrelated knob.
    // Neither is shrunk.
    let mut body: Vec<u8> = Vec::new();
    let mut scratch: Vec<u8> = Vec::new();
    let mut chunk_ordinal: u64 = 0;

    loop {
        // A truncated recording is a short one: the prefix is kept and the fact
        // recorded (`freeze --from-live` exists to capture faults, and faults
        // truncate recordings). Tail corruption reads as truncation.
        let mut header = [0u8; RECORD_HEADER_LEN];
        match read_full(&mut input, &mut header)? {
            0 => break,
            // The end magic is one byte short of a record header.
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
        // Compared in `u64` before the `usize` narrowing (32-bit targets), and
        // folded into one refusal so the remedy does not depend on word size.
        let fits = declared <= policy.max_record_bytes && usize::try_from(declared).is_ok();
        // The opcode is consulted before the length decides: the ceiling bounds
        // allocation, and a record never read (an attachment) costs none. Records
        // the reader needs still refuse, with the number to raise
        // ([`reader_needs`]).
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
            // A skipped length is unvalidated, so where it lands must look like a
            // record boundary (`resyncs_here`) or the ceiling refusal is returned.
            // A length landing on a real boundary passes: pinned by
            // `tests/record_ceiling.rs`'s
            // `a_skip_that_lands_on_a_later_boundary_loses_transforms_and_says_so`.
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
        // `u64::MAX`, and a 17-byte file declaring a huge length must not drive an
        // allocation (or "capacity overflow" panic) before `read_full` finds no
        // bytes. Where `declared <= file_len` nothing changes; otherwise `complete`
        // is false as before. It bounds by the whole file, not the remainder (a
        // running offset would be a second spelling of "where are we").
        let Ok(want) = usize::try_from(declared.min(file_len)) else {
            return Err(IngestError::RecordTooLarge {
                declared,
                ceiling: policy.max_record_bytes,
            });
        };
        // No `clear()` (the argument `decompress::decode_zstd` records): `resize`
        // zero-fills only the shortfall, `read_full` overwrites the rest, and
        // `truncate(got)` removes stale bytes. `reserve_exact` grows exactly, so
        // the `max_record_bytes` bound holds (`resize` would double).
        body.reserve_exact(want.saturating_sub(body.len()));
        body.resize(want, 0);
        let got = read_full(&mut input, &mut body)?;
        // Record-granular recovery ([`SkipCounts::truncated`]): whole-records-only
        // would lose the entire final chunk.
        //
        // Compared against `declared`, not the clamped `want`, so a file being
        // appended to while read cannot pass a length this reader chose.
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
            // A truncated non-chunk record has no recoverable interior.
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
/// Returns how many were read; a short `Read::read` is not EOF, which
/// truncation handling needs to know.
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

/// Whether the reader's current position looks like the start of a record.
///
/// The only check between a corrupt skipped length and a walk that parses
/// whatever it lands in.
///
/// Accepted: end of file, the end magic, or a header with an assigned opcode
/// whose length is no larger than the file (loose on purpose: a truncated final
/// record legitimately overshoots, and `read_tf` reports that). A private-use
/// opcode (`0x80`–`0xFF`) does not resync, so it is refused, the conservative
/// direction. The peek is undone before returning.
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

/// Read the records inside one chunk record's body.
///
/// `complete` is false when the file ended inside this chunk. A truncated
/// *uncompressed* chunk yields every whole record in its prefix; a truncated
/// **compressed** one yields none (a partial codec frame is undecodable) and
/// lands in [`SkipCounts::truncated`], never [`SkipCounts::bad_chunks`]:
/// incomplete, not damaged.
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
    // Schemas and channels may appear at top level or inside a chunk (summary
    // section or not), so one handler serves both. The chunk's message times are
    // kept so a skipped chunk can report its span.
    let span = decompress::chunk_span(body);
    let records = match decompress::chunk_records(body, complete, policy.limits, scratch) {
        Ok(r) => r,
        Err(fault) => return note_or_fail(fault, ordinal, policy.on_bad, span, skips),
    };
    // A truncated chunk ends mid-record by construction; that is the known truncation.
    let tolerate_tail = !complete;
    match decompress::for_each_record(records, tolerate_tail, |op, inner| {
        let rec = mcap::parse_record(op, inner).map_err(|e| map_mcap(&e))?;
        handle_record(rec, book, roles, skips, f)
    }) {
        Ok(()) => Ok(()),
        Err(fault) => note_or_fail(fault, ordinal, policy.on_bad, span, skips),
    }
}

/// Skip a bad chunk and count it, or fail naming it, per the policy.
///
/// Never skippable, since skipping would answer a question the user did not ask:
/// [`ChunkFault::Unsupported`] (every chunk shares the codec, so skipping all
/// yields "no transforms" about an intact file) and [`ChunkFault::Callback`] (the
/// caller's own verdict; swallowing it is silent data loss). A chunk refused by
/// this reader's own limits is skippable but counted twice
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
    // Matched on the fault so a new `BadChunkKind` forces a decision here.
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

/// Join a chunk fault to the ordinal of the chunk it came from.
///
/// A callback failure passes through: it is not a fact about the chunk.
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
///
/// Records other than Schema, Channel and Message are ignored.
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
            // Counted once: MCAP repeats every Schema and Channel in the summary section.
            if !book.seen_channels.insert(ch.id) {
                return Ok(());
            }
            if !roles.selects(&ch.topic) {
                skips.filtered_channels += 1;
                return Ok(());
            }
            // A non-`cdr` channel with the TF schema name is counted, not dropped.
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

/// Classify an `mcap` failure into this crate's `Copy` error type.
///
/// Codec classification happens in `crate::decompress` before `mcap` sees a
/// compression field, so what remains here is a malformed record body,
/// [`IngestError::Mcap`].
fn map_mcap(e: &mcap::McapError) -> IngestError {
    match e {
        // Unreachable in practice; mapped so the arm cannot rot.
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

    /// An explicit `--static-topic` disables the suffix rule.
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

    /// `--static-topic` alone does not narrow the read.
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
        assert!(r.is_static("/fixed_frames"));
        assert!(!r.is_static("/tf"));
    }

    /// `--tf-topic` narrows; a static topic named alongside it survives.
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
