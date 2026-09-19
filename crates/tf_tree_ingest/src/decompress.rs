//! Chunk handling: this crate owns MCAP's record framing and reads inside chunks.
//!
//! `mcap` is taken `default-features = false` (`docs/PHASE2.md` §2 forbids a C build
//! step), so its compressed-chunk features are off. Chunks are handed over whole
//! (`with_emit_chunks(true)`) and decoded here with pure-Rust `ruzstd` and `lz4_flex`
//! under the default `compression` feature; without it a zstd/lz4 chunk is
//! [`ChunkFault::Unsupported`] (`tests/codec_free.rs`). Owning the framing also gives
//! record-granular recovery inside a truncated chunk, byte offsets, and chunk CRC
//! validation (`mcap`'s runs only under `validate_chunk_crcs`, off by default).
//! The one unrecoverable case is a truncated compressed chunk (a partial codec frame),
//! reported as truncation, not corruption: see [`chunk_records`].
//!
//! The C-free decoder is slower: `ruzstd` decodes several times slower than libzstd,
//! and decompression repeats on every pass (`1 + groups + spilled edges`).
//! `docs/PHASE5.md` §12's throughput gate (`just gate5`) is met by more than an order
//! of magnitude.
//!
//! The framing is nine bytes (`opcode: u8`, `len: u64` LE, `body[len]`) plus an
//! eight-byte magic at each end of the file, and is the same inside a chunk. There are
//! two walks of it: [`for_each_record`] over an in-hand `&[u8]`, and `source::read_tf`
//! over a `BufReader`, which must bound each declared length before allocating. Keep
//! them consistent by hand. Record bodies (`parse_record`, `records::*`,
//! `ChunkHeader`) stay with `mcap`.
//!
//! # Decompression is bounded three times
//!
//! A chunk header is two numbers off a disk that may be lying. [`ChunkLimits`] bounds
//! `uncompressed_size` absolutely and as a ratio, before anything is allocated. Neither
//! codec crate bounds total output (`ruzstd` caps only the window, `lz4_flex` only the
//! per-block size), so the caller-sized buffer is the guarantee. The third bound,
//! `window_ceiling`, covers the zstd decoder's own working allocation, which neither
//! knob can see; a violation is [`BadChunkKind::ImplausibleWindow`].

use crate::IngestError;

/// Which codec a chunk's `compression` field names. `Copy` and `String`-free
/// (`docs/PROJECT.md` §5), so [`ChunkCodec::Other`] loses the name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChunkCodec {
    /// `""` — the chunk's records are stored uncompressed.
    None,
    /// `"zstd"`.
    Zstd,
    /// `"lz4"`.
    Lz4,
    /// A name this build does not recognise at all.
    Other,
}

impl ChunkCodec {
    /// Classify the `compression` field. Case-sensitive: the specification fixes the strings.
    pub(crate) fn parse(name: &str) -> Self {
        match name {
            "" => Self::None,
            "zstd" => Self::Zstd,
            "lz4" => Self::Lz4,
            _ => Self::Other,
        }
    }

    /// Whether this build carries a decoder. `None` always counts; `Zstd`/`Lz4` need
    /// the `compression` feature; `Other` never.
    pub(crate) fn is_built_in(self) -> bool {
        match self {
            Self::None => true,
            #[cfg(feature = "compression")]
            Self::Zstd | Self::Lz4 => true,
            #[cfg(not(feature = "compression"))]
            Self::Zstd | Self::Lz4 => false,
            Self::Other => false,
        }
    }

    /// The name as it appears in a chunk header, for a diagnostic.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Zstd => "zstd",
            Self::Lz4 => "lz4",
            Self::Other => "an unrecognised codec",
        }
    }
}

impl core::fmt::Display for ChunkCodec {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why one chunk could not be read. `Copy` and `String`-free
/// (`docs/PROJECT.md` §5); variants carry the numbers that locate the damage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum BadChunkKind {
    /// The codec's stream did not decode.
    #[error("its {codec} stream did not decode")]
    Decompress {
        /// Which codec was in use.
        codec: ChunkCodec,
    },
    /// Decompression produced a different number of bytes than declared; otherwise a
    /// short stream would parse as a valid short record list.
    #[error("it declared {declared} uncompressed bytes and produced {produced}")]
    LengthMismatch {
        /// `ChunkHeader::uncompressed_size`.
        declared: u32,
        /// What the decoder actually wrote.
        produced: u32,
    },
    /// The CRC32 in the header disagrees with the data.
    #[error("its CRC32 is {saved:#010x} but the data hashes to {calculated:#010x}")]
    Crc {
        /// `ChunkHeader::uncompressed_crc`.
        saved: u32,
        /// What the records actually hash to.
        calculated: u32,
    },
    /// The stream had more to give than the header declared. Separate from
    /// [`BadChunkKind::LengthMismatch`] because the exact count is unknowable without
    /// decoding unboundedly. The decoders detect it differently: `decode_lz4` via a
    /// budget of `declared + 1`, `decode_zstd` via `TargetTooSmall` on an exactly-sized
    /// slice; do not unify them.
    #[error("its {codec} stream produced more than the {declared} uncompressed bytes it declared")]
    Overrun {
        /// Which codec was in use.
        codec: ChunkCodec,
        /// `ChunkHeader::uncompressed_size`.
        declared: u32,
    },
    /// The header declares more uncompressed bytes than this reader will
    /// allocate for.
    #[error("it declares {declared} uncompressed bytes, past this reader's ceiling")]
    ImplausibleSize {
        /// `ChunkHeader::uncompressed_size`.
        declared: u64,
    },
    /// `compressed_size` names more bytes than the chunk record carries. Raised before
    /// any decoder runs, so it is not a [`BadChunkKind::LengthMismatch`].
    #[error("it declares {declared} compressed bytes but the record carries {present}")]
    CompressedSizeMismatch {
        /// `ChunkHeader::compressed_size`.
        declared: u32,
        /// How many bytes of the records field the chunk record actually holds.
        present: u32,
    },
    /// An uncompressed chunk's two size fields disagree. Verbatim records make
    /// `uncompressed_size == compressed_size` an invariant, checked with no decoder,
    /// so this is not a [`BadChunkKind::LengthMismatch`]. Raised only on a complete
    /// chunk: a truncated one's `compressed_size` describes unwritten bytes.
    #[error("it is stored uncompressed but declares {uncompressed} bytes against {compressed}")]
    StoredSizeMismatch {
        /// `ChunkHeader::uncompressed_size`.
        uncompressed: u32,
        /// `ChunkHeader::compressed_size`.
        compressed: u32,
    },
    /// A zstd frame asks for a window larger than this reader allocates for a chunk of
    /// this size. Distinct from [`BadChunkKind::ImplausibleSize`]: the window is the
    /// codec frame's field, bounded by `window_ceiling`, not by [`ChunkLimits`].
    #[error(
        "its zstd frame asks for a {requested}-byte window, past this chunk's {ceiling}-byte ceiling"
    )]
    ImplausibleWindow {
        /// The window size the frame header declared.
        requested: u64,
        /// What `window_ceiling` allowed for this chunk.
        ceiling: u64,
    },
    /// A record inside the chunk runs past the chunk's end, or a fragment too
    /// short to be a record header trails it.
    #[error("a record inside it is malformed, at offset {at}")]
    InnerFraming {
        /// Offset within the chunk's decompressed records field.
        at: u32,
    },
}

