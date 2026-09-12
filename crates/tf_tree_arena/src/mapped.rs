//! `memfd`-backed shared-memory arena — the Phase 2 backend.
//!
//! `docs/PHASE2.md` §4 is NORMATIVE: the read path's diff against Phase 1 is
//! **zero lines** — the arena is pointer-free, so identical code runs on a
//! different base pointer (`crates/tf_tree_bench/tests/relocation.rs` proves it
//! independently). Everything here is about *obtaining* that pointer safely.
//!
//! `memfd`, not `shm_open`, for sealing (§3.2 forbids the "simplification"):
//! sealed in [`MappedArena::create`] and verified — never trusted — in
//! [`MappedArena::attach`], the size is immutable, so no fd holder can
//! `ftruncate` a reader into a `SIGBUS` inside a lookup. Participants are
//! otherwise mutually trusting same-user processes (§0), so the MMU-enforced
//! **read-only** attach is the one real boundary and the consumer default.
//!
//! # SAFETY (module invariant)
//!
//! A [`MappedArena`] owns one `mmap`ping of `len` bytes at `base`, established
//! from `fd` and unmapped exactly once in [`Drop`]. For its whole lifetime:
//!
//! * `base` is non-null, page-aligned (hence 64-byte aligned), and addresses
//!   `len` readable bytes — writable as well when `writable` is true.
//! * `len` equals the segment size, which the seals make **immutable**, so the
//!   mapping can never be truncated out from under a reader.
//! * All typed access goes through `tf_tree_core`'s atomic protocols, which is
//!   what makes `Send + Sync` sound — [`crate::heap::HeapArena`]'s argument.

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
    /// `PROT_READ` only; the consumer default, enforced by the MMU.
    ReadOnly,
    /// `PROT_READ | PROT_WRITE`. Required to publish samples or claim edges.
    ReadWrite,
}

/// An [`Arena`] backed by a sealed `memfd` mapped `MAP_SHARED`.
///
/// Nothing above it knows it exists: the stack is written against [`Arena`], so
/// the same reader code runs unmodified against a [`crate::heap::HeapArena`].
pub struct MappedArena {
    base: NonNull<u8>,
    len: usize,
    fd: OwnedFd,
    writable: bool,
    /// The process that established this mapping.
    ///
    /// The mapping is `MADV_DONTFORK` (§7.3), so in a `fork` child the range is
    /// a *hole*; `munmap` there is a no-op unless the child has put a mapping of
    /// its own in the hole, and then the destructor unmaps it, silently and at a
    /// distance. `getpid` is a syscall, affordable once per teardown — the
    /// hot-path equivalent, `tf_tree_ipc::fork`'s atfork counter, is a
    /// dependency pointing the wrong way. **No test fails when this check is
    /// removed** (`crates/tf_tree_bench/tests/fork.rs` stayed green: `munmap` on
    /// a hole succeeds, and the harness cannot fill the hole without a public
    /// base-address accessor); kept because nothing else guards it.
    owner_pid: rustix::process::Pid,
}

impl MappedArena {
    /// Create a new sealed segment sized for `layout` and write its header.
    ///
    /// `docs/PHASE2.md` §3.2. Sealing happens **after** the mapping so the
    /// creator keeps write access: `F_ADD_SEALS SHRINK|GROW` succeeds under a
    /// writable mapping where `F_SEAL_WRITE` would return `EBUSY`.
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

        // Without `MFD_ALLOW_SEALING` the `F_ADD_SEALS` below is EPERM and the
        // segment can never be made SIGBUS-safe. `CLOEXEC`: sharing is explicit.
        let cname = CName::new(name);
        let fd = memfd_create(
            cname.as_cstr(),
            MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
        )
        .map_err(ShmError::Create)?;
        ftruncate(&fd, len as u64).map_err(ShmError::Truncate)?;

