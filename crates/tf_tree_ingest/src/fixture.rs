//! A **synthetic** MCAP writer for tests — not a recording.
//!
//! Nothing here came off a robot. The framing is real MCAP (from `mcap::Writer`,
//! or hand-written for the chunked fixtures) around real CDR payloads
//! ([`crate::cdr::encode_tf_message`]); it gates this crate's logic hermetically
//! and says nothing about real recordings' quirks (`docs/PHASE5.md` §0.0).
//!
//! # Compression
//!
//! [`write_mcap`] is uncompressed. [`ChunkedSpec::compressed`] compresses with
//! `ruzstd` or `lz4_flex`, the crates `crate::decompress` reads with, so it
//! proves round-trip, not conformance (`testdata/ATTRIBUTION.md`). A
//! [`ChunkDamage`] variant's documented fault is the one it produces on an
//! uncompressed chunk unless the variant says otherwise.
//!
//! # Why a second writer
//!
//! `mcap::Writer` cannot produce a damaged chunk and always emits a summary
//! section repeating every `Schema` and `Channel`. [`write_mcap_chunked`] writes
//! the framing itself with no summary, chunks on an explicit message count, and
//! damages the second chunk only. Record bodies are hand-rolled because `mcap`
//! exposes no public serializer; tests read them back with
//! `mcap::read::LinearReader` as the oracle.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write as _};
use std::path::Path;

use crate::cdr::{encode_tf_message, TransformStamped};

/// The schema name a real `rosbag2` writes for `/tf`.
pub const TF_SCHEMA: &str = "tf2_msgs/msg/TFMessage";

/// Target uncompressed chunk size for the `mcap::Writer` fixtures: **4 KiB**, so
/// each corpus spans several chunks. The hand-rolled fixtures chunk by message
/// count ([`ChunkedSpec`]).
pub const FIXTURE_CHUNK_SIZE: u64 = 4 * 1024;

/// One fabricated message: a topic, a log time, and the transforms in it.
#[derive(Clone, Debug)]
pub struct FixtureMessage {
    /// Topic to publish on; one ending in `/tf_static` is a static channel.
    pub topic: String,
    /// MCAP log time in nanoseconds.
    pub log_time_ns: i64,
    /// The transforms this `TFMessage` carries.
    pub transforms: Vec<TransformStamped>,
}

impl FixtureMessage {
    /// One transform on `/tf` whose log time equals its stamp.
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

    /// Move this message's log time away from its stamps (§3.2's future-stamp row).
    #[must_use]
    pub fn logged_at(mut self, log_time_ns: i64) -> FixtureMessage {
        self.log_time_ns = log_time_ns;
        self
    }
}

/// Write `messages` to `path` as an uncompressed MCAP.
///
/// Channels are created on first use; message order is preserved.
///
/// # Errors
///
/// Any I/O or `mcap` failure, boxed (test scaffolding).
pub fn write_mcap(
    path: &Path,
    messages: &[FixtureMessage],
) -> Result<(), Box<dyn std::error::Error>> {
    write_mcap_as(path, messages, TF_SCHEMA, &[])
}

