//! Chunk handling: this crate owns MCAP's record framing and reads inside chunks.
//!
//! # Why the framing is ours
//!
//! `mcap` is taken `default-features = false` because its `zstd` and `lz4`
//! features vendor C through `zstd-sys`/`lz4-sys`, and `docs/PHASE2.md` §2
//! forbids a C build step. The consequence used to be that **every compressed
//! recording was refused**, since the crate's own decompressor factory is the
//! only thing that reads a chunk and it can only report an unsupported codec —
//! and rosbag2 and Foxglove both write zstd chunks by default.
//!
//! The first fix was `LinearReaderOptions::with_emit_chunks(true)`, which hands
//! chunks over whole so the factory is never reached. It worked, and it cost
//! something that only a test revealed: **the reader will not hand over an
//! incomplete record**, so a recording cut mid-chunk lost that whole chunk. On a
//! real 1–4 MiB-chunked file that is the final chunk; on a small one it is
//! everything. Truncation is not an edge case here — §3.3's `freeze --from-live`
//! exists to "capture a fault in the field", and a fault in the field is how
//! recordings get truncated.
//!
//! So the framing is ours instead. It is nine bytes — `opcode: u8`, `len: u64`
//! little-endian, `body[len]` — plus an eight-byte magic at each end of the file,
//! and it is the *same* framing inside a chunk, so [`for_each_record`] serves
//! both. What stays with `mcap` is every record **body**: `parse_record`,
//! `records::*`, the `ChunkHeader` layout. That is the line worth holding — the
//! framing is trivial and we already walked it for chunk interiors, whereas a
//! `Schema`, `Channel` or `Message` body is genuinely not ours to re-derive.
//!
//! What that buys, beyond compressed chunks:
//!
//! * **Record-granular truncation recovery, including inside a chunk.** A cut
//!   chunk's prefix still yields every whole record in it.
//! * **Byte offsets**, for a diagnostic that can say where.
//! * **Chunk CRC validation** — `mcap`'s own runs only under
//!   `validate_chunk_crcs`, which its default leaves off, so chunk CRCs were
//!   never checked here at all. Doing it ourselves is a net gain.
//!
//! The one case still unrecoverable is a truncated **compressed** chunk: a partial
//! codec frame is not decodable by a one-shot decoder. The bound is one chunk, and
//! it is reported as truncation rather than as corruption, because nothing is
//! wrong with the file beyond where it stops.

use crate::IngestError;