/// What went wrong with a chunk, before it is joined to a chunk ordinal.
/// [`ChunkFault::Unsupported`] is never skippable (every chunk shares the codec);
/// [`ChunkFault::Bad`] is skippable by default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChunkFault {
    /// The codec is one this build has no decoder for.
    Unsupported(ChunkCodec),
    /// The chunk is damaged.
    Bad(BadChunkKind),
    /// The caller's own callback failed. Carried through unchanged: it is not a fact
    /// about the chunk, and a skip policy must not swallow it.
    Callback(IngestError),
}

/// What this reader will decompress a single chunk into before it believes a
/// header. On [`crate::IngestOptions`], not constants: whoever meets a limit cannot
/// patch the crate.
///
/// These bound the output buffer, not peak memory: `ruzstd`'s working set tracks
/// the frame's window (measured 1.98 MiB peak for a 1 MiB chunk, 6.48 MiB for
/// 4 MiB), so the peak is up to the buffer plus about twice `window_ceiling`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkLimits {
    /// Absolute ceiling on a chunk's declared `uncompressed_size`, i.e. on the
    /// output buffer. See the type's own docs for what it does **not** bound.
    pub max_uncompressed_bytes: u64,
    /// Ceiling on `uncompressed_size / compressed_size`; the absolute limit alone
    /// admits 64 MiB from 200 bytes.
    pub max_expansion_ratio: u64,
}

/// Hand back a chunk's `records` field, decompressing if this build can.
/// `scratch` is the caller's buffer, reused across the file; an uncompressed chunk
/// is returned by borrow.
///
/// Every check runs before any output byte is allocated. Parsing the header here,
/// not via `mcap::parse_record`, is what makes recovery record-granular:
/// `parse_record` rejects the truncated final chunk of a killed recording.
///
/// Order:
/// 1. [`ChunkHead::parse`]; then the codec (`Unsupported` is never skippable).
///
/// Uncompressed path (borrow):
/// 2. `compressed_size` against bytes present (complete chunks only; truncated is clamped).
/// 3. `uncompressed_size == compressed_size`, and the CRC, on complete chunks only.
///
/// Compressed path:
/// 4. Truncated: no records, no fault.
/// 5. Both [`ChunkLimits`] guards, on the raw header `u64`s, before step 6.
/// 6. `compressed_size` against bytes present; empty payload is a fault.
/// 7. Size `scratch`, decode under `window_ceiling`, compare produced length, then CRC.
pub(crate) fn chunk_records<'a>(
    body: &'a [u8],
    complete: bool,
    limits: ChunkLimits,
    scratch: &'a mut Vec<u8>,
) -> Result<&'a [u8], ChunkFault> {
    let head = ChunkHead::parse(body)?;
    if !head.codec.is_built_in() {
        return Err(ChunkFault::Unsupported(head.codec));
    }
    // Records bytes that survived; on a truncated chunk `compressed_size` is clamped.
    let available = body.len() - head.records_at;
    let payload_of = |take: usize| &body[head.records_at..head.records_at + take];
    // A complete chunk declaring more than it carries is corrupt, not truncated.
    let declared_fits = || match usize::try_from(head.compressed_size) {
        Ok(n) if n <= available => Ok(n),
        // `CompressedSizeMismatch`, not `LengthMismatch`: no decoder has run.
        _ => Err(ChunkFault::Bad(BadChunkKind::CompressedSizeMismatch {
            declared: clamp_u32(head.compressed_size),
            present: clamp_u32(available as u64),
        })),
    };

    if head.codec == ChunkCodec::None {
        // The uncompressed path borrows and allocates nothing; `scratch` is unused here
        // so the caller holds one buffer for the whole file.
        let _ = scratch;
        let payload = payload_of(if complete {
            declared_fits()?
        } else {
            available
        });

        // Stored chunks: sizes must agree (`BadChunkKind::StoredSizeMismatch`), asserted
        // of every fixture chunk by
        // `fixture::tests::a_clean_hand_rolled_file_is_accepted_by_the_mcap_crate`.
        if complete && head.uncompressed_size != head.compressed_size {
            return Err(ChunkFault::Bad(BadChunkKind::StoredSizeMismatch {
                uncompressed: clamp_u32(head.uncompressed_size),
                compressed: clamp_u32(head.compressed_size),
            }));
        }
        // A truncated chunk's CRC cannot be checked: the hash covers the whole field.
        if complete {
            check_crc(payload, head.uncompressed_crc)?;
        }
        // No ceiling here: nothing is allocated, the records borrow from `body`.
        return Ok(payload);
    }

    // A truncated compressed chunk is a short recording, not a damaged one: no
    // records and no fault, so `bad_chunks` does not count it.
    if !complete {
        return Ok(&[]);
    }

    // Both bomb guards run on the header's raw `u64`s before `declared_fits` bounds
    // `compressed_size` by the slice, so their overflow behaviour stays reachable
    // (`a_ratio_check_does_not_overflow_on_a_hostile_compressed_size`).
    let declared = head.uncompressed_size;
    if declared > limits.max_uncompressed_bytes {
        return Err(ChunkFault::Bad(BadChunkKind::ImplausibleSize { declared }));
    }
    // `saturating_mul`: a hostile `compressed_size` must not panic or wrap. A zero
    // `compressed_size` refuses any positive `declared` here.
    if declared
        > head
            .compressed_size
            .saturating_mul(limits.max_expansion_ratio)
    {
        return Err(ChunkFault::Bad(BadChunkKind::ImplausibleSize { declared }));
    }
    let Ok(want) = usize::try_from(declared) else {
        // Past the address space on a 32-bit host.
        return Err(ChunkFault::Bad(BadChunkKind::ImplausibleSize { declared }));
    };
    // A `want` no `Vec` can hold is refused here: `reserve_exact` panics past
    // `isize::MAX`, and `decode_lz4`'s `want + 1` could wrap. Unreachable at default
    // ceilings, but both guards above are caller-widenable.
    if want > isize::MAX as usize {
        return Err(ChunkFault::Bad(BadChunkKind::ImplausibleSize { declared }));
    }
    let payload = payload_of(declared_fits()?);

    // No codec frame is zero bytes (zstd >= 13, LZ4 >= 11).
    if payload.is_empty() {
        return Err(ChunkFault::Bad(BadChunkKind::Decompress {
            codec: head.codec,
        }));
    }

    decompress_into(head.codec, payload, want, scratch)?;
    let records = &scratch[..];
    // The saved hash covers the uncompressed bytes (MCAP specification); it is the
    // check that always runs, since neither codec's checksum is verified.
    check_crc(records, head.uncompressed_crc)?;
    Ok(records)
}

/// Decompress `payload` into `scratch`, leaving exactly `want` bytes.
///
/// `scratch` is the caller's whole-file buffer and is never shrunk; it is grown with
/// `reserve_exact`, since `want` is known and doubling would leave the overshoot
/// resident (`the_output_buffer_is_not_doubled_past_the_chunk`). The peak is one
/// chunk's ceiling, checked before this is called. Decoders are constructed per
/// chunk: see `decode_zstd`.
// Both allows apply only to the codec-free build, where the parameters go unused
// and narrowing `&mut Vec` would break the other configuration.
#[cfg_attr(not(feature = "compression"), allow(unused_variables, clippy::ptr_arg))]
fn decompress_into(
    codec: ChunkCodec,
    payload: &[u8],
    want: usize,
    scratch: &mut Vec<u8>,
) -> Result<(), ChunkFault> {
    match codec {
        #[cfg(feature = "compression")]
        ChunkCodec::Zstd => decode_zstd(payload, want, scratch),
        #[cfg(feature = "compression")]
        ChunkCodec::Lz4 => decode_lz4(payload, want, scratch),
        // Unreachable: `is_built_in` gated every codec. A fault, not a panic (denied).
        other => Err(ChunkFault::Unsupported(other)),
    }
}