        // **No `MAP_POPULATE`** — see `unsafe_map` for the measurement.
        let base = unsafe_map(len, ProtFlags::READ | ProtFlags::WRITE, &fd)?;

        // **Own the mapping before the first fallible step.** Until this value
        // exists there is no `Drop` to `munmap`, so a failing `?` below strands
        // the address space and — the mapping holding its own reference to the
        // memfd inode — its committed pages, for the life of the process.
        let arena = MappedArena {
            owner_pid: rustix::process::getpid(),
            base,
            len,
            fd,
            writable: true,
        };

        let uuid = instance_uuid()?;
        // SAFETY: `arena.base` addresses `len` freshly zeroed bytes (a memfd,
        // just sized), page-aligned hence 64-byte aligned, and no other mapping
        // of this fd exists yet, so this call uniquely owns the region.
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

        // SEAL itself prevents any future seal, so a peer cannot later add
        // F_SEAL_WRITE and freeze the writer out.
        fcntl_add_seals(&arena.fd, REQUIRED_SEALS | SealFlags::SEAL).map_err(ShmError::Seal)?;

        arena.advise();
        Ok(arena)
    }

    /// Map an existing segment from a received fd, validating it first.
    ///
    /// `docs/PHASE2.md` §3.3 steps 4-6: every check is a refusal to trust a peer
    /// about something this process can verify itself.
    ///
    /// # Errors
    ///
    /// [`ShmError::Unsealed`] if the segment could be truncated under us;
    /// [`ShmError::BadMagic`], [`ShmError::VersionMismatch`],
    /// [`ShmError::LayoutMismatch`] or [`ShmError::SizeMismatch`] if this build
    /// cannot read it.
    pub fn attach(fd: OwnedFd, mode: AttachMode) -> Result<MappedArena, ShmError> {
        // Refuse *before* mapping: afterwards a truncation by any fd holder
        // turns every subsequent read into an unrecoverable SIGBUS.
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

        // Validated only once mapped; on failure the `MappedArena` drops and
        // unmaps. In `crate::check` because the frozen-file backend shares them.
        validate_arena_header(arena.header(), size)?;
        Ok(arena)
    }

    /// Fault in `[offset, offset + len)` of this arena, up front.
    ///
    /// **Not `MADV_WILLNEED`** — `docs/PHASE2.md` §7.1: *"does not work here
    /// (measured: zero change in charged pages on a memfd). Do not substitute
    /// it."* A memfd's pages are already in the page cache; the page *tables*
    /// are what need populating. The advice follows the mapping's protection
    /// (`POPULATE_WRITE` on `PROT_READ` is `EINVAL`), and `WRITE` is right for a
    /// writable mapping even before any store, which `POPULATE_READ` would leave
    /// to take a copy-on-write fault.
    ///
    /// Infallible: `MADV_POPULATE_*` needs Linux 5.14 and is `EINVAL` below it,
    /// which falls back to touching pages by hand; any other errno leaves pages
    /// cold — slower, never incorrect.
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
        // arena's own live mapping, and `offset + len` is clamped to it above.
        let r = unsafe { madvise(self.base.as_ptr().add(offset).cast(), len, advice) };
        if r == Err(rustix::io::Errno::INVAL) {
            self.populate_by_touch(offset, len);
        }
    }

    /// Kernels before 5.14: fault the pages in by touching one byte per page.
    ///
    /// A **read** on both mapping modes, never a write: this runs on a live
    /// segment, so a store races every reader of a claim record or sample slot.
    fn populate_by_touch(&self, offset: usize, len: usize) {
        for at in touch_offsets(offset, len) {
            // SAFETY: `at` is within `[offset, offset + len)` (established and
            // tested in `touch_offsets`) and the caller clamped that range to
            // this mapping. `read_volatile`: the fault is the load's only point.
            unsafe {
                core::ptr::read_volatile(self.base.as_ptr().add(at));
            }
        }
    }

    /// The first 256 bytes, for tests asserting nothing wrote to the arena.
    #[cfg(test)]
    fn header_snapshot(&self) -> [u8; 256] {
        let mut out = [0u8; 256];
        // SAFETY: module invariant — the mapping is at least
        // `size_of::<ArenaHeader>()` (320 since FORMAT_VERSION 3) and this only
        // reads. Still 256: every field it pins lives below that.
        unsafe { core::ptr::copy_nonoverlapping(self.base.as_ptr(), out.as_mut_ptr(), 256) };
        out
    }

    /// Populate every region that is actually read, and nothing else (§7.1) —
    /// the cold headroom tails are the whole win: 66 MiB on the arena measured
    /// in `unsafe_map`. §7.1's *declaration* granularity survives `0004` moving
    /// declaration to build time because `frame_count`/`edge_count` are live
    /// header counters: an attaching process derives the extents itself, with
    /// nothing passed in and nothing to keep in sync with the builder.
    ///
    /// Deliberately left cold: the frame hash (probed by hash, so scattered, and
    /// interning is not the hot path), and the stamp and pose rings. The rings
    /// were populated in full, which is per-*arena* where §7.1 is NORMATIVE that
    /// population is **per-edge**; at 99.8% of a large arena, a process reading
    /// 5 edges of 200 paid for all 200. They moved to `Tree::claim` and to plan
    /// compilation, both off the query path by D3. Their extents cannot be
    /// computed here — they come from `EdgeRecord` in `tf_tree_core`, which
    /// depends on this crate (`ArenaView::ring_extents` is the other half).
    /// Frames interned after this fault once, which §7.1 requires.
    pub fn populate_hot(&self) {
        // SAFETY: module invariant — the mapping is at least `size_of::<ArenaHeader>()`
        // bytes (checked by `attach`, by construction in `create`), aligned, and
        // the header is only ever read through this shared reference.
        let h = unsafe { &*self.base.as_ptr().cast::<ArenaHeader>() };
        let frames = h.frame_count.load(core::sync::atomic::Ordering::Acquire) as usize;
        let edges = h.edge_count.load(core::sync::atomic::Ordering::Acquire) as usize;

        self.populate(0, core::mem::size_of::<ArenaHeader>());
        self.populate(h.frame_table_off as usize, frames * 64);

        // Strided, not contiguous: `blocks * stride` from the first would pull
        // in three blocks' worth of headroom.
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
        // Stamp and pose arenas are deliberately absent — see the doc comment.

        // v3's counter regions (`docs/PHASE5.md` §5.2) are on the *lookup* path
        // of a read-write participant, not diagnostics: `Guard::drop`
        // `fetch_add`s here every read batch. Left cold, an attaching process
        // takes ~34 minor faults at 1-3 µs each against a 150 ns p50 budget.
        self.populate(h.edge_counters_off as usize, edges * 128);
        self.populate(
            h.participant_counters_off as usize,
            h.max_participants as usize * 128,
        );
    }

    /// Apply `docs/PHASE2.md` §7's mapping policy. Both calls are best-effort: a
    /// kernel without transparent huge pages must not fail an attach.
    fn advise(&self) {
        // MADV_DONTFORK (§7.3) is easy to forget: a forked child would inherit
        // the mapping as an invisible participant no registry knows about.
        let _ = self.madvise(Advice::LinuxDontFork);
        let _ = self.madvise(Advice::LinuxHugepage);
    }

    fn madvise(&self, advice: Advice) -> rustix::io::Result<()> {
        // SAFETY: module invariant — `base`/`len` are this arena's live mapping.
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
        // size_of::<ArenaHeader>() readable bytes and is page-aligned.
        unsafe { &*self.base.as_ptr().cast::<ArenaHeader>() }
    }
}