/// Which codec a chunk's `compression` field names.
///
/// `Copy` and carries no `String`, so an error can hold it (`docs/PROJECT.md`
/// §5). [`ChunkCodec::Other`] therefore loses the codec's name; the alternative
/// was a fixed-capacity byte array in the variant, which is uglier and buys one
/// better message in a case nobody has met.
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
    /// Classify the `compression` field of a chunk header.
    ///
    /// **Case-sensitive, deliberately.** The MCAP specification fixes these as
    /// exact strings; accepting `"ZSTD"` would be inventing a dialect, and the
    /// writer that produced it is the thing that should be fixed.
    pub(crate) fn parse(name: &str) -> Self {
        match name {
            "" => Self::None,
            "zstd" => Self::Zstd,
            "lz4" => Self::Lz4,
            _ => Self::Other,
        }
    }

    /// Whether this build carries a decoder for it.
    ///
    /// [`ChunkCodec::None`] always counts: an uncompressed chunk needs no
    /// decoder. The rest depend on the `compression` feature, which is why this
    /// is a method rather than a `match` written out at each call site.
    pub(crate) fn is_built_in(self) -> bool {
        matches!(self, Self::None)
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

/// Why one chunk could not be read.
///
/// `Copy` and `String`-free (`docs/PROJECT.md` §5). Each variant carries the
/// numbers that distinguish "this recording is damaged" from "this file is not
/// what it claims to be", which is the same reason
/// [`CdrError`](crate::cdr::CdrError) carries a byte offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum BadChunkKind {
    /// The codec's stream did not decode.
    #[error("its {codec} stream did not decode")]
    Decompress {
        /// Which codec was in use.
        codec: ChunkCodec,
    },
    /// Decompression produced a different number of bytes than the header
    /// declared.
    ///
    /// A correctness guard as much as a safety one: a stream that stops early
    /// would otherwise parse as a valid *short* record list, losing transforms
    /// with no counter anywhere to say so.
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
    /// The header declares more uncompressed bytes than this reader will
    /// allocate for.
    #[error("it declares {declared} uncompressed bytes, past this reader's ceiling")]
    ImplausibleSize {
        /// `ChunkHeader::uncompressed_size`.
        declared: u64,
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
///
/// The split matters because the two halves have different policies:
/// [`ChunkFault::Unsupported`] is never skippable (every chunk in the file will
/// use the same codec, so skipping them all yields "no transforms" explaining
/// nothing), while [`ChunkFault::Bad`] is skippable by default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChunkFault {
    /// The codec is one this build has no decoder for.
    Unsupported(ChunkCodec),
    /// The chunk is damaged.
    Bad(BadChunkKind),
    /// The caller's own callback failed — an edge-kind change, a clock reset
    /// under `halt`, an undecodable CDR payload. Carried through rather than
    /// reshaped, because it is not a fact about the chunk and must never be
    /// treated as one: a skip policy that swallowed it would turn a hard error
    /// into silent data loss.
    Callback(IngestError),
}

/// Hand back a chunk's `records` field, decompressing if this build can.
///
/// `scratch` is the caller's buffer, allocated once and reused for every chunk
/// in the file. An uncompressed chunk is returned **by borrow** and never copied
/// through it, so the case that already worked gains no copy — the two
/// lifetimes are unified because the return value is one or the other.
///
/// In this revision no codec is compiled in, so every compressed chunk is
/// [`ChunkFault::Unsupported`] — the same outcome the reader produced before this
/// module existed.
///
/// # Why the header is parsed here rather than by `mcap::parse_record`
///
/// `parse_record` validates `compressed_size` against the bytes it was given and
/// fails with `BadChunkLength` when it is short. That is right for a whole file
/// and wrong for a **truncated** one: the final chunk of a SIGKILLed recording is
/// exactly the case where `compressed_size` describes bytes that were never
/// written, and refusing it there would throw away every complete record inside
/// its prefix. Parsing the six fixed fields here is twenty lines and is what makes
/// recovery record-granular rather than chunk-granular.
pub(crate) fn chunk_records<'a>(
    body: &'a [u8],
    complete: bool,
    scratch: &'a mut Vec<u8>,
) -> Result<&'a [u8], ChunkFault> {
    let head = ChunkHead::parse(body)?;
    if !head.codec.is_built_in() {
        return Err(ChunkFault::Unsupported(head.codec));
    }
    // Clamp rather than trust: on a truncated chunk `compressed_size` names bytes
    // past the end of the file.
    let available = body.len() - head.records_at;
    let take = if complete {
        match usize::try_from(head.compressed_size) {
            Ok(n) if n <= available => n,
            // A complete chunk whose declared size does not fit is corrupt, not
            // truncated — that is what `complete` distinguishes.
            _ => {
                return Err(ChunkFault::Bad(BadChunkKind::LengthMismatch {
                    declared: clamp_u32(head.compressed_size),
                    produced: clamp_u32(available as u64),
                }))
            }
        }
    } else {
        available
    };
    let records = &body[head.records_at..head.records_at + take];

    // Untouched on this path: `scratch` exists for the compressed one and is not
    // read here, so an uncompressed chunk is still never copied. Keeping the
    // borrow in the signature rather than splitting the function is what lets the
    // caller hold exactly one buffer for the whole file.
    let _ = scratch;

    // **A truncated chunk's CRC cannot be checked, and pretending otherwise would
    // turn every truncated recording into a corrupt one.** The saved hash covers
    // the whole records field; we have a prefix of it, so a mismatch is
    // guaranteed and means nothing.
    if complete {
        check_crc(records, head.uncompressed_crc)?;
    }
    Ok(records)
}

