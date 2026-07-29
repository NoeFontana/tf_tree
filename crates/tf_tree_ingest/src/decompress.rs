//! Chunk handling: this crate takes chunks whole and reads inside them itself.
//!
//! # Why the reader is configured this way
//!
//! `mcap` is taken `default-features = false` because its `zstd` and `lz4`
//! features vendor C through `zstd-sys`/`lz4-sys`, and `docs/PHASE2.md` §2
//! forbids a C build step. The consequence is that the crate's own
//! `get_decompressor` (`sans_io/linear_reader.rs`) can only ever return
//! `UnsupportedCompression`, so **every compressed recording was refused** — and
//! rosbag2 and Foxglove both write zstd chunks by default.
//!
//! `LinearReaderOptions::with_emit_chunks(true)` routes around that entirely.
//! The reader's `opcode == op::CHUNK && !emit_chunks` guard stops applying, the
//! chunk falls through to the generic record path, and `get_decompressor` is
//! never reached. What arrives is a `Record { opcode: op::CHUNK }` that
//! `mcap::parse_record` splits into a `ChunkHeader` and its still-compressed
//! body — neither of which is feature-gated. This is not a trick: the crate's
//! own buffer-based reader ships the same configuration.
//!
//! Two jobs become ours as a result. **Reading the records inside a chunk** is
//! [`for_each_inner_record`], below. **Decompressing** is [`chunk_records`],
//! which today serves only the uncompressed case and refuses the rest — the
//! pure-Rust codecs land behind a later `compression` feature, and the point of
//! separating the two changes is that this one is behaviour-preserving.
//!
//! # What this file does *not* yet do
//!
//! A chunk carries an `uncompressed_crc`, and validating it is now ours as well
//! — the crate's own check runs only under `validate_chunk_crcs`, which defaults
//! off and was never enabled here. It is deliberately not done in this revision:
//! a CRC failure needs an error variant that says so, `IngestError` does not yet
//! have one, and reporting it as the existing catch-all would be worse than the
//! silence it replaces. Both arrive together.

use crate::IngestError;

/// Which codec a chunk's `compression` field names.
///
/// `Copy` and carries no `String`, so an error can hold it (`docs/PROJECT.md`
/// §5). [`ChunkCodec::Other`] therefore loses the codec's name; the alternative
/// was a fixed-capacity byte array in the variant, which is uglier and buys one
/// better message in a case nobody has met.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChunkCodec {
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
}

/// Hand back a chunk's `records` field, decompressing if this build can.
///
/// `scratch` is the caller's buffer, allocated once and reused for every chunk
/// in the file. An uncompressed chunk is returned **by borrow** and never copied
/// through it, so the case that already worked gains no copy — the two
/// lifetimes are unified because the return value is one or the other.
///
/// In this revision every codec is refused with
/// [`IngestError::CompressedChunk`], which is exactly what the reader used to do
/// one layer down, so the observable behaviour is unchanged.
pub(crate) fn chunk_records<'a>(
    header: &mcap::records::ChunkHeader,
    compressed: &'a [u8],
    scratch: &'a mut Vec<u8>,
) -> Result<&'a [u8], IngestError> {
    match ChunkCodec::parse(&header.compression) {
        ChunkCodec::None => {
            // Untouched: `scratch` exists for the compressed path and is not
            // even read here. Keeping the borrow in the signature rather than
            // splitting the function is what lets the caller hold one buffer.
            let _ = scratch;
            Ok(compressed)
        }
        ChunkCodec::Zstd | ChunkCodec::Lz4 | ChunkCodec::Other => Err(IngestError::CompressedChunk),
    }
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
/// A trailing fragment shorter than a header, or a body that runs past the end,
/// is a corrupt chunk rather than a normal end, and is reported as such.
pub(crate) fn for_each_inner_record<F>(records: &[u8], mut g: F) -> Result<(), IngestError>
where
    F: FnMut(u8, &[u8]) -> Result<(), IngestError>,
{
    /// `opcode: u8` + `len: u64`.
    const HEADER: usize = 1 + 8;

    let mut at = 0usize;
    while at < records.len() {
        let remaining = records.len() - at;
        if remaining < HEADER {
            return Err(IngestError::Mcap);
        }
        let opcode = records[at];
        // `unwrap` is unavailable under this workspace's lints, and the slice is
        // exactly eight bytes by construction, so the fallible conversion is
        // written as a match rather than an assertion.
        let len_bytes: [u8; 8] = match records[at + 1..at + HEADER].try_into() {
            Ok(b) => b,
            Err(_) => return Err(IngestError::Mcap),
        };
        let len = u64::from_le_bytes(len_bytes);
        // Two checks, not one. `usize::try_from` catches a length past the
        // address space on a 32-bit host; the addition is checked because
        // `at + HEADER + len` is the value that could wrap.
        let len = match usize::try_from(len) {
            Ok(n) => n,
            Err(_) => return Err(IngestError::Mcap),
        };
        let end = match at.checked_add(HEADER).and_then(|h| h.checked_add(len)) {
            Some(e) if e <= records.len() => e,
            _ => return Err(IngestError::Mcap),
        };
        g(opcode, &records[at + HEADER..end])?;
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
        for_each_inner_record(&[], |_, _| {
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
        for_each_inner_record(&buf, |op, body| {
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
        let err = for_each_inner_record(&buf, |_, _| Ok(())).unwrap_err();
        assert_eq!(err, IngestError::Mcap);
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
        let err = for_each_inner_record(&buf, |_, _| Ok(())).unwrap_err();
        assert_eq!(err, IngestError::Mcap);
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
        for_each_inner_record(&buf, |op, body| {
            got.push((op, body.len()));
            Ok(())
        })
        .unwrap();
        assert_eq!(got, vec![(0x0f, 0), (0x10, 1)]);
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
        let err = for_each_inner_record(&buf, |_, _| {
            seen += 1;
            Err(IngestError::NoTransforms)
        })
        .unwrap_err();
        assert_eq!(seen, 1);
        assert_eq!(err, IngestError::NoTransforms);
    }
}
