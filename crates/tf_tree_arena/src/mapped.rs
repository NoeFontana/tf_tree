//! `memfd`-backed shared-memory arena.
//!
//! `docs/PHASE2.md` §4 (NORMATIVE): the read path's diff against the heap backend is **zero lines**;
//! everything here is about *obtaining* the base pointer safely.
//!
//! # SAFETY (module invariant)
//!
//! A [`MappedArena`] owns one `mmap`ping of `len` bytes at `base`, made from `fd` and unmapped
//! once in [`Drop`]. For its whole lifetime:
//!
//! * `base` is non-null, page-aligned (hence 64-byte aligned), and addresses `len` readable bytes,
//!   writable as well when `writable` is true.
//! * `len` equals the segment size, which the seals make **immutable**, so the mapping cannot be
//!   truncated out from under a reader.
//! * All typed access goes through `tf_tree_core`'s atomic protocols, which is what makes
//!   `Send + Sync` sound, as for [`crate::heap::HeapArena`].
//!
//! # Why `memfd` and not `shm_open`
//!
//! After [`MappedArena::create`] applies `F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_SEAL` the size is
//! immutable, so **`SIGBUS` is structurally impossible**. `shm_open` segments cannot be sealed
//! (`docs/PHASE2.md` §3.2). [`MappedArena::attach`] verifies the seals and refuses an unsealed segment.
//!
//! # Trust model
//!
//! `docs/PHASE2.md` §0: participants are mutually trusting, same-user processes. The **read-only**
//! attach mode is the one real boundary, enforced by the MMU.

use core::ptr::NonNull;

use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use rustix::fs::{
    fcntl_add_seals, fcntl_get_seals, ftruncate, memfd_create, MemfdFlags, SealFlags,
};
use rustix::mm::{madvise, mmap, munmap, Advice, MapFlags, ProtFlags};

use crate::check::{validate_arena_header, ShmError};
use crate::header::{ArenaHeader, TOPO_BLOCKS};
use crate::heap::{write_header_at, Arena};
use crate::layout::ArenaLayout;

/// Seals every tf_tree segment carries. Checked, not assumed, on attach.
const REQUIRED_SEALS: SealFlags = SealFlags::SHRINK.union(SealFlags::GROW);

/// How a process attaches to an existing segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachMode {
    /// `PROT_READ` only. The consumer default; the MMU enforces it.
    ReadOnly,
    /// `PROT_READ | PROT_WRITE`. Required to publish samples or claim edges.
    ReadWrite,
}

/// An [`Arena`] backed by a sealed `memfd` mapped `MAP_SHARED`; the stack above is written against [`Arena`].
pub struct MappedArena {
    base: NonNull<u8>,
    len: usize,
    fd: OwnedFd,
    writable: bool,
    /// The process that established this mapping. It is `MADV_DONTFORK` (§7.3), so a `fork` child has a
    /// hole there; `Drop` skips `munmap` unless this pid matches. `getpid` is affordable once per
    /// teardown; the hot-path equivalent is `tf_tree_ipc::fork`'s counter.
    ///
    /// No test fails when this check is removed (`munmap` of a hole is a no-op); kept because the
    /// failure it prevents is silent.
    owner_pid: rustix::process::Pid,
}

impl MappedArena {
    /// Create a new sealed segment sized for `layout` and write its header (`docs/PHASE2.md` §3.2).
    ///
    /// Sealing happens **after** mapping so the creator keeps write access: `SHRINK|GROW` succeed with
    /// a writable mapping held, where `F_SEAL_WRITE` would return `EBUSY`.
    ///
    /// # Errors
    ///
    /// Any of the syscalls in the sequence failing; see [`ShmError`].
    ///
    /// # Panics
    ///
    /// Asserts the host is little-endian (load-bearing invariant 7).
    pub fn create(
        name: &str,
        layout: &ArenaLayout,
        creator_pid: u32,
        owner_start_time: u64,
        boot_id: [u8; 16],
    ) -> Result<MappedArena, ShmError> {
        const {
            assert!(
                cfg!(target_endian = "little"),
                "tf_tree arenas are little-endian only"
            );
        }
        let len = layout.total_size();

        // `ALLOW_SEALING` is required for sealing; `CLOEXEC` so a segment never leaks into a child.
        let cname = CName::new(name);
        let fd = memfd_create(
            cname.as_cstr(),
            MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
        )
        .map_err(ShmError::Create)?;
        ftruncate(&fd, len as u64).map_err(ShmError::Truncate)?;

        // No `MAP_POPULATE`; see `unsafe_map`.
        let base = unsafe_map(len, ProtFlags::READ | ProtFlags::WRITE, &fd)?;

        // Take ownership before the first fallible step: an early `?` with no `Drop` would strand the
        // mapping and its committed pages.
        let arena = MappedArena {
            owner_pid: rustix::process::getpid(),
            base,
            len,
            fd,
            writable: true,
        };

        let uuid = instance_uuid()?;
        // SAFETY: `arena.base` addresses `len` freshly zeroed, page-aligned bytes and no other mapping of
        // this fd exists, so this call uniquely owns the region.
        unsafe {
            write_header_at(
                arena.base.as_ptr(),
                len,
                layout,
                creator_pid,
                owner_start_time,
                boot_id,
                uuid,
            )
        };

        // Step 5. SEAL blocks any later seal, so a peer cannot add F_SEAL_WRITE and freeze the writer out.
        fcntl_add_seals(&arena.fd, REQUIRED_SEALS | SealFlags::SEAL).map_err(ShmError::Seal)?;

        arena.advise();
        Ok(arena)
    }

