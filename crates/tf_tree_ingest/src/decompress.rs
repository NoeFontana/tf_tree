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
//! With the chunk in hand, the codec is then **ours** rather than `mcap`'s: the
//! `compression` feature (on by default) compiles pure-Rust `ruzstd` and
//! `lz4_flex`, so a zstd or lz4 recording ingests with no C build step. The no-C
//! rule is untouched; what changed is who owns the decoder. A build with the
//! feature off still refuses those chunks with [`ChunkFault::Unsupported`],
//! unchanged, and `tests/codec_free.rs` is compiled in exactly that
//! configuration.
//!
//! **What the C-free decoder costs, measured, because an earlier revision of this
//! paragraph said §0.0's `default-features = false` "costs nothing a user can see"
//! and that was false.** On this host (AMD EPYC-Milan, release, page cache warm),
//! [`crate::survey`] over a 160 000-transform recording in 16 chunks: **0.027 s
//! uncompressed, 0.035 s lz4 (1.33×), 0.048 s zstd (1.82×)**. Isolated, `ruzstd`
//! decodes a libzstd frame at a **small fraction of libzstd's own rate on the same
//! bytes** — several times slower, in the direction this paragraph says. Two things
//! multiply that: decompression is repeated on **every pass**, and the pass count
//! is `1 + groups + spilled edges` rather than a flat two, so `--max-memory`
//! pressure buys extra decodes. It is a price worth paying for no C build step, and
//! it is not the same as free — `docs/PHASE5.md` §12's throughput gate is measured
//! and gated since 2026-09-05 (`just gate5`) and is met by more than an order of
//! magnitude.
//!
//! **CORRECTION (2026-09-05): two MiB/s figures are deleted rather than kept.**
//! This paragraph read "**674 MiB/s** against libzstd's own **2 480 MiB/s** on the
//! same bytes (`zstd -b3`), i.e. **~3.7× slower**". Nothing in this repository
//! re-derives either number — `docs/benchmarks/EVIDENCE.md`'s own rule is that a
//! probe's figure needs a documented command, and these had none, which is the
//! shape that file exists to prevent. The qualitative claim is what the argument
//! rests on and it stays; a reader who needs the ratio should measure it and
//! register a probe. The three `survey` figures above are in the same position
//! and are **kept**, because they are what §0.0's row quotes and deleting them
//! there and here at once would leave that row citing nothing — they are a hand
//! measurement, and `just gate5` does not re-derive them: it times a whole `run`
//! on one codec arm rather than a `survey` on three.
//!
//! So the framing is ours instead. It is nine bytes — `opcode: u8`, `len: u64`
//! little-endian, `body[len]` — plus an eight-byte magic at each end of the file,
//! and it is the *same* framing inside a chunk. There are still **two** walks of
//! it, and neither can serve the other: [`for_each_record`] walks a chunk's
//! records field, which is a `&[u8]` already in hand, while `source::read_tf`
//! walks the file through a `BufReader` and must bound each record's declared
//! length before allocating for it. Keeping them separate is deliberate; keeping
//! them *consistent* is manual, so a change to one framing rule belongs in both.
//! What stays with `mcap` is every record **body**: `parse_record`,
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
//! wrong with the file beyond where it stops. [`chunk_records`] implements that by
//! handing back an *empty* records field for such a chunk — no fault, so
//! `SkipCounts::bad_chunks` never counts it, while `SkipCounts::truncated` (which
//! `read_tf` has already set for any short record) says the recording is a prefix.
//!
//! # Why decompression is bounded three times over
//!
//! A chunk header is two numbers off a disk that may be lying. `uncompressed_size`
//! is both the allocation size and the only thing standing between this reader and
//! a decompression bomb, so [`ChunkLimits`] bounds it absolutely *and* as a ratio
//! of the compressed bytes, **before anything is allocated** — see
//! [`chunk_records`] for the order. Neither codec crate does this for us: ruzstd's
//! `DEFAULT_MAX_WINDOW_SIZE` caps the decoder's *window* (its peak working
//! allocation), not its total output, and a 100 MiB-window frame can still decode
//! to terabytes; lz4_flex bounds only the *per-block* size (4 MiB for a standard
//! frame) and offers no cumulative-output knob at all. The caller-sized output
//! buffer is the actual guarantee for **output** in both cases.
//!
//! The third bound is `window_ceiling`, and it exists because those two bound
//! output while the zstd decoder's own **working** allocation is a different number
//! in a different header that neither of them can see. Measured before it existed:
//! a **26-byte** payload of two concatenated zstd frames whose second declares a
//! 64 MiB window, under an honest `uncompressed_size` of 8 and a correct CRC,
//! decoded to the right eight bytes and drove the allocator to a **134 226 570-byte
//! peak** — five million times the input, and unmoved by either [`ChunkLimits`]
//! knob. `window_ceiling` closes it: the same payload now costs 8 841 bytes and is
//! refused with [`BadChunkKind::ImplausibleWindow`]. Its own docs carry the
//! mechanism and the trade.

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
    /// decoder. `Zstd` and `Lz4` depend on the `compression` feature and
    /// [`ChunkCodec::Other`] never counts — a name this build cannot even
    /// classify is not one it can decode. One method rather than a `cfg` at each
    /// call site, so a build with the feature off differs from one with it on in
    /// exactly one place.
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
    /// The codec's stream had more to give than the header declared.
    ///
    /// **Separate from [`BadChunkKind::LengthMismatch`] because the exact
    /// produced count is unknowable**, and inventing one would be a lie in an
    /// error message: what is known is "more than `declared`", not how much more,
    /// and finding out would mean decoding an unbounded amount — precisely what
    /// [`ChunkLimits`] exists to refuse. It is not [`BadChunkKind::Decompress`]
    /// either: the codec did nothing wrong, the header disagrees with it about a
    /// length.
    ///
    /// **The two decoders detect it differently, and neither mechanism should be
    /// changed on the strength of the other.** `decode_lz4` is the one handed a
    /// budget one byte larger than the declared size, because `lz4_flex` has no
    /// one-shot frame API and the extra byte is also what drives its `EndMark`
    /// checks — see that function. `decode_zstd` hands `ruzstd` a slice of exactly
    /// `want` and reads the over-run off `FrameDecoderError::TargetTooSmall`; an
    /// earlier revision of this paragraph said both used the `+ 1`, which would
    /// invite "simplifying" the zstd slice to `want + 1` and losing the guard,
    /// since `decode_all` would then return `Ok(want + 1)` and land in the
    /// [`BadChunkKind::LengthMismatch`] arm with a fabricated produced count.
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
    /// `compressed_size` names more bytes than the chunk record actually carries.
    ///
    /// **Separate from [`BadChunkKind::LengthMismatch`] because neither number is
    /// an uncompressed byte count.** This fault is raised before any decoder runs —
    /// it is the header's own `compressed_size` against the bytes on disk — so
    /// reporting it as "it declared N uncompressed bytes and produced M" sent an
    /// operator to `uncompressed_size` and to the decompressor, neither of which
    /// had been consulted.
    #[error("it declares {declared} compressed bytes but the record carries {present}")]
    CompressedSizeMismatch {
        /// `ChunkHeader::compressed_size`.
        declared: u32,
        /// How many bytes of the records field the chunk record actually holds.
        present: u32,
    },
    /// An **uncompressed** chunk's two size fields disagree.
    ///
    /// Records stored verbatim make `uncompressed_size == compressed_size` an
    /// invariant, checkable from two `u64`s nine bytes apart with no decoder
    /// involved — which is exactly why it is not
    /// [`BadChunkKind::LengthMismatch`], as it used to be. That variant's
    /// `produced` field is documented as "what the decoder actually wrote", and on
    /// this path no decoder has run: the message read "it declared 87 uncompressed
    /// bytes and produced 23" over two *header* fields, sending an operator to a
    /// decompressor that was never consulted. That is the same misdiagnosis
    /// [`BadChunkKind::CompressedSizeMismatch`] was split out to end, left in place
    /// one branch over.
    ///
    /// Raised only on a **complete** chunk: a truncated one's `compressed_size`
    /// describes bytes that were never written, so the two disagree for a reason
    /// that is not damage.
    #[error("it is stored uncompressed but declares {uncompressed} bytes against {compressed}")]
    StoredSizeMismatch {
        /// `ChunkHeader::uncompressed_size`.
        uncompressed: u32,
        /// `ChunkHeader::compressed_size`.
        compressed: u32,
    },
    /// A zstd frame asks for a decoding window larger than this reader will
    /// allocate for a chunk of this size.
    ///
    /// A different fault from [`BadChunkKind::ImplausibleSize`] because it is a
    /// different number in a different header: `uncompressed_size` is what the
    /// *chunk* declares and is what [`ChunkLimits`] bounds, while the window is what
    /// the *codec frame* declares and is bounded by nothing the caller can set. See
    /// `window_ceiling` for why the second needs bounding at all, and for what the
    /// ceiling is derived from.
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