/// The smallest window ceiling ever imposed, 8 MiB: encoders declare the window
/// they might use (`zstd -19` declares 8 MiB, `-3` 2 MiB, `ruzstd` 128 KiB), and
/// `testdata/zstd_conformance.mcap` declares 8 MiB for ~660 bytes.
#[cfg(feature = "compression")]
const MIN_ZSTD_WINDOW_BYTES: u64 = 8 * 1024 * 1024;

/// The largest zstd decoding window allocated for a chunk declaring `want`
/// uncompressed bytes.
///
/// `ruzstd` allocates from the frame's declared window, eagerly on the second and
/// later frames of a payload, and neither [`ChunkLimits`] knob sees it: a 26-byte
/// two-frame payload drove a 134 226 570-byte peak. A window larger than the
/// frame's output is unusable (no dictionary), so `want` is the exact bound; it is
/// taken from the chunk rather than [`ChunkLimits`] so the defaults are covered.
/// `ruzstd` checks it before allocating.
///
/// This refuses frames that would decode: `zstd --ultra -20`/`-21` and `--long=27`
/// declare more than `max(want, 8 MiB)`. No MCAP writer uses them; the refusal names
/// both numbers ([`BadChunkKind::ImplausibleWindow`]), and raising
/// [`MIN_ZSTD_WINDOW_BYTES`] is the remedy.
#[cfg(feature = "compression")]
fn window_ceiling(want: usize) -> u64 {
    (want as u64).max(MIN_ZSTD_WINDOW_BYTES)
}

/// zstd, via `ruzstd`'s one-shot decode into an exactly-`want` slice, which detects
/// both a short and an over-long frame with no probe read (`decode_all_to_vec`
/// would decode into spare capacity and widen the over-run tolerance).
///
/// It accepts concatenated frames and skips skippable frames; the declared length
/// and `window_ceiling` constrain it. No content checksum is verified: the chunk
/// CRC32 in `chunk_records` is the check.
#[cfg(feature = "compression")]
fn decode_zstd(payload: &[u8], want: usize, scratch: &mut Vec<u8>) -> Result<(), ChunkFault> {
    use ruzstd::decoding::errors::FrameDecoderError;
    use ruzstd::decoding::FrameDecoder;

    // No `clear()` before `resize`: it would memset the whole buffer on every chunk,
    // and the decoder overwrites it. Stale bytes past `written` are unobservable
    // (that arm returns `LengthMismatch`). The lz4 arm does `clear()` because
    // `read_to_end` appends; do not unify them. `reserve_exact` rather than amortised
    // growth: the size is `want`, and doubling would stay resident.
    scratch.reserve_exact(want.saturating_sub(scratch.len()));
    scratch.resize(want, 0);
    // Constructed per chunk on purpose; do not hoist. Reuse goes through
    // `FrameDecoderState::reset`, which reserves the frame's window eagerly, and the
    // largest window would stay resident for the rest of the ingest.
    let mut decoder = FrameDecoder::new();
    // Bound the decoder's working allocation: see `window_ceiling`.
    let ceiling = window_ceiling(want);
    decoder.set_max_window_size(ceiling);
    match decoder.decode_all(payload, &mut scratch[..]) {
        Ok(written) if written == want => Ok(()),
        // A short frame: see `BadChunkKind::LengthMismatch`. Bytes past `written` are stale, so the comparison is what this arm relies on.
        Ok(written) => Err(ChunkFault::Bad(BadChunkKind::LengthMismatch {
            declared: clamp_u32(want as u64),
            produced: clamp_u32(written as u64),
        })),
        // The decoder filled the slice with bytes left: see `BadChunkKind::Overrun`.
        Err(FrameDecoderError::TargetTooSmall) => Err(ChunkFault::Bad(BadChunkKind::Overrun {
            codec: ChunkCodec::Zstd,
            declared: clamp_u32(want as u64),
        })),
        // Named, not `Decompress`: the stream is fine; the reader declined the window.
        Err(FrameDecoderError::WindowSizeTooBig { requested, .. }) => {
            Err(ChunkFault::Bad(BadChunkKind::ImplausibleWindow {
                requested,
                ceiling,
            }))
        }
        Err(_) => Err(ChunkFault::Bad(BadChunkKind::Decompress {
            codec: ChunkCodec::Zstd,
        })),
    }
}

/// lz4, via `lz4_flex`'s **frame** decoder (MCAP's `"lz4"` is the frame format,
/// `0x184D2204`; the block API would decode the wrong container).
///
/// The `+ 1` is load-bearing: `lz4_flex` checks content length and its xxh32
/// checksum only on reaching the `EndMark`, which `take(want)` never does. It is
/// also the only output bound on this path (`lz4_flex` has no cumulative limit).
#[cfg(feature = "compression")]
fn decode_lz4(payload: &[u8], want: usize, scratch: &mut Vec<u8>) -> Result<(), ChunkFault> {
    use std::io::Read;

    scratch.clear();
    // Sized up front so `read_to_end` does not double past the chunk (it stays
    // resident). `want + 1` matches the budget below: the extra byte lets an over-run
    // be detected rather than truncated.
    scratch.reserve_exact(want + 1);
    let decoder = lz4_flex::frame::FrameDecoder::new(std::io::Cursor::new(payload));
    let budget = (want as u64).saturating_add(1);
    if let Err(_e) = decoder.take(budget).read_to_end(scratch) {
        return Err(ChunkFault::Bad(BadChunkKind::Decompress {
            codec: ChunkCodec::Lz4,
        }));
    }
    match scratch.len() {
        n if n == want => Ok(()),
        n if n < want => Err(ChunkFault::Bad(BadChunkKind::LengthMismatch {
            declared: clamp_u32(want as u64),
            produced: clamp_u32(n as u64),
        })),
        // The budget was exhausted: the frame had more to give.
        _ => Err(ChunkFault::Bad(BadChunkKind::Overrun {
            codec: ChunkCodec::Lz4,
            declared: clamp_u32(want as u64),
        })),
    }
}

/// The fixed part of a chunk record's header, parsed by hand so a truncated chunk is readable.
struct ChunkHead {
    /// `ChunkHeader::uncompressed_size`, at offset 16: the allocation size and what
    /// [`ChunkLimits`] bounds; on the stored path it must equal `compressed_size`.
    uncompressed_size: u64,
    uncompressed_crc: u32,
    codec: ChunkCodec,
    compressed_size: u64,
    /// Offset within the chunk record's body at which the records field starts.
    records_at: usize,
}

