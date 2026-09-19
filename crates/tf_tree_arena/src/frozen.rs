//! The frozen `.tft` arena — `docs/PHASE5.md` §2.
//!
//! # The file *is* the arena
//!
//! §2.1 (NORMATIVE): the frozen read path is the identical `Plan::at` code;
//! [`FrozenArena`] only obtains the base pointer, as [`crate::mapped`] does.
//!
//! # SAFETY (module invariant)
//!
//! A [`FrozenArena`] owns one `mmap`ping of `len` bytes at `base`, from `file`
//! at offset `arena_off`, unmapped exactly once in [`Drop`]. For its lifetime:
//!
//! * `base` is non-null, page-aligned (hence 64-byte aligned), and addresses
//!   `len` **readable** bytes (`PROT_READ`; see [`FrozenArena::base`]).
//! * `len` is the [`FrozenHeader`]'s `arena_size`, checked against the file's
//!   size and the [`ArenaHeader`]'s own before acceptance.
//! * Typed access goes through `tf_tree_core`'s protocols, as for the other backends.
//!
//! # `SIGBUS`
//!
//! A file has no seals, so truncating a mapped `.tft` faults its readers;
//! mitigated by §2.4's trust model (no writers, a regenerable cache).
//!
//! No `MADV_DONTFORK`: inheriting the mapping across `fork` is §2.2's feature.

use core::ptr::NonNull;

use alloc::vec;
use alloc::vec::Vec;

use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use rustix::mm::{madvise, mmap, munmap, Advice, MapFlags, ProtFlags};

use crate::check::{validate_arena_header, ShmError};
use crate::header::{ArenaHeader, FORMAT_VERSION};
use crate::heap::Arena;
use crate::layout::layout_hash;

/// First eight bytes of a `.tft` file (§2.3); deliberately not
/// [`crate::header::TF_TREE_MAGIC`], so a raw arena image is not mistaken for one.
pub const FROZEN_MAGIC: [u8; 8] = *b"TFTFROZ\0";

/// Size of the on-disk [`FrozenHeader`], and the offset the manifest may start at.
pub const FROZEN_HEADER_SIZE: usize = 128;

/// Alignment of the arena image within the file (§2.3): 2 MiB, not one page,
/// because a huge page needs congruent virtual address and file offset.
pub const ARENA_FILE_ALIGN: u64 = 2 * 1024 * 1024;

/// The `.tft` container header — `docs/PHASE5.md` §2.3, NORMATIVE: 128 bytes
/// with 8 reserved, pinned by [`frozen_header_has_no_padding`](self#tests).
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
    /// BLAKE3 of the source recording, or all-zero when frozen from a live arena.
    pub source_digest: [u8; 32],
    /// Wall-clock time the file was written. Provenance only.
    pub created_unix_ns: i64,
    /// The freezing tool's version string, NUL-padded.
    pub tool_version: [u8; 32],
    /// Reserved; written zero, not checked on read.
    pub _reserved: [u8; 8],
}

/// Why a `.tft` could not be written or opened. `Copy` and `String`-free
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
    /// The arena image's record layout differs from this build's: a hard error
    /// naming both values, remedy **re-freeze** (§2.4, NORMATIVE).
    LayoutMismatch {
        /// Hash found in the file.
        found: u32,
        /// Hash this build computes.
        expected: u32,
    },
    /// The header's offsets do not describe a consistent file (misaligned arena,
    /// a region past `file_size`, or an overlapping manifest).
    HeaderInconsistent,
    /// The file's real size disagrees with the header's `file_size`.
    SizeMismatch {
        /// Bytes the file actually has.
        actual: u64,
        /// Bytes the header claims.
        expected: u64,
    },
    /// The arena image mapped, but its [`ArenaHeader`] did not validate.
    Arena(ShmError),
}

impl From<ShmError> for FrozenError {
    fn from(e: ShmError) -> FrozenError {
        FrozenError::Arena(e)
    }
}

// `Display` and `core::error::Error` follow `docs/decisions/0059` decision 2.