/// What this reader will decompress a single chunk into, before it believes a
/// header.
///
/// **These are on [`crate::IngestOptions`] and not constants here**, for one
/// reason: the person who meets a limit is the person who cannot patch the crate.
/// A recording whose writer used 128 MiB chunks is not corrupt, and a reader that
/// refuses it with no knob to turn is a reader that has to be forked.
///
/// # What these numbers bound, and what they do not
///
/// They bound the **output buffer**, not peak resident memory: the decoder allocates
/// its own working set beside it, and that set tracks the frame's declared window
/// rather than anything here. Measured with a counting allocator on libzstd `-3`
/// frames written by a streaming encoder, `ruzstd` adds **1.98 MiB of peak while
/// decoding a 1 MiB chunk** and **6.48 MiB for a 4 MiB chunk** — so the transient
/// peak is up to roughly 2.6× the declared size, bounded above by the output buffer
/// plus about twice what `window_ceiling` allows. An operator sizing a container
/// against `--max-chunk-size` should read that flag as "the buffer this reader will
/// size", not as the process's high-water mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkLimits {
    /// Absolute ceiling on a chunk's declared `uncompressed_size`, i.e. on the
    /// output buffer. See the type's own docs for what it does **not** bound.
    pub max_uncompressed_bytes: u64,
    /// Ceiling on `uncompressed_size / compressed_size`.
    ///
    /// The absolute limit alone is not enough: 64 MiB of output from 200 bytes of
    /// input is a bomb that fits under any ceiling generous enough for a real
    /// recording.
    pub max_expansion_ratio: u64,
}

/// Hand back a chunk's `records` field, decompressing if this build can.
///
/// `scratch` is the caller's buffer, allocated once and reused for every chunk
/// in the file. An uncompressed chunk is returned **by borrow** and never copied
/// through it, so the case that already worked gains no copy — the two
/// lifetimes are unified because the return value is one or the other.
///
/// # The order of the checks is the safety argument
///
/// Every one of these happens before a single byte is allocated for output, and
/// each rejects a class the next one could not:
///
/// 1. [`ChunkHead::parse`] — a header too short to hold its own fixed fields.
/// 2. The codec: one this build has no decoder for is
///    [`ChunkFault::Unsupported`], which is never skippable.
///
/// The two paths then diverge, and the numbering below is the *code's* order rather
/// than a tidier one — the compressed path's ordering is load-bearing, and the
/// comment at its guards says which test depends on it.
///
/// The uncompressed path, which allocates nothing and hands back a borrow:
///
/// 3. `compressed_size` against the bytes actually present — a *complete* chunk
///    that declares more than it carries is [`BadChunkKind::CompressedSizeMismatch`];
///    a truncated one is clamped.
/// 4. `uncompressed_size == compressed_size`, the invariant that holds when records
///    are stored verbatim, on a complete chunk only.
/// 5. The CRC, on a complete chunk only.
///
/// The compressed path:
///
/// 6. A truncated chunk: no records at all, and no fault. A partial codec frame is
///    not decodable, and the file is short rather than damaged.
/// 7. Both bomb guards — [`ChunkLimits`], on the header's own raw `u64`s and
///    **before** step 8, so the guard's own arithmetic is reachable.
/// 8. `compressed_size` against the bytes present, as in step 3, and the payload
///    slice is taken.
/// 9. An empty payload: no codec frame is zero bytes.
/// 10. Only now is `scratch` sized and the codec run into it — under one further
///     bound that comes off the codec's own header rather than the chunk's,
///     `window_ceiling`.
/// 11. The produced length against the declared one, then the CRC over the
///     decompressed bytes.
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
    limits: ChunkLimits,
    scratch: &'a mut Vec<u8>,
) -> Result<&'a [u8], ChunkFault> {
    let head = ChunkHead::parse(body)?;
    if !head.codec.is_built_in() {
        return Err(ChunkFault::Unsupported(head.codec));
    }
    // How many bytes of the records field actually survived. On a truncated chunk
    // `compressed_size` names bytes past the end of the file, so it is clamped
    // rather than trusted.
    let available = body.len() - head.records_at;
    let payload_of = |take: usize| &body[head.records_at..head.records_at + take];
    // A *complete* chunk that declares more records than it carries is corrupt,
    // not truncated — that is the distinction `complete` exists to make.
    let declared_fits = || match usize::try_from(head.compressed_size) {
        Ok(n) if n <= available => Ok(n),
        // **`CompressedSizeMismatch` and not `LengthMismatch`**, because neither
        // number here is an uncompressed byte count and no decoder has run: this is
        // the header's `compressed_size` against the bytes on disk. Reported as a
        // `LengthMismatch` it printed "it declared N uncompressed bytes and produced
        // M" over two *compressed* figures, sending a reader to `uncompressed_size`
        // and the decompressor — the two things this arm never consults.
        _ => Err(ChunkFault::Bad(BadChunkKind::CompressedSizeMismatch {
            declared: clamp_u32(head.compressed_size),
            present: clamp_u32(available as u64),
        })),
    };

    if head.codec == ChunkCodec::None {
        // **The uncompressed path, which is a borrow and allocates nothing.**
        // `scratch` exists for the compressed one and is not read here, so the
        // case that already worked gains no copy. Keeping the borrow in the
        // signature rather than splitting the function is what lets the caller
        // hold exactly one buffer for the whole file.
        let _ = scratch;
        let payload = payload_of(if complete {
            declared_fits()?
        } else {
            available
        });

        // **`uncompressed_size == compressed_size` is an invariant when the
        // records are stored verbatim**, checkable from two `u64`s nine bytes
        // apart, and until this commit a header rewritten by a bad sector passed.
        // It needs no decoder: `mcap`'s own writer and reader treat the two as
        // equal, and `fixture::tests::a_clean_hand_rolled_file_is_accepted_by_the_mcap_crate`
        // asserts it of every chunk this repository writes. Only on a *complete*
        // chunk — a truncated one's `compressed_size` describes bytes that were
        // never written, so the two disagree for a reason that is not damage.
        if complete && head.uncompressed_size != head.compressed_size {
            // **`StoredSizeMismatch` and not `LengthMismatch`**, for the reason
            // that variant records: both numbers here are header fields and no
            // decoder has run, so "declared N uncompressed bytes and produced M"
            // named a decompressor this arm never reaches.
            return Err(ChunkFault::Bad(BadChunkKind::StoredSizeMismatch {
                uncompressed: clamp_u32(head.uncompressed_size),
                compressed: clamp_u32(head.compressed_size),
            }));
        }
        // **A truncated chunk's CRC cannot be checked, and pretending otherwise
        // would turn every truncated recording into a corrupt one.** The saved
        // hash covers the whole records field; we have a prefix of it, so a
        // mismatch is guaranteed and means nothing.
        if complete {
            check_crc(payload, head.uncompressed_crc)?;
        }
        // **No ceiling is applied on this path, and that is not an omission.**
        // Nothing is allocated — the records are handed back as a borrow into
        // `body`, which the caller already read under its own record-size bound —
        // so there is no allocation for a bomb guard to bound.
        return Ok(payload);
    }

    // **A truncated compressed chunk is a short recording, not a damaged one.** A
    // partial codec frame is not decodable by any one-shot decoder, so its records
    // are lost — but nothing is *wrong* with the file beyond where it stops, and
    // `read_tf` has already recorded `SkipCounts::truncated` for the short record
    // that got us here. Returning an empty records field rather than a fault is
    // what keeps `bad_chunks` from counting it, which would tell an operator their
    // recording is corrupt when it is merely incomplete.
    if !complete {
        return Ok(&[]);
    }

    // **Both bomb guards, on the header's own two numbers, and deliberately
    // *before* `declared_fits` bounds `compressed_size` by the slice.** Checking
    // them after would leave the guard's own arithmetic seeing only values the
    // address space has already limited, so its overflow behaviour would be
    // unreachable — and an unreachable guard is one a later edit can break with no
    // test noticing. `decompress::tests::a_ratio_check_does_not_overflow_on_a_hostile_compressed_size`
    // is the test that depends on this order.
    let declared = head.uncompressed_size;
    if declared > limits.max_uncompressed_bytes {
        return Err(ChunkFault::Bad(BadChunkKind::ImplausibleSize { declared }));
    }
    // **`saturating_mul`, not `*`.** A `compressed_size` off a hostile disk times
    // the ratio overflows `u64`, and in any build with overflow checks that is a
    // panic in the reader — a denial of service reachable from a file — while in
    // one without them the wrapped product decides the comparison instead of the
    // real one.
    //
    // A zero `compressed_size` needs no separate arm: the product is then zero, so
    // any positive `declared` is refused here, and a chunk that names a codec and
    // declares no compressed bytes is a header contradicting itself either way.
    if declared
        > head
            .compressed_size
            .saturating_mul(limits.max_expansion_ratio)
    {
        return Err(ChunkFault::Bad(BadChunkKind::ImplausibleSize { declared }));
    }
    let Ok(want) = usize::try_from(declared) else {
        // A declared size past the address space on a 32-bit host. The ceiling
        // above will normally have caught it; this is the conversion, not a
        // second policy.
        return Err(ChunkFault::Bad(BadChunkKind::ImplausibleSize { declared }));
    };
    // **A `want` no `Vec` can hold is refused here rather than at the allocator.**
    //
    // `Vec::reserve_exact` panics with "capacity overflow" for any capacity past
    // `isize::MAX`, and `decode_lz4` reserves `want + 1` — the budget that its own
    // docs call the only output bound on that path. So without this, a chunk
    // header declaring such a size is a panic in the reader reachable from a file.
    // `usize::MAX` is worse still: the `+ 1` wraps to zero in a build without
    // overflow checks, pairing a `reserve_exact(0)` with a budget that saturates
    // to `u64::MAX` and leaving `read_to_end` unbounded — precisely the bomb the
    // budget exists to stop.
    //
    // Unreachable at the default ceilings; the guard exists because **both** of
    // the guards above are caller-widenable (`--max-chunk-size` saturates to
    // `u64::MAX`, and the ratio guard's `saturating_mul` pins there too), and it
    // covers the class rather than the `usize::MAX` corner of it.
    if want > isize::MAX as usize {
        return Err(ChunkFault::Bad(BadChunkKind::ImplausibleSize { declared }));
    }
    let payload = payload_of(declared_fits()?);

    // No codec frame is zero bytes — a zstd frame is at least 13 and an LZ4 frame
    // at least 11 — so a chunk that names a codec and carries nothing has nothing
    // to decode, and saying so is more useful than handing back an empty records
    // field as though the chunk were legitimately empty.
    if payload.is_empty() {
        return Err(ChunkFault::Bad(BadChunkKind::Decompress {
            codec: head.codec,
        }));
    }

    decompress_into(head.codec, payload, want, scratch)?;
    let records = &scratch[..];
    // The saved hash covers the **uncompressed** bytes, per the MCAP
    // specification, so this is the same check the uncompressed path makes and
    // not a weaker one. Note what it is *not*: neither codec's own content
    // checksum is verified here — ruzstd exposes the saved and computed zstd
    // checksums but compares nothing, and lz4_flex's xxhash32 check only runs
    // when its frame reaches its end mark. This CRC32 is the check that always
    // runs, which is why the lz4 arm below still goes out of its way to reach
    // that end mark.
    check_crc(records, head.uncompressed_crc)?;
    Ok(records)
}