impl ChunkHead {
    /// `message_start_time: u64`, `message_end_time: u64`, `uncompressed_size:
    /// u64`, `uncompressed_crc: u32`, `compression: u32-prefixed string`,
    /// `compressed_size: u64` — all little-endian, per the MCAP specification.
    fn parse(body: &[u8]) -> Result<Self, ChunkFault> {
        /// Up to the `compression` length prefix: two times, size, crc, prefix.
        const FIXED: usize = 8 + 8 + 8 + 4 + 4;
        let framing = |at: usize| {
            ChunkFault::Bad(BadChunkKind::InnerFraming {
                at: clamp_u32(at as u64),
            })
        };
        if body.len() < FIXED {
            return Err(framing(body.len()));
        }
        let u64_at = |at: usize| -> u64 {
            let mut b = [0u8; 8];
            b.copy_from_slice(&body[at..at + 8]);
            u64::from_le_bytes(b)
        };
        let u32_at = |at: usize| -> u32 {
            let mut b = [0u8; 4];
            b.copy_from_slice(&body[at..at + 4]);
            u32::from_le_bytes(b)
        };
        let uncompressed_size = u64_at(16);
        let uncompressed_crc = u32_at(24);
        let name_len = u32_at(28) as usize;
        let name_at = FIXED;
        let after_name = name_at
            .checked_add(name_len)
            .ok_or_else(|| framing(name_at))?;
        // The name, then `compressed_size`.
        let records_at = after_name
            .checked_add(8)
            .ok_or_else(|| framing(after_name))?;
        if records_at > body.len() {
            return Err(framing(body.len()));
        }
        // A non-UTF-8 codec name is not one this build knows.
        let codec = match core::str::from_utf8(&body[name_at..after_name]) {
            Ok(s) => ChunkCodec::parse(s),
            Err(_) => ChunkCodec::Other,
        };
        let compressed_size = u64_at(after_name);
        Ok(Self {
            uncompressed_size,
            uncompressed_crc,
            codec,
            compressed_size,
            records_at,
        })
    }
}

/// The message times a chunk header declares, for reporting what a skip lost;
/// `None` if the header cannot be parsed.
pub(crate) fn chunk_span(body: &[u8]) -> Option<(u64, u64)> {
    if body.len() < 16 {
        return None;
    }
    let at = |off: usize| -> u64 {
        let mut b = [0u8; 8];
        b.copy_from_slice(&body[off..off + 8]);
        u64::from_le_bytes(b)
    };
    let (start, end) = (at(0), at(8));
    // Both zero means untracked; that is not a span.
    if start == 0 && end == 0 {
        None
    } else {
        Some((start.min(end), start.max(end)))
    }
}

/// Saturate a `u64` into the `u32` an error variant carries.
fn clamp_u32(v: u64) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

/// Verify a chunk's records against the header CRC32. A saved `0` means "not
/// computed" (MCAP specification) and is skipped.
fn check_crc(records: &[u8], saved: u32) -> Result<(), ChunkFault> {
    if saved == 0 {
        return Ok(());
    }
    let calculated = crc32fast::hash(records);
    if calculated != saved {
        return Err(ChunkFault::Bad(BadChunkKind::Crc { saved, calculated }));
    }
    Ok(())
}

