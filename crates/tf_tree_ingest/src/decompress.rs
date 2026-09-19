//! Chunk handling: this crate owns MCAP's record framing and reads inside chunks.
//!
//! `mcap` is taken `default-features = false` (`docs/PHASE2.md` §2), so chunks are
//! handed over whole (`with_emit_chunks(true)`) and decoded here with pure-Rust
//! `ruzstd` and `lz4_flex` under `compression`; without it a zstd/lz4 chunk is
//! [`ChunkFault::Unsupported`] (`tests/codec_free.rs`). A truncated compressed
//! chunk is reported as truncation, not corruption: see [`chunk_records`].
//!
//! The framing (`opcode: u8`, `len: u64` LE, `body[len]`) is the same inside a
//! chunk. Two walks of it exist, [`for_each_record`] over `&[u8]` and
//! `source::read_tf` over a `BufReader` (which bounds each length before
//! allocating); keep them consistent by hand.
//!
//! # Decompression is bounded three times
//!
//! [`ChunkLimits`] bounds `uncompressed_size` absolutely and as a ratio before
//! anything is allocated, and the caller-sized buffer is then the guarantee.
//! `window_ceiling` covers the zstd decoder's own allocation
//! ([`BadChunkKind::ImplausibleWindow`]).

use crate::IngestError;

/// Which codec a chunk's `compression` field names. `Copy` and `String`-free
/// (`docs/PROJECT.md` §5).
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
    /// Classify the `compression` field; case-sensitive.
    pub(crate) fn parse(name: &str) -> Self {
        match name {
            "" => Self::None,
            "zstd" => Self::Zstd,
            "lz4" => Self::Lz4,
            _ => Self::Other,
        }
    }

    /// Whether this build carries a decoder (`Zstd`/`Lz4` need `compression`).
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

/// Why one chunk could not be read. `Copy` and `String`-free (`docs/PROJECT.md` §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum BadChunkKind {
    /// The codec's stream did not decode.
    #[error("its {codec} stream did not decode")]
    Decompress {
        /// Which codec was in use.
        codec: ChunkCodec,
    },
    /// Decompression produced a different number of bytes than declared.
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
    /// The stream had more to give than the header declared. `decode_lz4` detects
    /// it via a budget of `declared + 1`, `decode_zstd` via `TargetTooSmall`; do
    /// not unify them.
    #[error("its {codec} stream produced more than the {declared} uncompressed bytes it declared")]
    Overrun {
        /// Which codec was in use.
        codec: ChunkCodec,
        /// `ChunkHeader::uncompressed_size`.
        declared: u32,
    },
    /// The header declares more uncompressed bytes than this reader will allocate.
    #[error("it declares {declared} uncompressed bytes, past this reader's ceiling")]
    ImplausibleSize {
        /// `ChunkHeader::uncompressed_size`.
        declared: u64,
    },
    /// `compressed_size` names more bytes than the chunk record carries (raised before any decoder).
    #[error("it declares {declared} compressed bytes but the record carries {present}")]
    CompressedSizeMismatch {
        /// `ChunkHeader::compressed_size`.
        declared: u32,
        /// How many bytes of the records field the chunk record actually holds.
        present: u32,
    },
    /// An uncompressed chunk's two size fields disagree; raised only on a
    /// complete chunk.
    #[error("it is stored uncompressed but declares {uncompressed} bytes against {compressed}")]
    StoredSizeMismatch {
        /// `ChunkHeader::uncompressed_size`.
        uncompressed: u32,
        /// `ChunkHeader::compressed_size`.
        compressed: u32,
    },
    /// A zstd frame asks for a window past `window_ceiling` for this chunk.
    #[error(
        "its zstd frame asks for a {requested}-byte window, past this chunk's {ceiling}-byte ceiling"
    )]
    ImplausibleWindow {
        /// The window size the frame header declared.
        requested: u64,
        /// What `window_ceiling` allowed for this chunk.
        ceiling: u64,
    },
    /// A record inside the chunk runs past its end, or a short fragment trails it.
    #[error("a record inside it is malformed, at offset {at}")]
    InnerFraming {
        /// Offset within the chunk's decompressed records field.
        at: u32,
    },
}

