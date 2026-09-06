//! The frozen `.tft` arena — `docs/PHASE5.md` §2.
//!
//! # The file *is* the arena
//!
//! Phase 1 invariant 2 says there are no pointers in the arena: every internal
//! reference is a `u32` offset from the base. So an arena is relocatable by
//! `memcpy`, and therefore writable to disk and mappable back **with no parsing,
//! no deserialization and no fixups**. §2.1 is NORMATIVE that the frozen read
//! path uses the identical `Plan::at` code as the online one; this module is
//! only about *obtaining* the base pointer, exactly as [`crate::mapped`] is.
//!
//! That is why [`FrozenArena`] implements [`Arena`] and exposes nothing else:
//! the layers above cannot tell which backend they have, which is what makes
//! "identical code path" a structural property rather than a promise.
//!
//! # SAFETY (module invariant)
//!
//! A [`FrozenArena`] owns one `mmap`ping of `len` bytes at `base`, established
//! from `file` at file offset `arena_off` and unmapped exactly once in [`Drop`].
//! For its whole lifetime:
//!
//! * `base` is non-null, page-aligned (hence 64-byte aligned), and addresses
//!   `len` **readable** bytes. The mapping is `PROT_READ`; see
//!   [`FrozenArena::base`] for why the trait still hands out a `*mut u8` and what
//!   stops anything storing through it.
//! * `len` is the `arena_size` recorded in the [`FrozenHeader`], which was
//!   checked against both the file's actual size and the [`ArenaHeader`]'s own
//!   `arena_size` before the mapping was accepted.
//! * All typed access goes through `tf_tree_core`'s protocols, the same argument
//!   [`crate::heap::HeapArena`] and [`crate::mapped::MappedArena`] make for
//!   `Send + Sync`.
//!
//! # `SIGBUS`, and why a file cannot be sealed
//!
//! [`crate::mapped`] refuses an unsealed segment because a peer could
//! `ftruncate` it under a reader and turn every subsequent load into `SIGBUS`.
//! A regular file has no seals, so that guarantee is **not available here** and
//! pretending otherwise would be worse than saying so: anyone who truncates a
//! `.tft` while it is mapped will fault its readers. The mitigation is the trust
//! model, not a mechanism — §2.4 states a frozen arena has no writers, and a
//! `.tft` is a cache you regenerate, not a live segment peers coordinate on.
//!
//! # What is deliberately *not* here
//!
//! No `MADV_DONTFORK`. [`crate::mapped`] sets it so a `fork` child does not
//! silently become an unregistered participant of a live segment. A frozen
//! arena has no participant table to join and no writer to race, and §2.2's
//! entire argument is sixteen dataloader workers sharing one set of clean pages
//! — several of which will be `fork`ed by the framework. Inheriting the mapping
//! is the feature.

use core::ptr::NonNull;

use alloc::vec;
use alloc::vec::Vec;

use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use rustix::mm::{madvise, mmap, munmap, Advice, MapFlags, ProtFlags};

use crate::check::{validate_arena_header, ShmError};
use crate::header::{ArenaHeader, FORMAT_VERSION};
use crate::heap::Arena;
use crate::layout::layout_hash;

/// First eight bytes of a `.tft` file (§2.3).
///
/// Deliberately *not* [`crate::header::TF_TREE_MAGIC`]: a `.tft` is a container
/// whose arena starts two megabytes in, so a tool that finds the arena magic at
/// offset 0 is looking at a raw arena image and must not be told it is a frozen
/// file.
pub const FROZEN_MAGIC: [u8; 8] = *b"TFTFROZ\0";

/// Size of the on-disk [`FrozenHeader`], and the offset the manifest may start
/// at.
pub const FROZEN_HEADER_SIZE: usize = 128;

/// Alignment of the arena image within the file (§2.3).
///
/// **Two megabytes, not one page.** A huge page can only back a mapping when the
/// virtual address and the file offset are congruent modulo 2 MiB, so a
/// page-aligned-but-not-2-MiB-aligned `arena_off` makes `MADV_HUGEPAGE`
/// unsatisfiable no matter what address the kernel picks. §2.3's arithmetic is
/// the reason it is worth the padding: a 115 MB index needs ~28 000 TLB entries
/// on 4 KiB pages and 55 on 2 MiB ones.
pub const ARENA_FILE_ALIGN: u64 = 2 * 1024 * 1024;

/// The `.tft` container header — `docs/PHASE5.md` §2.3, NORMATIVE.
///
/// # Amendment to §2.3
///
/// The section lists the fields but gives no total size and no reserved tail.
/// This lays them out in the stated order, which happens to be free of implicit
/// padding, and pins the header at **128 bytes with 8 reserved** — see
/// [`frozen_header_has_no_padding`](self#tests). Without a fixed size the
/// manifest offset would be whatever `size_of` happened to be for the build that
/// wrote the file, which is exactly the class of accident `layout_hash` exists
/// to catch in the arena and which nothing would catch here.
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct FrozenHeader {
    /// [`FROZEN_MAGIC`].
    pub magic: [u8; 8],
    /// [`FORMAT_VERSION`] of the arena image, 3 as of Phase 5.
    pub format_version: u32,
    /// [`crate::layout::layout_hash`] of the build that wrote the arena image.
    pub layout_hash: u32,
    /// Total size of the file, in bytes. Checked against the real size on open.
    pub file_size: u64,
    /// Byte offset of the CBOR manifest.
    pub manifest_off: u32,
    /// Length of the CBOR manifest, in bytes.
    pub manifest_len: u32,
    /// Byte offset of the arena image. A multiple of [`ARENA_FILE_ALIGN`].
    pub arena_off: u64,
    /// Size of the arena image, in bytes. Equals its `ArenaHeader::arena_size`.
    pub arena_size: u64,
    /// BLAKE3 of the source recording, or all-zero when frozen from a live
    /// arena, which has no recording to name.
    pub source_digest: [u8; 32],
    /// Wall-clock time the file was written. Provenance only — nothing reads it
    /// to make a decision, so a clock step cannot break an open.
    pub created_unix_ns: i64,
    /// The freezing tool's version string, NUL-padded.
    pub tool_version: [u8; 32],
    /// Reserved. Written zero, not checked on read — a future field here must
    /// be optional by construction, because an old reader will ignore it.
    pub _reserved: [u8; 8],
}