/// Walk the records inside a chunk's decompressed `records` field: MCAP framing
/// minus the magic (`opcode: u8`, `len: u64` LE, body). It borrows the buffer;
/// a second `LinearReader` would copy the chunk again.
///
/// With `tolerate_tail`, a trailing fragment is a normal end (a truncated chunk);
/// without it, a fragment or overlong body is `InnerFraming` at its offset.
pub(crate) fn for_each_record<F>(
    records: &[u8],
    tolerate_tail: bool,
    mut g: F,
) -> Result<(), ChunkFault>
where
    F: FnMut(u8, &[u8]) -> Result<(), IngestError>,
{
    /// `opcode: u8` + `len: u64`.
    const HEADER: usize = 1 + 8;

    let framing = |at: usize| {
        ChunkFault::Bad(BadChunkKind::InnerFraming {
            at: clamp_u32(at as u64),
        })
    };
    let mut at = 0usize;
    while at < records.len() {
        let remaining = records.len() - at;
        if remaining < HEADER {
            return if tolerate_tail {
                Ok(())
            } else {
                Err(framing(at))
            };
        }
        let opcode = records[at];
        // `unwrap` is denied; the slice is exactly eight bytes.
        let len_bytes: [u8; 8] = match records[at + 1..at + HEADER].try_into() {
            Ok(b) => b,
            Err(_) => return Err(framing(at)),
        };
        let len = u64::from_le_bytes(len_bytes);
        // `usize::try_from` matters on 32-bit hosts: `len` came off disk as `u64`.
        let len = match usize::try_from(len) {
            Ok(n) => n,
            Err(_) => return Err(framing(at)),
        };
        // One comparison suffices: `remaining >= HEADER` was checked, and the sum is
        // bounded by `records.len()`. An overlong last record is the other face of
        // truncation.
        if len > remaining - HEADER {
            return if tolerate_tail {
                Ok(())
            } else {
                Err(framing(at))
            };
        }
        let end = at + HEADER + len;
        g(opcode, &records[at + HEADER..end]).map_err(ChunkFault::Callback)?;
        at = end;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// The limits a real ingest runs with, read from [`crate::IngestOptions`].
    fn limits() -> ChunkLimits {
        crate::IngestOptions::default().chunk_limits()
    }

    /// Assemble a chunk record body from its header fields and a payload, by hand:
    /// the tests need headers a writer would refuse to produce.
    fn chunk_body(
        codec: &str,
        uncompressed_size: u64,
        crc: u32,
        compressed_size: u64,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&1_000u64.to_le_bytes()); // message_start_time
        b.extend_from_slice(&2_000u64.to_le_bytes()); // message_end_time
        b.extend_from_slice(&uncompressed_size.to_le_bytes());
        b.extend_from_slice(&crc.to_le_bytes());
        b.extend_from_slice(&(codec.len() as u32).to_le_bytes());
        b.extend_from_slice(codec.as_bytes());
        b.extend_from_slice(&compressed_size.to_le_bytes());
        b.extend_from_slice(payload);
        b
    }

    /// One MCAP record, as it appears inside a chunk's records field.
    fn inner_record(opcode: u8, body: &[u8]) -> Vec<u8> {
        let mut b = vec![opcode];
        b.extend_from_slice(&(body.len() as u64).to_le_bytes());
        b.extend_from_slice(body);
        b
    }

    /// The three names the specification fixes, and nothing else.
    #[test]
    fn codec_names_are_exact_and_case_sensitive() {
        assert_eq!(ChunkCodec::parse(""), ChunkCodec::None);
        assert_eq!(ChunkCodec::parse("zstd"), ChunkCodec::Zstd);
        assert_eq!(ChunkCodec::parse("lz4"), ChunkCodec::Lz4);
        assert_eq!(ChunkCodec::parse("ZSTD"), ChunkCodec::Other);
        assert_eq!(ChunkCodec::parse("Zstd"), ChunkCodec::Other);
        assert_eq!(ChunkCodec::parse("lz4hc"), ChunkCodec::Other);
        assert_eq!(ChunkCodec::parse("gzip"), ChunkCodec::Other);
    }

    /// An empty `records` field yields nothing and is not an error.
    #[test]
    fn an_empty_records_field_is_not_an_error() {
        let mut seen = 0;
        for_each_record(&[], false, |_, _| {
            seen += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, 0);
    }

    /// Two records back to back are walked in order with their bodies intact.
    #[test]
    fn records_are_walked_in_order() {
        let mut buf = Vec::new();
        for (opcode, body) in [(0x05u8, &b"ab"[..]), (0x06, &b"cde"[..])] {
            buf.push(opcode);
            buf.extend_from_slice(&(body.len() as u64).to_le_bytes());
            buf.extend_from_slice(body);
        }
        let mut got: Vec<(u8, Vec<u8>)> = Vec::new();
        for_each_record(&buf, false, |op, body| {
            got.push((op, body.to_vec()));
            Ok(())
        })
        .unwrap();
        assert_eq!(got, vec![(0x05, b"ab".to_vec()), (0x06, b"cde".to_vec())]);
    }

    /// A body whose declared length runs past the chunk end is refused, not sliced.
    #[test]
    fn a_body_running_past_the_end_is_refused() {
        let mut buf = vec![0x05u8];
        buf.extend_from_slice(&64u64.to_le_bytes());
        buf.extend_from_slice(b"only four");
        let err = for_each_record(&buf, false, |_, _| Ok(())).unwrap_err();
        assert!(matches!(
            err,
            ChunkFault::Bad(BadChunkKind::InnerFraming { .. })
        ));
    }

    /// A trailing fragment too short to be a header is refused.
    #[test]
    fn a_short_trailing_fragment_is_refused() {
        let mut buf = vec![0x05u8];
        buf.extend_from_slice(&2u64.to_le_bytes());
        buf.extend_from_slice(b"ab");
        buf.extend_from_slice(b"tail");
        let err = for_each_record(&buf, false, |_, _| Ok(())).unwrap_err();
        assert!(matches!(
            err,
            ChunkFault::Bad(BadChunkKind::InnerFraming { .. })
        ));
    }

    /// A record of length zero is legal and advances the cursor.
    #[test]
    fn a_zero_length_record_advances() {
        let mut buf = vec![0x0fu8];
        buf.extend_from_slice(&0u64.to_le_bytes());
        buf.push(0x10);
        buf.extend_from_slice(&1u64.to_le_bytes());
        buf.push(b'z');
        let mut got: Vec<(u8, usize)> = Vec::new();
        for_each_record(&buf, false, |op, body| {
            got.push((op, body.len()));
            Ok(())
        })
        .unwrap();
        assert_eq!(got, vec![(0x0f, 0), (0x10, 1)]);
    }

    /// A CRC that disagrees with the data is caught, and a saved `0` is skipped.
    #[test]
    fn a_wrong_crc_is_caught_and_a_zero_crc_is_skipped() {
        let data = b"the records field of some chunk";
        let right = crc32fast::hash(data);
        assert!(check_crc(data, right).is_ok());
        assert!(
            check_crc(data, 0).is_ok(),
            "0 means not computed, per the spec"
        );
        let err = check_crc(data, right ^ 0xFFFF_FFFF).unwrap_err();
        match err {
            ChunkFault::Bad(BadChunkKind::Crc { saved, calculated }) => {
                assert_eq!(calculated, right);
                assert_ne!(saved, right);
            }
            other => panic!("expected a Crc fault, got {other:?}"),
        }
    }

    /// A chunk header shorter than its fixed fields is a framing fault, not a panic.
    #[test]
    fn a_chunk_header_too_short_to_parse_is_a_framing_fault() {
        for len in [0usize, 1, 15, 27, 31] {
            let body = vec![0u8; len];
            let mut scratch = Vec::new();
            let err = chunk_records(&body, false, limits(), &mut scratch).unwrap_err();
            assert!(
                matches!(err, ChunkFault::Bad(BadChunkKind::InnerFraming { .. })),
                "len {len} gave {err:?}"
            );
        }
    }

    /// A truncated chunk's CRC is not checked: the saved hash covers the whole
    /// records field and we hold a prefix.
    #[test]
    fn a_truncated_chunk_does_not_have_its_crc_checked() {
        // A chunk whose header claims a CRC that the prefix cannot match.
        let mut body = Vec::new();
        body.extend_from_slice(&1_000u64.to_le_bytes()); // message_start_time
        body.extend_from_slice(&2_000u64.to_le_bytes()); // message_end_time
        body.extend_from_slice(&64u64.to_le_bytes()); // uncompressed_size
        body.extend_from_slice(&0xDEAD_BEEFu32.to_le_bytes()); // uncompressed_crc
        body.extend_from_slice(&0u32.to_le_bytes()); // compression name length
        body.extend_from_slice(&64u64.to_le_bytes()); // compressed_size
        body.extend_from_slice(b"only a few bytes of the records field");

        // Two times, uncompressed_size, crc, the name's length prefix, an empty
        // name, then compressed_size: 8+8+8+4+4+0+8.
        const HEADER_BYTES: usize = 40;
        let mut scratch = Vec::new();
        let partial =
            chunk_records(&body, false, limits(), &mut scratch).expect("a prefix must be readable");
        assert_eq!(
            partial.len(),
            body.len() - HEADER_BYTES,
            "the whole surviving records prefix must be handed over"
        );

        // The same bytes declared complete fail on `compressed_size` against the bytes present.
        let mut scratch2 = Vec::new();
        let err = chunk_records(&body, true, limits(), &mut scratch2).unwrap_err();
        match err {
            ChunkFault::Bad(BadChunkKind::CompressedSizeMismatch { declared, present }) => {
                assert_eq!(declared, 64);
                assert_eq!(u64::from(present), (body.len() - HEADER_BYTES) as u64);
            }
            other => panic!("got {other:?}"),
        }
    }

    /// A skipped chunk's span comes from its header; an unset one is absent, not 1970.
    #[test]
    fn a_chunk_span_is_absent_rather_than_epoch() {
        let mut body = vec![0u8; 16];
        assert_eq!(chunk_span(&body), None);
        body[0..8].copy_from_slice(&7u64.to_le_bytes());
        body[8..16].copy_from_slice(&3u64.to_le_bytes());
        assert_eq!(chunk_span(&body), Some((3, 7)), "the span is ordered");
        assert_eq!(
            chunk_span(&body[..4]),
            None,
            "too short to hold either time"
        );
    }

    /// An error from the callback stops the walk immediately.
    #[test]
    fn a_callback_error_stops_the_walk() {
        let mut buf = Vec::new();
        for _ in 0..2 {
            buf.push(0x05u8);
            buf.extend_from_slice(&1u64.to_le_bytes());
            buf.push(b'x');
        }
        let mut seen = 0;
        let err = for_each_record(&buf, false, |_, _| {
            seen += 1;
            Err(IngestError::NoTransforms)
        })
        .unwrap_err();
        assert_eq!(seen, 1);
        assert_eq!(err, ChunkFault::Callback(IngestError::NoTransforms));
    }

    /// An uncompressed chunk whose two size fields disagree is refused on a complete
    /// chunk, with no decoder involved, and reported as `StoredSizeMismatch` (not a
    /// decoder's `LengthMismatch`). A truncated chunk's disagreement is not damage.
    #[test]
    fn an_uncompressed_chunk_with_disagreeing_sizes_is_refused() {
        let records = inner_record(0x05, b"a message body");
        let crc = crc32fast::hash(&records);
        let len = records.len() as u64;
        let body = chunk_body("", len + 64, crc, len, &records);

        let mut scratch = Vec::new();
        let err = chunk_records(&body, true, limits(), &mut scratch).unwrap_err();
        match err {
            ChunkFault::Bad(kind) => {
                let BadChunkKind::StoredSizeMismatch {
                    uncompressed,
                    compressed,
                } = kind
                else {
                    panic!("expected a StoredSizeMismatch, got {kind:?}")
                };
                assert_eq!(u64::from(uncompressed), len + 64);
                assert_eq!(u64::from(compressed), len);
                // It names two header fields, not a decoder's output.
                let text = kind.to_string();
                assert!(
                    text.contains("stored uncompressed") && !text.contains("produced"),
                    "the message must not describe a decode that never happened: {text}"
                );
            }
            other => panic!("expected a StoredSizeMismatch, got {other:?}"),
        }

        // A truncated chunk is not damage (`BadChunkKind::StoredSizeMismatch`).
        let mut scratch = Vec::new();
        assert!(
            chunk_records(&body, false, limits(), &mut scratch).is_ok(),
            "a truncated chunk's size disagreement is truncation, not corruption"
        );
    }

    /// A declared `uncompressed_size` past the ceiling is refused before the buffer
    /// is sized; the assertion is on the allocation, not merely the error. The ratio
    /// guard cannot catch it (1 GiB from 1 MiB is within 1024x).
    /// Gated on `compression`: without a decoder, `is_built_in` refuses the codec first.
    #[cfg(feature = "compression")]
    #[test]
    fn a_lying_uncompressed_size_is_refused_before_it_allocates() {
        const GIB: u64 = 1024 * 1024 * 1024;
        let payload = vec![0x5Au8; 1024 * 1024];
        let body = chunk_body("zstd", GIB, 0, payload.len() as u64, &payload);

        let mut scratch = Vec::new();
        let err = chunk_records(&body, true, limits(), &mut scratch).unwrap_err();
        // The allocation is what the guard is about; the fault kind corroborates.
        assert_eq!(
            scratch.capacity(),
            0,
            "the guard must fire before the output buffer is sized"
        );
        assert_eq!(
            err,
            ChunkFault::Bad(BadChunkKind::ImplausibleSize { declared: GIB })
        );
    }

    /// A chunk claiming to expand past the ratio is refused though its declared size
    /// is under the absolute ceiling, and before the buffer is sized.
    /// Gated on `compression`: without a decoder, `is_built_in` refuses the codec first.
    #[cfg(feature = "compression")]
    #[test]
    fn a_high_expansion_ratio_is_refused() {
        const TEN_MIB: u64 = 10 * 1024 * 1024;
        let payload = vec![0x11u8; 100];
        let body = chunk_body("zstd", TEN_MIB, 0, payload.len() as u64, &payload);
        assert!(
            TEN_MIB < limits().max_uncompressed_bytes,
            "the absolute ceiling must not be what refuses this"
        );

        let mut scratch = Vec::new();
        let err = chunk_records(&body, true, limits(), &mut scratch).unwrap_err();
        assert_eq!(
            scratch.capacity(),
            0,
            "the guard must fire before the output buffer is sized"
        );
        assert_eq!(
            err,
            ChunkFault::Bad(BadChunkKind::ImplausibleSize { declared: TEN_MIB })
        );
    }

    /// The ratio guard survives a `compressed_size` chosen to overflow it: with
    /// `saturating_mul` the guard declines and the chunk is refused as
    /// `CompressedSizeMismatch`, the truthful complaint about that header.
    /// Gated on `compression`: without a decoder, `is_built_in` refuses the codec first.
    #[cfg(feature = "compression")]
    #[test]
    fn a_ratio_check_does_not_overflow_on_a_hostile_compressed_size() {
        let payload = b"far fewer bytes than the header claims";
        let hostile = u64::MAX / 512;
        assert!(
            hostile.checked_mul(limits().max_expansion_ratio).is_none(),
            "the fixture must actually overflow the product, or this proves nothing"
        );
        let body = chunk_body("zstd", 1_000, 0, hostile, payload);

        let mut scratch = Vec::new();
        let err = chunk_records(&body, true, limits(), &mut scratch).unwrap_err();
        assert!(
            matches!(
                err,
                ChunkFault::Bad(BadChunkKind::CompressedSizeMismatch { .. })
            ),
            "got {err:?}"
        );
    }

    /// A chunk that names a codec and carries no payload is a header contradicting
    /// itself, not an empty chunk.
    #[cfg(feature = "compression")]
    #[test]
    fn a_compressed_chunk_with_no_payload_is_refused() {
        for codec in ["zstd", "lz4"] {
            let body = chunk_body(codec, 0, 0, 0, &[]);
            let mut scratch = Vec::new();
            let err = chunk_records(&body, true, limits(), &mut scratch).unwrap_err();
            assert!(
                matches!(err, ChunkFault::Bad(BadChunkKind::Decompress { .. })),
                "{codec} gave {err:?}"
            );
        }
    }

    /// A truncated compressed chunk yields no records and no fault: it is
    /// truncation, not corruption, so `bad_chunks` must not count it.
    #[cfg(feature = "compression")]
    #[test]
    fn a_truncated_compressed_chunk_is_not_a_bad_chunk() {
        for codec in ["zstd", "lz4"] {
            // A plausible header whose payload was cut off after a few bytes.
            let body = chunk_body(codec, 4096, 0x1234_5678, 900, b"\x28\xb5\x2f\xfd\x04");
            let mut scratch = Vec::new();
            let records = chunk_records(&body, false, limits(), &mut scratch)
                .unwrap_or_else(|e| panic!("{codec}: a truncated chunk must not fault: {e:?}"));
            assert!(
                records.is_empty(),
                "{codec}: a partial frame decodes to nothing"
            );
        }
    }

    /// Round-trip through each codec, and the exact fault on each side of the declared
    /// length. The CRC-0 rows (`0` means "not computed") isolate the length check:
    /// with a real CRC a wrong length is also caught by the hash.
    #[cfg(feature = "compression")]
    #[test]
    fn each_codec_round_trips_and_catches_both_length_disagreements() {
        let records = [
            inner_record(0x05, b"the first message body, long enough to compress"),
            inner_record(0x05, b"the second message body, also long enough"),
        ]
        .concat();
        let crc = crc32fast::hash(&records);
        let exact = records.len() as u64;

        for (name, payload) in [
            ("zstd", encode_zstd(&records)),
            ("lz4", encode_lz4(&records)),
        ] {
            let size = payload.len() as u64;

            // Exact: the ordinary case, and the records come back byte for byte.
            let body = chunk_body(name, exact, crc, size, &payload);
            let mut scratch = Vec::new();
            let got = chunk_records(&body, true, limits(), &mut scratch)
                .unwrap_or_else(|e| panic!("{name} round trip: {e:?}"));
            assert_eq!(got, &records[..], "{name} did not round-trip");

            // **Under-run with no CRC to fall back on.** `0` means "not computed"
            // per the specification, so the length comparison is the only thing
            // between a short decode and a silently shortened recording.
            let body = chunk_body(name, exact + 64, 0, size, &payload);
            let mut scratch = Vec::new();
            let err = chunk_records(&body, true, limits(), &mut scratch).unwrap_err();
            match err {
                ChunkFault::Bad(BadChunkKind::LengthMismatch { declared, produced }) => {
                    assert_eq!(u64::from(declared), exact + 64, "{name}");
                    assert_eq!(u64::from(produced), exact, "{name}");
                }
                other => panic!("{name} under-run without a CRC gave {other:?}"),
            }

            // Under-run: the header claims 64 bytes the stream does not have.
            let body = chunk_body(name, exact + 64, crc, size, &payload);
            let mut scratch = Vec::new();
            let err = chunk_records(&body, true, limits(), &mut scratch).unwrap_err();
            match err {
                ChunkFault::Bad(BadChunkKind::LengthMismatch { declared, produced }) => {
                    assert_eq!(u64::from(declared), exact + 64, "{name}");
                    assert_eq!(u64::from(produced), exact, "{name}");
                }
                other => panic!("{name} under-run gave {other:?}"),
            }

            // Over-run, again with no CRC: the stream has more to give than the
            // header declares, and nothing but the one-byte-over budget can tell.
            for (crc_of, label) in [(0u32, "without a CRC"), (crc, "with a CRC")] {
                let body = chunk_body(name, exact - 8, crc_of, size, &payload);
                let mut scratch = Vec::new();
                let err = chunk_records(&body, true, limits(), &mut scratch).unwrap_err();
                match err {
                    ChunkFault::Bad(BadChunkKind::Overrun { declared, .. }) => {
                        assert_eq!(u64::from(declared), exact - 8, "{name} {label}");
                    }
                    other => panic!("{name} over-run {label} gave {other:?}"),
                }
            }
        }
    }

    /// A conformance vector for lz4, written by hand from the frame and block formats
    /// rather than produced by an encoder (no `lz4` CLI is available; the zstd half is
    /// `testdata/zstd_conformance.mcap`). `lz4_flex`'s own encoder does not emit these
    /// bytes, asserted below. It covers a literal-length extension, a match-length
    /// extension, an overlapping match, and the frame framing including the `EndMark`
    /// and xxh32 content checksum. Asserted through `decode_lz4` and `chunk_records`.
    #[cfg(feature = "compression")]
    #[test]
    fn a_hand_authored_lz4_frame_decodes_per_the_specification() {
        let want = lz4_vector_content();
        assert_eq!(want.len(), 72, "the frame declares 72 content bytes");

        let mut scratch = Vec::new();
        decompress_into(ChunkCodec::Lz4, LZ4_SPEC_VECTOR, want.len(), &mut scratch)
            .unwrap_or_else(|e| panic!("the hand-authored frame did not decode: {e:?}"));
        assert_eq!(scratch, want, "lz4_flex disagrees with the specification");

        // And through the real entry point, under a real CRC, so the vector covers
        // the path a recording takes rather than only the decoder.
        let chunk = chunk_body(
            "lz4",
            want.len() as u64,
            crc32fast::hash(&want),
            LZ4_SPEC_VECTOR.len() as u64,
            LZ4_SPEC_VECTOR,
        );
        let mut scratch = Vec::new();
        let got = chunk_records(&chunk, true, limits(), &mut scratch)
            .unwrap_or_else(|e| panic!("the hand-authored chunk did not read: {e:?}"));
        assert_eq!(got, &want[..]);
        let mut seen = Vec::new();
        for_each_record(got, false, |op, b| {
            seen.push((op, b.to_vec()));
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, vec![(0x05u8, want[9..].to_vec())]);

        // **The vector is independent, asserted rather than claimed.** If
        // `lz4_flex`'s encoder happened to emit exactly these bytes, this test would
        // be `encode_lz4` round-tripping under another name and the conformance claim
        // above would be false.
        assert_ne!(
            encode_lz4(&want),
            LZ4_SPEC_VECTOR,
            "the vector must not be what lz4_flex's own encoder produces"
        );
    }

    /// Of the 656 single-bit perturbations of the vector, all but five are caught;
    /// the five survivors are asserted as an exact set.
    ///
    /// * Byte 60, bits 0-3: the last sequence's match-length nibble, unused because a
    ///   block's last sequence has no match.
    /// * Byte 77, bit 7: `EndMark` read as `0x80000000`, which `lz4_flex` accepts
    ///   (`size & 0x7fff_ffff == 0`); a decoder leniency.
    ///
    /// Byte 49 (match offset) and bytes 78-81 (content checksum) are caught only
    /// because `decode_lz4`'s `+ 1` budget reaches the `EndMark` arm.
    #[cfg(feature = "compression")]
    #[test]
    fn a_single_flipped_bit_in_the_lz4_vector_is_caught() {
        /// `(byte, bit)` pairs the format or the decoder treats as don't-care. See
        /// this test's doc comment for why each is one.
        const DONT_CARE: &[(usize, u32)] = &[(60, 0), (60, 1), (60, 2), (60, 3), (77, 7)];

        let want = lz4_vector_content();
        let mut survivors = Vec::new();
        let mut checked = 0usize;
        for at in 0..LZ4_SPEC_VECTOR.len() {
            for bit in 0..8u32 {
                let mut frame = LZ4_SPEC_VECTOR.to_vec();
                frame[at] ^= 1u8 << bit;
                let mut scratch = Vec::new();
                checked += 1;
                match decompress_into(ChunkCodec::Lz4, &frame, want.len(), &mut scratch) {
                    Ok(()) if scratch == want => survivors.push((at, bit)),
                    _ => {}
                }
            }
        }
        assert_eq!(checked, 82 * 8);
        assert_eq!(
            survivors, DONT_CARE,
            "the set of bits this vector does not cover has changed: a new entry is a \
             region of the frame it only appears to exercise, and a missing one is \
             lz4_flex having become stricter"
        );
    }

    /// The 82 hand-authored bytes of `a_hand_authored_lz4_frame_decodes_per_the_specification`.
    /// Deliberately not regenerable by a tool, which would make it a round-trip.
    #[cfg(feature = "compression")]
    const LZ4_SPEC_VECTOR: &[u8] = &[
        // magic, FLG, BD, content size (72), header checksum
        0x04, 0x22, 0x4d, 0x18, 0x6c, 0x40, 0x48, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0xd0, // block size: 55, high bit clear -> a compressed block
        0x37, 0x00, 0x00, 0x00,
        // sequence 1: token 0xff (literal nibble 15, match nibble 15), literal-length
        // extension 14 -> 29 literals; then offset 20 and match-length extension 1 ->
        // a 20-byte match, replaying the phrase.
        0xff, 0x0e, 0x05, 0x3f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x6c, 0x7a, 0x34, 0x20,
        0x66, 0x72, 0x6f, 0x6d, 0x20, 0x74, 0x68, 0x65, 0x20, 0x73, 0x70, 0x65, 0x63, 0x3a, 0x20,
        0x20, 0x14, 0x00, 0x01,
        // sequence 2: token 0x42 -> 4 literals, then a 6-byte match at offset 1: an
        // overlapping run of `!`.
        0x42, 0x20, 0x6e, 0x6f, 0x21, 0x01, 0x00,
        // sequence 3: token 0xd0 -> 13 literals and no match, which is how a block's
        // last sequence is spelled.
        0xd0, 0x20, 0x6c, 0x69, 0x62, 0x6c, 0x7a, 0x34, 0x2d, 0x66, 0x72, 0x65, 0x65, 0x2e,
        // EndMark, then xxh32 of the 72 uncompressed bytes
        0x00, 0x00, 0x00, 0x00, 0x1a, 0x6c, 0xf9, 0x70,
    ];

    /// What [`LZ4_SPEC_VECTOR`] must decode to: one MCAP inner record whose body
    /// repeats a phrase (the 20-byte match) and then a run of one byte (the
    /// overlapping match).
    #[cfg(feature = "compression")]
    fn lz4_vector_content() -> Vec<u8> {
        inner_record(
            0x05,
            b"lz4 from the spec:  lz4 from the spec:   no!!!!!!! liblz4-free.",
        )
    }

    /// The output buffer is sized to the chunk on reuse, not doubled past it: the
    /// buffer is never shrunk, so overshoot stays resident. Both codecs, since each
    /// grew it by a different path.
    #[cfg(feature = "compression")]
    #[test]
    fn the_output_buffer_is_not_doubled_past_the_chunk() {
        for codec in [ChunkCodec::Zstd, ChunkCodec::Lz4] {
            // One buffer across four chunks, one of which is barely larger than the
            // last: that step is what triggers a doubling, and a test whose sizes
            // all doubled cleanly would pass either way.
            let mut scratch = Vec::new();
            for kib in [1024usize, 1025, 2048, 4096] {
                let records = inner_record(0x05, &vec![0x41u8; kib * 1024]);
                let want = records.len();
                let payload = if codec == ChunkCodec::Zstd {
                    encode_zstd(&records)
                } else {
                    encode_lz4(&records)
                };
                decompress_into(codec, &payload, want, &mut scratch)
                    .unwrap_or_else(|e| panic!("{codec} at {kib} KiB: {e:?}"));
                assert_eq!(scratch.len(), want, "{codec} at {kib} KiB");
                // One byte of slack is lz4's read budget (`want + 1`); zstd is exact.
                let slack = if codec == ChunkCodec::Lz4 { 1 } else { 0 };
                assert_eq!(
                    scratch.capacity(),
                    want + slack,
                    "{codec} at {kib} KiB overshot: a buffer this reader never shrinks                      must not be doubled past the chunk it was sized for"
                );
            }
        }
    }

    /// A chunk labelled with a codec but carrying other bytes is a skippable bad
    /// chunk, not `Unsupported` (which is never skippable).
    #[cfg(feature = "compression")]
    #[test]
    fn a_mislabelled_chunk_is_a_bad_chunk_not_an_unsupported_one() {
        let records = inner_record(0x05, b"not compressed at all");
        let crc = crc32fast::hash(&records);
        for codec in ["zstd", "lz4"] {
            let body = chunk_body(
                codec,
                records.len() as u64,
                crc,
                records.len() as u64,
                &records,
            );
            let mut scratch = Vec::new();
            let err = chunk_records(&body, true, limits(), &mut scratch).unwrap_err();
            assert!(
                matches!(err, ChunkFault::Bad(BadChunkKind::Decompress { .. })),
                "{codec} gave {err:?}"
            );
        }
    }

    /// A decompressed chunk's CRC is checked against the uncompressed bytes; neither
    /// codec's own checksum is verified for us.
    #[cfg(feature = "compression")]
    #[test]
    fn a_decompressed_chunk_has_its_crc_checked() {
        let records = inner_record(0x05, b"a body whose hash the header will get wrong");
        let payload = encode_zstd(&records);
        let wrong = crc32fast::hash(&records) ^ 0x5555_5555;
        let body = chunk_body(
            "zstd",
            records.len() as u64,
            wrong,
            payload.len() as u64,
            &payload,
        );
        let mut scratch = Vec::new();
        let err = chunk_records(&body, true, limits(), &mut scratch).unwrap_err();
        assert!(
            matches!(err, ChunkFault::Bad(BadChunkKind::Crc { .. })),
            "got {err:?}"
        );
    }

    /// With `compression` off both codecs report `Unsupported`, never a skippable
    /// `Decompress`, which would skip every chunk of an intact file.
    #[cfg(not(feature = "compression"))]
    #[test]
    fn a_codec_free_build_reports_both_codecs_unsupported() {
        for (codec, want) in [("zstd", ChunkCodec::Zstd), ("lz4", ChunkCodec::Lz4)] {
            let body = chunk_body(codec, 64, 0, 8, b"whatever");
            let mut scratch = Vec::new();
            let err = chunk_records(&body, true, limits(), &mut scratch).unwrap_err();
            assert_eq!(err, ChunkFault::Unsupported(want), "{codec}");
        }
    }

    /// A hand-rolled zstd frame: one raw block and a chosen window exponent
    /// (window = `1 << (10 + exponent)`), which no encoder will emit.
    #[cfg(feature = "compression")]
    fn zstd_frame_with_window(exponent: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![0x28, 0xb5, 0x2f, 0xfd, 0x00, exponent << 3];
        let header = 1u32 | ((body.len() as u32) << 3);
        v.extend_from_slice(&header.to_le_bytes()[..3]);
        v.extend_from_slice(body);
        v
    }

    /// A zstd frame demanding a window larger than the chunk could need is refused,
    /// including as the second frame of a payload, where `ruzstd` allocates eagerly.
    /// The 26-byte two-frame payload once peaked at 134 226 570 bytes; the fault
    /// firing is the allocation not happening (no counting allocator under
    /// `forbid(unsafe_code)`).
    #[cfg(feature = "compression")]
    #[test]
    fn a_zstd_frame_demanding_an_oversized_window_is_refused() {
        /// `1 << (10 + 16)`, i.e. 64 MiB — what `zstd --ultra -21` declares, and
        /// eight times what any default-reachable encoder does.
        const HOSTILE_EXPONENT: u8 = 16;
        const HOSTILE_WINDOW: u64 = 1 << (10 + HOSTILE_EXPONENT as u64);

        // Two concatenated frames: the second is the one that reaches the eager
        // `reset` path, and `decode_all` accepts a multi-frame payload silently.
        let mut two = zstd_frame_with_window(3, b"bbbb");
        two.extend_from_slice(&zstd_frame_with_window(HOSTILE_EXPONENT, b"aaaa"));
        assert_eq!(two.len(), 26, "the amplification is the point of this row");

        for (label, payload, want) in [
            ("two frames", two, 8u64),
            (
                "one frame",
                zstd_frame_with_window(HOSTILE_EXPONENT, b"aaaa"),
                4,
            ),
        ] {
            // `uncompressed_crc == 0` is "not computed" per the specification, so the
            // window bound is the only thing that can refuse these bytes: they decode,
            // and they decode to exactly what the header declares.
            let body = chunk_body("zstd", want, 0, payload.len() as u64, &payload);
            let mut scratch = Vec::new();
            let got = chunk_records(&body, true, limits(), &mut scratch)
                .map(|r| String::from_utf8_lossy(r).into_owned());
            assert_eq!(
                got,
                Err(ChunkFault::Bad(BadChunkKind::ImplausibleWindow {
                    requested: HOSTILE_WINDOW,
                    ceiling: MIN_ZSTD_WINDOW_BYTES,
                })),
                "{label}"
            );
        }
    }

    /// The window floor admits the largest window a real encoder declares (8 MiB:
    /// `zstd -19` and every chunk of `testdata/zstd_conformance.mcap`), and nothing above.
    #[cfg(feature = "compression")]
    #[test]
    fn the_window_floor_admits_what_a_real_zstd_encoder_declares() {
        // `1 << (10 + 13)` is 8 MiB, exactly `MIN_ZSTD_WINDOW_BYTES`.
        assert_eq!(MIN_ZSTD_WINDOW_BYTES, 1 << (10 + 13));
        let payload = zstd_frame_with_window(13, b"aaaa");
        let body = chunk_body("zstd", 4, 0, payload.len() as u64, &payload);
        let mut scratch = Vec::new();
        let got = chunk_records(&body, true, limits(), &mut scratch)
            .unwrap_or_else(|e| panic!("an 8 MiB window must be accepted: {e:?}"));
        assert_eq!(got, b"aaaa");

        // One exponent higher is 16 MiB, and is refused — so the floor is a boundary
        // rather than a number that merely happens to be large enough.
        let payload = zstd_frame_with_window(14, b"aaaa");
        let body = chunk_body("zstd", 4, 0, payload.len() as u64, &payload);
        let mut scratch = Vec::new();
        assert_eq!(
            chunk_records(&body, true, limits(), &mut scratch),
            Err(ChunkFault::Bad(BadChunkKind::ImplausibleWindow {
                requested: 16 * 1024 * 1024,
                ceiling: MIN_ZSTD_WINDOW_BYTES,
            }))
        );
    }

    /// Compress with `ruzstd`'s encoder. Round-trip is not conformance:
    /// see `testdata/zstd_conformance.mcap`.
    #[cfg(feature = "compression")]
    fn encode_zstd(bytes: &[u8]) -> Vec<u8> {
        ruzstd::encoding::compress_to_vec(bytes, ruzstd::encoding::CompressionLevel::Fastest)
    }

    /// Compress with `lz4_flex`'s frame encoder. Not conformance either:
    /// see [`LZ4_SPEC_VECTOR`].
    #[cfg(feature = "compression")]
    fn encode_lz4(bytes: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut enc = lz4_flex::frame::FrameEncoder::new(Vec::new());
        enc.write_all(bytes).unwrap();
        enc.finish().unwrap()
    }
}