    /// Map an existing segment from a received fd, validating it first (`docs/PHASE2.md` §3.3 steps 4-6).
    ///
    /// # Errors
    ///
    /// [`ShmError::Unsealed`] if the segment could be truncated under us;
    /// [`ShmError::BadMagic`], [`ShmError::VersionMismatch`],
    /// [`ShmError::LayoutMismatch`] or [`ShmError::SizeMismatch`] if it is not a
    /// segment this build can read.
    pub fn attach(fd: OwnedFd, mode: AttachMode) -> Result<MappedArena, ShmError> {
        // Refuse an unsealed segment before mapping: a later truncation would SIGBUS every read.
        let seals = fcntl_get_seals(&fd).map_err(ShmError::SealQuery)?;
        if !seals.contains(REQUIRED_SEALS) {
            return Err(ShmError::Unsealed);
        }

        let size = rustix::fs::fstat(&fd).map_err(ShmError::Stat)?.st_size as u64;
        if (size as usize) < core::mem::size_of::<ArenaHeader>() {
            return Err(ShmError::TooSmall);
        }
        let len = size as usize;

        let prot = match mode {
            AttachMode::ReadOnly => ProtFlags::READ,
            AttachMode::ReadWrite => ProtFlags::READ | ProtFlags::WRITE,
        };
        let base = unsafe_map(len, prot, &fd)?;

        let arena = MappedArena {
            owner_pid: rustix::process::getpid(),
            base,
            len,
            fd,
            writable: mode == AttachMode::ReadWrite,
        };
        arena.advise();

        // Validate only now; on failure the drop unmaps. The checks live in `crate::check`, shared with
        // the frozen-file backend.
        validate_arena_header(arena.header(), size)?;
        Ok(arena)
    }

    /// Fault in `[offset, offset + len)` of this arena, up front.
    ///
    /// Not `MADV_WILLNEED`, which does nothing on a memfd (`docs/PHASE2.md` §7.1): this uses
    /// `MADV_POPULATE_*`, read or write following the mapping's protection (`POPULATE_WRITE` on a
    /// `PROT_READ` mapping is `EINVAL`).
    ///
    /// # Errors
    ///
    /// Never. Kernels before 5.14 (`EINVAL`) fall back to touching pages by hand; any other errno
    /// leaves pages cold, which is slower, never incorrect.
    pub fn populate(&self, offset: usize, len: usize) {
        if len == 0 || offset >= self.len {
            return;
        }
        let len = len.min(self.len - offset);
        let advice = if self.writable {
            Advice::LinuxPopulateWrite
        } else {
            Advice::LinuxPopulateRead
        };
        // SAFETY: module invariant — `base` addresses `self.len` bytes of this
        // arena's own live mapping, and `offset + len` is clamped to it above,
        // so the range passed is inside that mapping.
        let r = unsafe { madvise(self.base.as_ptr().add(offset).cast(), len, advice) };
        if r == Err(rustix::io::Errno::INVAL) {
            self.populate_by_touch(offset, len);
        }
    }

    /// Kernels before 5.14: touch one byte per page. A **read**, never a write: storing into a live
    /// claim record or sample slot would race every reader of it.
    fn populate_by_touch(&self, offset: usize, len: usize) {
        for at in touch_offsets(offset, len) {
            // SAFETY: `at` is within `[offset, offset + len)` (see `touch_offsets`) and the caller clamped
            // that range to the mapping; `read_volatile` keeps the load, whose fault is its purpose.
            unsafe {
                core::ptr::read_volatile(self.base.as_ptr().add(at));
            }
        }
    }