/// Write `messages` with an explicit schema name (e.g. the ROS 1 spelling
/// `tf2_msgs/TFMessage`) and a per-topic message encoding (else `cdr`).
///
/// # Errors
///
/// Any I/O or `mcap` failure, as a boxed error — see [`write_mcap`].
pub fn write_mcap_as(
    path: &Path,
    messages: &[FixtureMessage],
    schema_name: &str,
    encodings: &[(&str, &str)],
) -> Result<(), Box<dyn std::error::Error>> {
    let out = BufWriter::new(File::create(path)?);
    let mut w = mcap::WriteOptions::new()
        .compression(None)
        .profile("ros2")
        .chunk_size(Some(FIXTURE_CHUNK_SIZE))
        .library("tf_tree_ingest fixture (synthetic, not a recording)")
        .create(out)?;
    let schema = w.add_schema(schema_name, "ros2msg", b"")?;
    let mut channels: BTreeMap<String, u16> = BTreeMap::new();
    for (sequence, m) in messages.iter().enumerate() {
        let id = match channels.get(&m.topic) {
            Some(&id) => id,
            None => {
                let encoding = encodings
                    .iter()
                    .find(|(t, _)| *t == m.topic)
                    .map_or("cdr", |(_, e)| *e);
                let id = w.add_channel(schema, &m.topic, encoding, &BTreeMap::new())?;
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
    // `Writer::finish` does not flush the `BufWriter`; `Drop` would swallow an ENOSPC.
    w.into_inner().flush()?;
    Ok(())
}

/// A small, non-degenerate recording: two static edges and three dynamic ones
/// at 100, 50 and 10 Hz, every pose with a distinct quaternion and translation.
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

/// Messages per chunk in `testdata/zstd_conformance.mcap`.
pub const CONFORMANCE_MESSAGES_PER_CHUNK: usize = 4;

/// The corpus behind `testdata/zstd_conformance.mcap`: twelve messages over three
/// chunks. **Frozen**: changing it invalidates the file
/// (`ingest::a_real_libzstd_recording_ingests`).
#[must_use]
pub fn conformance_recording() -> Vec<FixtureMessage> {
    let mut out = vec![FixtureMessage::static_edge(
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
    )];
    for i in 0..11i64 {
        let t = 1_000_000_000 + i * 10_000_000;
        let a = i as f64 * 0.05;
        out.push(FixtureMessage::dynamic(
            "odom",
            "base_link",
            t,
            [a.cos(), 0.0, 0.0, a.sin(), a, a * 2.0, a * 3.0],
        ));
    }
    out
}

/// The shape of a real `/tf`: `odom -> base_link` is stamped as published
/// (100 Hz); `map -> odom` (10 Hz) is stamped `latency_ns` before it is
/// published, so the merged stamp stream is not monotone. At 200 ms the skew
/// exceeds the 100 ms threshold and a per-stream guard halts.
#[must_use]
pub fn two_publishers_with_latency(latency_ns: i64) -> Vec<FixtureMessage> {
    let mut out = Vec::new();
    for i in 0..100i64 {
        let t = 10_000_000_000 + i * 10_000_000;
        let a = i as f64 * 0.01;
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

/// `library` every hand-rolled fixture stamps into its `Header`.
const HAND_ROLLED_LIBRARY: &str = "tf_tree_ingest hand-rolled fixture (synthetic, not a recording)";

/// Zero-based ordinal of the chunk [`ChunkDamage`] is applied to: the second.
pub const DAMAGED_CHUNK_ORDINAL: u64 = 1;

/// Fewest chunks a damaged fixture must split into, so a survivor sits on each
/// side.
const fn min_chunks_for_damage() -> usize {
    DAMAGED_CHUNK_ORDINAL as usize + 2
}

/// Which chunk carries the `Schema` and `Channel` records (no summary section).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DefinitionsIn {
    /// The first chunk, where a real recorder puts them.
    #[default]
    FirstChunk,
    /// The chunk [`ChunkedSpec::damaged`] damages; skipping it costs the only
    /// `Channel` record ([`OnBadChunk`](crate::OnBadChunk)'s caveat).
    DamagedChunk,
}

/// Which codec a hand-rolled fixture's chunk records are compressed with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FixtureCodec {
    /// Records stored verbatim; `compression` is `""`.
    #[default]
    None,
    /// `"zstd"`, via `ruzstd`'s encoder.
    Zstd,
    /// `"lz4"`, via `lz4_flex`'s frame encoder (MCAP's container, not raw blocks).
    Lz4,
}

impl FixtureCodec {
    /// The string this codec writes into a chunk header's `compression` field.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            FixtureCodec::None => "",
            FixtureCodec::Zstd => "zstd",
            FixtureCodec::Lz4 => "lz4",
        }
    }

    /// Whether this build has an encoder; otherwise `chunked_mcap_bytes` refuses
    /// with [`FixturePlanError::CodecUnavailable`].
    #[must_use]
    pub fn is_available(self) -> bool {
        match self {
            FixtureCodec::None => true,
            #[cfg(feature = "compression")]
            FixtureCodec::Zstd | FixtureCodec::Lz4 => true,
            #[cfg(not(feature = "compression"))]
            FixtureCodec::Zstd | FixtureCodec::Lz4 => false,
        }
    }
}

/// Compress one chunk's records field at the cheapest level.
///
/// `ruzstd` frames prove round-trip only and are not shaped like a recorder's;
/// never read a timing from them as a bag timing
/// (`docs/decisions/0050-what-ten-times-real-time-divides.md`).
#[cfg_attr(not(feature = "compression"), allow(unused_variables))]
fn compress_records(codec: FixtureCodec, bytes: Vec<u8>) -> Result<Vec<u8>, FixturePlanError> {
    match codec {
        FixtureCodec::None => Ok(bytes),
        #[cfg(feature = "compression")]
        FixtureCodec::Zstd => Ok(ruzstd::encoding::compress_to_vec(
            &bytes[..],
            ruzstd::encoding::CompressionLevel::Fastest,
        )),
        #[cfg(feature = "compression")]
        FixtureCodec::Lz4 => {
            use std::io::Write;
            let mut enc = lz4_flex::frame::FrameEncoder::new(Vec::new());
            enc.write_all(&bytes)
                .map_err(|_| FixturePlanError::CodecFailed {
                    codec: codec.name(),
                })?;
            enc.finish().map_err(|_| FixturePlanError::CodecFailed {
                codec: codec.name(),
            })
        }
        #[cfg(not(feature = "compression"))]
        other => Err(FixturePlanError::CodecUnavailable {
            codec: other.name(),
        }),
    }
}

/// A deliberate defect in the second chunk of a hand-rolled fixture.
///
/// Checked by `fixture::tests::each_damage_variant_produces_its_documented_fault`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChunkDamage {
    /// `compressed_size` declares more bytes than the chunk contains:
    /// `BadChunkKind::CompressedSizeMismatch` in every build.
    CompressedSizeTooLarge,
    /// `compressed_size` declares four bytes fewer than the records field holds:
    /// `BadChunkKind::StoredSizeMismatch` (before the CRC); on a compressed chunk,
    /// `BadChunkKind::Decompress`.
    CompressedSizeTooSmall,
    /// `uncompressed_crc` holds a wrong, non-zero hash: `BadChunkKind::Crc`.
    UncompressedCrc,
    /// One bit flipped inside the records field, CRC left clean:
    /// `BadChunkKind::Crc`.
    FlippedBitInRecords,
    /// The `compression` field relabelled, records left uncompressed. An unknown
    /// name (`"brotli"`) is never-skippable
    /// [`IngestError::CompressedChunk`](crate::IngestError::CompressedChunk);
    /// `"zstd"`/`"lz4"` is skippable `BadChunkKind::Decompress`
    /// (`ingest::a_mislabelled_codec_is_damage_not_an_unsupported_codec`), or
    /// `Unsupported` under `--no-default-features`.
    Relabelled(&'static str),
    /// The last inner record's declared length inflated past the end of the
    /// records field, CRC recomputed: `BadChunkKind::InnerFraming`. Records before
    /// the fault were already delivered
    /// (`ingest::a_framing_fault_mid_chunk_keeps_what_it_already_delivered`).
    InnerRecordRunsPastTheEnd,
    /// `uncompressed_size` declares 64 bytes more than the records field holds:
    /// `StoredSizeMismatch` uncompressed, `LengthMismatch` compressed
    /// (`ingest::a_lying_uncompressed_size_is_refused`).
    UncompressedSizeTooLarge,
}

/// How [`write_mcap_chunked`] lays a fixture out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkedSpec {
    /// Messages per chunk — a count, not bytes.
    pub messages_per_chunk: usize,
    /// Which chunk carries the `Schema` and `Channel` records.
    pub definitions: DefinitionsIn,
    /// The defect to write into the second chunk, if any.
    pub damage: Option<ChunkDamage>,
    /// Which codec compresses every chunk's records field.
    pub codec: FixtureCodec,
}