/// Decompress `payload` into `scratch`, leaving it holding exactly `want` bytes.
///
/// `scratch` is the caller's whole-file buffer and is **not shrunk** between
/// chunks. That is a deliberate choice rather than an omission: a recording's
/// chunks are near-uniform in size, so reuse is the entire reason the buffer
/// belongs to the caller, and shrinking would trade a bounded resident peak for a
/// reallocation per chunk. The peak is bounded by
/// [`ChunkLimits::max_uncompressed_bytes`], which is checked before this is
/// called — so the worst case is one chunk's ceiling, not the largest chunk in
/// any file the process has ever read.
///
/// **Both arms below grow it with `reserve_exact`, and that is what makes the
/// sentence above true rather than approximately true.** `Vec`'s amortised growth
/// is the right default when the final size is unknown; here it is `want`, so
/// doubling only overshoots — measured at up to 2× before the change, and
/// permanent, because nothing shrinks the buffer afterwards.
/// `the_output_buffer_is_not_doubled_past_the_chunk` is the assertion.
///
/// **That argument is about the output buffer and extends to nothing else.** In
/// particular it does not extend to the codec decoders, which are constructed per
/// chunk on purpose: see the comment at `FrameDecoder::new` in `decode_zstd`, where
/// reuse would turn a lazy allocation into an eager one sized by an
/// attacker-chosen header field.
// Both allows exist only in the codec-free build, where every arm that uses these
// parameters is compiled out: `unused_variables` because nothing then reads
// `payload`, `want` or `scratch`, and `ptr_arg` because clippy then sees a
// `&mut Vec` it could narrow to `&mut [u8]` — which it could not, since the decoders
// resize and append. Narrowing the signature to satisfy the lint in one
// configuration would break the other, which is exactly the shape of change a
// `cfg_attr` is for.
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
        // Unreachable: `is_built_in` gated every codec above, and `None` returned
        // by borrow. Written as a fault rather than an `unwrap` or a `panic!`,
        // both of which this workspace denies, and rather than an `unreachable!`
        // that a future codec would silently turn into a crash.
        other => Err(ChunkFault::Unsupported(other)),
    }
}

/// The smallest window ceiling this reader will ever impose, 8 MiB.
///
/// The floor exists because an encoder declares the window it *might* use rather
/// than the one the frame needs, and a streaming encoder cannot know the source size
/// at all. Measured on this host, the largest window a `zstd` CLI invocation
/// reachable without `--ultra`/`--long` declares is exactly this: piping four
/// kilobytes through `zstd -19` declares **8 MiB**, `zstd -3` declares 2 MiB, and
/// `ruzstd`'s own encoder declares 128 KiB. Every chunk of the committed
/// `testdata/zstd_conformance.mcap` declares 8 MiB for ~660 uncompressed bytes, so a
/// ceiling without this floor would reject the repository's own conformance fixture.
#[cfg(feature = "compression")]
const MIN_ZSTD_WINDOW_BYTES: u64 = 8 * 1024 * 1024;