    /// The first 256 bytes of the arena, for tests that assert nothing wrote to it.
    #[cfg(test)]
    fn header_snapshot(&self) -> [u8; 256] {
        let mut out = [0u8; 256];
        // SAFETY: module invariant; the mapping is at least `size_of::<ArenaHeader>()` (>= 256) bytes and
        // this only reads. The snapshot covers the pinned header prefix.
        unsafe { core::ptr::copy_nonoverlapping(self.base.as_ptr(), out.as_mut_ptr(), 256) };
        out
    }

    /// Populate every region that is actually read, and nothing else (§7.1).
    ///
    /// An **attaching** process derives the used extents from `frame_count` and `edge_count` in the
    /// header (`0004` moved declaration to build time), with nothing passed in.
    ///
    /// | region | populated |
    /// |---|---|
    /// | header | all (320 B) |
    /// | frame table | `frame_count` records |
    /// | frame hash | **none** — probed by hash, so it is scattered; interning is not the hot path |
    /// | topology blocks | `frame_count` entries of each of the four |
    /// | claim table | `edge_count` records |
    /// | participant table | all (8 KiB, and every liveness check walks it) |
    /// | edge table | `edge_count` records |
    /// | stamp + pose arenas | **none — see below** |
    /// | edge counters | `edge_count` records — written by `Guard::drop` on every read batch |
    /// | participant counters | all (8 KiB) — same path, keyed by the reader's own slot |
    ///
    /// The ring arenas are not populated here: §7.1 is NORMATIVE that population is **per-edge**, so it
    /// happens at `Tree::claim` (writer) and plan compilation (reader), both off the query path (D3).
    /// The extents come from `EdgeRecord`, in `tf_tree_core`; `ArenaView::ring_extents` is the other
    /// half. Frames interned later fault once.
    pub fn populate_hot(&self) {
        // SAFETY: module invariant; the mapping holds at least `size_of::<ArenaHeader>()` bytes (checked by
        // `attach`) and the header is only read.
        let h = unsafe { &*self.base.as_ptr().cast::<ArenaHeader>() };
        let frames = h.frame_count.load(core::sync::atomic::Ordering::Acquire) as usize;
        let edges = h.edge_count.load(core::sync::atomic::Ordering::Acquire) as usize;

        self.populate(0, core::mem::size_of::<ArenaHeader>());
        self.populate(h.frame_table_off as usize, frames * 64);

        // Topology blocks are strided, so each block's used prefix is populated separately.
        let topo_used = frames * 12;
        for b in 0..TOPO_BLOCKS {
            let off = h.topo_block_off as usize + b * h.topo_block_stride as usize;
            self.populate(off, topo_used);
        }

        self.populate(h.claim_table_off as usize, edges * 64);
        self.populate(
            h.participant_table_off as usize,
            h.max_participants as usize * 128,
        );
        self.populate(h.edge_table_off as usize, edges * 128);
        // The stamp and pose arenas are deliberately absent — see the doc comment.

        // v3 counter regions (`docs/PHASE5.md` §5.2): `Guard::drop` writes them on every read batch, so
        // they are on the lookup path.
        self.populate(h.edge_counters_off as usize, edges * 128);
        self.populate(
            h.participant_counters_off as usize,
            h.max_participants as usize * 128,
        );
    }

    /// Apply the mapping policy of `docs/PHASE2.md` §7; both calls are best-effort.
    fn advise(&self) {
        // MADV_DONTFORK (§7.3): otherwise a forked child inherits the mapping as an invisible participant.
        let _ = self.madvise(Advice::LinuxDontFork);
        let _ = self.madvise(Advice::LinuxHugepage);
    }

    fn madvise(&self, advice: Advice) -> rustix::io::Result<()> {
        // SAFETY: module invariant — `base`/`len` describe this arena's own live
        // mapping, which is what `madvise` requires.
        unsafe { madvise(self.base.as_ptr().cast(), self.len, advice) }
    }

    /// The segment's file descriptor, for handing to another process.
    pub fn as_raw_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// Whether this mapping may be written (i.e. may publish).
    #[must_use]
    pub fn is_writable(&self) -> bool {
        self.writable
    }

