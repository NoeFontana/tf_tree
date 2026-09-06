//! A **synthetic** MCAP writer for tests — not a recording.
//!
//! # Say it plainly
//!
//! Nothing this module produces came off a robot. There is no rosbag2 bag in
//! this repository, and vendoring one would add tens of megabytes and a
//! licensing question for a test that needs a few kilobytes. So the fixtures
//! here are *fabricated*: real MCAP framing (written either by the `mcap` crate's
//! own writer or, for the chunked fixtures, by hand — see "Why there is a second
//! writer") around real CDR payloads (written by [`crate::cdr::encode_tf_message`],
//! whose byte-level agreement with the wire is proved separately against
//! hand-assembled bytes in `cdr::tests::wire_bytes_decode_w_last`).
//!
//! What that buys is a hermetic test of *this crate's* logic — schema-based
//! discovery, the two passes, every §3.2 anomaly — with no container and no
//! network. What it does **not** buy is any evidence about real recordings'
//! quirks. `docs/PHASE5.md` §0.0 records that ROS 2 is available in a container
//! and can produce a real recording; that is the test this one does not replace.
//!
//! # Compression: `mcap::Writer`'s fixtures are uncompressed, the hand-rolled
//! ones need not be
//!
//! [`write_mcap`] passes
//! [`WriteOptions::compression(None)`](mcap::WriteOptions::compression), and that
//! stays true for a reason that has not changed: `mcap` is taken
//! `default-features = false`, so *its* writer has no codecs to offer.
//!
//! The hand-rolled writer does. [`ChunkedSpec::compressed`] compresses each
//! chunk's records with `ruzstd` or `lz4_flex` — the same pure-Rust crates
//! `crate::decompress` reads them back with — so the compressed path has fixtures
//! at all. Two honest caveats, because "it round-trips" is a weaker claim than it
//! looks:
//!
//! * A fixture this module compresses proves **round-trip**, not conformance with
//!   what a real recorder writes. `testdata/zstd_conformance.mcap` is compressed
//!   by the real `zstd` CLI (libzstd 1.5.5) for exactly that gap; see
//!   `testdata/ATTRIBUTION.md`. There is no `lz4` CLI on this host, so lz4 closes
//!   the same gap from the other end — a **hand-authored 82-byte frame** in
//!   `crate::decompress`'s tests, written from the LZ4 specification rather than by
//!   any encoder. What lz4 still lacks is a whole *recording* from an independent
//!   writer; `testdata/ATTRIBUTION.md` states the remaining asymmetry exactly.
//! * The codec is orthogonal to [`ChunkDamage`], and each damage variant's
//!   documented fault is the one it produces on an **uncompressed** chunk unless
//!   the variant says otherwise. `ChunkDamage::UncompressedSizeTooLarge` is the
//!   one deliberately exercised both ways, because the check that catches it is a
//!   different check on each path.
//!
//! # Why there is a second writer
//!
//! [`write_mcap`] delegates to `mcap::Writer`, and two of that writer's
//! properties make it too weak a fixture for the corrupt-chunk skip policy
//! [`OnBadChunk`](crate::OnBadChunk) implements:
//!
//! * **It cannot produce a damaged chunk.** Every chunk it writes has a correct
//!   length and a correct CRC, by construction. Nothing it emits is skippable, so
//!   the skip path can only be reached by damaging bytes after the fact, and a
//!   blind byte-patch of its output cannot say *which* chunk it hit.
//! * **It always emits a summary section**, which repeats every `Schema` and
//!   `Channel` record at the end of the file. So even if a chunk carrying the only
//!   `Channel` record for `/tf` were skipped, the definition would still arrive
//!   from the summary and every later message would decode — which makes the
//!   caveat `OnBadChunk`'s docs promise ("if the skipped chunk held the only
//!   `Channel` record, every later message on that channel is dropped")
//!   *unreachable* with a fixture from that writer.
//!
//! [`write_mcap_chunked`] writes the framing itself, emits **no** summary
//! section, chunks on an explicit message count so a test knows exactly which
//! messages a chunk holds, and applies a [`ChunkDamage`] to the second chunk
//! only. It is strictly more demanding than the crate writer on all three counts,
//! which is the only reason to own a second writer.
//!
//! Every record *body* it writes is hand-rolled, because `mcap` exposes no public
//! function that serializes one: `records::{Header, SchemaHeader, Channel,
//! MessageHeader, ChunkHeader, Footer}` derive `binrw::BinWrite`, and writing
//! through that derive needs `binrw` in scope as a direct dependency, which the
//! dependency budget forbids. So the bodies are written by hand and read back in
//! this module's tests with `mcap::read::LinearReader`, whose iterator hands every
//! record body to `mcap::parse_record` — the crate remains the oracle even though
//! it cannot be the writer. What that oracle does *not* cover is stated where it
//! is used (`a_clean_hand_rolled_file_is_accepted_by_the_mcap_crate`): with chunks
//! emitted raw it never computes a chunk CRC, so the CRC is checked by an explicit
//! assertion there.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write as _};
use std::path::Path;

use crate::cdr::{encode_tf_message, TransformStamped};

/// The schema name a real `rosbag2` writes for `/tf`.
pub const TF_SCHEMA: &str = "tf2_msgs/msg/TFMessage";

/// Target uncompressed chunk size for the `mcap::Writer` fixtures in this module:
/// **4 KiB**, against `mcap`'s 1 MiB default. The hand-rolled fixtures do not read
/// it — they chunk by message count, see [`ChunkedSpec`] — so editing this number
/// moves [`write_mcap`] and [`write_mcap_as`] and nothing else.
///
/// The corpora here are a few kilobytes, so at the default each fixture would be
/// exactly one chunk — and one chunk is a degenerate fixture for two properties
/// this crate now depends on.
///
/// **Truncation recovery is record-granular, and a chunk boundary is where it
/// gets interesting.** `read_tf` owns the record framing (see
/// `crate::decompress` for why), so a recording cut mid-chunk still yields every
/// whole record in that chunk's prefix. What one chunk cannot exercise is the
/// case a real recording always presents — earlier chunks complete, the last one
/// cut — which is what `truncation_recovery_is_record_granular` measures across
/// several cut points.
///
/// An earlier revision of this comment claimed recovery was *chunk*-granular and
/// that a cut mid-chunk lost that chunk entirely. That was true before the framing
/// moved into this crate and is exactly the sort of stale claim a reader acts on: it
/// would justify deleting the record-granular tests as testing something the
/// reader does not do.
///
/// **The inner-record walk is entered once per chunk.** Records never straddle a
/// chunk boundary — the format does not permit it, and
/// `crate::decompress::for_each_record` is restarted from a clean start on each
/// chunk's records field — so a single-chunk fixture exercises that walk exactly
/// once, from a clean start, which is the case least likely to be wrong. Several
/// chunks enter it several times, each with a different prefix of the corpus
/// behind it.
pub const FIXTURE_CHUNK_SIZE: u64 = 4 * 1024;

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
    write_mcap_as(path, messages, TF_SCHEMA, &[])
}