/// Why a `.tft` could not be written or opened.
///
/// `Copy` and `String`-free like every other error in this workspace
/// (`docs/PROJECT.md` §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrozenError {
    /// A read, write, `fstat` or `ftruncate` on the file failed.
    Io(rustix::io::Errno),
    /// `mmap` of the arena region failed.
    Map(rustix::io::Errno),
    /// The file ended before a structure this header promised.
    Truncated,
    /// The first eight bytes are not [`FROZEN_MAGIC`] — not a `.tft`.
    BadMagic,
    /// The arena image was written by a different `FORMAT_VERSION`.
    VersionMismatch {
        /// Version found in the file.
        found: u32,
        /// Version this build speaks.
        expected: u32,
    },
    /// The arena image's record layout differs from this build's, so every
    /// region offset in it would be reinterpreted.
    ///
    /// §2.4 is NORMATIVE that this is a hard error naming both values, and that
    /// the stated remedy is to **re-freeze**: a `.tft` is a cache, not an
    /// archive. `tf_tree doctor --explain-version` prints the same reasoning.
    LayoutMismatch {
        /// Hash found in the file.
        found: u32,
        /// Hash this build computes.
        expected: u32,
    },
    /// The header's own offsets do not describe a consistent file: the arena is
    /// not [`ARENA_FILE_ALIGN`]-aligned, a region runs past `file_size`, or the
    /// manifest overlaps the header or the arena.
    HeaderInconsistent,
    /// The file's real size disagrees with the header's `file_size`.
    SizeMismatch {
        /// Bytes the file actually has.
        actual: u64,
        /// Bytes the header claims.
        expected: u64,
    },
    /// The arena image mapped, but its [`ArenaHeader`] did not validate.
    ///
    /// The *same* checks a `memfd` attach makes — see the `check` module.
    Arena(ShmError),
}

impl From<ShmError> for FrozenError {
    fn from(e: ShmError) -> FrozenError {
        FrozenError::Arena(e)
    }
}

/// Bytes copied per pass when snapshotting a live arena. One 64 KiB buffer, not
/// one arena-sized one: freezing must not need a second copy of a 233 MB index
/// resident at once.
const SNAPSHOT_CHUNK: usize = 64 * 1024;

/// Write `arena`'s bytes, `manifest` and a [`FrozenHeader`] into `fd` as a
/// `.tft` (§2.3).
///
/// `fd` must refer to a regular file that this call may size and overwrite from
/// offset 0. The gap between the manifest and the 2 MiB-aligned arena is left
/// unwritten, so on any filesystem that supports sparse files it costs no
/// blocks.
///
/// # The container header is written **last**, and that is the whole crash story
///
/// The file cannot be short: `ftruncate` sizes it up front so the gap is a hole,
/// so "a crash leaves a short file that the `file_size` check catches" is not
/// available and never was. A `SIGKILL`, a panic or an `ENOSPC` part-way through
/// the arena copy leaves a **full-length** file whose header would validate and
/// whose arena tail is zeros — an `ArenaHeader` in the first chunk, a valid
/// `layout_hash`, and every edge past the copied prefix reading back as
/// "published, stamp 0, zero quaternion". Silently wrong data, not a refusal.
///
/// So the order here is: discard any previous content, size the file, write the
/// manifest, write the arena, flush, and only then `pwrite` the header at offset
/// 0. Until that last write lands, offset 0 holds the zeros `ftruncate` left and
/// [`FrozenArena::open`] refuses the file on [`FrozenError::BadMagic`]. The
/// header is 128 bytes inside one block, so it lands or it does not.
///
/// The leading `ftruncate(fd, 0)` is load-bearing, not tidiness: `fd` may be
/// re-freezing over a previous `.tft` of the same geometry, whose header is
/// byte-identical to this one's. Without the discard, that stale header would
/// certify a half-written body.
///
/// # The snapshot is not atomic, and `--from-live` cannot make it one
///
/// A live arena has publishers storing into it while this runs. There is no
/// point-in-time snapshot of another process's shared memory available to a
/// library — no `CLONE_VM` trick, no `process_vm_readv` consistency — so the
/// bytes written are a *smear*: individually consistent per read, not mutually
/// consistent across the file. What makes that tolerable rather than silent
/// corruption is the per-slot seqlock the sample buffers already carry: a slot
/// caught mid-publish reads back as `SlotContended` on the frozen side, the same
/// way it would have on the live one. Freeze a quiesced arena, or a bag-built
/// one (§3), when a clean index matters.
///
/// The copy goes through a chunk buffer rather than forming a `&[u8]` over the
/// arena. That avoids fabricating a shared reference over memory a peer is
/// concurrently storing into — but it does **not** make the read race-free:
/// `copy_nonoverlapping` is a non-atomic bulk load of the same bytes and is a
/// data race under the same model. The race is deliberate and unavoidable (there
/// is no point-in-time snapshot of another process's memory to take); what the
/// per-slot seqlock buys is that the *result* is interpretable — a slot caught
/// mid-publish reads back as `SlotContended` rather than as a plausible pose.
///
/// # Errors
///
/// [`FrozenError::Io`] for any failing syscall; [`FrozenError::HeaderInconsistent`]
/// if `manifest` is so large that the arena would not fit under `u64`.
pub fn write_frozen<A: Arena + ?Sized>(
    fd: BorrowedFd<'_>,
    arena: &A,
    manifest: &[u8],
    source_digest: [u8; 32],
    created_unix_ns: i64,
    tool_version: [u8; 32],
) -> Result<FrozenHeader, FrozenError> {
    let header = plan_header(
        arena.len() as u64,
        manifest.len() as u64,
        source_digest,
        created_unix_ns,
        tool_version,
    )?;
    write_body(fd, arena, manifest, &header)?;
    commit_header(fd, &header)?;
    Ok(header)
}