/// **The text is a diagnostic, not a compatibility promise** (`docs/API.md`
/// R5); callers match on the discriminant. `source` returns `None`.
impl core::fmt::Display for FrozenError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            FrozenError::Io(e) => write!(
                f,
                "reading, writing or sizing the .tft failed with errno {} (Io)",
                e.raw_os_error()
            ),
            FrozenError::Map(e) => write!(
                f,
                "mapping the .tft's arena image failed with errno {} (Map)",
                e.raw_os_error()
            ),
            FrozenError::Truncated => write!(
                f,
                "the .tft ends before a structure its header promises (Truncated)"
            ),
            FrozenError::BadMagic => write!(f, "the file does not start with the .tft magic (BadMagic)"),
            FrozenError::VersionMismatch { found, expected } => write!(
                f,
                ".tft format version {found} is not this build's {expected} (VersionMismatch)"
            ),
            FrozenError::LayoutMismatch { found, expected } => write!(
                f,
                ".tft layout hash 0x{found:08X} is not this build's 0x{expected:08X}, so it must be re-frozen (LayoutMismatch)"
            ),
            FrozenError::HeaderInconsistent => write!(
                f,
                "the .tft header's offsets do not describe a consistent file (HeaderInconsistent)"
            ),
            FrozenError::SizeMismatch { actual, expected } => write!(
                f,
                ".tft is {actual} bytes but its header says {expected} (SizeMismatch)"
            ),
            FrozenError::Arena(inner) => write!(f, "the .tft must be re-frozen: {inner}"),
        }
    }
}

/// Lets a `FrozenError` leave a function through `?` into `Box<dyn Error>`;
/// `source` is `None`.
impl core::error::Error for FrozenError {}

/// Bytes copied per pass: one 64 KiB buffer, not a second copy of the index.
const SNAPSHOT_CHUNK: usize = 64 * 1024;

/// Write `arena`'s bytes, `manifest` and a [`FrozenHeader`] into `fd` as a
/// `.tft` (§2.3).
///
/// `fd` must refer to a regular file that this call may size and overwrite from
/// offset 0. The gap between the manifest and the 2 MiB-aligned arena is a hole.
///
/// # The container header is written **last**
///
/// A crash leaves a full-length file with a zeroed tail, which `file_size`
/// cannot catch. Order: `ftruncate(fd, 0)` (load-bearing: a stale same-geometry
/// header would certify a half-written body), size, write manifest and arena,
/// flush, then `pwrite` the header at offset 0; until then
/// [`FrozenArena::open`] refuses the file on [`FrozenError::BadMagic`].
///
/// # The snapshot is not atomic
///
/// A live arena is a *smear* (a slot caught mid-publish reads back
/// `SlotContended`); freeze a quiesced or bag-built (§3) arena. The chunked copy
/// is a deliberate data race under the memory model.
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

/// The [`FrozenHeader`] describing a file with this arena and manifest.
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
        // `try_from`, not `as`: an oversized manifest is a refusal.
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

/// Everything except the container header; separate from [`commit_header`] so a
/// test can produce the file a crash would leave.
fn write_body<A: Arena + ?Sized>(
    fd: BorrowedFd<'_>,
    arena: &A,
    manifest: &[u8],
    header: &FrozenHeader,
) -> Result<(), FrozenError> {
    // Discard first (see `write_frozen`), then size the file up.
    rustix::fs::ftruncate(fd, 0).map_err(FrozenError::Io)?;
    rustix::fs::ftruncate(fd, header.file_size).map_err(FrozenError::Io)?;
    pwrite_all(fd, manifest, u64::from(header.manifest_off))?;

    let arena_off = header.arena_off;
    let mut buf = vec![0u8; SNAPSHOT_CHUNK];
    let mut done = 0usize;
    while done < arena.len() {
        let n = SNAPSHOT_CHUNK.min(arena.len() - done);
        // SAFETY: `base()` is valid for `len()` bytes and `done + n <= len()`;
        // `buf` is a distinct allocation of at least `n` bytes. The race with a
        // publisher is deliberate; see `write_frozen`.
        unsafe {
            core::ptr::copy_nonoverlapping(arena.base().add(done), buf.as_mut_ptr(), n);
        }
        pwrite_all(fd, &buf[..n], arena_off + done as u64)?;
        done += n;
    }
    Ok(())
}

/// Publish the container header at offset 0, the commit point; the `fdatasync`
/// first extends the ordering to power loss.
fn commit_header(fd: BorrowedFd<'_>, header: &FrozenHeader) -> Result<(), FrozenError> {
    rustix::fs::fdatasync(fd).map_err(FrozenError::Io)?;
    pwrite_all(fd, bytemuck::bytes_of(header), 0)
}

/// `pwrite` until every byte of `buf` has landed at `off` (short writes are not
/// errors; a zero-length return is an I/O failure).
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
    /// Kept open for [`FrozenArena::read_manifest`].
    file: OwnedFd,
    /// Boxed: inline, this cold header grew `size_of::<Tree>()` from 224 to 344
    /// bytes. (D4 forbids `Box` inside an arena structure, not a process-local handle.)
    header: alloc::boxed::Box<FrozenHeader>,
}