/// Write `messages` with an explicit schema name, and a per-topic message
/// encoding for the topics named in `encodings` (everything else is `cdr`).
///
/// Both are things the reader keys on and neither is reachable through
/// [`write_mcap`]: the ROS 1 schema spelling `tf2_msgs/TFMessage`, which a bag
/// converted by `rosbags-convert` keeps, and a non-`cdr` encoding on a TF-schema
/// channel, which is counted and skipped rather than decoded. The encoding is
/// **per topic** rather than per file because the interesting recording is the
/// *mixed* one — a file where nothing decodes cannot show that the skip was
/// counted, only that the ingest failed.
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
        // **Deliberately far below `mcap`'s 1 MiB default, so a fixture spans
        // many chunks rather than exactly one.**
        //
        // Every corpus in this module is a few kilobytes, so at the default a
        // fixture is a single chunk — and a single chunk hides two things that
        // matter. `crate::decompress::for_each_record` is then walked once and
        // never across a boundary; and a truncation test can only ever cut inside
        // the *first* chunk, never with complete chunks before the cut, which is
        // the shape every real recording has (see [`FIXTURE_CHUNK_SIZE`]).
        //
        // A real recording is chunked at 1–4 MiB and holds far more per chunk,
        // so this is not realism — it is the same number of chunks a real
        // recording has, at this corpus's scale.
        .chunk_size(Some(FIXTURE_CHUNK_SIZE))
        .library("tf_tree_ingest fixture (synthetic, not a recording)")
        .create(out)?;
    // An empty schema payload: MCAP requires the schema *record* to exist so
    // discovery works, and nothing in this crate parses the IDL text. A real
    // rosbag2 puts the `.msg` definition here.
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
    // **`Writer::finish` does not flush the stream it wrote into.** It writes
    // `DataEnd`, the summary section and the closing magic *through* the writer
    // and returns; `mcap`'s own `into_inner` doc says to use it "if you wish to
    // handle any errors returned when the underlying stream is closed". The
    // stream here is a `BufWriter`, so without this the file's tail — footer and
    // end magic included — reaches disk only when `Drop` runs, and `Drop for
    // BufWriter` discards the flush error. A full or read-only filesystem
    // therefore produced a truncated fixture while this function returned `Ok`,
    // which would surface as the *ingest* under test failing rather than as the
    // ENOSPC that actually happened.
    w.into_inner().flush()?;
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

/// Messages per chunk in `testdata/zstd_conformance.mcap`.
///
/// A constant shared by the generator and the test rather than a number written
/// twice: the test compares the committed file against a control it writes from
/// [`conformance_recording`], and a chunk layout that differed between the two
/// would make the comparison a comparison of layouts.
pub const CONFORMANCE_MESSAGES_PER_CHUNK: usize = 4;

/// The corpus behind `testdata/zstd_conformance.mcap` — **small on purpose**.
///
/// Twelve messages over three chunks, a few kilobytes in total. It is committed as
/// a binary file, so its size is a permanent cost to every clone; what it has to
/// exercise is that libzstd's frames decode, that a chunk boundary is crossed, and
/// that statics and dynamics both survive. None of that needs a hundred messages.
///
/// Kept separate from [`small_recording`] because it is **frozen**: the committed
/// file was generated from these exact bytes, so changing this function invalidates
/// it and `ingest::a_real_libzstd_recording_ingests` will say so by failing on the
/// transform count.
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

// ---------------------------------------------------------------------------
// The hand-rolled chunk writer, and its deliberate-damage surface.
//
// See this module's docs for why `mcap::Writer` cannot serve these tests. What
// follows owns three things it cannot: the chunk boundary, the absence of a
// summary section, and the damage.
// ---------------------------------------------------------------------------

/// `library` every hand-rolled fixture stamps into its `Header` record.
///
/// It says "synthetic" for the same reason [`write_mcap`]'s does: a file recovered
/// from a scratch directory six months from now must not be mistakeable for a
/// recording.
const HAND_ROLLED_LIBRARY: &str = "tf_tree_ingest hand-rolled fixture (synthetic, not a recording)";

/// Zero-based ordinal of the chunk [`ChunkDamage`] is applied to: **the second**.
///
/// The second and not the first, so a test can assert that the chunks *before* and
/// *after* the damaged one both survived — a skip that silently ended the read
/// would pass a test that only looked at the first chunk.
pub const DAMAGED_CHUNK_ORDINAL: u64 = 1;

/// Fewest chunks a damaged fixture must split into, so there is a survivor on
/// each side of the damage.
///
/// **Derived from [`DAMAGED_CHUNK_ORDINAL`] rather than written down**, because the
/// two encode one property and a hardcoded `3` left behind by a moved ordinal
/// fails in the worst direction: asking to damage a chunk past the last one passes
/// the guard, matches nothing in the loop, and hands back a pristine recording —
/// exactly the vacuous fixture [`FixturePlanError`] exists to refuse.
const fn min_chunks_for_damage() -> usize {
    DAMAGED_CHUNK_ORDINAL as usize + 2
}

/// Which chunk carries the `Schema` and `Channel` records.
///
/// Only reachable because [`write_mcap_chunked`] emits **no summary section**: in
/// a file with one, the definitions are repeated at the end and their placement in
/// the data section stops mattering.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DefinitionsIn {
    /// The first chunk, which is where a real recorder puts them.
    #[default]
    FirstChunk,
    /// The chunk [`ChunkedSpec::damaged`] damages, wherever
    /// [`DAMAGED_CHUNK_ORDINAL`] puts it — named for that relationship and not for
    /// a position, because the placement is only interesting for its overlap with
    /// the damage.
    ///
    /// It is the layout that makes the caveat in
    /// [`OnBadChunk`](crate::OnBadChunk)'s docs reproducible: skipping this chunk
    /// costs the only `Channel` record in the file, so every message in every later
    /// chunk belongs to a channel the reader has never heard of and is dropped
    /// without a counter.
    DamagedChunk,
}