/// The [`FrozenHeader`] that describes a file with this arena and this manifest.
///
/// Pure arithmetic: it decides the geometry before a byte is written, so
/// [`write_body`] knows where the arena goes and [`commit_header`] has something
/// to publish at the end.
fn plan_header(
    arena_size: u64,
    manifest_len: u64,
    source_digest: [u8; 32],
    created_unix_ns: i64,
    tool_version: [u8; 32],
) -> Result<FrozenHeader, FrozenError> {
    let manifest_off = FROZEN_HEADER_SIZE as u64;
    let arena_off = manifest_off
        .checked_add(manifest_len)
        .map(|end| end.div_ceil(ARENA_FILE_ALIGN) * ARENA_FILE_ALIGN)
        .ok_or(FrozenError::HeaderInconsistent)?;
    let file_size = arena_off
        .checked_add(arena_size)
        .ok_or(FrozenError::HeaderInconsistent)?;

    Ok(FrozenHeader {
        magic: FROZEN_MAGIC,
        format_version: FORMAT_VERSION,
        layout_hash: layout_hash(),
        file_size,
        // Both fit: `manifest_off` is a constant and `arena_off` bounds
        // `manifest_len`. `try_from` rather than `as`, so a manifest that
        // somehow exceeded `u32` is a refusal and not a truncated offset.
        manifest_off: u32::try_from(manifest_off).map_err(|_| FrozenError::HeaderInconsistent)?,
        manifest_len: u32::try_from(manifest_len).map_err(|_| FrozenError::HeaderInconsistent)?,
        arena_off,
        arena_size,
        source_digest,
        created_unix_ns,
        tool_version,
        _reserved: [0; 8],
    })
}

/// Everything except the container header: the manifest, and the arena image.
///
/// Separate from [`commit_header`] so the ordering this module depends on is a
/// property of two calls in [`write_frozen`] rather than of a comment, and so a
/// test can produce the exact file a crash would leave.
fn write_body<A: Arena + ?Sized>(
    fd: BorrowedFd<'_>,
    arena: &A,
    manifest: &[u8],
    header: &FrozenHeader,
) -> Result<(), FrozenError> {
    // Discard first. See `write_frozen`'s docs: a previous `.tft` of the same
    // geometry would otherwise leave a *valid* header at offset 0 certifying the
    // body this call is only part-way through writing.
    rustix::fs::ftruncate(fd, 0).map_err(FrozenError::Io)?;
    // Then size the file up. Everything below is `pwrite` at an explicit offset,
    // so the file must already be long enough for the sparse gap to exist;
    // truncating afterwards would instead *discard* a short final write.
    rustix::fs::ftruncate(fd, header.file_size).map_err(FrozenError::Io)?;
    pwrite_all(fd, manifest, u64::from(header.manifest_off))?;

    let arena_off = header.arena_off;
    let mut buf = vec![0u8; SNAPSHOT_CHUNK];
    let mut done = 0usize;
    while done < arena.len() {
        let n = SNAPSHOT_CHUNK.min(arena.len() - done);
        // SAFETY: `arena` guarantees `base()` is valid for reads of `len()`
        // bytes for as long as `arena` is borrowed, and `done + n <= len()`.
        // `buf` is a distinct owned allocation of at least `n` bytes, so the
        // ranges cannot overlap. A concurrent publisher may be storing into the
        // source; see `write_frozen`'s docs for why that race is deliberate and
        // what makes its result interpretable.
        unsafe {
            core::ptr::copy_nonoverlapping(arena.base().add(done), buf.as_mut_ptr(), n);
        }
        pwrite_all(fd, &buf[..n], arena_off + done as u64)?;
        done += n;
    }
    Ok(())
}

/// Publish the container header at offset 0 — the file's commit point.
///
/// The `fdatasync` first is what extends the ordering guarantee from "this
/// process died" to "the machine lost power": without it the header block may
/// reach the platter before the arena blocks, and the file that comes back is
/// exactly the one [`write_frozen`]'s ordering exists to prevent. It costs one
/// flush per freeze, on a path that already wrote the whole arena.
fn commit_header(fd: BorrowedFd<'_>, header: &FrozenHeader) -> Result<(), FrozenError> {
    rustix::fs::fdatasync(fd).map_err(FrozenError::Io)?;
    pwrite_all(fd, bytemuck::bytes_of(header), 0)
}

/// `pwrite` until every byte of `buf` has landed at `off`.
///
/// A short write is not an error on a regular file and is not hypothetical
/// (signals, filesystem boundaries), so the loop is the correctness fix, not
/// belt-and-braces. A zero-length return with no error would spin, so it is
/// reported as the I/O failure it is.
fn pwrite_all(fd: BorrowedFd<'_>, mut buf: &[u8], mut off: u64) -> Result<(), FrozenError> {
    while !buf.is_empty() {
        match rustix::io::pwrite(fd, buf, off) {
            Ok(0) => return Err(FrozenError::Io(rustix::io::Errno::IO)),
            Ok(n) => {
                buf = &buf[n..];
                off += n as u64;
            }
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(FrozenError::Io(e)),
        }
    }
    Ok(())
}

/// `pread` until `buf` is full, or the file ends.
fn pread_exact(fd: BorrowedFd<'_>, buf: &mut [u8], mut off: u64) -> Result<(), FrozenError> {
    let mut filled = 0;
    while filled < buf.len() {
        match rustix::io::pread(fd, &mut buf[filled..], off) {
            Ok(0) => return Err(FrozenError::Truncated),
            Ok(n) => {
                filled += n;
                off += n as u64;
            }
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(FrozenError::Io(e)),
        }
    }
    Ok(())
}