impl ChunkedSpec {
    /// An undamaged, uncompressed fixture with the definitions in the first chunk.
    #[must_use]
    pub fn new(messages_per_chunk: usize) -> ChunkedSpec {
        ChunkedSpec {
            messages_per_chunk,
            definitions: DefinitionsIn::FirstChunk,
            damage: None,
            codec: FixtureCodec::None,
        }
    }

    /// Compress every chunk's records with `codec`.
    #[must_use]
    pub fn compressed(mut self, codec: FixtureCodec) -> ChunkedSpec {
        self.codec = codec;
        self
    }

    /// Damage the second chunk (see [`DAMAGED_CHUNK_ORDINAL`]).
    #[must_use]
    pub fn damaged(mut self, damage: ChunkDamage) -> ChunkedSpec {
        self.damage = Some(damage);
        self
    }

    /// Put the `Schema` and `Channel` records in the chunk
    /// [`ChunkedSpec::damaged`] damages.
    #[must_use]
    pub fn definitions_in_damaged_chunk(mut self) -> ChunkedSpec {
        self.definitions = DefinitionsIn::DamagedChunk;
        self
    }

    /// Whether this layout needs a chunk on each side of the damaged one.
    fn needs_a_survivor_each_side(self) -> bool {
        self.damage.is_some() || self.definitions == DefinitionsIn::DamagedChunk
    }

    /// Ordinal of the chunk that carries the definitions.
    fn definitions_chunk(self) -> usize {
        match self.definitions {
            DefinitionsIn::FirstChunk => 0,
            DefinitionsIn::DamagedChunk => DAMAGED_CHUNK_ORDINAL as usize,
        }
    }
}

/// Why a corpus and a [`ChunkedSpec`] could not be turned into a fixture.
///
/// `Copy` and `String`-free. Nothing is clamped, since a fixture that is not what
/// was asked for makes a damage test vacuous.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FixturePlanError {
    /// The corpus does not split into enough chunks to damage the second one and
    /// still have a survivor on each side.
    #[error(
        "the corpus splits into {chunks} chunk(s); damaging the second chunk needs \
         at least {needed}, so that a test can assert the chunks either side of it \
         survived"
    )]
    TooFewChunks {
        /// How many chunks the corpus and the spec produced.
        chunks: usize,
        /// How many were needed.
        needed: usize,
    },
    /// [`ChunkedSpec::messages_per_chunk`] was `0`; refused rather than read as `1`.
    #[error(
        "messages_per_chunk was 0; a chunk layout no caller chose is how a damage \
         test starts measuring nothing, so this is refused rather than read as 1"
    )]
    ZeroMessagesPerChunk,
    /// The chunk to be damaged holds no record the damage can land on.
    #[error(
        "the chunk to damage holds no records, so {damage} would leave it intact; a \
         fixture that is quietly undamaged is worse than one that fails to build"
    )]
    NothingToDamage {
        /// Which damage found nothing to apply itself to.
        damage: &'static str,
    },
    /// [`ChunkedSpec::compressed`] asked for a codec this build cannot encode.
    #[error(
        "this build cannot write {codec} fixtures: the `compression` feature is off, so no \
         encoder is compiled in"
    )]
    CodecUnavailable {
        /// The codec name that was asked for.
        codec: &'static str,
    },
    /// An encoder that is present failed on the records it was given.
    #[error("the {codec} encoder failed on a fixture's records")]
    CodecFailed {
        /// The codec name that failed.
        codec: &'static str,
    },
}

/// Write `messages` to `path` as an MCAP with hand-rolled framing, **no summary
/// section**, and an optional deliberate defect in its second chunk. Buffers the
/// whole file; schema name and encoding are fixed at [`TF_SCHEMA`] and `cdr`.
///
/// # Errors
///
/// [`FixturePlanError`] if the spec cannot be honoured, or any I/O failure, boxed.
pub fn write_mcap_chunked(
    path: &Path,
    messages: &[FixtureMessage],
    spec: ChunkedSpec,
) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::write(path, chunked_mcap_bytes(messages, spec)?)?;
    Ok(())
}