/// Which codec a hand-rolled fixture's chunk records are compressed with.
///
/// Defaults to [`FixtureCodec::None`] so that **every fixture written before this
/// existed is byte-identical to what it was** — the corrupt-chunk suite computes
/// its expectations from exact chunk layouts, and a fixture that quietly started
/// compressing would move all of them at once.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FixtureCodec {
    /// Records stored verbatim; `compression` is `""`.
    #[default]
    None,
    /// `"zstd"`, via `ruzstd`'s encoder.
    Zstd,
    /// `"lz4"`, via `lz4_flex`'s **frame** encoder — which is the container MCAP's
    /// `"lz4"` names, and not the raw block format.
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

    /// Whether this build has an **encoder** for it.
    ///
    /// Both encoders arrive with the `compression` feature, so a codec-free build
    /// can neither write nor read a compressed fixture — which is consistent, and
    /// is why `chunked_mcap_bytes` refuses with
    /// [`FixturePlanError::CodecUnavailable`] rather than silently writing an
    /// uncompressed chunk under a compressed name. That silent substitution is the
    /// vacuous-fixture failure this module's error type exists to prevent.
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

/// Compress one chunk's records field, as the codec MCAP names it does.
///
/// The compression level is the cheapest one on purpose: these corpora are a few
/// kilobytes and the property under test is the framing and the length bookkeeping,
/// not the ratio.
///
/// # These frames are written by the decoder's own crate — never read a timing taken
/// from them as a bag timing
///
/// `ruzstd`'s encoder, not libzstd, so every compressed fixture in this crate proves
/// **round-trip** and not conformance, and its frames are not shaped like the ones a
/// recorder writes: it declares a 128 KiB window where a streaming `zstd -19`
/// declares 8 MiB, so a fixture exercises a different window path, and `ruzstd`
/// decodes its own output faster than a libzstd frame of the same size.
/// `testdata/zstd_conformance.mcap` is what closes the conformance half for zstd;
/// for lz4, with no `lz4` CLI on this host, it is closed by a hand-authored spec
/// frame instead of by an independently written file, and
/// `testdata/ATTRIBUTION.md` states what that does and does not cover.
///
/// **CORRECTION (2026-09-05): a decode-rate ratio was being read as a throughput
/// one, and the paragraph that did it is deleted.** This doc read: "Measured on
/// this host: `ruzstd` decodes its own `Fastest` output at 849 MiB/s and a libzstd
/// `-3` frame at 674 MiB/s … So a fixture understates the decode cost of a real
/// recording by roughly **1.3×**", followed by "There is also **no ingest benchmark
/// in `crates/tf_tree_bench`**, so `just bench-check` cannot see a throughput
/// regression on this path and `docs/PHASE5.md` §12's ingest gate is asserted by no
/// code. A benchmark whose corpus came from *this* function would understate it by
/// the factor above, which is why one has not simply been added; §11 records what
/// it would take." Three things were wrong with that. The two MiB/s figures had
/// **no producer anywhere in this repository**, which is what
/// `docs/benchmarks/EVIDENCE.md` exists to prevent. The 1.3× is a **per-byte**
/// ratio and does not transfer to a recording, because a libzstd frame decodes
/// slower per byte *and* there are fewer bytes of it — the two act in opposite
/// directions and their net is derived nowhere. And §11 recorded no such thing: it
/// had no ingest-benchmark row at all until `docs/PHASE5.md` §12 criterion 5's
/// gate landed, which is `just gate5` and
/// `docs/decisions/0050-what-ten-times-real-time-divides.md`.
///
/// **§12's ingest gate is asserted by code now**, and it generates its corpus with
/// this writer — so what the gate measures is a round trip through the decoder's
/// own encoder, which `0050` records as a stated limit rather than as a
/// justification for not measuring at all.
///
/// **`bytes` is taken by value so the uncompressed arm is a move.** It used to take
/// a slice and `to_vec()` it, which reintroduced exactly the full-size copy the
/// `body.append` at this function's only call site claims to have removed — the copy
/// was back and only the comment said otherwise. The compressed arms read it as a
/// slice and would not care either way; the uncompressed one is every fixture this
/// repository wrote before codecs existed, so it is the arm worth not copying.
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
/// Each variant documents the fault it produces, and
/// `fixture::tests::each_damage_variant_produces_its_documented_fault` is where
/// each of those claims is checked against `crate::decompress` rather than
/// asserted in prose.
///
/// **Six of the seven produce a** [`BadChunkKind`](crate::BadChunkKind) on an
/// uncompressed chunk — including [`ChunkDamage::UncompressedSizeTooLarge`], which
/// this paragraph used to say produced nothing at all and which now produces
/// `StoredSizeMismatch`. Two of those six name a *different* `BadChunkKind` when the
/// chunk is compressed, and each says which.
///
/// [`ChunkDamage::Relabelled`] is the one whose fault depends on the **build**: a
/// codec name no build has a decoder for is `ChunkFault::Unsupported`, which is
/// deliberately not damage in the `BadChunkKind` sense and is never skippable, while
/// `"zstd"`/`"lz4"` in a default build (`compression` is on by default) reach the
/// decoder and come back as `BadChunkKind::Decompress`, which *is* skippable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChunkDamage {
    /// `compressed_size` declares more bytes than the chunk record contains.
    ///
    /// Detected in every build, as `BadChunkKind::CompressedSizeMismatch`: the
    /// chunk is complete, so its declared size is compared against what is present
    /// rather than clamped the way a truncated chunk's is.
    ///
    /// **Not `LengthMismatch`**, which it used to be. Neither number in this fault
    /// is an uncompressed byte count and no decoder has run, so the message that
    /// variant renders ("it declared N uncompressed bytes and produced M") named
    /// the wrong field of the header.
    CompressedSizeTooLarge,
    /// `compressed_size` declares four bytes fewer than the records field holds.
    ///
    /// Detected in every build, as `BadChunkKind::StoredSizeMismatch`, because on
    /// an uncompressed chunk the two size fields must agree and this variant makes
    /// them differ by four. **Not `LengthMismatch`**, which it used to be: neither
    /// number is a decoder's output, and the variant's own docs say why the
    /// distinction is worth a variant.
    ///
    /// **It used to be caught by the CRC instead, and the change closed a real
    /// gap.** A short declaration is satisfiable, so the reader used to hand over a
    /// shortened records field and the CRC — which covers the whole field — was the
    /// only thing that disagreed. `decompress::check_crc` returns `Ok`
    /// unconditionally when the saved hash is `0`, which the MCAP specification
    /// defines as "not computed" and which real writers do produce, so a chunk with
    /// no computed CRC whose `compressed_size` was short by exactly the framed size
    /// of its last inner record lost that record with **no fault at all**: the walk
    /// stopped on a clean boundary and had nothing to complain about. The
    /// `uncompressed_size == compressed_size` check now fires first and does not
    /// depend on a CRC being present.
    ///
    /// On a *compressed* chunk the same lie truncates the codec frame instead, so
    /// it surfaces as `BadChunkKind::Decompress`. No fixture combines the two;
    /// stated so that nobody reads this row as covering both.
    CompressedSizeTooSmall,
    /// `uncompressed_crc` holds a hash the records do not have.
    ///
    /// Detected in every build, as `BadChunkKind::Crc`. Deliberately never `0`,
    /// which the MCAP specification defines as "not computed" and the reader
    /// therefore skips.
    UncompressedCrc,
    /// One bit flipped inside the records field, with `uncompressed_crc` left as
    /// computed over the **clean** bytes.
    ///
    /// Detected in every build, as `BadChunkKind::Crc`. This is the variant that
    /// models real corruption — a bad sector, a truncated write replayed over — and
    /// the CRC is the only thing that can catch it, since the flipped byte is
    /// inside a CDR payload that would otherwise decode to a wrong-but-plausible
    /// transform.
    FlippedBitInRecords,
    /// The `compression` field relabelled to a codec name, with the records left
    /// uncompressed.
    ///
    /// **The fault depends on whether this build has a decoder for the name.**
    /// A name no build knows ([`ChunkCodec::Other`](crate::ChunkCodec::Other), e.g.
    /// `"brotli"`) is `ChunkFault::Unsupported` and therefore
    /// [`IngestError::CompressedChunk`](crate::IngestError::CompressedChunk), which is
    /// never skippable. `"zstd"` and `"lz4"` classify as those codecs, so in the
    /// default build the decoder runs over uncompressed bytes and fails:
    /// `BadChunkKind::Decompress`, which **is** skippable under
    /// [`OnBadChunk::Skip`](crate::OnBadChunk::Skip) — see
    /// `ingest::a_mislabelled_codec_is_damage_not_an_unsupported_codec`. Only under
    /// `--no-default-features` is `"zstd"` itself `Unsupported`.
    Relabelled(&'static str),
    /// The last inner record's declared length inflated so its body runs past the
    /// end of the records field, with `uncompressed_crc` recomputed over the
    /// patched bytes.
    ///
    /// Detected in every build, as `BadChunkKind::InnerFraming`. The CRC is
    /// recomputed **on purpose**: a CRC that failed first would mean this variant
    /// never reached the framing walk, and it would then be testing the same code
    /// path as [`ChunkDamage::UncompressedCrc`] while appearing to test another.
    ///
    /// **This is the one variant whose skip is not all-or-nothing, and a test using
    /// it must say so.** The fault is raised by `for_each_record` *during* the walk,
    /// so every record before it has already been handed to the reader's callback
    /// and is in the tree; `note_or_fail` then counts the whole chunk as skipped and
    /// reports its whole header span as lost. The chunk is simultaneously partly
    /// ingested and reported as gone.
    /// `ingest::a_framing_fault_mid_chunk_keeps_what_it_already_delivered` pins the
    /// exact numbers and records the consequence as a `source.rs` finding; the
    /// variants caught by `chunk_records` ([`ChunkDamage::UncompressedCrc`],
    /// [`ChunkDamage::FlippedBitInRecords`], the two `compressed_size` lies) are
    /// rejected before any record is delivered and are the all-or-nothing case.
    InnerRecordRunsPastTheEnd,
    /// `uncompressed_size` declares 64 bytes more than the records field holds.
    ///
    /// **Detected in every build, but by a different check on each path and as a
    /// different fault, which is why this is the one variant worth writing both
    /// ways.**
    ///
    /// * Uncompressed: `BadChunkKind::StoredSizeMismatch`. The records are stored
    ///   verbatim, so `uncompressed_size == compressed_size` is an invariant,
    ///   checkable from two `u64`s nine bytes apart in a header `ChunkHead::parse`
    ///   already walks past. `mcap`'s own writer and reader treat them as equal, and
    ///   `fixture::tests::a_clean_hand_rolled_file_is_accepted_by_the_mcap_crate`
    ///   asserts it of every clean chunk here. No decoder is involved, which is
    ///   exactly what makes the fault kind a different one.
    /// * Compressed: `BadChunkKind::LengthMismatch`. The field is the allocation
    ///   size, so the decoder is handed a 64-byte-too-large output buffer, produces
    ///   less than it, and the produced length — a real count, from a decoder that
    ///   really ran — disagrees with the declared one.
    ///
    /// An earlier revision of this variant was undetected in every build, and its
    /// docs said so at length. Both halves of that gap are closed;
    /// `ingest::a_lying_uncompressed_size_is_refused` and
    /// `ingest::a_short_decompression_is_not_read_as_a_short_recording` are where.
    UncompressedSizeTooLarge,
}