/// The fixed part of a chunk record's header.
///
/// Only the fields this module needs, parsed by hand so a truncated chunk is
/// still readable — see [`chunk_records`].
struct ChunkHead {
    // `uncompressed_size` sits at offset 16 and is *not* kept: nothing in this
    // revision reads it, and parsing a field nothing reads would be one more thing
    // to keep true for no gain. Once a codec exists it becomes the exact allocation
    // size and the value the decompression-bomb guard bounds.
    //
    // **A codec is not the only reason to retain it.** On the uncompressed path the
    // records are stored verbatim, so `uncompressed_size == compressed_size` is an
    // invariant checkable here today, from two `u64`s nine bytes apart, and a chunk
    // header rewritten by a bad sector currently passes. That check is a `decompress`
    // change with its own test, deliberately not smuggled into the commit that added
    // the fixture for it; `ingest::a_lying_uncompressed_size_is_not_detected_by_this_build`
    // pins today's behaviour so it cannot be closed silently either way.
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
        // A codec name is ASCII in practice; a non-UTF-8 one is not a codec this
        // build knows, which `ChunkCodec::Other` already says.
        let codec = match core::str::from_utf8(&body[name_at..after_name]) {
            Ok(s) => ChunkCodec::parse(s),
            Err(_) => ChunkCodec::Other,
        };
        let compressed_size = u64_at(after_name);
        Ok(Self {
            uncompressed_crc,
            codec,
            compressed_size,
            records_at,
        })
    }
}

/// The message times a chunk header declares, for reporting what a skip lost.
///
/// `None` when the header cannot be parsed at all, which is the one case where
/// there is nothing to report.
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
    // A writer that does not track them leaves both zero; reporting
    // "1970-01-01 to 1970-01-01" as the lost span would be worse than silence.
    if start == 0 && end == 0 {
        None
    } else {
        Some((start.min(end), start.max(end)))
    }
}

/// Saturate a `u64` into the `u32` an error variant carries.
///
/// The variants are `u32` to keep `IngestError` small; every real value is far
/// below the ceiling, and a corrupt one only needs to read as "absurdly large".
fn clamp_u32(v: u64) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

/// Verify a chunk's records against the CRC32 in its header.
///
/// # This is a gain, not a cost, and the reason is easy to misread
///
/// `LinearReaderOptions` derives `Default`, so `validate_chunk_crcs` is `false`
/// and the crate's own check has **never** run in this crate. Doing it here means
/// chunk CRCs are validated for the first time — including on the uncompressed
/// chunks that already worked.
///
/// A saved CRC of `0` means "not computed" per the MCAP specification, so it is
/// skipped rather than compared. Treating it as a real hash would fail every
/// recording from a writer that does not compute one.
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