/// Draw 16 random bytes for a new arena's `instance_uuid`.
///
/// The refill loop exists because a signal can short-read even 16 bytes, and a
/// partly filled uuid still looks random downstream. It **blocks** deliberately:
/// creation is startup work, and blocking flags cannot return `EAGAIN` at all.
fn instance_uuid() -> Result<[u8; 16], ShmError> {
    use rustix::rand::{getrandom, GetRandomFlags};

    let mut uuid = [0u8; 16];
    let mut filled = 0;
    while filled < uuid.len() {
        match getrandom(&mut uuid[filled..], GetRandomFlags::empty()) {
            // No progress on a non-empty buffer is not legitimate; do not spin.
            Ok(0) => return Err(ShmError::Random(rustix::io::Errno::IO)),
            Ok(n) => filled += n,
            // A blocking `getrandom` is interruptible; other errnos are real.
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(ShmError::Random(e)),
        }
    }
    Ok(uuid)
}

/// Byte offsets to touch so that every page overlapping `[offset, offset + len)`
/// is faulted in — one per page, plus the first, and **never past the end**.
///
/// Pure so the bound can be tested: residency needs `mincore`, which `rustix`
/// lacks and which is not worth a `libc` dependency here, so a test of the
/// *effect* cannot tell "touched every page" from "stopped one short". Every
/// offset is `< offset + len`, which makes the caller's `unsafe` read in-bounds.
fn touch_offsets(offset: usize, len: usize) -> impl Iterator<Item = usize> {
    const PAGE: usize = 4096;
    // The range's first page starts at `offset`, not necessarily page-aligned;
    // stepping from the aligned base would touch a page before the range.
    (0..len).step_by(PAGE).map(move |d| offset + d)
}