/// The largest zstd decoding window this reader will allocate for a chunk that
/// declares `want` uncompressed bytes.
///
/// # Why a window bound is needed at all
///
/// `FrameDecoder::new` leaves `max_window_size` at ruzstd's
/// `DEFAULT_MAX_WINDOW_SIZE` (100 MiB), and that number is reachable from a file.
/// `decode_all` re-`init`s once per frame in the payload and accepts concatenated
/// frames; the first frame takes `FrameDecoderState::new`, whose `DecodeBuffer` only
/// *records* the window and lets the ring grow on demand, but every subsequent frame
/// takes `reset`, which calls `RingBuffer::reserve(window_size)` **eagerly**.
/// Measured: a 26-byte payload of two frames, the second declaring a 64 MiB window
/// over four raw bytes, decoded correctly under an honest `uncompressed_size` of 8
/// and drove the allocator to a 134 226 570-byte peak. Neither [`ChunkLimits`] knob
/// could see it — they bound `uncompressed_size`, and the window is a different field
/// in a different header — so lowering `--max-chunk-size` to harden against a hostile
/// file changed nothing.
///
/// # Why the ceiling comes from the chunk and not from [`ChunkLimits`]
///
/// A match offset cannot reach further back than the bytes already decoded in the
/// frame, and no dictionary is in play (ruzstd refuses a dictionary frame outright),
/// so **a window larger than the frame's total output is unusable by any conformant
/// decoder**. `want` is therefore the semantically exact bound, and it is already
/// bounded by [`ChunkLimits::max_uncompressed_bytes`] two guards earlier. Using the
/// limit instead — the other obvious choice — would leave a 26-byte chunk entitled to
/// a 64 MiB window under the default 64 MiB ceiling, i.e. would not fix the case
/// above at the defaults, only for a user who lowered the knob.
///
/// ruzstd evaluates the declared window against this **before** allocating, in both
/// `FrameDecoderState::new` and `reset`, so a frame asking for more is refused rather
/// than served: the 26-byte payload above now costs 8 841 bytes.
///
/// # The trade, stated
///
/// This refuses frames that would decode. Measured window declarations that exceed
/// `max(want, 8 MiB)` for a 4 MiB chunk: `zstd --ultra -20` (32 MiB), `--ultra -21`
/// (64 MiB) and `--long=27` (128 MiB — already past ruzstd's own 100 MiB default and
/// refused today). No MCAP writer uses those: `mcap`'s Rust and C++ writers, rosbag2
/// and Foxglove all encode at levels whose declared window is ≤ 8 MiB. The refusal is
/// loud and names both numbers ([`BadChunkKind::ImplausibleWindow`]), so if such a
/// recording ever turns up the fault says exactly what to raise
/// [`MIN_ZSTD_WINDOW_BYTES`] to — which is why this is a constant and not a fourth
/// knob nobody has yet had a reason to turn.
#[cfg(feature = "compression")]
fn window_ceiling(want: usize) -> u64 {
    (want as u64).max(MIN_ZSTD_WINDOW_BYTES)
}

