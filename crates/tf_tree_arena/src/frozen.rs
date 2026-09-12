//! The frozen `.tft` arena — `docs/PHASE5.md` §2.
//!
//! Phase 1 invariant 2 bans pointers in the arena, so it is relocatable by
//! `memcpy` and mappable straight back from disk; §2.1 is NORMATIVE that the
//! frozen read path is the identical `Plan::at` code, and [`FrozenArena`]
//! exposes nothing but [`Arena`] so that stays structural. A regular file has
//! no seals, so truncating a mapped `.tft` `SIGBUS`es its readers; §2.4's trust
//! model is the mitigation — no writers, a regenerable cache. `MADV_DONTFORK`
//! is left off likewise: nothing to join, no writer to race, and §2.2's sixteen
//! `fork`ed dataloader workers *want* the pages.
//!
//! # SAFETY (module invariant)
//!
//! A [`FrozenArena`] owns one `mmap`ping of `len` bytes at `base` from `file`
//! at offset `arena_off`, unmapped exactly once in [`Drop`]. `base` is
//! non-null, page-aligned (hence 64-byte aligned), over `len` **readable**
//! `PROT_READ` bytes (see [`FrozenArena::base`] on the `*mut u8`); `len` is the
//! [`FrozenHeader`]'s `arena_size`, checked against the file's real size and
//! the [`ArenaHeader`]'s own before the map was accepted; and typed access goes
//! through `tf_tree_core`'s protocols, as in [`crate::heap::HeapArena`].

use core::ptr::NonNull;

use alloc::vec;
use alloc::vec::Vec;

use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use rustix::mm::{madvise, mmap, munmap, Advice, MapFlags, ProtFlags};

use crate::check::{validate_arena_header, ShmError};
use crate::header::{ArenaHeader, FORMAT_VERSION};
use crate::heap::Arena;
use crate::layout::layout_hash;

/// First eight bytes of a `.tft` file (§2.3). Not
/// [`crate::header::TF_TREE_MAGIC`]: the arena starts two megabytes in, so that
/// magic at offset 0 means a raw arena image, not a frozen file.
pub const FROZEN_MAGIC: [u8; 8] = *b"TFTFROZ\0";

/// Size of the on-disk [`FrozenHeader`], and where the manifest may start.
pub const FROZEN_HEADER_SIZE: usize = 128;

/// Alignment of the arena image within the file (§2.3): two megabytes, not one
/// page, because a huge page needs virtual address and file offset congruent
/// modulo 2 MiB. §2.3 — a 115 MB index costs ~28 000 TLB entries at 4 KiB, 55
/// at 2 MiB.
pub const ARENA_FILE_ALIGN: u64 = 2 * 1024 * 1024;

/// The `.tft` container header — `docs/PHASE5.md` §2.3, NORMATIVE. Amends
/// §2.3, which gives no total size: fields in the stated order (no implicit
/// padding), pinned at 128 bytes with 8 reserved by
/// [`frozen_header_has_no_padding`](self#tests), or the manifest offset would
/// be whatever `size_of` was for the writing build.
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
    /// BLAKE3 of the source recording, all-zero when frozen from a live arena.
    pub source_digest: [u8; 32],
    /// Wall-clock write time. Provenance only; no decision reads it.
    pub created_unix_ns: i64,
    /// The freezing tool's version string, NUL-padded.
    pub tool_version: [u8; 32],
    /// Reserved. Written zero, not checked — a future field here must be
    /// optional, because an old reader ignores it.
    pub _reserved: [u8; 8],
}

/// Why a `.tft` could not be written or opened. `Copy` and `String`-free like
/// every error in this workspace (`docs/PROJECT.md` §5).
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
    /// region offset would be reinterpreted. §2.4 is NORMATIVE: a hard error
    /// naming both values, remedied by a re-freeze — a `.tft` is a cache.
    LayoutMismatch {
        /// Hash found in the file.
        found: u32,
        /// Hash this build computes.
        expected: u32,
    },
    /// Inconsistent offsets: the arena is not [`ARENA_FILE_ALIGN`]-aligned, a
    /// region runs past `file_size`, or the manifest overlaps header or arena.
    HeaderInconsistent,
    /// The file's real size disagrees with the header's `file_size`.
    SizeMismatch {
        /// Bytes the file actually has.
        actual: u64,
        /// Bytes the header claims.
        expected: u64,
    },
    /// The arena image mapped, but its [`ArenaHeader`] did not validate — the
    /// *same* checks a `memfd` attach makes (see the `check` module).
    Arena(ShmError),
}