/// `mmap` `len` bytes of `fd` shared, **without** prefaulting.
fn unsafe_map(len: usize, prot: ProtFlags, fd: &OwnedFd) -> Result<NonNull<u8>, ShmError> {
    // SAFETY: a null hint lets the kernel choose the address, so no existing
    // mapping can be replaced; `len` is the segment's size and `fd` a memfd of
    // at least that size. `NonNull::new` checks the result for null.
    let raw = unsafe {
        mmap(
            core::ptr::null_mut(),
            len,
            prot,
            // No `MAP_POPULATE`: `docs/PHASE2.md` §7.1 is NORMATIVE that
            // population happens at declaration granularity. Measured on an
            // arena declaring one 1024-slot edge with 200k slots of headroom, it
            // charged **66.3 MiB of RSS against 66.1 MiB declared**.
            // `populate_hot` puts back the pages touched, and reports failure.
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
        // SAFETY: module invariant — `base`/`len` are exactly what `mmap`
        // returned, unmapped here exactly once, and the `getpid` guard
        // establishes that this is still the process it was made in.
        let _ = unsafe { munmap(self.base.as_ptr().cast(), self.len) };
    }
}

// SAFETY: `MappedArena` owns its mapping and exposes only base and length; it
// hands out no interior references that alias the bytes, and all concurrent
// access is mediated by `tf_tree_core`'s atomic protocols.
unsafe impl Send for MappedArena {}
// SAFETY: see the `Send` impl above.
unsafe impl Sync for MappedArena {}

// SAFETY: `base()`/`len()` describe one live mapping at a fixed page-aligned
// address, valid for `len` bytes until `Drop`. The seals (applied in `create`,
// verified in `attach`) make `len` immutable for the fd's lifetime.
unsafe impl Arena for MappedArena {
    fn base(&self) -> *mut u8 {
        self.base.as_ptr()
    }

    fn len(&self) -> usize {
        self.len
    }
}

/// A NUL-terminated copy of a short segment name, on the stack.
///
/// `memfd_create` wants a `&CStr` and this crate's dependency budget is tiny.
/// The name is debug-only (`/proc/<pid>/fd`), so truncating is right.
struct CName {
    buf: [u8; Self::CAP],
    len: usize,
}

impl CName {
    const CAP: usize = 64;

    fn new(name: &str) -> CName {
        let mut buf = [0u8; Self::CAP];
        let src = name.as_bytes();
        // Truncate at the first interior NUL: `from_bytes_with_nul_unchecked`
        // requires exactly one, at the end, and `name` is arbitrary caller input
        // (`build_shared("a\0b")`). The kernel stops there anyway.
        let end = src.iter().position(|&b| b == 0).unwrap_or(src.len());
        // Leave room for the terminator. The kernel treats the name as opaque
        // bytes, so a truncated multi-byte sequence is harmless.
        let n = core::cmp::min(end, Self::CAP - 1);
        buf[..n].copy_from_slice(&src[..n]);
        CName { buf, len: n }
    }

    fn as_cstr(&self) -> &core::ffi::CStr {
        // SAFETY: `buf` was zero-initialized and only `buf[..len]` written, with
        // `len <= CAP - 1`, so `buf[len]` is a NUL and is the only one.
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

    /// **The fallback must not write**: it runs on a live segment, so a store
    /// races every reader of a claim record or sample slot. `MADV_POPULATE_*`
    /// (Linux 5.14+) means [`MappedArena::populate`] never reaches it here, so
    /// only a direct call covers it; its page bound is in [`touch_offsets`].
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

    /// The fallback's bound, tested where it is observable (residency is not).
    /// Mutant: `while at + PAGE < end`, dropping the final partial page ⇒ the
    /// last two cases below fail.
    #[test]
    fn touch_offsets_covers_every_page_and_never_passes_the_end() {
        let v = |o, l| touch_offsets(o, l).collect::<alloc::vec::Vec<_>>();
        assert_eq!(v(0, 0), alloc::vec![]);
        assert_eq!(v(0, 1), alloc::vec![0]);
        assert_eq!(v(0, 4096), alloc::vec![0]);
        assert_eq!(v(0, 4097), alloc::vec![0, 4096]);
        assert_eq!(v(0, 8192), alloc::vec![0, 4096]);
        // A range starting mid-page must touch *that* page, not the one before.
        assert_eq!(v(100, 1), alloc::vec![100]);
        assert_eq!(v(4095, 2), alloc::vec![4095]);
        // Skipping the last partial page leaves the fault this exists to remove.
        assert_eq!(v(0, 4096 * 3 + 1), alloc::vec![0, 4096, 8192, 12288]);
        for (o, l) in [(0usize, 12345usize), (7, 99999), (4095, 4097)] {
            for at in touch_offsets(o, l) {
                assert!(at >= o && at < o + l, "{at} escaped [{o}, {o}+{l})");
            }
        }
    }

    /// `populate` must never walk off the end — the clamp is all that stands
    /// between a caller's slip and an `madvise` over memory we do not own.
    #[test]
    fn populate_clamps_to_the_mapping() {
        let arena = create();
        arena.populate(0, usize::MAX);
        arena.populate(arena.len - 1, usize::MAX);
        arena.populate(arena.len, 4096);
        arena.populate(arena.len + 1_000_000, 4096);
        arena.populate(0, 0);
        assert_eq!(arena.header().magic, u64::from_le_bytes(TF_TREE_MAGIC));
    }

    /// The point of an instance id is to tell two *different* segments apart: a
    /// constant would satisfy every other assertion here while making the
    /// split-brain check (`docs/PHASE2.md` §11.2 scenario 9) compare equal.
    #[test]
    fn two_arenas_never_share_an_instance_uuid() {
        let a = create();
        let b = create();
        assert_ne!(a.header().instance_uuid, b.header().instance_uuid);
        // ...nor the all-zero "not a shared instance" sentinel a heap arena writes.
        assert_ne!(a.header().instance_uuid, [0; 16]);
        assert_ne!(b.header().instance_uuid, [0; 16]);
    }

    /// A joiner must read the *creator's* id, not one of its own: `HelloResponse`
    /// carries the owner's `instance_uuid` and the client compares it against
    /// the header it just mapped, so a fresh id would fail every legitimate join.
    #[test]
    fn attach_preserves_the_creators_instance_uuid() {
        let created = create();
        let uuid = created.header().instance_uuid;
        // Assert non-zero *before* comparing: an unwritten field would read
        // all-zero on both sides and the equality below would prove nothing.
        assert_ne!(uuid, [0; 16], "instance_uuid was never written");

        let fd = rustix::io::fcntl_dupfd_cloexec(created.as_raw_fd(), 0).unwrap();
        let attached = MappedArena::attach(fd, AttachMode::ReadOnly).unwrap();

        assert_eq!(attached.header().instance_uuid, uuid);
    }

    /// **The seal check is the whole `memfd`-not-`shm_open` argument** and runs
    /// before the segment is mapped: after that, any fd holder could `ftruncate`
    /// it into a `SIGBUS` no library can recover from. Mutant: delete the
    /// `seals.contains(REQUIRED_SEALS)` guard in `attach` ⇒ the unsealed case
    /// below maps happily and fails. Nothing else in the workspace covers it.
    #[test]
    fn an_unsealed_or_undersized_segment_is_refused_before_it_is_mapped() {
        let len = fixture().total_size() as u64;

        // No `ALLOW_SEALING`, so a peer could shrink it under us.
        let raw = memfd_create(c"tf_tree.unsealed", MemfdFlags::CLOEXEC).unwrap();
        ftruncate(&raw, len).unwrap();
        let refused = MappedArena::attach(raw, AttachMode::ReadOnly).err();
        assert_eq!(refused, Some(ShmError::Unsealed));

        // Sealed, but too small to hold the header that says what it is.
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

    /// **`docs/PHASE2.md` §11.2 scenario 4**: a segment from a different build
    /// is rejected by value, naming both sides. Each case is a single-field edit
    /// — the real failure's shape, the same binary rebuilt. Mutant: drop any of
    /// `validate_arena_header`'s three comparisons ⇒ that case fails.
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
            // SAFETY: `owner` is this test's own read-write mapping of a segment
            // no other process holds, its base is a live, page-aligned (hence
            // 64-byte aligned), initialized `ArenaHeader`, and no other
            // reference to it is live across this call.
            unsafe { poke(&mut *owner.base().cast::<ArenaHeader>()) };
            let fd = rustix::io::fcntl_dupfd_cloexec(owner.as_raw_fd(), 0).unwrap();
            let refused = MappedArena::attach(fd, AttachMode::ReadOnly).err();
            assert_eq!(refused, Some(want));
        }
    }

    /// `CName::as_cstr`'s `from_bytes_with_nul_unchecked` requires **exactly
    /// one** NUL, at the end, and `MappedArena::create`'s name is arbitrary
    /// caller input (`tf_tree::TreeBuilder::build_shared`), so the truncation is
    /// its sole guarantor. Mutants: drop the interior-NUL truncation ⇒ `"a\0b"`
    /// holds two NULs, the `unsafe` is unsound and it fails; `CAP` for
    /// `CAP - 1` ⇒ the terminator is overwritten and the long case fails.
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
        // The truncated name still reaches the kernel rather than being refused.
        MappedArena::create(&long, &fixture(), 0, 0, [0; 16]).unwrap();
    }

    /// Adding a field must not have moved the segment's size or its hash, or
    /// every already-running peer would fail to attach to a new build.
    #[test]
    fn the_new_field_did_not_change_the_wire_contract() {
        let arena = create();
        let h = arena.header();
        assert_eq!(h.format_version, FORMAT_VERSION);
        assert_eq!(h.layout_hash, layout_hash());
        assert_eq!(h.arena_size, fixture().total_size() as u64);
    }
}