/// An [`Arena`] backed by the arena image inside a `.tft` file, mapped
/// `PROT_READ` (§2.4).
pub struct FrozenArena {
    base: NonNull<u8>,
    len: usize,
    /// Kept open for [`FrozenArena::read_manifest`]. The mapping itself does not
    /// need it — `mmap` holds its own reference to the inode — but a `.tft`
    /// whose manifest could only be read before the arena was mapped would be an
    /// awkward API for no gain.
    file: OwnedFd,
    /// **Boxed, and the indirection is the point.** `FrozenHeader` is 128 bytes
    /// of cold provenance, and `FrozenArena` is a variant of the `ArenaBacking`
    /// enum inside `tf_tree::Tree` — the hottest struct in the workspace, sized
    /// by its largest variant. Stored inline it grew `size_of::<Tree>()` from
    /// 224 to 344 bytes, pushing `Tree` from four cache lines to six and
    /// charging that to the `Heap` and `Mapped` backings too, which never touch
    /// a `FrozenHeader`. Nothing after `open` reads it on any path that runs
    /// more than once per file. (`docs/PROJECT.md` §5 D4 forbids `Box` *inside
    /// an arena structure* — this is a process-local handle, not arena bytes.)
    header: alloc::boxed::Box<FrozenHeader>,
}

impl FrozenArena {
    /// Validate a `.tft` and map its arena image read-only (§2.4).
    ///
    /// `fd` must refer to a regular file opened for reading. The mapping is
    /// `MAP_PRIVATE | MAP_NORESERVE`: private on a read-only mapping still
    /// shares clean page cache across every process that opens the same file —
    /// which is §2.2's whole argument — and removes any possibility of
    /// accidental writeback.
    ///
    /// # Errors
    ///
    /// See [`FrozenError`]. In particular a `layout_hash` mismatch is refused
    /// here and not worked around: §2.4 is NORMATIVE that the file must be
    /// re-frozen.
    pub fn open(fd: OwnedFd) -> Result<FrozenArena, FrozenError> {
        let actual = rustix::fs::fstat(&fd).map_err(FrozenError::Io)?.st_size as u64;
        if actual < FROZEN_HEADER_SIZE as u64 {
            return Err(FrozenError::Truncated);
        }

        let mut raw = [0u8; FROZEN_HEADER_SIZE];
        pread_exact(fd.as_fd(), &mut raw, 0)?;
        // `pod_read_unaligned` rather than a cast: `raw` is a `[u8; 128]` on the
        // stack with no alignment guarantee, and `FrozenHeader`'s is 8.
        let header: FrozenHeader = bytemuck::pod_read_unaligned(&raw);

        // Identity, then vocabulary, then geometry, then self-consistency — the
        // same order `crate::check` uses, each narrowing what the next may
        // assume.
        if header.magic != FROZEN_MAGIC {
            return Err(FrozenError::BadMagic);
        }
        if header.format_version != FORMAT_VERSION {
            return Err(FrozenError::VersionMismatch {
                found: header.format_version,
                expected: FORMAT_VERSION,
            });
        }
        if header.layout_hash != layout_hash() {
            return Err(FrozenError::LayoutMismatch {
                found: header.layout_hash,
                expected: layout_hash(),
            });
        }
        if header.file_size != actual {
            return Err(FrozenError::SizeMismatch {
                actual,
                expected: header.file_size,
            });
        }
        check_extents(&header)?;

        let len =
            usize::try_from(header.arena_size).map_err(|_| FrozenError::HeaderInconsistent)?;
        // SAFETY: a null hint lets the kernel choose the address, so no existing
        // mapping can be replaced. `header.arena_off` is a multiple of 2 MiB
        // (hence of the page size, which `mmap` requires of an offset) and
        // `arena_off + arena_size <= file_size == actual` was established by
        // `check_extents` and the size check above, so the whole mapped range is
        // backed by the file and cannot fault with `SIGBUS`. Nullness of the
        // result is checked immediately.
        let raw_ptr = unsafe {
            mmap(
                core::ptr::null_mut(),
                len,
                ProtFlags::READ,
                // NORESERVE because a frozen index is mapped in full and touched
                // sparsely — a dataloader worker seeks to the timestamps it
                // needs — so charging commit for all of it against the overcommit
                // limit would price the mapping at its size rather than its
                // working set.
                MapFlags::PRIVATE | MapFlags::NORESERVE,
                &fd,
                header.arena_off,
            )
        }
        .map_err(FrozenError::Map)?;
        let base =
            NonNull::new(raw_ptr.cast::<u8>()).ok_or(FrozenError::Map(rustix::io::Errno::NOMEM))?;

        let arena = FrozenArena {
            base,
            len,
            file: fd,
            header: alloc::boxed::Box::new(header),
        };
        // Best effort, per §2.4: a kernel without transparent huge pages, or one
        // that declines to mark this mapping, is not a reason to fail an open.
        // SAFETY: module invariant — `base`/`len` describe this arena's own live
        // mapping, which is what `madvise` requires.
        let _ = unsafe { madvise(arena.base.as_ptr().cast(), arena.len, Advice::LinuxHugepage) };

        // Only now that the image is mapped can its `ArenaHeader` be read, and it
        // gets the *identical* checks a `memfd` attach makes. On failure the
        // `FrozenArena` is dropped, unmapping cleanly.
        validate_arena_header(arena.arena_header(), header.arena_size)?;
        Ok(arena)
    }

    /// The container header this file was opened with.
    #[must_use]
    pub fn frozen_header(&self) -> &FrozenHeader {
        &self.header
    }