/// The bytes [`write_mcap_chunked`] would write, without the filesystem.
///
/// # Errors
///
/// [`FixturePlanError`] if the spec cannot be honoured.
pub fn chunked_mcap_bytes(
    messages: &[FixtureMessage],
    spec: ChunkedSpec,
) -> Result<Vec<u8>, FixturePlanError> {
    if spec.messages_per_chunk == 0 {
        return Err(FixturePlanError::ZeroMessagesPerChunk);
    }
    let per = spec.messages_per_chunk;
    let chunks = messages.len().div_ceil(per);
    if spec.needs_a_survivor_each_side() && chunks < min_chunks_for_damage() {
        return Err(FixturePlanError::TooFewChunks {
            chunks,
            needed: min_chunks_for_damage(),
        });
    }

    let mut topics: Vec<&str> = Vec::new();
    for m in messages {
        if !topics.contains(&m.topic.as_str()) {
            topics.push(&m.topic);
        }
    }

    let mut out = Vec::new();
    out.extend_from_slice(mcap::MAGIC);
    push_record(
        &mut out,
        mcap::records::op::HEADER,
        &header_body("ros2", HAND_ROLLED_LIBRARY),
    );

    for (ordinal, group) in messages.chunks(per).enumerate() {
        let mut records = RecordBuf::default();
        if ordinal == spec.definitions_chunk() {
            records.push(
                mcap::records::op::SCHEMA,
                &schema_body(SCHEMA_ID, TF_SCHEMA, "ros2msg", b""),
            );
            for (i, topic) in topics.iter().enumerate() {
                records.push(
                    mcap::records::op::CHANNEL,
                    &channel_body(channel_id(i), SCHEMA_ID, topic, "cdr"),
                );
            }
        }
        for (i, m) in group.iter().enumerate() {
            let Some(slot) = topics.iter().position(|t| *t == m.topic.as_str()) else {
                continue;
            };
            records.push(
                mcap::records::op::MESSAGE,
                &message_body(
                    channel_id(slot),
                    (ordinal * per + i) as u32,
                    log_time_of(m),
                    &encode_tf_message(&m.transforms),
                ),
            );
        }
        // Real min and max: `decompress::chunk_span` reads both zero as "untracked".
        let times = group.iter().map(log_time_of);
        let start = times.clone().min().unwrap_or(0);
        let end = times.max().unwrap_or(0);
        let damage = if ordinal == DAMAGED_CHUNK_ORDINAL as usize {
            spec.damage
        } else {
            None
        };
        push_record(
            &mut out,
            mcap::records::op::CHUNK,
            &chunk_body(records, start, end, damage, spec.codec)?,
        );
    }

    // `DataEnd` with CRC `0`; a footer naming no summary.
    push_record(&mut out, mcap::records::op::DATA_END, &0u32.to_le_bytes());
    push_record(&mut out, mcap::records::op::FOOTER, &footer_body());
    out.extend_from_slice(mcap::MAGIC);
    Ok(out)
}

/// The one schema id every hand-rolled fixture uses (`0` means "no schema").
const SCHEMA_ID: u16 = 1;

/// Channel id for the `i`th topic, one-based.
fn channel_id(i: usize) -> u16 {
    (i + 1) as u16
}

/// A message's log time as MCAP stores it; negative becomes `0`, as in `write_mcap`.
fn log_time_of(m: &FixtureMessage) -> u64 {
    u64::try_from(m.log_time_ns).unwrap_or(0)
}

/// One chunk's records field under construction.
#[derive(Default)]
struct RecordBuf {
    /// The records field so far.
    bytes: Vec<u8>,
    /// Offset of the last record's `len: u64`, or `None` if there are no records.
    last_len_at: Option<usize>,
}

impl RecordBuf {
    /// Append `opcode: u8`, `len: u64`, `body` — MCAP's framing.
    fn push(&mut self, opcode: u8, body: &[u8]) {
        self.bytes.push(opcode);
        self.last_len_at = Some(self.bytes.len());
        put_u64(&mut self.bytes, body.len() as u64);
        self.bytes.extend_from_slice(body);
    }
}

/// Append one top-level record.
fn push_record(out: &mut Vec<u8>, opcode: u8, body: &[u8]) {
    out.push(opcode);
    put_u64(out, body.len() as u64);
    out.extend_from_slice(body);
}