/// How [`write_mcap_chunked`] lays a fixture out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkedSpec {
    /// Messages per chunk. **A count, not a byte budget**, so a test that damages
    /// the second chunk knows exactly which messages that costs — a size heuristic
    /// would make the answer depend on the CDR payload's length.
    pub messages_per_chunk: usize,
    /// Which chunk carries the `Schema` and `Channel` records.
    pub definitions: DefinitionsIn,
    /// The defect to write into the second chunk, if any.
    pub damage: Option<ChunkDamage>,
    /// Which codec compresses **every** chunk's records field.
    ///
    /// Every chunk and not one, because that is what a recorder writes: a bag is
    /// compressed or it is not. A per-chunk codec would be a file no tool
    /// produces, and the interesting property of a compressed recording is that
    /// the reader meets the codec on every chunk rather than once.
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
/// `Copy` and `String`-free like every other error in this crate
/// (`docs/PROJECT.md` §5), even though its only consumer is a test: a fixture
/// helper that silently produced something *other* than what was asked for is how
/// a damage test goes vacuous, so this is an error rather than a clamp.
///
/// **Nothing about a [`ChunkedSpec`] is clamped**, which is the half of that policy
/// that has to be maintained in the writer rather than stated here: a zero chunk
/// size and a damage that lands on nothing are both refused below rather than
/// reinterpreted, because either would change the chunk layout every damage test
/// computes its expectations from.
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
    /// [`ChunkedSpec::messages_per_chunk`] was `0`.
    ///
    /// Refused rather than read as `1`, because a computed count that came out zero
    /// (`messages.len() / groups` with too many groups, say) would otherwise make
    /// [`DAMAGED_CHUNK_ORDINAL`] name the second *message* instead of the second
    /// group, and a long enough corpus still splits into three chunks — so
    /// [`FixturePlanError::TooFewChunks`] would not fire and the test would assert
    /// survivor counts against a layout nobody chose.
    #[error(
        "messages_per_chunk was 0; a chunk layout no caller chose is how a damage \
         test starts measuring nothing, so this is refused rather than read as 1"
    )]
    ZeroMessagesPerChunk,
    /// The chunk to be damaged holds no record the damage can land on.
    ///
    /// Three variants are defined relative to the records field's own contents — the
    /// bit flip, the inflated inner length, and the four-byte-short
    /// `compressed_size` — and an empty field leaves each of them with nothing to
    /// change. All three would otherwise return a chunk that is byte-for-byte
    /// intact, which is a silently undamaged fixture: the corrupt-chunk tests would
    /// pass while nothing was corrupt. Unreachable as this writer stands
    /// (`slice::chunks` yields no empty group and every message becomes a record);
    /// refused so that it stays
    /// unreachable when a later spec knob can produce an empty chunk.
    #[error(
        "the chunk to damage holds no records, so {damage} would leave it intact; a \
         fixture that is quietly undamaged is worse than one that fails to build"
    )]
    NothingToDamage {
        /// Which damage found nothing to apply itself to.
        damage: &'static str,
    },
    /// [`ChunkedSpec::compressed`] asked for a codec this build cannot encode.
    ///
    /// Refused rather than written uncompressed under a compressed name, which
    /// would be a fixture that silently stopped testing compression — and would
    /// then be *read* as a relabelled chunk, i.e. as a different test entirely.
    #[error(
        "this build cannot write {codec} fixtures: the `compression` feature is off, so no \
         encoder is compiled in"
    )]
    CodecUnavailable {
        /// The codec name that was asked for.
        codec: &'static str,
    },
    /// An encoder that is present failed on the records it was given.
    ///
    /// Its own variant rather than a `panic!` (which this workspace denies) or a
    /// silent fallback: an encoder failing on a few kilobytes of MCAP records
    /// would mean the codec crate is broken, and a fixture helper is the wrong
    /// place to decide that quietly.
    #[error("the {codec} encoder failed on a fixture's records")]
    CodecFailed {
        /// The codec name that failed.
        codec: &'static str,
    },
}