/// Walk the records inside a chunk's decompressed `records` field.
///
/// The framing is MCAP's own, minus the file-level magic: `opcode: u8`, then
/// `len: u64` little-endian, then `len` bytes of body, repeated to the end.
///
/// # Why this is hand-rolled
///
/// A second `LinearReader` over the same bytes would work — the crate supports
/// it with `skip_start_magic` and `skip_end_magic` — but it pumps every byte
/// through `RwBuf::insert`, so it **copies the whole chunk a second time** and
/// re-applies a length limit the outer reader has already applied to this record.
/// The loop below borrows the buffer directly, so `parse_record` gets a slice
/// into it and no intermediate copy exists.
///
/// With `tolerate_tail`, a trailing fragment is a normal end rather than
/// corruption — which is what a **truncated** chunk's records field always looks
/// like, since the file stopped in the middle of one. Without it, a fragment or a
/// body running past the end is corruption and says so, carrying the offset.
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
        // `unwrap` is unavailable under this workspace's lints, and the slice is
        // exactly eight bytes by construction, so the fallible conversion is
        // written as a match rather than an assertion.
        let len_bytes: [u8; 8] = match records[at + 1..at + HEADER].try_into() {
            Ok(b) => b,
            Err(_) => return Err(framing(at)),
        };
        let len = u64::from_le_bytes(len_bytes);
        // Two checks, not one. `usize::try_from` catches a length past the
        // address space on a 32-bit host; the addition is checked because
        // `at + HEADER + len` is the value that could wrap.
        let len = match usize::try_from(len) {
            Ok(n) => n,
            Err(_) => return Err(framing(at)),
        };
        let end = match at.checked_add(HEADER).and_then(|h| h.checked_add(len)) {
            Some(e) if e <= records.len() => e,
            // A body running past the end is the *other* face of truncation: the
            // last record in a cut chunk declares more than survived.
            _ => {
                return if tolerate_tail {
                    Ok(())
                } else {
                    Err(framing(at))
                }
            }
        };
        g(opcode, &records[at + HEADER..end]).map_err(ChunkFault::Callback)?;
        at = end;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// The three names the specification fixes, and nothing else.
    ///
    /// Mutant: make `parse` case-insensitive (`name.to_ascii_lowercase()`) ⇒ the
    /// `"ZSTD"` and `"Zstd"` cases stop being `Other` and this fails. Accepting
    /// them would mean a build without a zstd decoder silently treating an
    /// unknown codec as one it knows.
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
    ///
    /// Mutant: make the loop reject `records.is_empty()` ⇒ this fails. A chunk
    /// with no records is legal, and a writer that flushes on a timer produces
    /// them.
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
    ///
    /// Mutant: drop `HEADER` from the body's start offset
    /// (`&records[at..end]`) ⇒ the first body reads back as the opcode and
    /// length bytes instead of `b"ab"`, and this fails.
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

    /// A body whose declared length runs past the end of the chunk is refused,
    /// rather than slicing out of range.
    ///
    /// Mutant: change the bound to `e < records.len()` — or drop the `end`
    /// check entirely — ⇒ the slice panics instead of returning an error, and
    /// this test fails on a panic rather than a `Result`. A corrupt length here
    /// is attacker-controlled.
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
    ///
    /// Mutant: `if remaining < HEADER` → `if remaining == 0` ⇒ the four trailing
    /// bytes are read as a header, `records[at + 1..at + 9]` slices out of
    /// range, and this panics instead of returning.
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
    ///
    /// Mutant: `at = end` → `at = end + 1` ⇒ the second record's opcode is
    /// misread and this fails. Guards against fixing the previous test by
    /// skipping a byte.
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
    ///
    /// Two properties in one test because they are the same decision made twice.
    /// Mutant: delete the comparison ⇒ the wrong-hash case is accepted. Mutant:
    /// drop the `saved == 0` early return ⇒ the not-computed case starts failing,
    /// and every recording from a writer that does not compute a CRC breaks.
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

    /// A chunk header shorter than its fixed fields is a framing fault, not a
    /// panic.
    ///
    /// Mutant: drop the `body.len() < FIXED` guard ⇒ the `u64_at`/`u32_at` slices
    /// go out of range and this panics instead of returning. The input is a
    /// truncated chunk, i.e. the ordinary case this rewrite exists to serve, so
    /// the guard is on a real path rather than a defensive one.
    #[test]
    fn a_chunk_header_too_short_to_parse_is_a_framing_fault() {
        for len in [0usize, 1, 15, 27, 31] {
            let body = vec![0u8; len];
            let mut scratch = Vec::new();
            let err = chunk_records(&body, false, &mut scratch).unwrap_err();
            assert!(
                matches!(err, ChunkFault::Bad(BadChunkKind::InnerFraming { .. })),
                "len {len} gave {err:?}"
            );
        }
    }

    /// A truncated chunk's CRC is not checked, because it cannot be.
    ///
    /// The saved hash covers the whole records field and we hold a prefix, so a
    /// mismatch is guaranteed and means nothing. Checking it anyway would turn
    /// every truncated recording into a corrupt one.
    ///
    /// Mutant: make the `check_crc` call unconditional (drop `if complete`) ⇒ the
    /// partial read below fails with a `Crc` fault.
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
        let partial = chunk_records(&body, false, &mut scratch).expect("a prefix must be readable");
        assert_eq!(
            partial.len(),
            body.len() - HEADER_BYTES,
            "the whole surviving records prefix must be handed over"
        );

        // The same bytes declared complete: now the length disagreement is real.
        let mut scratch2 = Vec::new();
        let err = chunk_records(&body, true, &mut scratch2).unwrap_err();
        assert!(
            matches!(err, ChunkFault::Bad(BadChunkKind::LengthMismatch { .. })),
            "got {err:?}"
        );
    }

    /// The span a skipped chunk reports comes from its header, and an unset one is
    /// reported as absent rather than as 1970.
    ///
    /// Mutant: drop the `start == 0 && end == 0` case ⇒ a writer that does not
    /// track message times has its lost span reported as the epoch, which reads as
    /// a real answer and is not one.
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
    ///
    /// Mutant: swallow the callback's error (`let _ = g(..)`) ⇒ both records are
    /// visited and this fails. The callback is where a `TFMessage` is decoded,
    /// so continuing past its failure would report a partial recording as whole.
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
}