/// What went wrong with a chunk, before it is joined to a chunk ordinal.
/// [`ChunkFault::Unsupported`] is never skippable; [`ChunkFault::Bad`] is by default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChunkFault {
    /// The codec is one this build has no decoder for.
    Unsupported(ChunkCodec),
    /// The chunk is damaged.
    Bad(BadChunkKind),
    /// The caller's own callback failed; a skip policy must not swallow it.
    Callback(IngestError),
}

/// What this reader will decompress a single chunk into before it believes a
/// header; on [`crate::IngestOptions`], not constants.
///
/// These bound the output buffer, not peak memory: `ruzstd`'s working set tracks
/// the frame's window, up to about twice `window_ceiling` beyond the buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkLimits {
    /// Absolute ceiling on a chunk's declared `uncompressed_size`.
    pub max_uncompressed_bytes: u64,
    /// Ceiling on `uncompressed_size / compressed_size`.
    pub max_expansion_ratio: u64,
}

/// Hand back a chunk's `records` field, decompressing if this build can;
/// an uncompressed chunk is returned by borrow, `scratch` is reused across the file.
///
/// Every check runs before any output byte is allocated. The header is parsed
/// here, not via `mcap::parse_record`, so a truncated final chunk stays readable.
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
    let available = body.len() - head.records_at;
    let payload_of = |take: usize| &body[head.records_at..head.records_at + take];
    let declared_fits = || match usize::try_from(head.compressed_size) {
        Ok(n) if n <= available => Ok(n),
        _ => Err(ChunkFault::Bad(BadChunkKind::CompressedSizeMismatch {
            declared: clamp_u32(head.compressed_size),
            present: clamp_u32(available as u64),
        })),
    };

    if head.codec == ChunkCodec::None {
        let _ = scratch;
        let payload = payload_of(if complete {
            declared_fits()?
        } else {
            available
        });

        if complete && head.uncompressed_size != head.compressed_size {
            return Err(ChunkFault::Bad(BadChunkKind::StoredSizeMismatch {
                uncompressed: clamp_u32(head.uncompressed_size),
                compressed: clamp_u32(head.compressed_size),
            }));
        }
        if complete {
            check_crc(payload, head.uncompressed_crc)?;
        }
        return Ok(payload);
    }

    if !complete {
        return Ok(&[]);
    }

    // Both guards run on the header's raw `u64`s before `declared_fits`
    // (`a_ratio_check_does_not_overflow_on_a_hostile_compressed_size`).
    let declared = head.uncompressed_size;
    if declared > limits.max_uncompressed_bytes {
        return Err(ChunkFault::Bad(BadChunkKind::ImplausibleSize { declared }));
    }
    if declared
        > head
            .compressed_size
            .saturating_mul(limits.max_expansion_ratio)
    {
        return Err(ChunkFault::Bad(BadChunkKind::ImplausibleSize { declared }));
    }
    let Ok(want) = usize::try_from(declared) else {
        return Err(ChunkFault::Bad(BadChunkKind::ImplausibleSize { declared }));
    };
    // `reserve_exact` panics past `isize::MAX` and `decode_lz4`'s `want + 1` could wrap;
    // unreachable at default ceilings, but both guards are caller-widenable.
    if want > isize::MAX as usize {
        return Err(ChunkFault::Bad(BadChunkKind::ImplausibleSize { declared }));
    }
    let payload = payload_of(declared_fits()?);

    if payload.is_empty() {
        return Err(ChunkFault::Bad(BadChunkKind::Decompress {
            codec: head.codec,
        }));
    }

    decompress_into(head.codec, payload, want, scratch)?;
    let records = &scratch[..];
    check_crc(records, head.uncompressed_crc)?;
    Ok(records)
}