/// Write `messages` to `path` as an MCAP with hand-rolled framing, **no summary
/// section**, and an optional deliberate defect in its second chunk.
///
/// See this module's docs for why this exists beside [`write_mcap`], and
/// [`ChunkDamage`] for what each defect produces.
///
/// Two asymmetries with [`write_mcap`] are worth knowing before sizing a fixture
/// for this writer. It **buffers the whole file** and writes it in one call, where
/// `write_mcap` streams through a `BufWriter`, so a large corpus costs its own size
/// in memory. And the schema name and message encoding are fixed at [`TF_SCHEMA`]
/// and `cdr`: the knobs [`write_mcap_as`] exposes have no equivalent here, and a
/// chunked fixture that needs one should grow [`ChunkedSpec`] rather than fork a
/// third writer.
///
/// # Errors
///
/// [`FixturePlanError`] if the spec cannot be honoured, or any I/O failure, as a
/// boxed error — this is test scaffolding and its caller is a test.
pub fn write_mcap_chunked(
    path: &Path,
    messages: &[FixtureMessage],
    spec: ChunkedSpec,
) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::write(path, chunked_mcap_bytes(messages, spec)?)?;
    Ok(())
}

/// The bytes [`write_mcap_chunked`] would write.
///
/// Exposed separately because a test that wants to *truncate* a hand-rolled
/// fixture, or to compare a damaged file against a clean one byte for byte, has no
/// business going through the filesystem to do it.
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

    // **Every channel is declared in one place, whichever chunk that is.** A
    // recorder declares a channel just before its first message, but this writer's
    // whole purpose is to make the *placement* of the definitions a variable, and
    // channels spread over three chunks would mean a skip cost some channels and
    // not others — a fixture whose loss is a function of the corpus rather than of
    // the spec.
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
                // An empty schema payload, as in `write_mcap_as`: MCAP requires the
                // record to exist so discovery works, and nothing in this crate
                // parses the IDL text.
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
            // Unreachable by construction: `topics` was collected from these same
            // messages a few lines above, so every topic has a slot. Written as a
            // `continue` rather than an error because an error variant for a state
            // this function makes impossible would be untestable dead code.
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
        // **The real min and max, not zero.** `decompress::chunk_span` reports both
        // zero as "this writer did not track message times" and hands the reader
        // `None`, so a lazy zero here would make every assertion about a skipped
        // chunk's lost span pass without measuring anything.
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

    // `DataEnd` carries a data-section CRC of `0`, i.e. "not computed". Written
    // because a conforming data section ends with it and a fixture that omitted it
    // would be a weaker file than a recorder's; `handle_record` ignores it, which
    // is itself worth exercising.
    push_record(&mut out, mcap::records::op::DATA_END, &0u32.to_le_bytes());
    // **The whole point: a footer that names no summary.** All three fields zero
    // means there is no summary section, so the `Schema` and `Channel` records
    // exist only where this writer put them.
    push_record(&mut out, mcap::records::op::FOOTER, &footer_body());
    out.extend_from_slice(mcap::MAGIC);
    Ok(out)
}

/// The one schema id every hand-rolled fixture uses. `0` is reserved by the MCAP
/// specification for "no schema".
const SCHEMA_ID: u16 = 1;

/// Channel id for the `i`th topic, one-based to keep `0` free the way a real
/// recorder does.
fn channel_id(i: usize) -> u16 {
    (i + 1) as u16
}

/// A message's log time as MCAP stores it.
///
/// Negative is not representable, and a fixture that wanted one is asking for a
/// file no recorder can write; `write_mcap` makes the same substitution.
fn log_time_of(m: &FixtureMessage) -> u64 {
    u64::try_from(m.log_time_ns).unwrap_or(0)
}

/// One chunk's records field under construction.
///
/// It tracks where the **last** record's length field sits, which is the one thing
/// [`ChunkDamage::InnerRecordRunsPastTheEnd`] needs and which cannot be recovered
/// from the finished bytes without walking them again.
#[derive(Default)]
struct RecordBuf {
    /// The records field so far.
    bytes: Vec<u8>,
    /// Offset of the last record's `len: u64`, or `None` if there are no records.
    last_len_at: Option<usize>,
}

impl RecordBuf {
    /// Append `opcode: u8`, `len: u64`, `body` — MCAP's framing, which is the same
    /// inside a chunk as at the top level.
    fn push(&mut self, opcode: u8, body: &[u8]) {
        self.bytes.push(opcode);
        self.last_len_at = Some(self.bytes.len());
        put_u64(&mut self.bytes, body.len() as u64);
        self.bytes.extend_from_slice(body);
    }
}