impl From<ShmError> for FrozenError {
    fn from(e: ShmError) -> FrozenError {
        FrozenError::Arena(e)
    }
}

/// Bytes copied per pass when snapshotting a live arena. One 64 KiB buffer, so
/// freezing never needs a second copy of a 233 MB index resident at once.
const SNAPSHOT_CHUNK: usize = 64 * 1024;

/// Write `arena`'s bytes, `manifest` and a [`FrozenHeader`] into `fd` as a
/// `.tft` (§2.3). `fd` must be a regular file this call may size and overwrite
/// from offset 0; the gap before the 2 MiB-aligned arena stays unwritten, and
/// so sparse.
///
/// The header is written **last**. `ftruncate` sizes the file up front, so a
/// crash leaves a full-length file whose header would validate and whose
/// uncopied tail reads as "published, stamp 0, zero quaternion". Hence discard,
/// size, manifest, arena, flush, then `pwrite` the 128-byte header (one block:
/// all or nothing) at offset 0; until then [`FrozenArena::open`] refuses on
/// [`FrozenError::BadMagic`]. The leading `ftruncate(fd, 0)` is load-bearing:
/// a same-geometry re-freeze would else leave a stale header over a torn body.
///
/// The snapshot is **not atomic** — nothing can snapshot another process's
/// shared memory point-in-time, and `--from-live` cannot change that. The chunk
/// buffer avoids fabricating a `&[u8]` over memory a peer is storing into but
/// still races it by design; the per-slot seqlock keeps the result
/// interpretable, a mid-publish slot reading back as `SlotContended`. Freeze a
/// quiesced or bag-built (§3) arena for a clean index.
///
/// # Errors
///
/// [`FrozenError::Io`] for a failing syscall; [`FrozenError::HeaderInconsistent`]
/// if `manifest` is so large the arena would not fit under `u64`.
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

/// The [`FrozenHeader`] for this arena and manifest: pure arithmetic, fixing
/// the geometry before a byte is written.
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
        // `try_from`, not `as`: an oversized manifest must refuse, not wrap.
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
/// Split from [`commit_header`] so [`write_frozen`]'s ordering is two calls,
/// not a comment, and a test can reproduce a crashed write exactly.
fn write_body<A: Arena + ?Sized>(
    fd: BorrowedFd<'_>,
    arena: &A,
    manifest: &[u8],
    header: &FrozenHeader,
) -> Result<(), FrozenError> {
    // Discard first: a previous `.tft` of the same geometry would otherwise
    // leave a *valid* header at offset 0 certifying this half-written body.
    rustix::fs::ftruncate(fd, 0).map_err(FrozenError::Io)?;
    // Then size up: everything below is `pwrite` at an explicit offset, and
    // truncating afterwards would discard a short final write instead.
    rustix::fs::ftruncate(fd, header.file_size).map_err(FrozenError::Io)?;
    pwrite_all(fd, manifest, u64::from(header.manifest_off))?;

    let arena_off = header.arena_off;
    let mut buf = vec![0u8; SNAPSHOT_CHUNK];
    let mut done = 0usize;
    while done < arena.len() {
        let n = SNAPSHOT_CHUNK.min(arena.len() - done);
        // SAFETY: `arena.base()` is valid for reads of `len()` while borrowed
        // and `done + n <= len()`; `buf` is a distinct owned allocation of at
        // least `n` bytes, so the ranges cannot overlap. A publisher may store
        // into the source concurrently — deliberate, see `write_frozen`.
        unsafe {
            core::ptr::copy_nonoverlapping(arena.base().add(done), buf.as_mut_ptr(), n);
        }
        pwrite_all(fd, &buf[..n], arena_off + done as u64)?;
        done += n;
    }
    Ok(())
}

/// Publish the container header at offset 0 — the file's commit point. The
/// `fdatasync` extends [`write_frozen`]'s ordering from "this process died" to
/// "the machine lost power": the header block could reach the platter first.
fn commit_header(fd: BorrowedFd<'_>, header: &FrozenHeader) -> Result<(), FrozenError> {
    rustix::fs::fdatasync(fd).map_err(FrozenError::Io)?;
    pwrite_all(fd, bytemuck::bytes_of(header), 0)
}

/// `pwrite` until every byte of `buf` has landed at `off`. Short writes are
/// real on a regular file (signals, filesystem boundaries), so the loop is a
/// correctness fix; a zero-length return would spin, hence EIO.
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