    /// Borrow the arena header at the base of the mapping.
    #[must_use]
    pub fn header(&self) -> &ArenaHeader {
        // SAFETY: module invariant — the base addresses at least
        // size_of::<ArenaHeader>() readable bytes (checked in `attach`, sized in
        // `create`) and is page-aligned, hence aligned for ArenaHeader.
        unsafe { &*self.base.as_ptr().cast::<ArenaHeader>() }
    }
}

/// Draw 16 random bytes for a new arena's `instance_uuid`.
///
/// Refills after a short read (a signal can interrupt `getrandom`, and a partial uuid still looks
/// random). Blocks deliberately: creation is a startup operation.
fn instance_uuid() -> Result<[u8; 16], ShmError> {
    use rustix::rand::{getrandom, GetRandomFlags};

    let mut uuid = [0u8; 16];
    let mut filled = 0;
    while filled < uuid.len() {
        match getrandom(&mut uuid[filled..], GetRandomFlags::empty()) {
            // A zero-length read would spin forever; treat it as an I/O failure.
            Ok(0) => return Err(ShmError::Random(rustix::io::Errno::IO)),
            Ok(n) => filled += n,
            // Every other errno is a real failure and must not be retried.
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(ShmError::Random(e)),
        }
    }
    Ok(uuid)
}

/// Byte offsets to touch so every page overlapping `[offset, offset + len)` is faulted in: one per
/// page, **never past the end**.
///
/// A pure function so the bound is testable (residency is not observable here). Every offset is
/// `< offset + len`, which makes the caller's `unsafe` read in-bounds.
fn touch_offsets(offset: usize, len: usize) -> impl Iterator<Item = usize> {
    const PAGE: usize = 4096;
    // Step from `offset`, not an aligned base, which would touch a page before the range.
    (0..len).step_by(PAGE).map(move |d| offset + d)
}

/// `mmap` `len` bytes of `fd` shared, **without** prefaulting.
fn unsafe_map(len: usize, prot: ProtFlags, fd: &OwnedFd) -> Result<NonNull<u8>, ShmError> {
    // SAFETY: a null hint lets the kernel choose the address, so no mapping is replaced; `len` is the
    // segment size and `fd` a memfd of at least that size. The result is null-checked.
    let raw = unsafe {
        mmap(
            core::ptr::null_mut(),
            len,
            prot,
            // No `MAP_POPULATE`: `docs/PHASE2.md` §7.1 is NORMATIVE that population is per-declaration
            // (it charged 66.3 MiB RSS against 66.1 MiB declared on the measured arena). `populate_hot`
            // restores the touched pages and reports failure.
            MapFlags::SHARED,
            fd,
            0,
        )
    }
    .map_err(ShmError::Map)?;
    NonNull::new(raw.cast::<u8>()).ok_or(ShmError::Map(rustix::io::Errno::NOMEM))
}

impl Drop for MappedArena {
    fn drop(&mut self) {
        // See `owner_pid`. In a `fork` child this range is not ours to unmap.
        if rustix::process::getpid() != self.owner_pid {
            return;
        }
        // SAFETY: module invariant; `base`/`len` are what `mmap` returned, unmapped once, in the process
        // that made the mapping (the `getpid` guard).
        let _ = unsafe { munmap(self.base.as_ptr().cast(), self.len) };
    }
}

// SAFETY: `MappedArena` owns its mapping and hands out no interior references aliasing the bytes;
// all concurrent access is mediated by the atomic protocols in `tf_tree_core`.
unsafe impl Send for MappedArena {}
// SAFETY: see the `Send` impl above.
unsafe impl Sync for MappedArena {}

// SAFETY: `base()`/`len()` describe one live mapping at a fixed page-aligned address, valid for
// `len` bytes until `Drop`; the seals make `len` immutable for the fd's lifetime.
unsafe impl Arena for MappedArena {
    fn base(&self) -> *mut u8 {
        self.base.as_ptr()
    }

    fn len(&self) -> usize {
        self.len
    }
}

/// A NUL-terminated stack copy of a short, debug-only segment name; an over-long name is truncated.
struct CName {
    buf: [u8; Self::CAP],
    len: usize,
}

impl CName {
    const CAP: usize = 64;

    fn new(name: &str) -> CName {
        let mut buf = [0u8; Self::CAP];
        let src = name.as_bytes();
        // Truncate at the first interior NUL: `from_bytes_with_nul_unchecked` requires exactly one, at the end.
        let end = src.iter().position(|&b| b == 0).unwrap_or(src.len());
        // Leave room for the terminator.
        let n = core::cmp::min(end, Self::CAP - 1);
        buf[..n].copy_from_slice(&src[..n]);
        CName { buf, len: n }
    }