/// Decompress `payload` into `scratch`, leaving exactly `want` bytes.
///
/// `scratch` is never shrunk and grows by `reserve_exact`
/// (`the_output_buffer_is_not_doubled_past_the_chunk`). Decoders are constructed
/// per chunk: see `decode_zstd`.
// Codec-free build only: the parameters go unused and narrowing `&mut Vec` would break the other.
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
        other => Err(ChunkFault::Unsupported(other)),
    }
}

/// The smallest window ceiling ever imposed, 8 MiB: `zstd -19` declares 8 MiB
/// and `testdata/zstd_conformance.mcap` is built with it.
#[cfg(feature = "compression")]
const MIN_ZSTD_WINDOW_BYTES: u64 = 8 * 1024 * 1024;

/// The largest zstd decoding window allocated for a chunk declaring `want`
/// uncompressed bytes.
///
/// `ruzstd` allocates from the frame's declared window and neither
/// [`ChunkLimits`] knob sees it. A window larger than the output is unusable, so
/// `want` is the exact bound. `zstd --ultra -20`/`-21` and `--long=27` exceed
/// `max(want, 8 MiB)` and are refused ([`BadChunkKind::ImplausibleWindow`]);
/// raise [`MIN_ZSTD_WINDOW_BYTES`] to admit them.
#[cfg(feature = "compression")]
fn window_ceiling(want: usize) -> u64 {
    (want as u64).max(MIN_ZSTD_WINDOW_BYTES)
}