    /// The CBOR manifest bytes (§2.3).
    ///
    /// Read from the file rather than mapped: it is cold by construction, and
    /// mapping it would put the manifest's pages in every dataloader worker's
    /// address space to serve a reader that runs once, in a tool.
    ///
    /// # Errors
    ///
    /// [`FrozenError::Io`] or [`FrozenError::Truncated`].
    pub fn read_manifest(&self) -> Result<Vec<u8>, FrozenError> {
        let mut out = vec![0u8; self.header.manifest_len as usize];
        if !out.is_empty() {
            pread_exact(
                self.file.as_fd(),
                &mut out,
                u64::from(self.header.manifest_off),
            )?;
        }
        Ok(out)
    }

    /// Borrow the [`ArenaHeader`] at the base of the mapped image.
    #[must_use]
    pub fn arena_header(&self) -> &ArenaHeader {
        // SAFETY: module invariant — the mapping is `arena_size` bytes, which
        // `check_extents` required to be at least `size_of::<ArenaHeader>()`,
        // and is page-aligned hence aligned for `ArenaHeader`'s `align(64)`.
        unsafe { &*self.base.as_ptr().cast::<ArenaHeader>() }
    }
}

/// Whether the header's own offsets describe a file that hangs together.
///
/// Called **before** any of them is handed to `mmap` or `pread`. Every branch
/// here is a way a hand-edited or truncated `.tft` could otherwise make this
/// process map or read something that is not there — **with one exception,
/// named because the sentence above certified it for a while and a reader
/// deciding whether a new field needs its own guard would have believed it.**
/// The `manifest_off + manifest_len` overflow arm is unreachable while both
/// fields are `u32`: `u64::from(u32::MAX) * 2` is 8 589 934 590, nine orders of
/// magnitude below `u64::MAX`. It is kept as a width guard for the day either
/// field is widened, the same way `plan_header`'s write side spends a
/// `u32::try_from` refusal rather than an `as`; replacing it with `+` would be a
/// silent wrap on that day. The `arena_off + arena_size` arm is *not* in that
/// class — both operands are `u64` and a hand-edited header reaches it.
fn check_extents(h: &FrozenHeader) -> Result<(), FrozenError> {
    if !h.arena_off.is_multiple_of(ARENA_FILE_ALIGN) {
        return Err(FrozenError::HeaderInconsistent);
    }
    // The manifest must live strictly between the container header and the
    // arena. Overlapping the arena would not corrupt anything — the mapping is
    // read-only — but it would mean the two structures disagree about what the
    // bytes are, and one of them is wrong.
    if u64::from(h.manifest_off) < FROZEN_HEADER_SIZE as u64 {
        return Err(FrozenError::HeaderInconsistent);
    }
    let manifest_end = u64::from(h.manifest_off)
        .checked_add(u64::from(h.manifest_len))
        .ok_or(FrozenError::HeaderInconsistent)?;
    if manifest_end > h.arena_off {
        return Err(FrozenError::HeaderInconsistent);
    }
    // An arena smaller than its own header cannot even be validated, and
    // `arena_header` would read past the mapping.
    if h.arena_size < core::mem::size_of::<ArenaHeader>() as u64 {
        return Err(FrozenError::Truncated);
    }
    let arena_end = h
        .arena_off
        .checked_add(h.arena_size)
        .ok_or(FrozenError::HeaderInconsistent)?;
    if arena_end > h.file_size {
        return Err(FrozenError::Truncated);
    }
    Ok(())
}

impl core::fmt::Debug for FrozenArena {
    /// Deliberately omits the base address: it is a fresh `mmap` result, so it
    /// differs on every run and would make any output containing a `.tft`
    /// unstable to diff.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FrozenArena")
            .field("len", &self.len)
            .field("arena_off", &self.header.arena_off)
            .field("layout_hash", &self.header.layout_hash)
            .finish_non_exhaustive()
    }
}

impl Drop for FrozenArena {
    fn drop(&mut self) {
        // No `getpid` guard, unlike `MappedArena`: that one exists because
        // `MADV_DONTFORK` leaves a *hole* in a child's address space, and this
        // mapping is deliberately inherited (see the module docs). A child that
        // drops its own `FrozenArena` is unmapping its own address space.
        //
        // SAFETY: module invariant — `base`/`len` are exactly what `mmap`
        // returned for this arena, unmapped here exactly once.
        let _ = unsafe { munmap(self.base.as_ptr().cast(), self.len) };
    }
}

// SAFETY: `FrozenArena` owns its mapping and exposes only the base pointer and
// length. It hands out no interior references that alias the bytes, and the
// bytes are immutable for the mapping's lifetime — nothing in this process can
// store through a `PROT_READ` mapping.
unsafe impl Send for FrozenArena {}
// SAFETY: see the `Send` impl above.
unsafe impl Sync for FrozenArena {}

// SAFETY: `base()`/`len()` describe one live mapping at a fixed page-aligned
// address, valid for reads of `len` bytes until `Drop`.
unsafe impl Arena for FrozenArena {
    /// # A `*mut u8` into a `PROT_READ` mapping
    ///
    /// [`Arena`]'s contract asks for a pointer valid for reads *and writes*, and
    /// this one is not writable: a store through it delivers `SIGSEGV`. That is
    /// the identical situation as a [`crate::mapped::MappedArena`] attached
    /// [`crate::mapped::AttachMode::ReadOnly`], which is the documented consumer
    /// default, and it is handled the same way — the `Tree` above consults
    /// `is_writable()` and refuses every mutating entry point before one is
    /// reached. A frozen arena is *permanently* read-only (§2.4: `AttachMode` is
    /// implicitly and permanently `ReadOnly`), so there is no mode in which that
    /// check can be skipped.
    fn base(&self) -> *mut u8 {
        self.base.as_ptr()
    }