    fn as_cstr(&self) -> &core::ffi::CStr {
        // SAFETY: only `buf[..len]` was written, `len <= CAP - 1`, over a zeroed buffer, so the slice up to
        // `buf[len]` holds exactly one NUL, at the end.
        unsafe { core::ffi::CStr::from_bytes_with_nul_unchecked(&self.buf[..=self.len]) }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::header::{FORMAT_VERSION, TF_TREE_MAGIC};
    use crate::layout::layout_hash;
    use alloc::vec;

    fn fixture() -> ArenaLayout {
        ArenaLayout::new(8, 4, vec![16, 0, 4, 64]).unwrap()
    }

    fn create() -> MappedArena {
        MappedArena::create("tf_tree.uuid_test", &fixture(), 1234, 5678, [7; 16]).unwrap()
    }

    /// **The fallback must not write** — see [`MappedArena::populate_by_touch`].
    ///
    /// On kernels >= 5.14 [`MappedArena::populate`] takes the `madvise` branch, so calling this
    /// directly is the only way it runs. The every-page bound is tested in [`touch_offsets`].
    #[test]
    fn the_pre_5_14_fallback_writes_nothing() {
        let arena = create();
        let before = arena.header_snapshot();
        arena.populate_by_touch(0, arena.len);
        arena.populate_by_touch(arena.len - 1, 1);
        assert_eq!(
            arena.header_snapshot(),
            before,
            "the fallback wrote to the arena"
        );
    }

    /// The fallback's bound. Mutant: `while at + PAGE < end` (dropping the final partial page) fails
    /// the last two cases.
    #[test]
    fn touch_offsets_covers_every_page_and_never_passes_the_end() {
        let v = |o, l| touch_offsets(o, l).collect::<alloc::vec::Vec<_>>();
        assert_eq!(v(0, 0), alloc::vec![]);
        assert_eq!(v(0, 1), alloc::vec![0]);
        assert_eq!(v(0, 4096), alloc::vec![0]);
        assert_eq!(v(0, 4097), alloc::vec![0, 4096]);
        assert_eq!(v(0, 8192), alloc::vec![0, 4096]);
        // A range starting mid-page must touch that page.
        assert_eq!(v(100, 1), alloc::vec![100]);
        assert_eq!(v(4095, 2), alloc::vec![4095]);
        // The last partial page is still a page.
        assert_eq!(v(0, 4096 * 3 + 1), alloc::vec![0, 4096, 8192, 12288]);
        for (o, l) in [(0usize, 12345usize), (7, 99999), (4095, 4097)] {
            for at in touch_offsets(o, l) {
                assert!(at >= o && at < o + l, "{at} escaped [{o}, {o}+{l})");
            }
        }
    }

    /// `populate` must never walk off the end; the clamp guards `madvise` over memory this arena does not own.
    #[test]
    fn populate_clamps_to_the_mapping() {
        let arena = create();
        arena.populate(0, usize::MAX);
        arena.populate(arena.len - 1, usize::MAX);
        arena.populate(arena.len, 4096);
        arena.populate(arena.len + 1_000_000, 4096);
        arena.populate(0, 0);
        // Still intact and still readable.
        assert_eq!(arena.header().magic, u64::from_le_bytes(TF_TREE_MAGIC));
    }

    /// Two different segments must get different instance ids; a constant would pass every other
    /// assertion and defeat the split-brain check (`docs/PHASE2.md` §11.2 scenario 9).
    #[test]
    fn two_arenas_never_share_an_instance_uuid() {
        let a = create();
        let b = create();
        assert_ne!(a.header().instance_uuid, b.header().instance_uuid);
        // ...and neither is the all-zero heap-arena sentinel.
        assert_ne!(a.header().instance_uuid, [0; 16]);
        assert_ne!(b.header().instance_uuid, [0; 16]);
    }

    /// A joiner must read the *creator's* id: the wire compares `HelloResponse`'s `instance_uuid`
    /// against the mapped header.
    #[test]
    fn attach_preserves_the_creators_instance_uuid() {
        let created = create();
        let uuid = created.header().instance_uuid;
        // Assert non-zero first: if the field was never written, both sides read zero and equality proves nothing.
        assert_ne!(uuid, [0; 16], "instance_uuid was never written");

        let fd = rustix::io::fcntl_dupfd_cloexec(created.as_raw_fd(), 0).unwrap();
        let attached = MappedArena::attach(fd, AttachMode::ReadOnly).unwrap();

        assert_eq!(attached.header().instance_uuid, uuid);
    }

    /// **The seal check is the `memfd`-not-`shm_open` argument**, and it runs before mapping.
    ///
    /// Mutant: delete the `seals.contains(REQUIRED_SEALS)` guard in `attach`; the unsealed case then maps
    /// and this fails.
    #[test]
    fn an_unsealed_or_undersized_segment_is_refused_before_it_is_mapped() {
        let len = fixture().total_size() as u64;

        // No `ALLOW_SEALING`: a peer could shrink it under us.
        let raw = memfd_create(c"tf_tree.unsealed", MemfdFlags::CLOEXEC).unwrap();
        ftruncate(&raw, len).unwrap();
        let refused = MappedArena::attach(raw, AttachMode::ReadOnly).err();
        assert_eq!(refused, Some(ShmError::Unsealed));

        // Sealed but too small to hold a header.
        let tiny = memfd_create(
            c"tf_tree.tiny",
            MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
        )
        .unwrap();
        ftruncate(&tiny, 64).unwrap();
        fcntl_add_seals(&tiny, REQUIRED_SEALS | SealFlags::SEAL).unwrap();
        let refused = MappedArena::attach(tiny, AttachMode::ReadOnly).err();
        assert_eq!(refused, Some(ShmError::TooSmall));
    }

    /// **`docs/PHASE2.md` §11.2 scenario 4**: a segment from a different build is rejected by value,
    /// naming both sides.
    ///
    /// Each case edits one field of a good segment. Mutant: drop any one comparison in
    /// `validate_arena_header` and the matching case fails.
    #[test]
    fn attach_refuses_a_segment_this_build_cannot_read() {
        type Poke = fn(&mut ArenaHeader);
        let cases: [(Poke, ShmError); 3] = [
            (|h| h.magic ^= 1, ShmError::BadMagic),
            (
                |h| h.format_version ^= 0x5555,
                ShmError::VersionMismatch {
                    found: FORMAT_VERSION ^ 0x5555,
                    expected: FORMAT_VERSION,
                },
            ),
            (
                |h| h.layout_hash ^= 0x5555,
                ShmError::LayoutMismatch {
                    found: layout_hash() ^ 0x5555,
                    expected: layout_hash(),
                },
            ),
        ];

        for (poke, want) in cases {
            let owner = create();
            // SAFETY: `owner` is this test's own read-write mapping, no other process holds it, and its base
            // is a live, aligned `ArenaHeader`; no other reference is live across this call.
            unsafe { poke(&mut *owner.base().cast::<ArenaHeader>()) };
            let fd = rustix::io::fcntl_dupfd_cloexec(owner.as_raw_fd(), 0).unwrap();
            let refused = MappedArena::attach(fd, AttachMode::ReadOnly).err();
            assert_eq!(refused, Some(want));
        }
    }

    /// `CName::as_cstr` requires **exactly one** NUL, at the end, and `create("a\0b")` is reachable
    /// public API, so the truncation is the sole guarantor.
    ///
    /// Mutants: drop the interior-NUL truncation (two NULs, unsound) or bound by `CAP` instead of
    /// `CAP - 1` (terminator overwritten); the `"a\0b"` and long cases fail.
    #[test]
    fn a_segment_name_is_always_exactly_one_nul_terminated_string() {
        for (input, want) in [
            ("tf_tree.default", "tf_tree.default"),
            ("", ""),
            ("a\0b", "a"),
            ("\0leading", ""),
        ] {
            let n = CName::new(input);
            assert_eq!(n.as_cstr().to_bytes(), want.as_bytes(), "{input:?}");
        }

        let long = "x".repeat(4 * CName::CAP);
        let n = CName::new(&long);
        assert_eq!(n.as_cstr().to_bytes().len(), CName::CAP - 1);
        // The truncated name still reaches the kernel.
        MappedArena::create(&long, &fixture(), 0, 0, [0; 16]).unwrap();
    }

    /// Adding a field must not move the segment's size or hash, or running peers fail to attach.
    #[test]
    fn the_new_field_did_not_change_the_wire_contract() {
        let arena = create();
        let h = arena.header();
        assert_eq!(h.format_version, FORMAT_VERSION);
        assert_eq!(h.layout_hash, layout_hash());
        assert_eq!(h.arena_size, fixture().total_size() as u64);
    }
}