/// zstd, via `ruzstd`'s one-shot decode into a caller-sized slice.
///
/// **`decode_all` into an exactly-`want` slice detects both a short frame and an
/// over-long one with no probe read**, which is why it is used rather than
/// `decode_all_to_vec`: that variant decodes into the vector's *capacity*, and
/// `Vec::with_capacity(n)` may hand back more than `n`, which would silently
/// widen the over-run tolerance this function exists to enforce.
///
/// Two things it does not do, said here rather than implied. It accepts several
/// concatenated frames and silently skips zstd *skippable* frames, so a chunk
/// whose payload is more than one frame is not rejected — the declared length and
/// `window_ceiling` are what constrain it, and the multi-frame case is precisely the
/// one that reaches ruzstd's eager window allocation. And it does not verify zstd's
/// own content checksum — it no longer even computes one, `ruzstd` being taken
/// without its `hash` feature (see this crate's manifest for why). `chunk_records`
/// checks the chunk CRC32 over the same bytes, which is a stronger claim than either
/// codec's internal hash because it is the one the MCAP writer actually committed to.
#[cfg(feature = "compression")]
fn decode_zstd(payload: &[u8], want: usize, scratch: &mut Vec<u8>) -> Result<(), ChunkFault> {
    use ruzstd::decoding::errors::FrameDecoderError;
    use ruzstd::decoding::FrameDecoder;

    // **No `clear()` before this, deliberately.** `resize` alone zero-fills only
    // the shortfall and merely truncates when the previous chunk was larger, which
    // in a recording's steady state of near-uniform chunks writes nothing;
    // `clear()` first made every chunk memset its whole output buffer — a second
    // full-width pass over the recording's entire decompressed size, on every ingest
    // pass, measured at 37 789 ns per 4 MiB chunk. Every one of those bytes was then
    // overwritten by the decoder. What changes is that the bytes below `written`
    // after a short decode are now stale rather than zero, which no caller can
    // observe: that is exactly the arm returning `LengthMismatch`, which hands back
    // nothing. The lz4 arm below *does* `clear()`, because `read_to_end` appends
    // rather than overwriting — the asymmetry is real and neither side should be
    // "tidied" into the other.
    // **Grown exactly, before `resize` grows it amortised.** `resize` reserves the
    // shortfall through `Vec`'s doubling path, so a chunk one kilobyte larger than
    // the previous one doubles the buffer: measured, a 1 049 609-byte chunk
    // following a 1 048 585-byte one took the capacity to 2 097 170 — 1 047 561
    // bytes of overshoot that then stayed resident, since this buffer is
    // deliberately never shrunk. Doubling is the right default when the final size
    // is unknown; here it is `want`, checked against the ceiling two guards
    // earlier, so the exact request is both cheaper and what makes
    // `--max-chunk-size` mean what its help text says.
    //
    // `reserve_exact(0)` is a no-op, so the steady state of near-uniform chunks —
    // where `scratch` is already long enough — costs nothing.
    scratch.reserve_exact(want.saturating_sub(scratch.len()));
    scratch.resize(want, 0);
    // **The decoder is constructed per chunk on purpose. Do not hoist it beside
    // `scratch`.** ruzstd documents `new()` as designed for reuse, and its reuse
    // path is `FrameDecoderState::reset`, which calls
    // `RingBuffer::reserve(window_size)` **eagerly** where the fresh path lets the
    // ring grow on demand. Measured with a counting allocator: a 13-byte frame
    // declaring a 64 MiB window costs a fresh decoder 8 841 bytes and a reused one
    // 134 226 570 — and since neither ruzstd nor this module shrinks a buffer, a
    // hoisted decoder would hold the largest window any chunk in the file declared
    // for the rest of the ingest. What hoisting would save is 44 small allocations
    // per chunk, worth 0.6–1.8% of that chunk's decode time. `window_ceiling` bounds
    // the damage either way; the lazy path is what keeps the bound from being paid at
    // all.
    let mut decoder = FrameDecoder::new();
    // Bound the decoder's *working* allocation, which is a different number in a
    // different header from the one `ChunkLimits` bounds. See `window_ceiling`.
    let ceiling = window_ceiling(want);
    decoder.set_max_window_size(ceiling);
    match decoder.decode_all(payload, &mut scratch[..]) {
        Ok(written) if written == want => Ok(()),
        // A frame that stopped early. **This is a correctness guard, not merely a
        // safety one**: the short output would otherwise parse as a valid but
        // shorter record list, losing transforms with no counter anywhere to say so.
        // The bytes past `written` are whatever the buffer held — zeros on a fresh
        // `scratch`, the previous chunk's records once it has been reused, since the
        // `clear()` above was removed. Either way the walk finds a plausible record
        // list and nothing complains, which is why the comparison and not the shape
        // of the tail is what this arm relies on.
        Ok(written) => Err(ChunkFault::Bad(BadChunkKind::LengthMismatch {
            declared: clamp_u32(want as u64),
            produced: clamp_u32(written as u64),
        })),
        // The decoder filled the slice and still had bytes to collect, which is
        // the over-run. See `BadChunkKind::Overrun` for why it is a length fault
        // carrying no produced count.
        Err(FrameDecoderError::TargetTooSmall) => Err(ChunkFault::Bad(BadChunkKind::Overrun {
            codec: ChunkCodec::Zstd,
            declared: clamp_u32(want as u64),
        })),
        // Named rather than folded into `Decompress`, because "its zstd stream did
        // not decode" would be true and useless: nothing is wrong with the stream,
        // this reader declined to allocate what the frame header asked for. The two
        // numbers are what tells the two apart.
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

/// lz4, via `lz4_flex`'s **frame** decoder.
///
/// MCAP's `"lz4"` is the LZ4 frame format (magic `0x184D2204`), which is also
/// what `mcap`'s own lz4 path calls into liblz4 for. `lz4_flex::block::*` would
/// compile and would silently decode the wrong container; the crate
/// `#[deprecated]`s its crate-root block re-exports for exactly that reason.
///
/// # The `+ 1` is load-bearing — do not "simplify" it to `take(want)`
///
/// There is no one-shot frame helper in `lz4_flex`, so the `Read` impl is driven
/// directly. It runs its own content-length check **and** its xxhash32 content
/// checksum only when it reaches the frame's `EndMark`. With `take(want)`,
/// `read_to_end` stops the instant the limit is hit and never reaches that arm,
/// so both checks are skipped. With `take(want + 1)` a correct `want`-byte frame
/// still has budget for one more `read`, which drives the `EndMark` arm,
/// validates the length and the checksum, and returns `Ok(0)` — and a frame with
/// more to give lands on `want + 1` bytes and is caught as an over-run. Same
/// cost, strictly more checking.
///
/// It is also the **only** output bound that exists on this path: lz4_flex
/// bounds the per-block size (4 MiB for a standard frame) and has no cumulative
/// limit and no knob, so a plain `read_to_end` on a `FrameDecoder` is a genuine
/// decompression-bomb vector.
#[cfg(feature = "compression")]
fn decode_lz4(payload: &[u8], want: usize, scratch: &mut Vec<u8>) -> Result<(), ChunkFault> {
    use std::io::Read;

    scratch.clear();
    // **Sized up front, because `read_to_end` would otherwise double its way there
    // and overshoot by up to 2×.** Measured before this line existed: a 4 MiB chunk
    // left `scratch` holding 8 388 608 bytes of capacity, a 1 MiB chunk 2 097 152 —
    // and since the buffer is deliberately never shrunk, that peak stayed resident
    // for the rest of the ingest. It contradicted the bound `decompress_into`
    // states ("the worst case is one chunk's ceiling") and the arithmetic an
    // operator does with `--max-chunk-size`, which is the number they size a
    // container against.
    //
    // `want + 1` and not `want`, because that is exactly the budget below: a
    // conforming frame fills `want`, and the one extra byte is what a *lying* one
    // needs room for so the over-run is detected rather than silently truncated.
    // Exact rather than amortised, since the size is known: this is one allocation
    // for the whole recording instead of ~20 realloc-and-copy rounds on the first
    // chunk of every pass.
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
        // `want + 1`, i.e. the budget was exhausted: the frame had more to give.
        _ => Err(ChunkFault::Bad(BadChunkKind::Overrun {
            codec: ChunkCodec::Lz4,
            declared: clamp_u32(want as u64),
        })),
    }
}

/// The fixed part of a chunk record's header.
///
/// Only the fields this module needs, parsed by hand so a truncated chunk is
/// still readable — see [`chunk_records`].
struct ChunkHead {
    /// `ChunkHeader::uncompressed_size`, at offset 16.
    ///
    /// It does three jobs, which is why an earlier revision that dropped it as
    /// "nothing reads this" was wrong twice over. On the compressed path it is the
    /// exact allocation size **and** the value [`ChunkLimits`] bounds. On the
    /// uncompressed path the records are stored verbatim, so
    /// `uncompressed_size == compressed_size` is an invariant checkable from two
    /// `u64`s nine bytes apart — and until it was checked, a chunk header rewritten
    /// by a bad sector passed with `bad_chunks == 0`.
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
        // A codec name is ASCII in practice; a non-UTF-8 one is not a codec this
        // build knows, which `ChunkCodec::Other` already says.
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
        // `usize::try_from` is load-bearing on a 32-bit host: `len` came off disk as
        // a `u64` and need not fit an address space at all.
        let len = match usize::try_from(len) {
            Ok(n) => n,
            Err(_) => return Err(framing(at)),
        };
        // **One comparison, and no checked arithmetic, because the invariant just
        // above makes both unnecessary.** `remaining >= HEADER` was checked, so
        // `remaining - HEADER` cannot underflow; and the sum below is bounded by
        // `at + remaining == records.len()`, so it cannot overflow either. An earlier
        // revision wrote `at.checked_add(HEADER).and_then(|h| h.checked_add(len))`,
        // whose first check could not fire — a dead branch in the loop that runs once
        // per record in the recording, which a later reader has to reason their way
        // out of before they can trust the walk.
        //
        // A body running past the end is the *other* face of truncation: the last
        // record in a cut chunk declares more than survived.
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
    ///
    /// Taken from [`crate::IngestOptions`] rather than written down, so a test can
    /// never assert against a bound the product does not use — the two numbers
    /// have exactly one definition and this is a read of it.
    fn limits() -> ChunkLimits {
        crate::IngestOptions::default().chunk_limits()
    }

    /// Assemble a chunk record body from its six header fields and a payload.
    ///
    /// Hand-built rather than routed through `crate::fixture`, because every test
    /// below needs a header that a *writer* would refuse to produce: a lying
    /// `uncompressed_size`, a `compressed_size` past the address space, a codec
    /// name over bytes that are not that codec.
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
            let err = chunk_records(&body, false, limits(), &mut scratch).unwrap_err();
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
        let partial =
            chunk_records(&body, false, limits(), &mut scratch).expect("a prefix must be readable");
        assert_eq!(
            partial.len(),
            body.len() - HEADER_BYTES,
            "the whole surviving records prefix must be handed over"
        );

        // The same bytes declared complete: now the size disagreement is real, and
        // it is a **compressed**-size one — `compressed_size` against the bytes the
        // record carries, which is a different fact from a decoder producing the
        // wrong number of bytes and now says so.
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

    /// **An uncompressed chunk whose two size fields disagree is refused**, on a
    /// complete chunk, without a decoder being involved at all.
    ///
    /// This is the check a previous revision left unwritten while pinning its own
    /// absence: the records are stored verbatim under `compression == ""`, so
    /// `uncompressed_size == compressed_size` is an invariant, and a header
    /// rewritten by a bad sector passed with no fault.
    ///
    /// Mutant: neutralise the `head.uncompressed_size != head.compressed_size` arm
    /// — applied, and this failed on the first `unwrap_err`, which returned `Ok`
    /// holding the whole 23-byte records field. It killed two other tests with it:
    /// `fixture::tests::each_damage_variant_produces_its_documented_fault` (the
    /// `CompressedSizeTooSmall` row) and
    /// `ingest::a_lying_uncompressed_size_is_refused` (`bad_chunks` 0 against 1).
    ///
    /// Mutant 3: report the disagreement as `LengthMismatch` again — applied, and
    /// this failed on `expected a StoredSizeMismatch`. That is the fault kind this
    /// check raised until the message was read as prose: "declared 87 uncompressed
    /// bytes and produced 23" over two header fields sends an operator to the
    /// decompressor, which this arm never reaches.
    ///
    /// Mutant 2: drop the `complete &&` term so the check also runs on a truncated
    /// chunk — applied, and the truncated case below failed ("a truncated chunk's
    /// size disagreement is truncation, not corruption"), which is a SIGKILLed
    /// recording's last chunk reported as damage. It was the **only** failure in
    /// the crate, which is the point: nothing else distinguishes the two.
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
                // **And it names two header fields rather than a decoder's
                // output.** The fault this used to raise rendered as "it declared N
                // uncompressed bytes and produced M", a sentence about a
                // decompressor; nothing on this path decompresses anything.
                let text = kind.to_string();
                assert!(
                    text.contains("stored uncompressed") && !text.contains("produced"),
                    "the message must not describe a decode that never happened: {text}"
                );
            }
            other => panic!("expected a StoredSizeMismatch, got {other:?}"),
        }

        // **The same bytes as a truncated chunk are not damage.** A recording cut
        // inside a chunk has a `compressed_size` describing bytes that were never
        // written, so the two fields disagree for a reason that is the file being
        // short rather than wrong.
        let mut scratch = Vec::new();
        assert!(
            chunk_records(&body, false, limits(), &mut scratch).is_ok(),
            "a truncated chunk's size disagreement is truncation, not corruption"
        );
    }

    /// **A declared `uncompressed_size` past the ceiling is refused before the
    /// buffer is sized**, and the assertion is on the allocation and not merely on
    /// the error.
    ///
    /// The ratio guard cannot catch this one: the payload is 1 MiB, so 1 GiB of
    /// declared output is within the 1024× ratio and only the absolute ceiling
    /// stands in the way. That separation is deliberate — each guard is tested
    /// where the other is silent.
    ///
    /// Mutant: neutralise the `declared > limits.max_uncompressed_bytes` arm —
    /// applied, and this failed on the capacity assertion, `scratch` having grown to
    /// **1 073 741 824 bytes** for a chunk carrying 1 MiB of nonsense; the fault then
    /// degraded to `Bad(Decompress { codec: zstd })`, so the second assertion would
    /// have failed too. The capacity check is deliberately first, because an error
    /// alone is also what a reader returns *after* allocating a gigabyte and failing
    /// to decode into it.
    ///
    /// Gated on `compression`: without a decoder, `is_built_in` refuses the codec
    /// before any of this is reached, so the guard is unreachable rather than
    /// untested there.
    #[cfg(feature = "compression")]
    #[test]
    fn a_lying_uncompressed_size_is_refused_before_it_allocates() {
        const GIB: u64 = 1024 * 1024 * 1024;
        let payload = vec![0x5Au8; 1024 * 1024];
        let body = chunk_body("zstd", GIB, 0, payload.len() as u64, &payload);

        let mut scratch = Vec::new();
        let err = chunk_records(&body, true, limits(), &mut scratch).unwrap_err();
        // **The point of the guard, asserted first and directly.** An error alone is
        // also what a reader returns after allocating a gigabyte and *then* failing
        // to decode into it, which is the failure this bound exists to prevent — so
        // the allocation is what this test is about and the fault kind is the
        // corroboration.
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

    /// **A chunk claiming to expand by more than the ratio allows is refused**,
    /// even though its declared size is comfortably under the absolute ceiling.
    ///
    /// 10 MiB from 100 bytes is the shape of the attack the ceiling cannot see: any
    /// ceiling loose enough for a real 8 MiB chunk admits it.
    ///
    /// Mutant: neutralise the ratio arm — applied, and this failed on the capacity
    /// assertion with `scratch` at **10 485 760 bytes**, the fault having degraded to
    /// `Bad(Decompress { codec: zstd })`. 10 MiB from 100 bytes is a modest bomb; the
    /// same shape at the 64 MiB ceiling is six orders of magnitude of amplification.
    ///
    /// Gated on `compression`: without a decoder, `is_built_in` refuses the codec
    /// before any of this is reached, so the guard is unreachable rather than
    /// untested there.
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

    /// **The ratio guard's own arithmetic survives a `compressed_size` chosen to
    /// overflow it.**
    ///
    /// `u64::MAX / 512` times the 1024× ratio does not fit in a `u64`. With
    /// `saturating_mul` the product pins at `u64::MAX`, the guard declines to fire,
    /// and the chunk is then refused a line later for declaring more compressed
    /// bytes than the record contains — which is the truthful complaint about this
    /// header, and is why the assertion names `CompressedSizeMismatch`. Reported as a
    /// `LengthMismatch`, as it was, the same fault printed "it declared 4294967295
    /// uncompressed bytes and produced 38" over two *compressed* figures, while the
    /// header's only uncompressed number (1 000) appeared nowhere.
    ///
    /// Mutant: `saturating_mul` → `*` — applied, and this test failed with
    /// `attempt to multiply with overflow` inside `chunk_records`. That is a panic
    /// in the reader reachable from a file, i.e. a denial of service, and in a
    /// release build without overflow checks the wrapped product decides the
    /// comparison instead of the real one.
    /// Gated on `compression`: without a decoder, `is_built_in` refuses the codec
    /// before any of this is reached, so the guard is unreachable rather than
    /// untested there.
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
    ///
    /// Mutant: neutralise the `payload.is_empty()` arm — applied, and this failed on
    /// `unwrap_err` with `Ok([])`: zstd returns `Ok(0)` for an empty input, `0 ==
    /// want` holds, and a chunk that declares a codec and carries nothing silently
    /// becomes "no transforms here".
    /// Gated on `compression`: without a decoder, `is_built_in` refuses the codec
    /// before any of this is reached, so the guard is unreachable rather than
    /// untested there.
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

    /// A truncated **compressed** chunk yields no records and **no fault**, so it
    /// is reported as truncation rather than as corruption.
    ///
    /// A partial codec frame is not decodable, so the chunk's records are lost
    /// either way; what this pins is that `bad_chunks` does not count it. Telling
    /// an operator their recording is damaged when it is merely incomplete sends
    /// them looking for a bad disk.
    ///
    /// Mutant: neutralise the `if !complete { return Ok(&[]) }` arm so the partial
    /// payload flows on — applied, and this failed with
    /// `Bad(LengthMismatch { declared: 900, produced: 5 })`, which
    /// `source::note_or_fail` counts as a bad chunk. It killed
    /// `ingest::a_truncated_compressed_recording_is_truncated_not_corrupt` with it
    /// (`bad_chunks` 1 against 0), which is the same claim measured end to end.
    /// Gated on `compression`: without a decoder, `is_built_in` refuses the codec
    /// before any of this is reached, so the guard is unreachable rather than
    /// untested there.
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

    /// Round-trip through each codec, and the exact fault on each side of the
    /// declared length.
    ///
    /// One test per property would need the same three-line encode five times; the
    /// cases here are the same encoded frame read against five different headers,
    /// which is what makes them comparable.
    ///
    /// # The CRC-0 rows are the ones that isolate the length check
    ///
    /// A chunk with a **computed** CRC is protected twice over: a short decode leaves
    /// the buffer's previous contents in the records field and a truncated read drops
    /// bytes from it, so
    /// the CRC over the whole field disagrees either way. The MCAP specification
    /// defines `uncompressed_crc == 0` as "not computed", real writers produce it, and
    /// `check_crc` therefore returns `Ok` unconditionally for it — so the under-run
    /// and over-run cases are each written **with** a CRC and **without** one, and the
    /// CRC-free variant comes first. Only there is the produced-against-declared
    /// comparison the sole witness, and only there does breaking it show up as data
    /// loss rather than as a differently-named fault.
    ///
    /// Mutant (zstd): `Ok(written) if written == want` → `Ok(_)` — applied, and the
    /// CRC-0 under-run row failed on `unwrap_err`, holding `Ok` with the records
    /// followed by 64 bytes of whatever `scratch` held — zeros here, because each row
    /// starts from a fresh `Vec`, and the previous chunk's records in a real ingest,
    /// since `decode_zstd` no longer `clear()`s. Zeros frame as empty records and
    /// stale bytes frame as garbage, so the walk complains about neither reliably:
    /// the chunk is silently short and no counter anywhere says so. Re-verified after
    /// the `clear()` removal — the mutant is still the only failure in the crate. Before the CRC-0 rows existed the same mutant surfaced as
    /// `Bad(Crc { saved: 440882894, calculated: 1134732146 })` instead — a kill, but
    /// of the CRC rather than of this check, which is why they were added.
    ///
    /// Mutant 2 (lz4): `take(budget)` → `take(want as u64)` — applied, and the CRC-0
    /// over-run row failed on `unwrap_err` with `Ok` holding a records field cut eight
    /// bytes short. `read_to_end` stops the instant the limit is hit, so
    /// `scratch.len() == want` and the over-run is invisible; with an honest CRC the
    /// same mutant surfaced as
    /// `Bad(Crc { saved: 440882894, calculated: 3376832092 })`.
    ///
    /// Mutant 3 (lz4): swap the frame decoder for `lz4_flex::block::decompress` —
    /// applied, and this failed on the round-trip case with
    /// `lz4 round trip: Bad(Decompress { codec: Lz4 })`. It took four other tests with
    /// it, including `ingest::an_lz4_recording_ingests_identically`, which failed with
    /// **`the recording contains no tf2_msgs/msg/TFMessage transforms`** — every chunk
    /// rejected, so an intact recording reads as empty. That is the silent-container
    /// bug the frame/block distinction exists to prevent, and it is why the crate
    /// `#[deprecated]`s its crate-root block re-exports.
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

    /// **A conformance vector for lz4, authored from the specification rather than
    /// produced by an encoder.**
    ///
    /// This is the lz4 half of the argument `testdata/ATTRIBUTION.md` makes for the
    /// zstd fixture: an encoder and a decoder *from the same crate* can agree with
    /// each other and both disagree with the liblz4 that rosbag2 and Foxglove link,
    /// so `encode_lz4` round-tripping proves the two halves of `lz4_flex` consistent
    /// and nothing more. zstd closes that with a frame from the real `zstd` CLI. There
    /// is no `lz4` CLI on this host, and installing one is not available to a test, so
    /// this closes it the other way: **the 82 bytes below were written by hand from
    /// the LZ4 frame and block formats**, and `lz4_flex` has to agree with the
    /// specification about what they mean.
    ///
    /// It is an independent vector rather than a round-trip in disguise, and the last
    /// assertion proves it: `lz4_flex`'s own encoder does **not** produce these bytes
    /// for this input. The sequences were chosen by hand, not searched for by a
    /// compressor, so that one small frame exercises every decoding rule that could
    /// plausibly be got wrong:
    ///
    /// * **A literal-length extension.** 29 literals, so the token's high nibble is
    ///   15 and a `14` continuation byte follows — the `255`-continuation scheme.
    /// * **A match-length extension.** A 20-byte match, so the low nibble is 15 and a
    ///   `1` continuation byte follows (`20 - 4 - 15`). The `- 4` is the format's
    ///   minimum match length and a decoder that forgets it is off by four.
    /// * **An overlapping match**, offset 1 and length 6: a run of `!`. This is the
    ///   rule a decoder breaks by copying the match with a wide `memcpy` instead of
    ///   byte by byte, which is *correct* for the non-overlapping match above and
    ///   wrong here.
    /// * **The frame's own framing**: the `0x184D2204` magic, an `FLG` byte declaring
    ///   block independence with both a content size and a content checksum, a `BD`
    ///   byte, the header checksum (`xxh32(descriptor) >> 8`), a block-size word whose
    ///   high bit is clear for "compressed", the `EndMark`, and the trailing xxh32 of
    ///   the content. `decode_lz4`'s `+ 1` read budget is what makes `lz4_flex` reach
    ///   the `EndMark` arm and check the last two of those at all, so this vector is
    ///   also the only test in which that checksum is ever verified.
    ///
    /// The frame decodes to one MCAP inner record, so it is asserted twice: through
    /// `decode_lz4` for the bytes, and through `chunk_records` — the real entry point,
    /// with a real CRC — for the whole path.
    ///
    /// The `xxh32` values were computed from that algorithm's specification and
    /// checked against its published vectors (`""` → `0x02cc5d05`, `"a"` →
    /// `0x550d7456`, `"abc"` → `0x32d153ff`) before this frame was assembled, so a bug
    /// in the checksum arithmetic could not have produced a frame that is
    /// self-consistently wrong.
    ///
    /// Every byte of the frame is load-bearing, which
    /// `a_single_flipped_bit_in_the_lz4_vector_is_caught` asserts exhaustively rather
    /// than by spot check — see it for the one nibble that is *correctly* insensitive.
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

    /// **The vector's non-vacuity, exhaustively: of the 656 single-bit
    /// perturbations of its 82 bytes, 651 are caught and the 5 that are not are
    /// enumerated and explained.**
    ///
    /// A hand-authored fixture is worth exactly what its assertions are worth, and
    /// "any byte flipped ⇒ the test fails" is a claim that can be *checked* instead of
    /// asserted in a comment. Every bit of every byte is flipped in turn, and the
    /// result must be a fault or content that differs from the expected 72 bytes —
    /// never a clean decode of the right ones.
    ///
    /// The survivors are asserted as an exact **set** rather than a count, because
    /// that is the form in which a change means something: a survivor appearing
    /// elsewhere is a region of the frame this vector does not really cover, and one
    /// of these two disappearing is `lz4_flex` becoming stricter. Either is worth a
    /// failure that names the byte.
    ///
    /// # Why those five bits are don't-cares
    ///
    /// * **Byte 60, bits 0–3.** The final sequence's token is `0xd0`: 13 literals in
    ///   the high nibble, `0` in the low one. The low nibble is the match length, and
    ///   a block's last sequence has no match — the decoder stops after its literals
    ///   and never reads an offset — so the format leaves those four bits unused.
    ///   Measured, the split is exactly that: all four low bits inert, all four high
    ///   bits (the literal count) lethal.
    /// * **Byte 77, bit 7.** The high bit of the `EndMark`, which turns the
    ///   all-zero word into `0x80000000` — the block-size field's "stored
    ///   uncompressed" flag over a length of zero. `lz4_flex` ends the frame on
    ///   `size & 0x7fff_ffff == 0`, so it reads that as the `EndMark`; the format
    ///   spells the mark as exactly four zero bytes. A leniency in the decoder, not
    ///   a hole in the vector, and it costs nothing here: the frame still ends where
    ///   it should and the content checksum after it is still verified.
    ///
    /// # What the failure modes say, measured
    ///
    /// Almost every caught flip is `Decompress`; the interesting ones are not:
    ///
    /// * Byte 53 (`0x42`, the second sequence's token) `^ 1` is the lone `Overrun`:
    ///   its match length goes from 6 to 7, so the frame produces 73 bytes for a
    ///   declared 72 and is caught by the one-byte-over read budget rather than by any
    ///   checksum.
    /// * Byte 49 (the `0x14` match offset) `^ 1` still produces exactly 72 bytes, so
    ///   no length check can see it. It is caught because `lz4_flex` verifies the
    ///   frame's xxh32 content checksum — which happens only because `decode_lz4`
    ///   reads one byte past `want` and so reaches the `EndMark` arm.
    /// * Bytes 78–81 (that checksum) are caught for the same reason. So this test is
    ///   a second, independent mutant for `decode_lz4`'s `take(budget)`: under
    ///   `take(want)` the `EndMark` is never reached and all five of those flips
    ///   decode clean.
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

    /// The 82 hand-authored bytes of
    /// `a_hand_authored_lz4_frame_decodes_per_the_specification`.
    ///
    /// Written out from the LZ4 frame and block formats; see that test for what each
    /// region is and why each was chosen. There is deliberately **no** recipe that
    /// regenerates this — a vector regenerated by a tool is a round-trip again, and a
    /// round-trip is what it exists to replace.
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

    /// **The output buffer is sized to the chunk, not doubled past it.**
    ///
    /// `scratch` is the caller's buffer for the whole recording and is deliberately
    /// never shrunk, so any overshoot is not transient — it stays resident for the
    /// rest of the ingest. That makes `Vec`'s amortised growth the wrong policy
    /// here: the final size is *known* (`want`, already checked against
    /// `ChunkLimits::max_uncompressed_bytes` two guards earlier), so doubling buys
    /// nothing and costs up to 2×.
    ///
    /// It is also what `--max-chunk-size` promises. `decompress_into`'s docs say
    /// "the worst case is one chunk's ceiling", and an operator sizing a container
    /// against that flag is doing arithmetic this test keeps true.
    ///
    /// Both codecs, because they reached the same defect by different routes and a
    /// fix to one is not a fix to the other: `read_to_end` doubles as it appends,
    /// and `resize` reserves its shortfall through the same doubling path.
    ///
    /// # The growth case is the one that matters
    ///
    /// Decoding into a *fresh* buffer was already exact, because `Vec`'s growth
    /// takes `max(2 * capacity, needed)` and `2 * 0` loses. The defect only appears
    /// on reuse, which is the only thing that ever happens in a real ingest — and
    /// measured before the fix, a 1 049 609-byte chunk following a 1 048 585-byte
    /// one took zstd's buffer to 2 097 170 bytes, and every lz4 chunk overshot by
    /// nearly 2× regardless of order (4 MiB of records leaving 8 388 608 bytes
    /// resident).
    ///
    /// Mutant: drop either `reserve_exact` — applied, and this fails on the codec
    /// whose line was removed, at the 1025 KiB step for zstd and at the first step
    /// for lz4.
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
                // **One byte of slack, and it is lz4's read budget rather than
                // rounding.** `decode_lz4` reserves `want + 1` so an over-long frame
                // has somewhere to land and is caught instead of being truncated;
                // `decode_zstd` needs no such byte and is asserted exact.
                let slack = if codec == ChunkCodec::Lz4 { 1 } else { 0 };
                assert_eq!(
                    scratch.capacity(),
                    want + slack,
                    "{codec} at {kib} KiB overshot: a buffer this reader never shrinks                      must not be doubled past the chunk it was sized for"
                );
            }
        }
    }

    /// A chunk labelled with a codec whose payload is not that codec fails to
    /// decode, and is a **skippable** bad chunk rather than an unsupported one.
    ///
    /// The distinction matters to the skip policy: an unsupported codec is never
    /// skippable, because every chunk in a file uses the same one. A chunk that
    /// claims zstd and carries something else is damage, and one damaged chunk must
    /// not cost the recording.
    ///
    /// Mutant: map every non-`TargetTooSmall` `Err` to `ChunkFault::Unsupported`, in
    /// both decoders — applied, and this failed with `zstd gave Unsupported(Zstd)`.
    /// It killed `ingest::a_mislabelled_codec_is_damage_not_an_unsupported_codec`
    /// (`the recording uses zstd-compressed chunks, which this build cannot read`)
    /// and the `Relabelled` row of
    /// `fixture::tests::each_damage_variant_produces_its_documented_fault` with it:
    /// one mislabelled chunk in a 400 000-chunk recording would take the whole file,
    /// and would blame a codec the build has.
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

    /// A decompressed chunk's CRC is checked against the **uncompressed** bytes,
    /// which is what the MCAP specification says the field covers.
    ///
    /// Neither codec crate verifies its own content checksum for us — ruzstd
    /// computes one and compares nothing, and lz4_flex's runs only at the frame's
    /// end mark — so this is the check that catches a payload corrupted in a way
    /// that still decodes.
    ///
    /// Mutant: drop the `check_crc` call on the compressed path — applied, and this
    /// failed on `unwrap_err`, which returned `Ok` holding the decoded records: a
    /// chunk whose contents disagree with the hash its writer committed to, handed
    /// over as sound. It was the **only** failure in the crate, because no fixture
    /// combines a compressed chunk with a wrong CRC — which is what this test is.
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

    /// With the `compression` feature off, both codecs report themselves
    /// unsupported and nothing tries to decode.
    ///
    /// Mutant: make `is_built_in` return `true` for `Zstd`/`Lz4` unconditionally,
    /// i.e. drop its `#[cfg]` — applied, and **the whole suite still passed**, in both
    /// feature configurations. That is not a gap being hidden: `decompress_into`'s
    /// fallback arm returns `ChunkFault::Unsupported` for any codec it has no decoder
    /// for, so the answer is identical by construction. The property is **structurally guarded** by that
    /// arm, which exists precisely so that a `cfg` mistake cannot become a wrong
    /// answer, and saying so is more useful than inventing a kill.
    ///
    /// Mutant 2, which does kill it: turn `chunk_records`'s early return into
    /// `ChunkFault::Bad(BadChunkKind::Decompress { codec })` — applied, and this
    /// failed with `Bad(Decompress { codec: zstd })` in place of `Unsupported(Zstd)`,
    /// taking `codec_free`'s own test with it
    /// (`got 6 of 9 transforms and 1 bad chunk(s)`). That mutant is the real hazard:
    /// a missing decoder reported as damage is *skippable*, so on a real recording
    /// every chunk would be skipped and the answer would be `NoTransforms` about an
    /// intact file.
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

    /// A hand-rolled zstd frame: one raw block, and a **chosen window descriptor**.
    ///
    /// Hand-built because no encoder will produce this. `ruzstd`'s declares 128 KiB
    /// and the `zstd` CLI's largest default-reachable declaration is 8 MiB, so the
    /// frame that reaches the allocation these tests are about has to be written by
    /// hand. Per the zstd specification: the magic, a frame-header descriptor of
    /// `0x00` (no content size, not single-segment, no checksum, no dictionary), then
    /// a `Window_Descriptor` with `Exponent` in bits 7..3 and `Mantissa` in 2..0 — so
    /// the window is `1 << (10 + exponent)` — then a three-byte block header (last
    /// block, raw type, size in bits 3..23) and the body verbatim.
    #[cfg(feature = "compression")]
    fn zstd_frame_with_window(exponent: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![0x28, 0xb5, 0x2f, 0xfd, 0x00, exponent << 3];
        let header = 1u32 | ((body.len() as u32) << 3);
        v.extend_from_slice(&header.to_le_bytes()[..3]);
        v.extend_from_slice(body);
        v
    }

    /// **A zstd frame demanding a decoding window larger than this chunk could
    /// possibly need is refused, and the two-frame row is the one that matters.**
    ///
    /// A window is the furthest back a match may reach, so one larger than the
    /// frame's whole output is unusable — but `ruzstd` allocates from the *declared*
    /// number, and on the second and later frames of a payload it does so **eagerly**
    /// (`FrameDecoderState::reset` → `DecodeBuffer::reset` →
    /// `RingBuffer::reserve(window_size)`; the first frame's `new` path lets the ring
    /// grow on demand). Neither [`ChunkLimits`] guard can see it: both bound
    /// `uncompressed_size`, and the window is a different field in a different
    /// header, so lowering `--max-chunk-size` did not help.
    ///
    /// **Measured, out of tree, before `window_ceiling` existed:** this exact 26-byte
    /// payload under an honest `uncompressed_size` of 8 and a correct CRC decoded to
    /// the right eight bytes and drove the allocator to a **134 226 570-byte peak** —
    /// 5 162 560× the input, and identical with `max_uncompressed_bytes` set to
    /// 1 MiB. With `set_max_window_size(8 MiB)` the same payload peaked at **8 841
    /// bytes** and returned
    /// `Err(WindowSizeTooBig { requested: 67108864, max: 8388608 })`.
    ///
    /// That measurement is out of tree and stays there: it needs a counting
    /// `#[global_allocator]`, which is an `unsafe impl` this crate's
    /// `#![forbid(unsafe_code)]` will not admit even under `cfg(test)`. What is
    /// asserted here is the fault — and ruzstd evaluates `check_window_size` *before*
    /// `DecoderScratch::new`/`reset` in both paths, so the fault firing is the
    /// allocation not happening.
    ///
    /// Mutant: drop `decoder.set_max_window_size(ceiling)` — applied, and this failed
    /// on the two-frame row with `left: Ok("bbbbaaaa")` against
    /// `right: Err(Bad(ImplausibleWindow { requested: 67108864, ceiling: 8388608 }))`.
    /// That `Ok` **is** the 134 226 570-byte allocation, served from twenty-six bytes
    /// and handed back as a correct answer. It killed
    /// `the_window_floor_admits_what_a_real_zstd_encoder_declares` with it (its 16 MiB
    /// row) and nothing else: 96 tests run, 94 passed, 2 failed — which is why the
    /// hazard needed a test written for it rather than an existing one to notice.
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

    /// **The window floor admits the largest window a real encoder declares, and
    /// nothing above it.**
    ///
    /// This is the other half of the bound, and the half that a bomb guard tuned by
    /// feel gets wrong: too low a floor rejects ordinary recordings, because a
    /// streaming encoder declares the window it *might* use and cannot know the source
    /// size. Measured on this host, piping bytes through `zstd -19` declares exactly
    /// 8 MiB whatever the input size, and every chunk of the committed
    /// `testdata/zstd_conformance.mcap` declares it for ~660 uncompressed bytes.
    ///
    /// Mutant: `window_ceiling` returning `want` with no floor — applied, and the
    /// first row failed with
    /// `an 8 MiB window must be accepted: Bad(ImplausibleWindow { requested: 8388608, ceiling: 4 })`.
    /// It took eight other tests with it, including
    /// `ingest::a_real_libzstd_recording_ingests` (the same claim against bytes libzstd
    /// actually produced), `fixture::tests::a_compressed_fixture_round_trips_through_the_reader`
    /// and `ingest::a_zstd_recording_ingests_identically` — i.e. a floor chosen too low
    /// refuses every compressed recording this crate can write or read.
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

    /// Compress with `ruzstd`'s own encoder, for a round-trip fixture.
    ///
    /// **Round-trip is not conformance**, and this function is why the repository
    /// also carries `testdata/zstd_conformance.mcap`, compressed by the real
    /// `zstd` CLI: a decoder and an encoder from the same crate can agree with
    /// each other and both disagree with libzstd.
    #[cfg(feature = "compression")]
    fn encode_zstd(bytes: &[u8]) -> Vec<u8> {
        ruzstd::encoding::compress_to_vec(bytes, ruzstd::encoding::CompressionLevel::Fastest)
    }

    /// Compress with `lz4_flex`'s **frame** encoder — the container MCAP's `"lz4"`
    /// names.
    ///
    /// **Round-trip is not conformance here either**, for the same reason
    /// `encode_zstd` says so. zstd answers it with a file from the real `zstd` CLI;
    /// there is no `lz4` CLI on this host, so lz4 answers it from the other end, with
    /// the hand-authored [`LZ4_SPEC_VECTOR`] that
    /// `a_hand_authored_lz4_frame_decodes_per_the_specification` reads. This function
    /// is what that test asserts it is *not*.
    #[cfg(feature = "compression")]
    fn encode_lz4(bytes: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut enc = lz4_flex::frame::FrameEncoder::new(Vec::new());
        enc.write_all(bytes).unwrap();
        enc.finish().unwrap()
    }
}