    fn len(&self) -> usize {
        self.len
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::heap::HeapArena;
    use crate::layout::ArenaLayout;

    fn fixture() -> ArenaLayout {
        ArenaLayout::new(8, 4, vec![16, 0, 4, 64]).unwrap()
    }

    /// A scratch file that is unlinked immediately, so the test leaves nothing
    /// behind and needs no temp-file crate.
    fn scratch() -> OwnedFd {
        use rustix::fs::{Mode, OFlags};
        let mut name = alloc::string::String::from("/tmp/tf_tree_frozen_test_");
        // Pid *and* a counter. The pid alone is enough under nextest (a process
        // per test) but not under `cargo test`, which runs this module's tests
        // as threads of one process — two of them would then race on the same
        // path and the loser's `unlink` would fail.
        static N: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
        let pid = rustix::process::getpid().as_raw_nonzero().get();
        let n = N.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        name.push_str(&alloc::format!("{pid}_{n}"));
        let path = alloc::ffi::CString::new(name).unwrap();
        let fd = rustix::fs::open(
            &path,
            OFlags::CREATE | OFlags::TRUNC | OFlags::RDWR,
            Mode::RUSR | Mode::WUSR,
        )
        .unwrap();
        rustix::fs::unlink(&path).unwrap();
        fd
    }

    fn freeze(arena: &HeapArena, manifest: &[u8]) -> (OwnedFd, FrozenHeader) {
        let fd = scratch();
        let h = write_frozen(fd.as_fd(), arena, manifest, [9; 32], 1234, [b'v'; 32]).unwrap();
        (fd, h)
    }

    /// The header is a wire structure, so implicit padding in it would be
    /// uninitialised bytes written to disk and compared on read.
    ///
    /// `bytemuck::Pod`'s derive already rejects padding at compile time; this
    /// pins the *size*, which nothing else does. Mutant: change `_reserved` to
    /// `[u8; 16]` ⇒ fails.
    #[test]
    fn frozen_header_has_no_padding() {
        assert_eq!(core::mem::size_of::<FrozenHeader>(), FROZEN_HEADER_SIZE);
        assert_eq!(core::mem::align_of::<FrozenHeader>(), 8);
    }

    /// A `FrozenArena` is a variant of `tf_tree::Tree`'s backing enum, so its
    /// size is charged to every tree in the workspace, including heap ones.
    ///
    /// Four pointer-ish words is base + len + fd + boxed header. Mutant: store
    /// the `FrozenHeader` inline instead of boxing it ⇒ 152 bytes, and this
    /// fails. (The bound is what keeps `size_of::<Tree>()` at its pre-Phase-5
    /// 224; the `Tree`-side number is not asserted here because `tf_tree` is
    /// three crates up.)
    #[test]
    fn the_frozen_handle_stays_pointer_sized() {
        assert!(
            core::mem::size_of::<FrozenArena>() <= 4 * core::mem::size_of::<usize>(),
            "FrozenArena is {} bytes",
            core::mem::size_of::<FrozenArena>()
        );
    }

    /// A crash between the last arena byte and the container header must leave a
    /// file that will not open — not one that opens and serves zeros.
    ///
    /// `ftruncate` sizes the file up front, so a crash never leaves a *short*
    /// file: it leaves a full-length one with a zeroed tail. The only thing
    /// standing between that and a silently-wrong offline dataset is that the
    /// header is written last. This reproduces the exact bytes such a crash
    /// leaves by calling the same `write_body` that `write_frozen` calls, and
    /// then stopping.
    ///
    /// The scratch fd deliberately already holds a **complete, valid `.tft` of
    /// identical geometry** — the re-freeze-over-yesterday's-file case — so its
    /// header is byte-for-byte the header the interrupted write would have
    /// published. Mutant: drop the leading `ftruncate(fd, 0)` in `write_body` ⇒
    /// that stale header survives, the file opens, and the assertion fails; the
    /// second half of the test then shows it would have served the *old* arena's
    /// bytes under the new one's header.
    #[test]
    fn a_crash_before_the_header_lands_leaves_an_unopenable_file() {
        let layout = fixture();
        let mut first = HeapArena::new(&layout, 1, 1, [1; 16]);
        let mut second = HeapArena::new(&layout, 1, 1, [1; 16]);
        // Distinguishable bodies, or "it opened" could not be told from "it
        // opened the wrong file".
        scribble(&mut first, 0x11);
        scribble(&mut second, 0x22);
        assert_ne!(bytes(&first), bytes(&second), "fixture is degenerate");

        let fd = scratch();
        let manifest = b"\xa1\x64test\x01";
        let complete = write_frozen(fd.as_fd(), &first, manifest, [0; 32], 1, [0; 32]).unwrap();
        FrozenArena::open(dup(&fd)).expect("the complete file must open");

        // Now the crash: the body of a *second* freeze lands, the header does
        // not. Same geometry, so the bytes at offset 0 are the only difference
        // between this file and a good one.
        let planned = plan_header(
            second.len() as u64,
            manifest.len() as u64,
            [0; 32],
            1,
            [0; 32],
        )
        .unwrap();
        assert_eq!(planned.file_size, complete.file_size, "geometry differs");
        write_body(fd.as_fd(), &second, manifest, &planned).unwrap();

        assert_eq!(
            FrozenArena::open(dup(&fd)).unwrap_err(),
            FrozenError::BadMagic,
            "a body with no header must not open"
        );

        // And once the header is committed the same file is good — so the
        // refusal above is the ordering, not a file this test broke.
        commit_header(fd.as_fd(), &planned).unwrap();
        let opened = FrozenArena::open(fd).unwrap();
        // SAFETY: both arenas are live and `len` bytes each.
        let mapped =
            unsafe { core::slice::from_raw_parts(opened.base().cast_const(), opened.len()) };
        assert_eq!(mapped, bytes(&second));
    }

    /// Fill the pose region with a recognisable, non-zero pattern.
    ///
    /// `HeapArena::new` zeroes everything past the header, so two fresh arenas
    /// of the same layout are byte-identical and any "did the body change"
    /// assertion would hold vacuously.
    fn scribble(arena: &mut HeapArena, tag: u8) {
        let off = fixture().pose_arena().offset;
        // SAFETY: the pose region is non-empty for this fixture and `off + 64`
        // is inside it; the caller uniquely owns the allocation (`&mut`).
        unsafe {
            for i in 0..64u8 {
                *arena.base().add(off + i as usize) = i ^ tag;
            }
        }
    }

    fn bytes(arena: &HeapArena) -> &[u8] {
        // SAFETY: the arena is live and `len()` bytes long.
        unsafe { core::slice::from_raw_parts(arena.base().cast_const(), arena.len()) }
    }

    /// A second handle on the same open file description, so a test can attempt
    /// an `open` (which consumes the fd) and still keep writing to the file.
    fn dup(fd: &OwnedFd) -> OwnedFd {
        rustix::io::dup(fd).unwrap()
    }

    /// A `.tft` round-trips: the mapped image is byte-for-byte the arena that
    /// was frozen, and the manifest comes back unchanged.
    ///
    /// The fixture is deliberately not a fresh arena — `HeapArena::new` zeroes
    /// everything past the header, so a freeze that wrote only the header would
    /// still compare equal past it. Scribbling a recognisable pattern into the
    /// pose arena is what makes the body comparison mean something. Mutant: drop
    /// the `pwrite_all` of the arena chunk in `write_frozen` ⇒ fails on the body
    /// (and, without the scribble, would not).
    #[test]
    fn a_frozen_file_maps_back_to_the_same_bytes() {
        let layout = fixture();
        let arena = HeapArena::new(&layout, 11, 22, [3; 16]);
        let off = layout.pose_arena().offset;
        // SAFETY: `off + 64` is inside the arena (the pose region is non-empty
        // for this fixture), and this test uniquely owns the allocation.
        unsafe {
            for i in 0..64u8 {
                *arena.base().add(off + i as usize) = i ^ 0xA5;
            }
        }

        let manifest = b"\xa1\x64test\x01"; // CBOR {"test": 1}
        let (fd, written) = freeze(&arena, manifest);
        let frozen = FrozenArena::open(fd).unwrap();

        assert_eq!(frozen.len(), arena.len());
        assert_eq!(frozen.frozen_header().arena_off, written.arena_off);
        assert_eq!(frozen.read_manifest().unwrap(), manifest);
        // SAFETY: both arenas are live, `len` bytes each, and equal in length.
        let (a, b) = unsafe {
            (
                core::slice::from_raw_parts(arena.base(), arena.len()),
                core::slice::from_raw_parts(frozen.base(), frozen.len()),
            )
        };
        assert_eq!(a, b, "the frozen image is not the arena it came from");
    }

    /// The arena image must be 2 MiB aligned in the file, or `MADV_HUGEPAGE` is
    /// unsatisfiable no matter what address the kernel picks (§2.3).
    ///
    /// Mutant: round `arena_off` up to 4096 instead of `ARENA_FILE_ALIGN` ⇒
    /// fails, and the manifest here is far too short to reach 2 MiB by accident.
    /// ...and the 2 MiB it skips is a **hole**, not two megabytes of zeros.
    ///
    /// §2.3 pays for huge-page eligibility with padding, and the padding is only
    /// free because nothing writes it: `ftruncate` sizes the file and every
    /// subsequent write is a `pwrite` at an explicit offset. A 25 KB arena in a
    /// 2.1 MB file must therefore occupy well under 100 KB of blocks — the bound
    /// below is loose enough for any block size up to 64 KiB and still an order
    /// of magnitude under a filled gap. Mutant: in `write_body`, `pwrite_all` a
    /// `vec![0u8; (arena_off - manifest_end) as usize]` into the gap ⇒ `st_blocks`
    /// jumps to the full file size and this fails.
    #[test]
    fn the_arena_image_is_two_megabyte_aligned_and_the_gap_is_a_hole() {
        let arena = HeapArena::new(&fixture(), 0, 0, [0; 16]);
        let (fd, h) = freeze(&arena, b"x");
        assert_eq!(h.arena_off % ARENA_FILE_ALIGN, 0);
        assert!(h.arena_off > FROZEN_HEADER_SIZE as u64);

        let st = rustix::fs::fstat(&fd).unwrap();
        // `st_blocks` is in 512-byte units by POSIX, whatever the fs block size.
        let allocated = st.st_blocks as u64 * 512;
        assert!(
            allocated < h.file_size / 8,
            "{allocated} bytes allocated for a {} byte file: the gap is not sparse",
            h.file_size
        );
    }

    /// A `.tft` from a different build must be refused, not reinterpreted.
    ///
    /// §2.4 is NORMATIVE about this. Both mutations below are single-field
    /// edits to an otherwise perfectly good file, which is exactly the shape of
    /// the real failure: the same tool, rebuilt. Mutant: delete either check in
    /// `FrozenArena::open` ⇒ the corresponding case fails.
    #[test]
    fn a_stale_layout_or_version_is_refused_by_value() {
        let arena = HeapArena::new(&fixture(), 0, 0, [0; 16]);

        for (patch, want) in [
            (
                8usize, // format_version
                FrozenError::VersionMismatch {
                    found: FORMAT_VERSION ^ 0x5555,
                    expected: FORMAT_VERSION,
                },
            ),
            (
                12, // layout_hash
                FrozenError::LayoutMismatch {
                    found: layout_hash() ^ 0x5555,
                    expected: layout_hash(),
                },
            ),
        ] {
            let (fd, _) = freeze(&arena, b"");
            let mut word = [0u8; 4];
            pread_exact(fd.as_fd(), &mut word, patch as u64).unwrap();
            let scrambled = u32::from_le_bytes(word) ^ 0x5555;
            pwrite_all(fd.as_fd(), &scrambled.to_le_bytes(), patch as u64).unwrap();
            assert_eq!(FrozenArena::open(fd).unwrap_err(), want);
        }
    }

    /// Truncation is the failure mode a `.tft` actually meets — an interrupted
    /// copy, a full disk — and it must be an error rather than a `SIGBUS` from
    /// inside a lookup.
    ///
    /// A regular file has no seals (see the module docs), so `mmap` will happily
    /// map past the end and fault on touch. The size check is the only thing
    /// standing there. Mutant: delete the `file_size != actual` comparison in
    /// `open` ⇒ this fails (with `Truncated` from `check_extents`, which is a
    /// different error than the one asserted, so the assertion pins the check
    /// that actually ran).
    #[test]
    fn a_truncated_file_is_refused_before_it_is_mapped() {
        let arena = HeapArena::new(&fixture(), 0, 0, [0; 16]);
        let (fd, h) = freeze(&arena, b"");
        rustix::fs::ftruncate(&fd, h.file_size - 4096).unwrap();
        assert_eq!(
            FrozenArena::open(fd).unwrap_err(),
            FrozenError::SizeMismatch {
                actual: h.file_size - 4096,
                expected: h.file_size,
            }
        );
    }

    /// Anything that is not a `.tft` must be rejected on the magic, before a
    /// single offset in it is believed.
    ///
    /// The `0xEE` fill is chosen so that *every* subsequent field is garbage
    /// too: `arena_off` would be 0xEEEE… and the `mmap` that followed it would
    /// be nonsense. Mutant: delete the `header.magic != FROZEN_MAGIC` check ⇒
    /// the first case returns `VersionMismatch` instead and the assertion fails.
    /// The second case pins that the `< FROZEN_HEADER_SIZE` guard runs before
    /// the `pread`, which would otherwise short-read.
    #[test]
    fn a_foreign_file_is_not_a_tft() {
        let fd = scratch();
        pwrite_all(fd.as_fd(), &[0xEE; FROZEN_HEADER_SIZE + 32], 0).unwrap();
        assert_eq!(FrozenArena::open(fd).unwrap_err(), FrozenError::BadMagic);

        let short = scratch();
        pwrite_all(short.as_fd(), b"TFTFROZ\0", 0).unwrap();
        assert_eq!(
            FrozenArena::open(short).unwrap_err(),
            FrozenError::Truncated
        );
    }

    /// `check_extents` is the guard between a hand-edited header and an `mmap`
    /// of something that is not there, so its branches are exercised directly:
    /// every one of them is unreachable through `write_frozen`, which is why
    /// they would otherwise be untested.
    ///
    /// Mutant: drop the `manifest_end > arena_off` comparison ⇒ the overlapping
    /// case below passes and the assertion fails.
    ///
    /// Mutant: replace the `arena_off.checked_add(arena_size)` refusal with
    /// `wrapping_add` ⇒ `arena_end` wraps to a small number, the `> file_size`
    /// guard passes, and the header reaches `mmap` at an offset past EOF, where
    /// `validate_arena_header`'s deref is a `SIGBUS` rather than a typed error.
    /// The `wraps` case below is what kills it; before that case existed the
    /// mutant survived the whole suite.
    ///
    /// The *other* `checked_add`, on the manifest, is deliberately not covered:
    /// `check_extents`'s own doc records that it cannot be reached while both
    /// its operands are `u32`, and a test asserting an unreachable branch is a
    /// test that can never go red.
    #[test]
    fn check_extents_rejects_every_way_the_offsets_can_lie() {
        let good = FrozenHeader {
            magic: FROZEN_MAGIC,
            format_version: FORMAT_VERSION,
            layout_hash: layout_hash(),
            file_size: ARENA_FILE_ALIGN + 4096,
            manifest_off: FROZEN_HEADER_SIZE as u32,
            manifest_len: 16,
            arena_off: ARENA_FILE_ALIGN,
            arena_size: 4096,
            source_digest: [0; 32],
            created_unix_ns: 0,
            tool_version: [0; 32],
            _reserved: [0; 8],
        };
        assert_eq!(check_extents(&good), Ok(()));

        let mut misaligned = good;
        misaligned.arena_off = ARENA_FILE_ALIGN + 4096;
        misaligned.file_size = misaligned.arena_off + 4096;
        assert_eq!(
            check_extents(&misaligned),
            Err(FrozenError::HeaderInconsistent)
        );

        let mut in_header = good;
        in_header.manifest_off = 8;
        assert_eq!(
            check_extents(&in_header),
            Err(FrozenError::HeaderInconsistent)
        );

        let mut overlapping = good;
        overlapping.manifest_len = ARENA_FILE_ALIGN as u32;
        assert_eq!(
            check_extents(&overlapping),
            Err(FrozenError::HeaderInconsistent)
        );

        let mut tiny = good;
        tiny.arena_size = 64;
        assert_eq!(check_extents(&tiny), Err(FrozenError::Truncated));

        let mut past_end = good;
        past_end.arena_size = ARENA_FILE_ALIGN;
        assert_eq!(check_extents(&past_end), Err(FrozenError::Truncated));

        // `arena_off + arena_size` overflowing `u64`. The offset is
        // `2^64 - 2^21`, so it is still a multiple of `ARENA_FILE_ALIGN` and
        // clears the alignment branch; the sum is exactly `2^64`. Without the
        // `checked_add` this wraps to 0, which is not greater than any
        // `file_size`, so the header would reach `mmap`.
        //
        // The `is_multiple_of` line below **cannot fail while
        // `ARENA_FILE_ALIGN` is a power of two** — `2^64 − A` is a multiple of
        // any such `A` — and it is not the case's assertion. It guards the
        // constant: if `ARENA_FILE_ALIGN` ever stops being a power of two this
        // fixture starts failing the alignment branch instead of the overflow
        // one, and the `Err(HeaderInconsistent)` below would still be green for
        // the wrong reason.
        let mut wraps = good;
        wraps.arena_off = u64::MAX - ARENA_FILE_ALIGN + 1;
        wraps.arena_size = ARENA_FILE_ALIGN;
        wraps.file_size = u64::MAX;
        assert!(wraps.arena_off.is_multiple_of(ARENA_FILE_ALIGN));
        assert_eq!(check_extents(&wraps), Err(FrozenError::HeaderInconsistent));
    }
}