impl FrozenArena {
    /// Validate a `.tft` and map its arena image read-only (§2.4).
    ///
    /// `fd` must refer to a regular file opened for reading. The mapping is
    /// `MAP_PRIVATE | MAP_NORESERVE`: clean page cache is shared across
    /// processes (§2.2), with no writeback.
    /// # Errors
    ///
    /// See [`FrozenError`]; in particular a `layout_hash` mismatch is refused
    /// here, not worked around.
    pub fn open(fd: OwnedFd) -> Result<FrozenArena, FrozenError> {
        let actual = rustix::fs::fstat(&fd).map_err(FrozenError::Io)?.st_size as u64;
        if actual < FROZEN_HEADER_SIZE as u64 {
            return Err(FrozenError::Truncated);
        }

        let mut raw = [0u8; FROZEN_HEADER_SIZE];
        pread_exact(fd.as_fd(), &mut raw, 0)?;
        // `pod_read_unaligned`: `raw` has no alignment guarantee.
        let header: FrozenHeader = bytemuck::pod_read_unaligned(&raw);

        // Order of `crate::check`.
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
        // SAFETY: a null hint lets the kernel choose the address; `arena_off` is
        // a multiple of 2 MiB (hence the page size) and `arena_off + arena_size
        // <= file_size == actual` by `check_extents` and the size check, so the
        // range is file-backed and cannot fault with `SIGBUS`.
        let raw_ptr = unsafe {
            mmap(
                core::ptr::null_mut(),
                len,
                ProtFlags::READ,
                // NORESERVE: mapped in full, touched sparsely.
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
        // Best effort (§2.4).
        // SAFETY: module invariant; `base`/`len` describe this live mapping.
        let _ = unsafe { madvise(arena.base.as_ptr().cast(), arena.len, Advice::LinuxHugepage) };

        // Only now can the `ArenaHeader` be read; on failure the drop unmaps.
        validate_arena_header(arena.arena_header(), header.arena_size)?;
        Ok(arena)
    }

    /// The container header this file was opened with.
    #[must_use]
    pub fn frozen_header(&self) -> &FrozenHeader {
        &self.header
    }

    /// The CBOR manifest bytes (§2.3), read from the file so its cold pages stay
    /// out of every worker.
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

/// Whether the header's own offsets describe a file that hangs together, checked
/// before any is handed to `mmap` or `pread`. The manifest `+` arm is a width
/// guard (unreachable while both fields are `u32`); the arena arm is reachable
/// from a hand-edited header.
fn check_extents(h: &FrozenHeader) -> Result<(), FrozenError> {
    if !h.arena_off.is_multiple_of(ARENA_FILE_ALIGN) {
        return Err(FrozenError::HeaderInconsistent);
    }
    // The manifest must live strictly between the container header and the arena.
    if u64::from(h.manifest_off) < FROZEN_HEADER_SIZE as u64 {
        return Err(FrozenError::HeaderInconsistent);
    }
    let manifest_end = u64::from(h.manifest_off)
        .checked_add(u64::from(h.manifest_len))
        .ok_or(FrozenError::HeaderInconsistent)?;
    if manifest_end > h.arena_off {
        return Err(FrozenError::HeaderInconsistent);
    }
    // An arena smaller than its own header cannot be validated.
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
    /// Omits the base address, which differs on every run.
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
        // No `getpid` guard, unlike `MappedArena`: the mapping is inherited across `fork`.
        //
        // SAFETY: module invariant — `base`/`len` are exactly what `mmap`
        // returned for this arena, unmapped here exactly once.
        let _ = unsafe { munmap(self.base.as_ptr().cast(), self.len) };
    }
}

// SAFETY: `FrozenArena` owns its mapping and exposes only the base pointer and
// length; the bytes are immutable (nothing can store through `PROT_READ`).
unsafe impl Send for FrozenArena {}
// SAFETY: see the `Send` impl above.
unsafe impl Sync for FrozenArena {}

// SAFETY: `base()`/`len()` describe one live mapping at a fixed page-aligned
// address, valid for reads of `len` bytes until `Drop`.
unsafe impl Arena for FrozenArena {
    /// # A `*mut u8` into a `PROT_READ` mapping
    ///
    /// A store through it delivers `SIGSEGV`; the `Tree` above consults
    /// `is_writable()` and refuses every mutating entry point (§2.4).
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

    /// A scratch file, unlinked immediately.
    fn scratch() -> OwnedFd {
        use rustix::fs::{Mode, OFlags};
        let mut name = alloc::string::String::from("/tmp/tf_tree_frozen_test_");
        // Pid and a counter: `cargo test` runs these as threads of one process.
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

    /// The header is a wire structure; `Pod` rejects padding, this pins the size.
    /// Mutant: `_reserved` → `[u8; 16]`.
    #[test]
    fn frozen_header_has_no_padding() {
        assert_eq!(core::mem::size_of::<FrozenHeader>(), FROZEN_HEADER_SIZE);
        assert_eq!(core::mem::align_of::<FrozenHeader>(), 8);
    }

    /// Pins the boxing on the `header` field. Mutant: store `FrozenHeader` inline.
    #[test]
    fn the_frozen_handle_stays_pointer_sized() {
        assert!(
            core::mem::size_of::<FrozenArena>() <= 4 * core::mem::size_of::<usize>(),
            "FrozenArena is {} bytes",
            core::mem::size_of::<FrozenArena>()
        );
    }

    /// A crash between the last arena byte and the container header must leave a
    /// file that will not open. Stops after `write_body`, so the scratch fd holds
    /// a complete `.tft` whose header is the one the interrupted write would
    /// publish. Mutant: drop the leading `ftruncate(fd, 0)` in `write_body`.
    #[test]
    fn a_crash_before_the_header_lands_leaves_an_unopenable_file() {
        let layout = fixture();
        let mut first = HeapArena::new(&layout, 1, 1, [1; 16]);
        let mut second = HeapArena::new(&layout, 1, 1, [1; 16]);
        // Distinguishable bodies, so "it opened" is not "it opened the wrong file".
        scribble(&mut first, 0x11);
        scribble(&mut second, 0x22);
        assert_ne!(bytes(&first), bytes(&second), "fixture is degenerate");

        let fd = scratch();
        let manifest = b"\xa1\x64test\x01";
        let complete = write_frozen(fd.as_fd(), &first, manifest, [0; 32], 1, [0; 32]).unwrap();
        FrozenArena::open(dup(&fd)).expect("the complete file must open");

        // The crash: the body of a *second* freeze lands, the header does not.
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

        // Once the header is committed the same file is good.
        commit_header(fd.as_fd(), &planned).unwrap();
        let opened = FrozenArena::open(fd).unwrap();
        // SAFETY: both arenas are live and `len` bytes each.
        let mapped =
            unsafe { core::slice::from_raw_parts(opened.base().cast_const(), opened.len()) };
        assert_eq!(mapped, bytes(&second));
    }

    /// Fill the pose region with a non-zero pattern, or a body comparison is vacuous.
    fn scribble(arena: &mut HeapArena, tag: u8) {
        let off = fixture().pose_arena().offset;
        // SAFETY: `off + 64` is inside the pose region; `&mut` owns the allocation.
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

    /// A second handle on the same file, since `open` consumes the fd.
    fn dup(fd: &OwnedFd) -> OwnedFd {
        rustix::io::dup(fd).unwrap()
    }

    /// A `.tft` round-trips: mapped image byte-identical, manifest unchanged.
    /// Mutant: drop the arena-chunk `pwrite_all` in `write_frozen`.
    #[test]
    fn a_frozen_file_maps_back_to_the_same_bytes() {
        let layout = fixture();
        let arena = HeapArena::new(&layout, 11, 22, [3; 16]);
        let off = layout.pose_arena().offset;
        // SAFETY: `off + 64` is inside the pose region; this test owns the allocation.
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

    /// The arena image is [`ARENA_FILE_ALIGN`]-aligned (§2.3) and the gap is a
    /// hole. Mutants: round `arena_off` up to 4096; `pwrite_all` zeros into the gap.
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

    /// A `.tft` from a different build is refused (§2.4, NORMATIVE). Mutant:
    /// delete either check in `FrozenArena::open`.
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

    /// Truncation is an error, not a `SIGBUS`. Mutant: delete the
    /// `file_size != actual` comparison (fails with `Truncated`).
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

    /// A non-`.tft` is rejected on the magic. Mutant: delete the magic check. The
    /// second case pins the `< FROZEN_HEADER_SIZE` guard.
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

    /// `check_extents` guards a hand-edited header; each branch is unreachable
    /// through `write_frozen`, so each is exercised directly. Mutants: drop
    /// `manifest_end > arena_off`; `checked_add` → `wrapping_add` on the arena.
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

        // `arena_off + arena_size` overflowing `u64`; the `is_multiple_of`
        // assertion keeps this failing the overflow branch, not alignment.
        let mut wraps = good;
        wraps.arena_off = u64::MAX - ARENA_FILE_ALIGN + 1;
        wraps.arena_size = ARENA_FILE_ALIGN;
        wraps.file_size = u64::MAX;
        assert!(wraps.arena_off.is_multiple_of(ARENA_FILE_ALIGN));
        assert_eq!(check_extents(&wraps), Err(FrozenError::HeaderInconsistent));
    }

    /// `docs/PHASE5.md` §2.4 requires the re-freeze statement
    /// (`docs/decisions/0059` decision 6).
    fn mentions_refreezing(shown: &str) -> bool {
        shown.contains("re-freez") || shown.contains("re-frozen")
    }

    /// `docs/decisions/0059` step 1(b) for `FrozenError`: every variant renders
    /// by decision 2's rules, and every `ShmError` in `FrozenError::Arena` must
    /// contain the payload's own `Display` and state re-freezing.
    ///
    /// Mutants: (M5) `Arena`'s arm → `{inner:?}`; (M6) drop `, so it must be
    /// re-frozen` from `LayoutMismatch`; (M9) `Arena`'s arm → `"{inner}; the .tft
    /// must be re-frozen"`.
    #[test]
    fn every_frozen_error_variant_renders_by_0059s_rules() {
        use crate::check::every_shm_error;
        use crate::render_test::{assert_structure, variant_name};
        use alloc::format;
        use alloc::string::ToString;

        fn index(e: &FrozenError) -> usize {
            match e {
                FrozenError::Io(_) => 0,
                FrozenError::Map(_) => 1,
                FrozenError::Truncated => 2,
                FrozenError::BadMagic => 3,
                FrozenError::VersionMismatch { .. } => 4,
                FrozenError::LayoutMismatch { .. } => 5,
                FrozenError::HeaderInconsistent => 6,
                FrozenError::SizeMismatch { .. } => 7,
                FrozenError::Arena(_) => 8,
            }
        }

        let errno = rustix::io::Errno::from_raw_os_error(4095);
        let e = || vec!["errno 4095".to_string()];
        let own = vec![
            (FrozenError::Io(errno), e()),
            (FrozenError::Map(errno), e()),
            (FrozenError::Truncated, vec![]),
            (FrozenError::BadMagic, vec![]),
            (
                FrozenError::VersionMismatch {
                    found: u32::MAX,
                    expected: u32::MAX,
                },
                vec![u32::MAX.to_string(), u32::MAX.to_string()],
            ),
            (
                FrozenError::LayoutMismatch {
                    found: u32::MAX,
                    expected: u32::MAX,
                },
                vec!["0xFFFFFFFF".to_string(), "0xFFFFFFFF".to_string()],
            ),
            (FrozenError::HeaderInconsistent, vec![]),
            (
                FrozenError::SizeMismatch {
                    actual: u64::MAX,
                    expected: u64::MAX,
                },
                vec![u64::MAX.to_string(), u64::MAX.to_string()],
            ),
        ];
        let nested: Vec<_> = every_shm_error()
            .into_iter()
            .map(|(inner, numbers)| (inner, FrozenError::Arena(inner), numbers))
            .collect();

        let mut hit: Vec<usize> = own
            .iter()
            .map(|(e, _)| e)
            .chain(nested.iter().map(|(_, e, _)| e))
            .map(index)
            .collect();
        hit.sort_unstable();
        hit.dedup();
        assert_eq!(
            hit,
            (0..9).collect::<Vec<_>>(),
            "the lists must hold one value of every FrozenError variant"
        );

        for (e, numbers) in &own {
            let debug = format!("{e:?}");
            let shown = format!("{e}");
            assert_structure(&shown, &debug, variant_name(&debug), numbers);
            if matches!(e, FrozenError::LayoutMismatch { .. }) {
                assert!(
                    mentions_refreezing(&shown),
                    "{debug} does not state that the file must be re-frozen: {shown:?}"
                );
            }
        }

        for (inner, e, numbers) in &nested {
            let debug = format!("{e:?}");
            let inner_debug = format!("{inner:?}");
            let shown = format!("{e}");
            // First: only this holds a unit payload.
            let inner_shown = format!("{inner}");
            assert!(
                shown.contains(&inner_shown),
                "{debug} does not contain its payload's Display {inner_shown:?}: {shown:?}"
            );
            // The innermost name is the key (decision 2(g)).
            assert_structure(&shown, &debug, variant_name(&inner_debug), numbers);
            assert!(
                mentions_refreezing(&shown),
                "{debug} does not state that the file must be re-frozen: {shown:?}"
            );
        }
    }
}