/// Assemble a chunk record's body under `codec`, applying `damage`.
///
/// # Phase order
///
/// 1. Damage that rewrites the records, before compression.
/// 2. Compression: the size and CRC fields describe the bytes that went in.
/// 3. Damage that lies in the header, after those numbers are computed.
fn chunk_body(
    records: RecordBuf,
    start_ns: u64,
    end_ns: u64,
    damage: Option<ChunkDamage>,
    codec: FixtureCodec,
) -> Result<Vec<u8>, FixturePlanError> {
    let RecordBuf {
        mut bytes,
        last_len_at,
    } = records;
    let clean_crc = crc32fast::hash(&bytes);

    match damage {
        Some(ChunkDamage::FlippedBitInRecords) => {
            let at = bytes.len() / 2;
            match bytes.get_mut(at) {
                Some(b) => *b ^= 0x01,
                None => {
                    return Err(FixturePlanError::NothingToDamage {
                        damage: "FlippedBitInRecords",
                    })
                }
            }
        }
        Some(ChunkDamage::InnerRecordRunsPastTheEnd) => {
            let Some(at) = last_len_at else {
                return Err(FixturePlanError::NothingToDamage {
                    damage: "InnerRecordRunsPastTheEnd",
                });
            };
            let declared = u64_at(&bytes, at).saturating_add(64);
            bytes[at..at + 8].copy_from_slice(&declared.to_le_bytes());
        }
        _ => {}
    }

    // Phase 2: compression; the inner-length variant is re-hashed so the framing
    // walk, not the CRC, catches it.
    let uncompressed_size_true = bytes.len() as u64;
    let mut uncompressed_crc = if damage == Some(ChunkDamage::InnerRecordRunsPastTheEnd) {
        crc32fast::hash(&bytes)
    } else {
        clean_crc
    };
    if !codec.is_available() {
        return Err(FixturePlanError::CodecUnavailable {
            codec: codec.name(),
        });
    }
    let mut payload = compress_records(codec, bytes)?;
    let mut compression = codec.name();
    let mut uncompressed_size = uncompressed_size_true;
    let mut compressed_size = payload.len() as u64;

    match damage {
        Some(ChunkDamage::CompressedSizeTooLarge) => compressed_size += 64,
        Some(ChunkDamage::CompressedSizeTooSmall) => {
            if payload.len() < 4 {
                return Err(FixturePlanError::NothingToDamage {
                    damage: "CompressedSizeTooSmall",
                });
            }
            compressed_size -= 4;
        }
        Some(ChunkDamage::UncompressedCrc) => {
            uncompressed_crc = a_wrong_but_nonzero_crc(uncompressed_crc);
        }
        Some(ChunkDamage::Relabelled(name)) => compression = name,
        Some(ChunkDamage::UncompressedSizeTooLarge) => uncompressed_size += 64,
        _ => {}
    }

    let mut body = Vec::new();
    put_u64(&mut body, start_ns);
    put_u64(&mut body, end_ns);
    put_u64(&mut body, uncompressed_size);
    put_u32(&mut body, uncompressed_crc);
    put_str(&mut body, compression);
    put_u64(&mut body, compressed_size);
    body.append(&mut payload);
    Ok(body)
}

/// A CRC that is wrong and not `0` (`decompress::check_crc` skips `0`).
fn a_wrong_but_nonzero_crc(clean: u32) -> u32 {
    match clean ^ 0x5555_5555 {
        0 => 1,
        other => other,
    }
}

/// `Header`: `profile`, `library`.
fn header_body(profile: &str, library: &str) -> Vec<u8> {
    let mut b = Vec::new();
    put_str(&mut b, profile);
    put_str(&mut b, library);
    b
}

/// `Footer`: `summary_start`, `summary_offset_start`, `summary_crc`, all zero.
fn footer_body() -> Vec<u8> {
    let mut b = Vec::new();
    put_u64(&mut b, 0);
    put_u64(&mut b, 0);
    put_u32(&mut b, 0);
    b
}

/// `Schema`: `id`, `name`, `encoding`, `data` as a `u32`-length-prefixed blob.
fn schema_body(id: u16, name: &str, encoding: &str, data: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    put_u16(&mut b, id);
    put_str(&mut b, name);
    put_str(&mut b, encoding);
    put_u32(&mut b, data.len() as u32);
    b.extend_from_slice(data);
    b
}

/// `Channel`: `id`, `schema_id`, `topic`, `message_encoding`, `metadata`.
/// The metadata map is a `u32` byte length, not a count.
fn channel_body(id: u16, schema_id: u16, topic: &str, message_encoding: &str) -> Vec<u8> {
    let mut b = Vec::new();
    put_u16(&mut b, id);
    put_u16(&mut b, schema_id);
    put_str(&mut b, topic);
    put_str(&mut b, message_encoding);
    put_u32(&mut b, 0);
    b
}

/// `Message`: `channel_id`, `sequence`, `log_time`, `publish_time`, then the
/// payload; `publish_time` is `log_time`.
fn message_body(channel_id: u16, sequence: u32, log_time: u64, payload: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    put_u16(&mut b, channel_id);
    put_u32(&mut b, sequence);
    put_u64(&mut b, log_time);
    put_u64(&mut b, log_time);
    b.extend_from_slice(payload);
    b
}