/// zstd, via `ruzstd`'s one-shot decode into an exactly-`want` slice, which
/// detects both a short and an over-long frame with no probe read. No content
/// checksum is verified: the chunk CRC32 in `chunk_records` is the check.
#[cfg(feature = "compression")]
fn decode_zstd(payload: &[u8], want: usize, scratch: &mut Vec<u8>) -> Result<(), ChunkFault> {
    use ruzstd::decoding::errors::FrameDecoderError;
    use ruzstd::decoding::FrameDecoder;

    // No `clear()` before `resize`: the decoder overwrites it, and stale bytes past
    // `written` are unobservable. The lz4 arm clears because `read_to_end` appends;
    // do not unify them.
    scratch.reserve_exact(want.saturating_sub(scratch.len()));
    scratch.resize(want, 0);
    let mut decoder = FrameDecoder::new();
    let ceiling = window_ceiling(want);
    decoder.set_max_window_size(ceiling);
    match decoder.decode_all(payload, &mut scratch[..]) {
        Ok(written) if written == want => Ok(()),
        Ok(written) => Err(ChunkFault::Bad(BadChunkKind::LengthMismatch {
            declared: clamp_u32(want as u64),
            produced: clamp_u32(written as u64),
        })),
        Err(FrameDecoderError::TargetTooSmall) => Err(ChunkFault::Bad(BadChunkKind::Overrun {
            codec: ChunkCodec::Zstd,
            declared: clamp_u32(want as u64),
        })),
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

/// lz4, via `lz4_flex`'s **frame** decoder (MCAP's `"lz4"` is the frame format).
///
/// The `+ 1` is load-bearing: `lz4_flex` checks content length and checksum only on
/// reaching the `EndMark`, and this is the only output bound on this path.
#[cfg(feature = "compression")]
fn decode_lz4(payload: &[u8], want: usize, scratch: &mut Vec<u8>) -> Result<(), ChunkFault> {
    use std::io::Read;

    scratch.clear();
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
        _ => Err(ChunkFault::Bad(BadChunkKind::Overrun {
            codec: ChunkCodec::Lz4,
            declared: clamp_u32(want as u64),
        })),
    }
}

/// The fixed part of a chunk record's header, parsed by hand so a truncated chunk is readable.
struct ChunkHead {
    /// `ChunkHeader::uncompressed_size`, at offset 16.
    uncompressed_size: u64,
    uncompressed_crc: u32,
    codec: ChunkCodec,
    compressed_size: u64,
    /// Offset in the chunk record's body at which the records field starts.
    records_at: usize,
}

impl ChunkHead {
    /// `message_start_time: u64`, `message_end_time: u64`, `uncompressed_size: u64`,
    /// `uncompressed_crc: u32`, `compression: u32-prefixed string`, `compressed_size: u64`,
    /// all little-endian.
    fn parse(body: &[u8]) -> Result<Self, ChunkFault> {
        /// Up to the `compression` length prefix.
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
        let records_at = after_name
            .checked_add(8)
            .ok_or_else(|| framing(after_name))?;
        if records_at > body.len() {
            return Err(framing(body.len()));
        }
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

/// The message times a chunk header declares; `None` if it cannot be parsed.
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

/// Verify a chunk's records against the header CRC32; a saved `0` is skipped.
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

/// Walk the records inside a chunk's decompressed `records` field, borrowing the
/// buffer. With `tolerate_tail`, a trailing fragment is a normal end; without
/// it, a fragment or overlong body is `InnerFraming` at its offset.
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
        let len_bytes: [u8; 8] = match records[at + 1..at + HEADER].try_into() {
            Ok(b) => b,
            Err(_) => return Err(framing(at)),
        };
        let len = u64::from_le_bytes(len_bytes);
        let len = match usize::try_from(len) {
            Ok(n) => n,
            Err(_) => return Err(framing(at)),
        };
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

    /// The limits a real ingest runs with.
    fn limits() -> ChunkLimits {
        crate::IngestOptions::default().chunk_limits()
    }

    /// Assemble a chunk record body by hand: the tests need headers a writer would refuse.
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

    fn inner_record(opcode: u8, body: &[u8]) -> Vec<u8> {
        let mut b = vec![opcode];
        b.extend_from_slice(&(body.len() as u64).to_le_bytes());
        b.extend_from_slice(body);
        b
    }

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

    /// A truncated chunk's CRC is not checked: the hash covers the whole field.
    #[test]
    fn a_truncated_chunk_does_not_have_its_crc_checked() {
        let mut body = Vec::new();
        body.extend_from_slice(&1_000u64.to_le_bytes()); // message_start_time
        body.extend_from_slice(&2_000u64.to_le_bytes()); // message_end_time
        body.extend_from_slice(&64u64.to_le_bytes()); // uncompressed_size
        body.extend_from_slice(&0xDEAD_BEEFu32.to_le_bytes()); // uncompressed_crc
        body.extend_from_slice(&0u32.to_le_bytes()); // compression name length
        body.extend_from_slice(&64u64.to_le_bytes()); // compressed_size
        body.extend_from_slice(b"only a few bytes of the records field");

        const HEADER_BYTES: usize = 40;
        let mut scratch = Vec::new();
        let partial =
            chunk_records(&body, false, limits(), &mut scratch).expect("a prefix must be readable");
        assert_eq!(
            partial.len(),
            body.len() - HEADER_BYTES,
            "the whole surviving records prefix must be handed over"
        );

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

    /// Disagreeing size fields on an uncompressed complete chunk are
    /// `StoredSizeMismatch`, not `LengthMismatch`; a truncated one is not damage.
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
                let text = kind.to_string();
                assert!(
                    text.contains("stored uncompressed") && !text.contains("produced"),
                    "the message must not describe a decode that never happened: {text}"
                );
            }
            other => panic!("expected a StoredSizeMismatch, got {other:?}"),
        }

        let mut scratch = Vec::new();
        assert!(
            chunk_records(&body, false, limits(), &mut scratch).is_ok(),
            "a truncated chunk's size disagreement is truncation, not corruption"
        );
    }

    /// A declared `uncompressed_size` past the ceiling is refused before the buffer
    /// is sized (asserted on the allocation). Gated on `compression`.
    #[cfg(feature = "compression")]
    #[test]
    fn a_lying_uncompressed_size_is_refused_before_it_allocates() {
        const GIB: u64 = 1024 * 1024 * 1024;
        let payload = vec![0x5Au8; 1024 * 1024];
        let body = chunk_body("zstd", GIB, 0, payload.len() as u64, &payload);

        let mut scratch = Vec::new();
        let err = chunk_records(&body, true, limits(), &mut scratch).unwrap_err();
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

    /// A chunk claiming to expand past the ratio is refused before the buffer is
    /// sized, though under the absolute ceiling. Gated on `compression`.
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

    /// The ratio guard survives an overflowing `compressed_size`: the chunk is
    /// refused as `CompressedSizeMismatch`. Gated on `compression`.
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

    /// A codec with no payload is a self-contradicting header, not an empty chunk.
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

    /// A truncated compressed chunk yields no records and no fault.
    #[cfg(feature = "compression")]
    #[test]
    fn a_truncated_compressed_chunk_is_not_a_bad_chunk() {
        for codec in ["zstd", "lz4"] {
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

    /// Round-trip through each codec, and the exact fault on each side of the
    /// declared length; the CRC-0 rows isolate the length check.
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

            let body = chunk_body(name, exact, crc, size, &payload);
            let mut scratch = Vec::new();
            let got = chunk_records(&body, true, limits(), &mut scratch)
                .unwrap_or_else(|e| panic!("{name} round trip: {e:?}"));
            assert_eq!(got, &records[..], "{name} did not round-trip");

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

    /// A conformance vector for lz4, hand-written from the frame and block formats
    /// (the zstd half is `testdata/zstd_conformance.mcap`): literal- and match-length
    /// extensions, an overlapping match, `EndMark` and xxh32 checksum.
    #[cfg(feature = "compression")]
    #[test]
    fn a_hand_authored_lz4_frame_decodes_per_the_specification() {
        let want = lz4_vector_content();
        assert_eq!(want.len(), 72, "the frame declares 72 content bytes");

        let mut scratch = Vec::new();
        decompress_into(ChunkCodec::Lz4, LZ4_SPEC_VECTOR, want.len(), &mut scratch)
            .unwrap_or_else(|e| panic!("the hand-authored frame did not decode: {e:?}"));
        assert_eq!(scratch, want, "lz4_flex disagrees with the specification");

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

        assert_ne!(
            encode_lz4(&want),
            LZ4_SPEC_VECTOR,
            "the vector must not be what lz4_flex's own encoder produces"
        );
    }

    /// Of the 656 single-bit perturbations of the vector, all but five are caught;
    /// the survivors are asserted as an exact set.
    ///
    /// * Byte 60, bits 0-3: the last sequence's unused match-length nibble.
    /// * Byte 77, bit 7: `EndMark` read as `0x80000000`, which `lz4_flex` accepts.
    ///
    /// Bytes 49 and 78-81 are caught only because `decode_lz4`'s `+ 1` budget
    /// reaches the `EndMark` arm.
    #[cfg(feature = "compression")]
    #[test]
    fn a_single_flipped_bit_in_the_lz4_vector_is_caught() {
        /// `(byte, bit)` pairs the format or the decoder treats as don't-care.
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

    /// The 82 hand-authored bytes of `a_hand_authored_lz4_frame_decodes_per_the_specification`;
    /// not regenerable by a tool, which would make it a round-trip.
    #[cfg(feature = "compression")]
    const LZ4_SPEC_VECTOR: &[u8] = &[
        0x04, 0x22, 0x4d, 0x18, 0x6c, 0x40, 0x48, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0xd0, // block size: 55, high bit clear -> a compressed block
        0x37, 0x00, 0x00, 0x00,
        // sequence 1: 29 literals, then a 20-byte match at offset 20.
        0xff, 0x0e, 0x05, 0x3f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x6c, 0x7a, 0x34, 0x20,
        0x66, 0x72, 0x6f, 0x6d, 0x20, 0x74, 0x68, 0x65, 0x20, 0x73, 0x70, 0x65, 0x63, 0x3a, 0x20,
        0x20, 0x14, 0x00, 0x01,
        // sequence 2: 4 literals, then a 6-byte overlapping match at offset 1.
        0x42, 0x20, 0x6e, 0x6f, 0x21, 0x01, 0x00, // sequence 3: 13 literals, no match.
        0xd0, 0x20, 0x6c, 0x69, 0x62, 0x6c, 0x7a, 0x34, 0x2d, 0x66, 0x72, 0x65, 0x65, 0x2e, 0x00,
        0x00, 0x00, 0x00, 0x1a, 0x6c, 0xf9, 0x70,
    ];

    /// What [`LZ4_SPEC_VECTOR`] must decode to.
    #[cfg(feature = "compression")]
    fn lz4_vector_content() -> Vec<u8> {
        inner_record(
            0x05,
            b"lz4 from the spec:  lz4 from the spec:   no!!!!!!! liblz4-free.",
        )
    }

    /// The output buffer is sized to the chunk on reuse, not doubled past it, for both codecs.
    #[cfg(feature = "compression")]
    #[test]
    fn the_output_buffer_is_not_doubled_past_the_chunk() {
        for codec in [ChunkCodec::Zstd, ChunkCodec::Lz4] {
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
                let slack = if codec == ChunkCodec::Lz4 { 1 } else { 0 };
                assert_eq!(
                    scratch.capacity(),
                    want + slack,
                    "{codec} at {kib} KiB overshot: a buffer this reader never shrinks                      must not be doubled past the chunk it was sized for"
                );
            }
        }
    }

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

    /// A hand-rolled zstd frame: one raw block, window `1 << (10 + exponent)`.
    #[cfg(feature = "compression")]
    fn zstd_frame_with_window(exponent: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![0x28, 0xb5, 0x2f, 0xfd, 0x00, exponent << 3];
        let header = 1u32 | ((body.len() as u32) << 3);
        v.extend_from_slice(&header.to_le_bytes()[..3]);
        v.extend_from_slice(body);
        v
    }

    /// A zstd frame demanding an oversized window is refused, including as the
    /// second frame of a payload, where `ruzstd` allocates eagerly.
    #[cfg(feature = "compression")]
    #[test]
    fn a_zstd_frame_demanding_an_oversized_window_is_refused() {
        /// `1 << (10 + 16)`, i.e. 64 MiB: what `zstd --ultra -21` declares.
        const HOSTILE_EXPONENT: u8 = 16;
        const HOSTILE_WINDOW: u64 = 1 << (10 + HOSTILE_EXPONENT as u64);

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

    /// The window floor admits 8 MiB (`zstd -19`) and nothing above.
    #[cfg(feature = "compression")]
    #[test]
    fn the_window_floor_admits_what_a_real_zstd_encoder_declares() {
        assert_eq!(MIN_ZSTD_WINDOW_BYTES, 1 << (10 + 13));
        let payload = zstd_frame_with_window(13, b"aaaa");
        let body = chunk_body("zstd", 4, 0, payload.len() as u64, &payload);
        let mut scratch = Vec::new();
        let got = chunk_records(&body, true, limits(), &mut scratch)
            .unwrap_or_else(|e| panic!("an 8 MiB window must be accepted: {e:?}"));
        assert_eq!(got, b"aaaa");

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

    /// Compress with `ruzstd`'s encoder; round-trip is not conformance.
    #[cfg(feature = "compression")]
    fn encode_zstd(bytes: &[u8]) -> Vec<u8> {
        ruzstd::encoding::compress_to_vec(bytes, ruzstd::encoding::CompressionLevel::Fastest)
    }

    /// Compress with `lz4_flex`'s frame encoder; see [`LZ4_SPEC_VECTOR`].
    #[cfg(feature = "compression")]
    fn encode_lz4(bytes: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut enc = lz4_flex::frame::FrameEncoder::new(Vec::new());
        enc.write_all(bytes).unwrap();
        enc.finish().unwrap()
    }
}