/// An [`Arena`] over a `.tft`'s arena image, mapped `PROT_READ` (§2.4).
pub struct FrozenArena {
    base: NonNull<u8>,
    len: usize,
    /// Kept open for [`FrozenArena::read_manifest`]; `mmap` holds its own ref.
    file: OwnedFd,
    /// Boxed deliberately: `FrozenArena` is a variant of `tf_tree::Tree`'s
    /// `ArenaBacking` enum, sized by its largest, and inline these 128 cold
    /// bytes took `size_of::<Tree>()` from 224 to 344 — four cache lines to
    /// six, charged to `Heap` and `Mapped` too. (D4 forbids `Box` *inside an
    /// arena structure*; this is a process-local handle.)
    header: alloc::boxed::Box<FrozenHeader>,
}

impl FrozenArena {
    /// Validate a `.tft` and map its arena image read-only (§2.4). `fd` must be
    /// a regular file opened for reading; `MAP_PRIVATE` still shares clean page
    /// cache across every opener (§2.2's argument) and rules out writeback.
    ///
    /// # Errors
    ///
    /// See [`FrozenError`]; a `layout_hash` mismatch is refused, §2.4 being
    /// NORMATIVE that the file be re-frozen rather than worked around.
    pub fn open(fd: OwnedFd) -> Result<FrozenArena, FrozenError> {
        let actual = rustix::fs::fstat(&fd).map_err(FrozenError::Io)?.st_size as u64;
        if actual < FROZEN_HEADER_SIZE as u64 {
            return Err(FrozenError::Truncated);
        }

        let mut raw = [0u8; FROZEN_HEADER_SIZE];
        pread_exact(fd.as_fd(), &mut raw, 0)?;
        // `pod_read_unaligned`: `raw` is a stack array, header alignment is 8.
        let header: FrozenHeader = bytemuck::pod_read_unaligned(&raw);

        // Identity, vocabulary, geometry, self-consistency: `crate::check`'s
        // order, each narrowing what the next may assume.
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
        // SAFETY: a null hint lets the kernel pick the address, so no mapping
        // is replaced; `arena_off` is a multiple of 2 MiB (hence of `mmap`'s
        // required page size) and `arena_off + arena_size <= file_size ==
        // actual` by `check_extents` and the size check above, so the range is
        // wholly file-backed and cannot `SIGBUS`. Null is checked below.
        let raw_ptr = unsafe {
            mmap(
                core::ptr::null_mut(),
                len,
                ProtFlags::READ,
                // NORESERVE: a frozen index is mapped whole but touched
                // sparsely, so commit would price its size, not its working set.
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
        // Best effort per §2.4: no huge pages is no reason to fail an open.
        // SAFETY: module invariant — `base`/`len` are this arena's live mapping.
        let _ = unsafe { madvise(arena.base.as_ptr().cast(), arena.len, Advice::LinuxHugepage) };

        // Only now can the `ArenaHeader` be read; it gets the *identical*
        // checks a `memfd` attach makes, and a failure drops/unmaps cleanly.
        validate_arena_header(arena.arena_header(), header.arena_size)?;
        Ok(arena)
    }

    /// The container header this file was opened with.
    #[must_use]
    pub fn frozen_header(&self) -> &FrozenHeader {
        &self.header
    }

    /// The CBOR manifest bytes (§2.3). Read, not mapped: cold, and mapping it
    /// would put its pages in every dataloader worker's space for a one-shot
    /// tool.
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

/// Whether the header's own offsets describe a file that hangs together,
/// checked **before** any reaches `mmap` or `pread`. Every branch is a way a
/// hand-edited `.tft` could make this process read what is not there, bar one:
/// the `manifest_off + manifest_len` overflow arm is unreachable while both are
/// `u32` (`u64::from(u32::MAX) * 2` is nine orders below `u64::MAX`), kept as a
/// width guard for the day either widens, where `+` would silently wrap. The
/// `arena_off + arena_size` arm *is* reachable — both are `u64`.
fn check_extents(h: &FrozenHeader) -> Result<(), FrozenError> {
    if !h.arena_off.is_multiple_of(ARENA_FILE_ALIGN) {
        return Err(FrozenError::HeaderInconsistent);
    }
    // Strictly between container header and arena: an overlap corrupts nothing
    // (read-only) but means the two structures disagree about the bytes.
    if u64::from(h.manifest_off) < FROZEN_HEADER_SIZE as u64 {
        return Err(FrozenError::HeaderInconsistent);
    }
    let manifest_end = u64::from(h.manifest_off)
        .checked_add(u64::from(h.manifest_len))
        .ok_or(FrozenError::HeaderInconsistent)?;
    if manifest_end > h.arena_off {
        return Err(FrozenError::HeaderInconsistent);
    }
    // An arena smaller than its own header cannot be validated, and
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
    /// Omits the base address: it differs every run and would not diff stably.
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
        // `MADV_DONTFORK` holes a child's address space, and here the mapping
        // is deliberately inherited, so a child unmaps its own.
        // SAFETY: module invariant — `base`/`len` are `mmap`'s own return,
        // unmapped exactly once.
        let _ = unsafe { munmap(self.base.as_ptr().cast(), self.len) };
    }
}

// SAFETY: `FrozenArena` owns its mapping and hands out no interior references
// aliasing the bytes, which stay immutable — nothing stores through `PROT_READ`.
unsafe impl Send for FrozenArena {}
// SAFETY: see the `Send` impl above.
unsafe impl Sync for FrozenArena {}

// SAFETY: `base()`/`len()` describe one live mapping at a fixed page-aligned
// address, valid for reads of `len` bytes until `Drop`.
unsafe impl Arena for FrozenArena {
    /// # A `*mut u8` into a `PROT_READ` mapping
    ///
    /// [`Arena`] asks for a pointer valid for reads *and* writes; a store here
    /// is `SIGSEGV`. As with a [`crate::mapped::MappedArena`] attached
    /// [`crate::mapped::AttachMode::ReadOnly`], the `Tree` above consults
    /// `is_writable()` first, and §2.4 makes this one *permanently* read-only.
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

    /// A scratch file unlinked immediately: no temp-file crate, no leftovers.
    fn scratch() -> OwnedFd {
        use rustix::fs::{Mode, OFlags};
        let mut name = alloc::string::String::from("/tmp/tf_tree_frozen_test_");
        // Pid *and* counter: the pid alone suffices under nextest (a process
        // per test) but not under `cargo test`, whose threads share one.
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

    /// The header is a wire structure: padding would be uninitialised bytes on
    /// disk. `Pod`'s derive rejects padding; this pins the *size* instead.
    /// Mutant: `_reserved: [u8; 16]` ⇒ fails.
    #[test]
    fn frozen_header_has_no_padding() {
        assert_eq!(core::mem::size_of::<FrozenHeader>(), FROZEN_HEADER_SIZE);
        assert_eq!(core::mem::align_of::<FrozenHeader>(), 8);
    }

    /// `FrozenArena` is a variant of `tf_tree::Tree`'s backing enum, so its
    /// size is charged to every tree; this bound keeps `size_of::<Tree>()` at
    /// its pre-Phase-5 224 (`tf_tree` is three crates up, so not asserted
    /// here). Mutant: inline the `FrozenHeader` ⇒ 152 bytes, fails.
    #[test]
    fn the_frozen_handle_stays_pointer_sized() {
        assert!(
            core::mem::size_of::<FrozenArena>() <= 4 * core::mem::size_of::<usize>(),
            "FrozenArena is {} bytes",
            core::mem::size_of::<FrozenArena>()
        );
    }

    /// A crash between the last arena byte and the container header must leave
    /// a file that will not open — not one that opens and serves zeros.
    /// `ftruncate` sizes the file up front, so the crash leaves a full-length
    /// file with a zeroed tail and only the header-last ordering stands between
    /// that and a silently-wrong dataset. The fd already holds a complete `.tft`
    /// of identical geometry (the re-freeze case), so its stale header is the
    /// one the interrupted write would publish. Mutant: drop `write_body`'s
    /// `ftruncate(fd, 0)` ⇒ it opens, this fails.
    #[test]
    fn a_crash_before_the_header_lands_leaves_an_unopenable_file() {
        let layout = fixture();
        let mut first = HeapArena::new(&layout, 1, 1, [1; 16]);
        let mut second = HeapArena::new(&layout, 1, 1, [1; 16]);
        // Distinguishable, or "it opened" hides "it opened the wrong file".
        scribble(&mut first, 0x11);
        scribble(&mut second, 0x22);
        assert_ne!(bytes(&first), bytes(&second), "fixture is degenerate");

        let fd = scratch();
        let manifest = b"\xa1\x64test\x01";
        let complete = write_frozen(fd.as_fd(), &first, manifest, [0; 32], 1, [0; 32]).unwrap();
        FrozenArena::open(dup(&fd)).expect("the complete file must open");

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

        // The same file opens once the header lands: the refusal is the order.
        commit_header(fd.as_fd(), &planned).unwrap();
        let opened = FrozenArena::open(fd).unwrap();
        // SAFETY: both arenas are live and `len` bytes each.
        let mapped =
            unsafe { core::slice::from_raw_parts(opened.base().cast_const(), opened.len()) };
        assert_eq!(mapped, bytes(&second));
    }

    /// Fill the pose region with a recognisable, non-zero pattern: fresh
    /// arenas of one layout are byte-identical (`HeapArena::new` zeroes past
    /// the header), so "did the body change" would hold vacuously.
    fn scribble(arena: &mut HeapArena, tag: u8) {
        let off = fixture().pose_arena().offset;
        // SAFETY: `off + 64` is inside this fixture's non-empty pose region;
        // `&mut` gives unique ownership.
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

    /// A second handle: `open` consumes the fd, the test still writes to it.
    fn dup(fd: &OwnedFd) -> OwnedFd {
        rustix::io::dup(fd).unwrap()
    }

    /// A `.tft` round-trips: the mapped image is byte-for-byte the arena
    /// frozen, and the manifest comes back unchanged. The scribbled pattern is
    /// what makes the body comparison mean anything — a header-only freeze
    /// would still compare equal past the zeroed header. Mutant: drop the arena
    /// chunk's `pwrite_all` ⇒ fails on the body (and, without it, would not).
    #[test]
    fn a_frozen_file_maps_back_to_the_same_bytes() {
        let layout = fixture();
        let arena = HeapArena::new(&layout, 11, 22, [3; 16]);
        let off = layout.pose_arena().offset;
        // SAFETY: `off + 64` is inside this fixture's non-empty pose region;
        // the test uniquely owns the allocation.
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

    /// The arena image must be 2 MiB aligned or `MADV_HUGEPAGE` is
    /// unsatisfiable whatever address the kernel picks (§2.3), and the skipped
    /// 2 MiB must stay a hole, so a 25 KB arena in a 2.1 MB file occupies well
    /// under 100 KB of blocks (loose for any block size to 64 KiB). Mutants:
    /// round `arena_off` to 4096 ⇒ fails, the manifest being far too short to
    /// reach 2 MiB; fill the gap ⇒ `st_blocks` jumps and this fails.
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

    /// A `.tft` from a different build must be refused, not reinterpreted
    /// (§2.4, NORMATIVE). Both mutations are single-field edits to an otherwise
    /// good file — the real failure's shape, the same tool rebuilt. Mutant:
    /// delete either check in `open` ⇒ that case fails.
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
    /// copy, a full disk — and must be an error, not a `SIGBUS` mid-lookup. A
    /// regular file has no seals (module docs), so `mmap` maps past the end and
    /// faults on touch; the size check is all that stands there. Mutant: delete
    /// `file_size != actual` ⇒ this fails with `Truncated` instead, so the
    /// assertion pins the check that actually ran.
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
    /// single offset in it is believed; the `0xEE` fill makes every later field
    /// garbage too. Mutant: delete the magic check ⇒ the first case returns
    /// `VersionMismatch` and fails. The second pins the `< FROZEN_HEADER_SIZE`
    /// guard as running before the `pread`.
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

    /// `check_extents` guards a hand-edited header against an `mmap` of what is
    /// not there, and every branch is unreachable through `write_frozen`, so
    /// they are exercised directly. Mutant: drop `manifest_end > arena_off` ⇒
    /// the overlapping case passes and this fails. Mutant: `wrapping_add` for
    /// the `arena_off` sum ⇒ `arena_end` wraps small, `> file_size` passes, and
    /// `mmap` gets an offset past EOF where `validate_arena_header` `SIGBUS`es;
    /// the `wraps` case kills it, and it survived the suite before. The
    /// manifest `checked_add` stays uncovered: unreachable.
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

        // `arena_off + arena_size` overflowing `u64`: offset `2^64 − 2^21`
        // clears the alignment branch and the sum is exactly `2^64`, wrapping
        // to 0 without `checked_add` and reaching `mmap`. The `is_multiple_of`
        // assert cannot fail while `ARENA_FILE_ALIGN` is a power of two; it
        // guards that, or this fixture would trip alignment instead and the
        // `Err` below be green for the wrong reason.
        let mut wraps = good;
        wraps.arena_off = u64::MAX - ARENA_FILE_ALIGN + 1;
        wraps.arena_size = ARENA_FILE_ALIGN;
        wraps.file_size = u64::MAX;
        assert!(wraps.arena_off.is_multiple_of(ARENA_FILE_ALIGN));
        assert_eq!(check_extents(&wraps), Err(FrozenError::HeaderInconsistent));
    }
}