/// Append one top-level record: the same framing, at the outer level.
fn push_record(out: &mut Vec<u8>, opcode: u8, body: &[u8]) {
    out.push(opcode);
    put_u64(out, body.len() as u64);
    out.extend_from_slice(body);
}

/// Assemble a chunk record's body under `codec`, applying `damage`.
///
/// The field order is the specification's and is checked against
/// `decompress::ChunkHead::parse` and `mcap::parse_record` by this module's tests:
/// `message_start_time`, `message_end_time`, `uncompressed_size`,
/// `uncompressed_crc`, `compression`, `compressed_size`, then the records.
///
/// # The three phases are ordered, and the order is the whole subtlety
///
/// 1. **Damage that rewrites the records themselves** — the bit flip and the
///    inflated inner length. It must happen before compression, or the codec would
///    faithfully encode clean bytes and the fixture would be undamaged.
/// 2. **Compression**, which fixes `compressed_size` and leaves
///    `uncompressed_size` and `uncompressed_crc` describing the bytes that went
///    *in*. That is what the MCAP specification says both fields cover, and it is
///    what makes the CRC a check on the decoder's output rather than on its input.
/// 3. **Damage that lies in the header** — the two `compressed_size` variants, the
///    CRC, the relabel and `uncompressed_size`. After compression, because each of
///    them is a lie about a number compression has just computed.
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
    // **Hashed before phase 1**, so [`ChunkDamage::FlippedBitInRecords`] keeps the
    // hash of the *clean* bytes — a CRC that agreed with the flip would make that
    // variant an intact chunk, and the CRC is the only thing that can witness a
    // bit flip inside a CDR payload.
    let clean_crc = crc32fast::hash(&bytes);

    // Phase 1: damage that rewrites the records field.
    match damage {
        Some(ChunkDamage::FlippedBitInRecords) => {
            // Mid-field, so the flip lands inside a message body rather than in the
            // first record's opcode — a corrupted opcode would be caught by the
            // framing walk and this variant is about the CRC being the only witness.
            let at = bytes.len() / 2;
            match bytes.get_mut(at) {
                Some(b) => *b ^= 0x01,
                // An empty records field has no bit to flip, and returning the chunk
                // unflipped would hand back an intact fixture under a damaged name.
                None => {
                    return Err(FixturePlanError::NothingToDamage {
                        damage: "FlippedBitInRecords",
                    })
                }
            }
        }
        Some(ChunkDamage::InnerRecordRunsPastTheEnd) => {
            // No last record means no length field to inflate; the CRC computed
            // below over unpatched bytes would then be the honest hash and the
            // chunk would read as intact.
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

    // Phase 2: compression, and the two fields that describe what went into it.
    //
    // **Which bytes the CRC covers is the whole difference** between the CRC check
    // catching a variant and the framing walk catching it.
    // `InnerRecordRunsPastTheEnd` is re-hashed over its *patched* bytes on purpose:
    // a CRC that failed first would mean it never reached the framing walk, and it
    // would then be testing the same code path as `UncompressedCrc` while appearing
    // to test another.
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

    // Phase 3: damage that lies in the header.
    match damage {
        Some(ChunkDamage::CompressedSizeTooLarge) => compressed_size += 64,
        Some(ChunkDamage::CompressedSizeTooSmall) => {
            // Fewer than four bytes to give up means the subtraction saturates and
            // the declaration stops being a lie, so the fixture would be intact.
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
        // The relabel overrides the codec's own name by design: its purpose is a
        // `compression` field that disagrees with the bytes, and on an
        // uncompressed fixture (the only way it is used) there is no name to lose.
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
    // Moved rather than copied: the records field is the whole chunk and this is the
    // one full-size copy in the writer that costs nothing to remove. It is a move all
    // the way back to the caller's records for an uncompressed fixture, which is what
    // `compress_records` taking `bytes` by value buys — an earlier revision copied
    // there and left this comment describing a saving it had undone.
    body.append(&mut payload);
    Ok(body)
}

/// A CRC that is wrong and is **not** `0`.
///
/// `0` means "not computed" per the MCAP specification and `decompress::check_crc`
/// skips it, so a lie that happened to land on zero would be a fixture that
/// quietly stopped being damaged — and the test asserting a `Crc` fault would fail
/// for a reason no one could read off the assertion.
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

/// `Footer`: `summary_start`, `summary_offset_start`, `summary_crc` — all zero,
/// which is how a file says it has no summary section.
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
///
/// The metadata map is a `u32` **byte** length followed by key/value strings, not a
/// count — an empty map is therefore a bare zero, and getting that wrong makes
/// `mcap::parse_record` read the next field as a string length.
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
/// payload to the end of the record.
///
/// `publish_time` is `log_time`: these fixtures are not modelling transport delay,
/// and `RawRecord::log_time_ns` is the only one of the two the reader consumes.
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

/// Read back a little-endian `u64` this module wrote, for patching one in place.
///
/// Falls back to `0` rather than panicking on a short slice, because the workspace
/// lints deny `unwrap` and the offset is always in range by construction.
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

    /// Six messages on one topic, ten milliseconds apart, with a distinct pose
    /// each so a swapped or duplicated message body is visible.
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
    ///
    /// Walked with `decompress::for_each_record`, which is legitimate here for one
    /// specific reason: MCAP's framing is identical at the top level and inside a
    /// chunk, and this helper is used to reach a *damaged* chunk's body, which the
    /// `mcap` crate's own reader refuses to hand over (it validates chunk CRCs and
    /// lengths, which is exactly what the damage breaks).
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

    /// **The `mcap` crate accepts every byte this writer produces.** Its own reader
    /// walks the file and hands every record body to `mcap::parse_record`, which
    /// reads back the field values that went in — including the outer framing and
    /// each chunk's declared `compressed_size` against the bytes present.
    ///
    /// **What that reader does not check is the chunk CRC**, so the explicit
    /// `uncompressed_crc` assertion below is not redundant. `LinearReader::new` does
    /// set `with_validate_chunk_crcs(true)`, but it also sets
    /// `with_emit_chunks(true)` (mcap-0.25.0 `src/read.rs:56`), and the CRC is only
    /// computed on the chunk-expansion path, which `!emit_chunks` gates
    /// (`sans_io/linear_reader.rs:475`). Verified rather than reasoned: a fixture
    /// damaged with `UncompressedCrc` or `FlippedBitInRecords` is read by
    /// `LinearReader::new` as six good records and no error. The chunk CRC is
    /// covered here and in `each_damage_variant_produces_its_documented_fault`, via
    /// `crate::decompress::check_crc`.
    ///
    /// This is the test that keeps every damage test below meaningful: a writer
    /// whose clean output was subtly wrong would produce faults that look like the
    /// damage and are not.
    ///
    /// Mutant: emit `uncompressed_size` before `message_end_time` in `chunk_body`,
    /// i.e. one field pair transposed — applied, and this failed on
    /// `uncompressed_size`, which read back as `1010000000` (the first chunk's end
    /// time) against the 345 bytes actually present. Mutant 2: write a string's
    /// length prefix as `u16` in `put_str` — applied, and `LinearReader` refused
    /// the very first record with "not enough bytes in reader", before any
    /// assertion ran. Mutant 3: add a millisecond to `log_time` for every message
    /// except the first of each chunk — applied, and the per-message table below
    /// failed. That mutant is the reason the table exists: it survives the entire
    /// integration suite, because `IngestReport` takes its per-edge times from the
    /// CDR *stamps* and never from MCAP's `log_time`, so nothing outside this loop
    /// can see a `log_time` the writer got wrong after the first record.
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
            // The real message times, which is what a skipped chunk reports as the
            // span it lost.
            let group = &messages[i * 2..i * 2 + 2];
            assert_eq!(header.message_start_time, log_time_of(&group[0]));
            assert_eq!(header.message_end_time, log_time_of(&group[1]));
        }

        // The definitions are in the first chunk, and the first chunk only.
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
        // A later chunk carries messages and nothing else, which is what makes the
        // definitions' placement a variable worth having.
        let later: Vec<mcap::records::Record<'_>> =
            mcap::read::LinearReader::sans_magic(chunks[1].1)
                .map(|r| r.expect("every inner record must parse"))
                .collect();
        assert_eq!(later.len(), 2);
        assert!(later
            .iter()
            .all(|r| matches!(r, mcap::records::Record::Message { .. })));

        // **Every message, not just the first.** A stamp or a payload that was right
        // in the first record of the first chunk and wrong afterwards is invisible to
        // the report — `IngestReport` derives its per-edge times from the CDR
        // *stamps*, so a drifting MCAP `log_time` reaches no assertion outside this
        // loop — and the fixture would then make some unrelated timing test fail with
        // a diagnosis pointing at the engine.
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

        // **No summary section**, which is the property that makes a skipped
        // definitions chunk unrecoverable and is therefore the point.
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

    /// **Damage lands on the second chunk and nowhere else.** Every variant leaves
    /// chunks 0 and 2 byte-identical to the clean fixture's.
    ///
    /// Without this, a damage variant that corrupted the whole file would still
    /// make `one_corrupt_chunk_does_not_lose_the_recording` fail loudly but for the
    /// wrong reason, and one that corrupted *nothing* would make it pass
    /// vacuously — the second is the dangerous one, so the differing-chunk
    /// assertion is here too.
    ///
    /// Mutant: drop the `ordinal == DAMAGED_CHUNK_ORDINAL as usize` guard in
    /// `chunked_mcap_bytes`, damaging every chunk — applied, and this failed on
    /// chunk 0 for the first variant.
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

    /// Every [`ChunkDamage`]'s documented fault, checked against
    /// `crate::decompress` rather than asserted in prose.
    ///
    /// Three of these are not what a reader would guess, which is why the table is
    /// worth pinning. A **long** `compressed_size` is a `CompressedSizeMismatch`
    /// and not a `LengthMismatch`, because no decoder has run and neither number is
    /// an uncompressed byte count. A **short** one is caught by the
    /// `uncompressed_size == compressed_size` invariant rather than by the CRC —
    /// which is what closed the "no computed CRC" gap the variant's docs used to
    /// record as live. And a lying `uncompressed_size` is caught in *every* build,
    /// including the codec-free one, by that same invariant; an earlier revision of
    /// this paragraph said it was caught by nothing, which the row at the bottom of
    /// this test disproves.
    ///
    /// Mutant: `chunk_body` hashing the records *after* the flip for
    /// `FlippedBitInRecords` — applied, and the `FlippedBitInRecords` row failed
    /// with **no fault at all**: the flip lands inside a CDR payload, so with a
    /// matching CRC the chunk reads as intact and the fixture has silently stopped
    /// being damaged. Mutant 2: `a_wrong_but_nonzero_crc` returning `0` — applied,
    /// and the `UncompressedCrc` row failed the same way, since `0` means "not
    /// computed" per the specification and `check_crc` skips it.
    #[test]
    fn each_damage_variant_produces_its_documented_fault() {
        let messages = corpus(9);
        let spec = ChunkedSpec::new(3);
        // What `read_chunk` does: take the records field, then walk it.
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
        // A short `compressed_size` is now caught by the size invariant rather than
        // by the CRC, which is what closes the "no computed CRC" gap the variant's
        // docs used to record as a live one.
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
        // **`Relabelled("zstd")`'s fault depends on the build, and that is the
        // point of the variant rather than a wart.** With a zstd decoder compiled
        // in, `"zstd"` over uncompressed records is a chunk that lies about its
        // payload — damage, and skippable. Without one it is a codec this build
        // does not have — unsupported, and never skippable. Both rows are asserted
        // in the configuration that can reach them.
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
        // **The row that used to read `None`.** `uncompressed_size` is now retained
        // and, on an uncompressed chunk, compared against `compressed_size` — an
        // invariant that needed no decoder and was simply unchecked.
        assert!(
            matches!(
                fault_of(ChunkDamage::UncompressedSizeTooLarge),
                Some(ChunkFault::Bad(BadChunkKind::StoredSizeMismatch { .. }))
            ),
            "got {:?}",
            fault_of(ChunkDamage::UncompressedSizeTooLarge)
        );
    }

    /// **A compressed fixture round-trips, and the codec is really in the file.**
    ///
    /// Both halves matter. The chunk header must name the codec and carry a
    /// `compressed_size` that differs from `uncompressed_size` — otherwise the
    /// fixture is an uncompressed one under a compressed name, which is a
    /// *different* test (`ChunkDamage::Relabelled`) wearing this one's name. And
    /// the records must come back byte-identical to what an uncompressed fixture
    /// of the same corpus holds, which is what makes the ingest-level comparison
    /// in `tests/ingest.rs` a comparison of the reader rather than of two corpora.
    ///
    /// Mutant: have `compress_records` return `bytes.to_vec()` for every codec, i.e.
    /// write the codec name without compressing — applied, and this failed on
    /// `Zstd chunk 0 was written uncompressed under a codec name`, with both sizes at
    /// 476. Mutant 2: `compressed_size = uncompressed_size_true`, i.e. describe the
    /// bytes that went *into* the codec — applied, and this failed the same
    /// assertion, the same way. Both took four other tests with them, including
    /// `ingest::a_zstd_recording_ingests_identically`, which failed on its own
    /// vacuity guard: `the compressed fixture (21332 B) is not smaller than the
    /// uncompressed one (21320 B), so this test compares nothing`.
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
                // The header really names the codec, and really shrank.
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
    ///
    /// Only the compressed round-trip test reads it, so it is gated with that test
    /// rather than carrying an `allow(dead_code)`: a helper nothing calls is dead in
    /// exactly one configuration, and the `cfg` says which.
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

    /// A codec-free build **refuses** to write a compressed fixture rather than
    /// writing an uncompressed one under a compressed name.
    ///
    /// The silent substitution would be worse than a missing test: the fixture
    /// would then be read as a *relabelled* chunk, so a compression test would pass
    /// while exercising the mislabelled-payload path instead.
    ///
    /// Mutant: delete `chunk_body`'s `codec.is_available()` guard — applied, and
    /// **the whole suite still passed**, in both feature configurations:
    /// `compress_records`'s own `#[cfg(not(feature = "compression"))]` arm returns
    /// the same `CodecUnavailable`, with the same codec name. The property is therefore
    /// **structurally guarded** by that arm; the guard in `chunk_body` is a second,
    /// earlier statement of it, kept because it is where a reader looks. What this
    /// test adds is that the refusal is an error a caller can read rather than a
    /// silent substitution, asserted in the one configuration where it happens.
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

    /// A corpus too small to have a survivor on each side of the damage is an
    /// **error**, not a quietly undamaged fixture.
    ///
    /// This is the guard that stops a damage test from going vacuous after someone
    /// shortens its corpus: with two chunks there is no third to assert survived,
    /// and with one there is not even a second to damage.
    ///
    /// Mutant: delete the `chunks < min_chunks_for_damage()` branch — applied, and
    /// this failed on the first `unwrap_err`, with the whole two-chunk file printed
    /// as the unexpected `Ok`. It was the **only** failure in the crate, which is
    /// the point: nothing else notices a damage fixture that has no surviving chunk
    /// after the damage. Mutant 2: `min_chunks_for_damage` returning
    /// `DAMAGED_CHUNK_ORDINAL as usize + 1`, i.e. requiring only that the damaged
    /// chunk *exist* — applied, and this failed on the first `unwrap_err`, which
    /// returned the whole two-chunk file: a fixture whose damage is in its last
    /// chunk, so nothing after the fault is left to prove the read resumed.
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
        // The same corpus is fine when nothing is damaged and the definitions are
        // where a recorder puts them.
        assert!(chunked_mcap_bytes(&corpus(6), ChunkedSpec::new(3)).is_ok());
        // …and asking for the definitions in the damaged chunk needs three too,
        // because that layout exists to be damaged.
        assert!(chunked_mcap_bytes(
            &corpus(6),
            ChunkedSpec::new(3).definitions_in_damaged_chunk()
        )
        .is_err());
        // The requirement is derived from the ordinal, not written down: a survivor
        // on each side of chunk `DAMAGED_CHUNK_ORDINAL` is two more chunks than the
        // ordinal itself.
        assert_eq!(min_chunks_for_damage(), DAMAGED_CHUNK_ORDINAL as usize + 2);
    }

    /// `messages_per_chunk: 0` is **refused**, not read as one.
    ///
    /// A count that came out zero by arithmetic — `messages.len() / groups` with more
    /// groups than messages — used to be clamped to one, which silently made
    /// `DAMAGED_CHUNK_ORDINAL` name the second *message* instead of the second group.
    /// A corpus long enough still splits into three chunks that way, so
    /// `TooFewChunks` never fired and the damage tests asserted survivor counts
    /// against a layout nobody chose.
    ///
    /// Mutant: restore `let per = spec.messages_per_chunk.max(1)` and delete the
    /// guard — applied, and this failed on `unwrap_err` with a nine-chunk file, one
    /// message each.
    #[test]
    fn a_zero_chunk_size_is_refused_rather_than_clamped() {
        assert_eq!(
            chunked_mcap_bytes(&corpus(9), ChunkedSpec::new(0)).unwrap_err(),
            FixturePlanError::ZeroMessagesPerChunk
        );
        // And with damage asked for as well, since that is the caller who would be
        // hurt by the clamp.
        assert_eq!(
            chunked_mcap_bytes(
                &corpus(9),
                ChunkedSpec::new(0).damaged(ChunkDamage::UncompressedCrc)
            )
            .unwrap_err(),
            FixturePlanError::ZeroMessagesPerChunk
        );
    }

    /// A damage with nothing to land on is **refused**, not applied to nothing.
    ///
    /// Three arms patch or shorten the records field itself, and an empty field
    /// leaves each of them with nothing to do: the bit flip has no byte, the inflated
    /// inner length has no length field (and would then recompute the *honest* CRC
    /// over unpatched bytes), and the short `compressed_size` saturates back to the
    /// truthful one. Each would hand back an intact chunk under a damaged name, which
    /// makes every test using it pass while nothing is corrupt.
    ///
    /// `chunk_body` is called directly because the public writer cannot reach this
    /// state today — `slice::chunks` yields no empty group and every message becomes
    /// a record — and the guard exists so that it stays unreachable when a later spec
    /// knob (a flush-on-timer chunk, a definitions-only chunk) can produce an empty
    /// one.
    ///
    /// Mutant: restore `if let Some(b) = bytes.get_mut(at)` in the
    /// `FlippedBitInRecords` arm, so an empty field is flipped and nothing happens —
    /// applied, and this failed on the first `unwrap_err`, which returned a 40-byte
    /// all-zero chunk body: a well-formed, empty, entirely undamaged chunk.
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
        // An empty chunk is still writable when nothing is asked of it, so the guard
        // is about the damage and not about the emptiness.
        assert!(chunk_body(RecordBuf::default(), 0, 0, None, FixtureCodec::None).is_ok());
    }

    /// `DefinitionsIn::DamagedChunk` really moves the `Schema` and `Channel`
    /// records, and moves *all* of them.
    ///
    /// Mutant: `definitions_chunk` returning `0` for both variants — applied, and
    /// this failed on chunk **0**, which came back as `[3, 4, 5, 5, 5]` (a Schema, a
    /// Channel and three Messages) against the three Messages expected. Without
    /// that guarantee
    /// `a_skipped_chunk_that_carried_the_only_channel_drops_the_rest` would be
    /// testing the ordinary layout and passing for the wrong reason.
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