/// A `u32`-length-prefixed string, MCAP's only string encoding.
fn put_str(out: &mut Vec<u8>, s: &str) {
    put_u32(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}

/// Little-endian, like every integer in the format.
fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// Little-endian, like every integer in the format.
fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// Little-endian, like every integer in the format.
fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// Read back a little-endian `u64` this module wrote; `0` on a short slice.
fn u64_at(bytes: &[u8], at: usize) -> u64 {
    match bytes
        .get(at..at + 8)
        .and_then(|s| <[u8; 8]>::try_from(s).ok())
    {
        Some(b) => u64::from_le_bytes(b),
        None => 0,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::decompress::{self, BadChunkKind, ChunkCodec, ChunkFault};

    /// `n` messages on one topic, 10 ms apart, each with a distinct pose.
    fn corpus(n: usize) -> Vec<FixtureMessage> {
        (0..n)
            .map(|i| {
                let k = i as f64 + 1.0;
                FixtureMessage::dynamic(
                    "base_link",
                    "sensor",
                    1_000_000_000 + i as i64 * 10_000_000,
                    [k.cos(), k.sin(), 0.0, 0.0, k, k * 2.0, k * 3.0],
                )
            })
            .collect()
    }

    /// The top-level records of a hand-rolled file, as `(opcode, body)`.
    fn top_level(bytes: &[u8]) -> Vec<(u8, Vec<u8>)> {
        let inner = &bytes[mcap::MAGIC.len()..bytes.len() - mcap::MAGIC.len()];
        let mut out = Vec::new();
        decompress::for_each_record(inner, false, |op, body| {
            out.push((op, body.to_vec()));
            Ok(())
        })
        .unwrap();
        out
    }

    /// The body of the `ordinal`th chunk record.
    fn chunk_at(bytes: &[u8], ordinal: usize) -> Vec<u8> {
        top_level(bytes)
            .into_iter()
            .filter(|(op, _)| *op == mcap::records::op::CHUNK)
            .map(|(_, body)| body)
            .nth(ordinal)
            .expect("the fixture must have that many chunks")
    }

    /// The `mcap` crate accepts every byte this writer produces. The explicit
    /// `uncompressed_crc` assertion is not redundant: `LinearReader::new` does not
    /// check chunk CRCs.
    #[test]
    fn a_clean_hand_rolled_file_is_accepted_by_the_mcap_crate() {
        let messages = corpus(6);
        let bytes = chunked_mcap_bytes(&messages, ChunkedSpec::new(2)).unwrap();

        let records: Vec<mcap::records::Record<'_>> = mcap::read::LinearReader::new(&bytes)
            .unwrap()
            .map(|r| r.expect("every record must parse"))
            .collect();

        match &records[0] {
            mcap::records::Record::Header(h) => {
                assert_eq!(h.profile, "ros2");
                assert_eq!(h.library, HAND_ROLLED_LIBRARY);
            }
            other => panic!("expected a Header first, got {other:?}"),
        }

        let chunks: Vec<(&mcap::records::ChunkHeader, &[u8])> = records
            .iter()
            .filter_map(|r| match r {
                mcap::records::Record::Chunk { header, data } => Some((header, data.as_ref())),
                _ => None,
            })
            .collect();
        assert_eq!(chunks.len(), 3, "two messages per chunk over six messages");
        for (i, (header, data)) in chunks.iter().enumerate() {
            assert_eq!(header.compression, "", "chunk {i} claims a codec");
            assert_eq!(header.compressed_size, data.len() as u64);
            assert_eq!(header.uncompressed_size, data.len() as u64);
            assert_eq!(header.uncompressed_crc, crc32fast::hash(data));
            let group = &messages[i * 2..i * 2 + 2];
            assert_eq!(header.message_start_time, log_time_of(&group[0]));
            assert_eq!(header.message_end_time, log_time_of(&group[1]));
        }

        let inner: Vec<mcap::records::Record<'_>> =
            mcap::read::LinearReader::sans_magic(chunks[0].1)
                .map(|r| r.expect("every inner record must parse"))
                .collect();
        match &inner[0] {
            mcap::records::Record::Schema { header, data } => {
                assert_eq!(header.id, SCHEMA_ID);
                assert_eq!(header.name, TF_SCHEMA);
                assert_eq!(header.encoding, "ros2msg");
                assert!(data.is_empty());
            }
            other => panic!("expected a Schema first, got {other:?}"),
        }
        match &inner[1] {
            mcap::records::Record::Channel(ch) => {
                assert_eq!(ch.id, channel_id(0));
                assert_eq!(ch.schema_id, SCHEMA_ID);
                assert_eq!(ch.topic, "/tf");
                assert_eq!(ch.message_encoding, "cdr");
                assert!(ch.metadata.is_empty());
            }
            other => panic!("expected a Channel second, got {other:?}"),
        }
        match &inner[2] {
            mcap::records::Record::Message { header, data } => {
                assert_eq!(header.channel_id, channel_id(0));
                assert_eq!(header.sequence, 0);
                assert_eq!(header.log_time, log_time_of(&messages[0]));
                assert_eq!(header.publish_time, header.log_time);
                assert_eq!(data.as_ref(), encode_tf_message(&messages[0].transforms));
            }
            other => panic!("expected a Message third, got {other:?}"),
        }
        assert_eq!(inner.len(), 4, "a Schema, a Channel and two Messages");
        let later: Vec<mcap::records::Record<'_>> =
            mcap::read::LinearReader::sans_magic(chunks[1].1)
                .map(|r| r.expect("every inner record must parse"))
                .collect();
        assert_eq!(later.len(), 2);
        assert!(later
            .iter()
            .all(|r| matches!(r, mcap::records::Record::Message { .. })));

        let all_messages: Vec<(u16, u32, u64, u64, Vec<u8>)> = chunks
            .iter()
            .flat_map(|(_, data)| mcap::read::LinearReader::sans_magic(data))
            .filter_map(|r| match r.expect("every inner record must parse") {
                mcap::records::Record::Message { header, data } => Some((
                    header.channel_id,
                    header.sequence,
                    header.log_time,
                    header.publish_time,
                    data.to_vec(),
                )),
                _ => None,
            })
            .collect();
        let want: Vec<(u16, u32, u64, u64, Vec<u8>)> = messages
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let t = log_time_of(m);
                (
                    channel_id(0),
                    i as u32,
                    t,
                    t,
                    encode_tf_message(&m.transforms),
                )
            })
            .collect();
        assert_eq!(
            all_messages, want,
            "every message must carry its own channel, sequence, times and payload"
        );

        match records.last() {
            Some(mcap::records::Record::Footer(f)) => {
                assert_eq!(f.summary_start, 0, "a summary section would be repeated");
                assert_eq!(f.summary_offset_start, 0);
                assert_eq!(f.summary_crc, 0);
            }
            other => panic!("expected a Footer last, got {other:?}"),
        }
        assert!(
            records
                .iter()
                .any(|r| matches!(r, mcap::records::Record::DataEnd(_))),
            "a conforming data section ends with DataEnd"
        );
        assert!(
            !records.iter().any(|r| matches!(
                r,
                mcap::records::Record::Statistics(_)
                    | mcap::records::Record::ChunkIndex(_)
                    | mcap::records::Record::SummaryOffset(_)
            )),
            "this writer must emit no summary records at all"
        );
    }

    /// Damage lands on the second chunk only.
    #[test]
    fn damage_lands_on_the_second_chunk_and_nowhere_else() {
        let messages = corpus(9);
        let spec = ChunkedSpec::new(3);
        let clean = chunked_mcap_bytes(&messages, spec).unwrap();
        for damage in [
            ChunkDamage::CompressedSizeTooLarge,
            ChunkDamage::CompressedSizeTooSmall,
            ChunkDamage::UncompressedCrc,
            ChunkDamage::FlippedBitInRecords,
            ChunkDamage::Relabelled("zstd"),
            ChunkDamage::InnerRecordRunsPastTheEnd,
            ChunkDamage::UncompressedSizeTooLarge,
        ] {
            let bad = chunked_mcap_bytes(&messages, spec.damaged(damage)).unwrap();
            for survivor in [0usize, 2] {
                assert_eq!(
                    chunk_at(&clean, survivor),
                    chunk_at(&bad, survivor),
                    "{damage:?} altered chunk {survivor}"
                );
            }
            assert_ne!(
                chunk_at(&clean, DAMAGED_CHUNK_ORDINAL as usize),
                chunk_at(&bad, DAMAGED_CHUNK_ORDINAL as usize),
                "{damage:?} changed nothing, so any test using it is vacuous"
            );
        }
    }

    /// Every [`ChunkDamage`]'s documented fault, checked against `crate::decompress`.
    #[test]
    fn each_damage_variant_produces_its_documented_fault() {
        let messages = corpus(9);
        let spec = ChunkedSpec::new(3);
        let fault_of = |damage: ChunkDamage| -> Option<ChunkFault> {
            let bytes = chunked_mcap_bytes(&messages, spec.damaged(damage)).unwrap();
            let body = chunk_at(&bytes, DAMAGED_CHUNK_ORDINAL as usize);
            let mut scratch = Vec::new();
            let limits = crate::IngestOptions::default().chunk_limits();
            match decompress::chunk_records(&body, true, limits, &mut scratch) {
                Err(fault) => Some(fault),
                Ok(records) => decompress::for_each_record(records, false, |_, _| Ok(())).err(),
            }
        };

        assert!(matches!(
            fault_of(ChunkDamage::CompressedSizeTooLarge),
            Some(ChunkFault::Bad(BadChunkKind::CompressedSizeMismatch { .. }))
        ));
        assert!(matches!(
            fault_of(ChunkDamage::CompressedSizeTooSmall),
            Some(ChunkFault::Bad(BadChunkKind::StoredSizeMismatch { .. }))
        ));
        assert!(matches!(
            fault_of(ChunkDamage::UncompressedCrc),
            Some(ChunkFault::Bad(BadChunkKind::Crc { .. }))
        ));
        assert!(matches!(
            fault_of(ChunkDamage::FlippedBitInRecords),
            Some(ChunkFault::Bad(BadChunkKind::Crc { .. }))
        ));
        #[cfg(feature = "compression")]
        assert!(
            matches!(
                fault_of(ChunkDamage::Relabelled("zstd")),
                Some(ChunkFault::Bad(BadChunkKind::Decompress {
                    codec: ChunkCodec::Zstd
                }))
            ),
            "got {:?}",
            fault_of(ChunkDamage::Relabelled("zstd"))
        );
        #[cfg(not(feature = "compression"))]
        assert_eq!(
            fault_of(ChunkDamage::Relabelled("zstd")),
            Some(ChunkFault::Unsupported(ChunkCodec::Zstd))
        );
        assert_eq!(
            fault_of(ChunkDamage::Relabelled("brotli")),
            Some(ChunkFault::Unsupported(ChunkCodec::Other)),
            "a name no build knows classifies as Other, not as a damaged chunk"
        );
        assert!(matches!(
            fault_of(ChunkDamage::InnerRecordRunsPastTheEnd),
            Some(ChunkFault::Bad(BadChunkKind::InnerFraming { .. }))
        ));
        assert!(
            matches!(
                fault_of(ChunkDamage::UncompressedSizeTooLarge),
                Some(ChunkFault::Bad(BadChunkKind::StoredSizeMismatch { .. }))
            ),
            "got {:?}",
            fault_of(ChunkDamage::UncompressedSizeTooLarge)
        );
    }

    /// A compressed fixture round-trips and the codec is really in the file.
    #[cfg(feature = "compression")]
    #[test]
    fn a_compressed_fixture_round_trips_through_the_reader() {
        let messages = corpus(9);
        let plain = chunked_mcap_bytes(&messages, ChunkedSpec::new(3)).unwrap();
        let limits = crate::IngestOptions::default().chunk_limits();
        for codec in [FixtureCodec::Zstd, FixtureCodec::Lz4] {
            let spec = ChunkedSpec::new(3).compressed(codec);
            let bytes = chunked_mcap_bytes(&messages, spec).unwrap();
            for ordinal in 0..3usize {
                let body = chunk_at(&bytes, ordinal);
                let compression = compression_of(&body);
                assert_eq!(compression, codec.name(), "chunk {ordinal}");
                let (declared_uncompressed, declared_compressed) = sizes_of(&body);
                assert_ne!(
                    declared_compressed, declared_uncompressed,
                    "{codec:?} chunk {ordinal} was written uncompressed under a codec name"
                );

                let mut scratch = Vec::new();
                let got = decompress::chunk_records(&body, true, limits, &mut scratch)
                    .unwrap_or_else(|e| panic!("{codec:?} chunk {ordinal}: {e:?}"));
                let mut plain_scratch = Vec::new();
                let plain_body = chunk_at(&plain, ordinal);
                let want = decompress::chunk_records(&plain_body, true, limits, &mut plain_scratch)
                    .unwrap();
                assert_eq!(got, want, "{codec:?} chunk {ordinal} did not round-trip");
            }
        }
    }

    /// The `compression` field of a chunk record body.
    #[cfg(feature = "compression")]
    fn compression_of(body: &[u8]) -> &str {
        let len = u32::from_le_bytes([body[28], body[29], body[30], body[31]]) as usize;
        core::str::from_utf8(&body[32..32 + len]).unwrap()
    }

    /// `(uncompressed_size, compressed_size)` from a chunk record body.
    #[cfg(feature = "compression")]
    fn sizes_of(body: &[u8]) -> (u64, u64) {
        let name_len = u32::from_le_bytes([body[28], body[29], body[30], body[31]]) as usize;
        (u64_at(body, 16), u64_at(body, 32 + name_len))
    }

    /// A codec-free build refuses to write a compressed fixture.
    #[cfg(not(feature = "compression"))]
    #[test]
    fn a_codec_free_build_refuses_to_write_a_compressed_fixture() {
        for codec in [FixtureCodec::Zstd, FixtureCodec::Lz4] {
            let err =
                chunked_mcap_bytes(&corpus(9), ChunkedSpec::new(3).compressed(codec)).unwrap_err();
            assert_eq!(
                err,
                FixturePlanError::CodecUnavailable {
                    codec: codec.name()
                }
            );
        }
    }

    /// A corpus without a survivor on each side of the damage is an error.
    #[test]
    fn a_corpus_too_short_to_damage_is_refused() {
        let spec = ChunkedSpec::new(3).damaged(ChunkDamage::UncompressedCrc);
        assert_eq!(
            chunked_mcap_bytes(&corpus(6), spec).unwrap_err(),
            FixturePlanError::TooFewChunks {
                chunks: 2,
                needed: 3
            }
        );
        assert_eq!(
            chunked_mcap_bytes(&corpus(2), spec).unwrap_err(),
            FixturePlanError::TooFewChunks {
                chunks: 1,
                needed: 3
            }
        );
        assert!(chunked_mcap_bytes(&corpus(6), ChunkedSpec::new(3)).is_ok());
        assert!(chunked_mcap_bytes(
            &corpus(6),
            ChunkedSpec::new(3).definitions_in_damaged_chunk()
        )
        .is_err());
        assert_eq!(min_chunks_for_damage(), DAMAGED_CHUNK_ORDINAL as usize + 2);
    }

    /// `messages_per_chunk: 0` is refused, not read as one.
    #[test]
    fn a_zero_chunk_size_is_refused_rather_than_clamped() {
        assert_eq!(
            chunked_mcap_bytes(&corpus(9), ChunkedSpec::new(0)).unwrap_err(),
            FixturePlanError::ZeroMessagesPerChunk
        );
        assert_eq!(
            chunked_mcap_bytes(
                &corpus(9),
                ChunkedSpec::new(0).damaged(ChunkDamage::UncompressedCrc)
            )
            .unwrap_err(),
            FixturePlanError::ZeroMessagesPerChunk
        );
    }

    /// A damage with nothing to land on is refused, not applied to nothing.
    #[test]
    fn a_damage_with_nothing_to_land_on_is_refused() {
        for damage in [
            ChunkDamage::FlippedBitInRecords,
            ChunkDamage::InnerRecordRunsPastTheEnd,
            ChunkDamage::CompressedSizeTooSmall,
        ] {
            let err = chunk_body(RecordBuf::default(), 0, 0, Some(damage), FixtureCodec::None)
                .unwrap_err();
            assert!(
                matches!(err, FixturePlanError::NothingToDamage { .. }),
                "{damage:?} left an empty chunk intact: {err:?}"
            );
        }
        assert!(chunk_body(RecordBuf::default(), 0, 0, None, FixtureCodec::None).is_ok());
    }

    /// `DefinitionsIn::DamagedChunk` moves all the `Schema` and `Channel` records.
    #[test]
    fn the_definitions_can_be_moved_into_the_damaged_chunk() {
        let bytes = chunked_mcap_bytes(
            &corpus(9),
            ChunkedSpec::new(3).definitions_in_damaged_chunk(),
        )
        .unwrap();
        let ops_in = |ordinal: usize| -> Vec<u8> {
            let body = chunk_at(&bytes, ordinal);
            let mut scratch = Vec::new();
            let limits = crate::IngestOptions::default().chunk_limits();
            let records = decompress::chunk_records(&body, true, limits, &mut scratch).unwrap();
            let mut ops = Vec::new();
            decompress::for_each_record(records, false, |op, _| {
                ops.push(op);
                Ok(())
            })
            .unwrap();
            ops
        };
        use mcap::records::op::{CHANNEL, MESSAGE, SCHEMA};
        assert_eq!(ops_in(0), vec![MESSAGE, MESSAGE, MESSAGE]);
        assert_eq!(
            ops_in(1),
            vec![SCHEMA, CHANNEL, MESSAGE, MESSAGE, MESSAGE],
            "chunk 1 must hold the definitions"
        );
        assert_eq!(ops_in(2), vec![MESSAGE, MESSAGE, MESSAGE]);
    }
}
