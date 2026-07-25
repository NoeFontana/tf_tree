//! `memfd`-backed shared-memory arena — the Phase 2 backend.
//!
//! `docs/PHASE2.md` §4 makes one thing NORMATIVE: the diff against Phase 1 in
//! the read path must be **zero lines**. `Plan::at`, the bracket search, slot
//! reads and interning are byte-identical code operating on a different base
//! pointer. That is achievable because the arena is pointer-free, which
//! `crates/tf_tree_bench/tests/relocation.rs` proves independently of this
//! module. Everything here is about *obtaining* that base pointer safely.
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
//! * All typed access to the bytes goes through `tf_tree_core`'s atomic
//!   protocols, which is what makes `Send + Sync` sound — the identical argument
//!   [`crate::heap::HeapArena`] makes.
//!
//! # Why `memfd` and not `shm_open`
//!
//! Sealing. After [`MappedArena::create`] applies `F_SEAL_SHRINK | F_SEAL_GROW |
//! F_SEAL_SEAL`, the segment's size is immutable for the life of the fd, and
//! **`SIGBUS` becomes structurally impossible**. Without a seal, any process
//! holding the fd could `ftruncate` the segment, and every reader touching a
//! truncated page would take `SIGBUS` from inside a lookup — an unrecoverable
//! fault in the middle of a control loop that a library cannot handle sanely.
//! `shm_open` segments cannot be sealed, which is why `docs/PHASE2.md` §3.2
//! forbids that "simplification".
//!
//! [`MappedArena::attach`] *verifies* the seals rather than trusting them, and
//! refuses an unsealed segment. A peer that hands you an unsealed fd is either
//! buggy or hostile, and the two are indistinguishable from here.
//!
//! # Trust model
//!
//! Per `docs/PHASE2.md` §0: participants are mutually trusting, same-user,
//! cooperating processes. A read-write peer can corrupt any part of the arena
//! and no checksum changes that. The **read-only** attach mode is the one real
//! boundary, and it is enforced by the MMU, not by convention — which is why it
//! is the right default for consumers.

use core::ptr::NonNull;

use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use rustix::fs::{
    fcntl_add_seals, fcntl_get_seals, ftruncate, memfd_create, MemfdFlags, SealFlags,
};
use rustix::mm::{madvise, mmap, munmap, Advice, MapFlags, ProtFlags};

use crate::header::{ArenaHeader, FORMAT_VERSION, TF_TREE_MAGIC};
use crate::heap::{write_header_at, Arena};
use crate::layout::{layout_hash, ArenaLayout};

/// Seals every tf_tree segment carries. Checked, not assumed, on attach.
const REQUIRED_SEALS: SealFlags = SealFlags::SHRINK.union(SealFlags::GROW);

/// How a process attaches to an existing segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachMode {
    /// `PROT_READ` only. The consumer default: the MMU makes corruption
    /// impossible rather than merely impolite.
    ReadOnly,
    /// `PROT_READ | PROT_WRITE`. Required to publish samples or claim edges.
    ReadWrite,
}

/// Everything that can go wrong obtaining or validating a shared segment.
///
/// `Copy` and `String`-free, like every other error in this workspace
/// (`docs/PROJECT.md` §5): an errno and what was being attempted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShmError {
    /// `memfd_create` failed.
    Create(rustix::io::Errno),
    /// `ftruncate` to the arena size failed.
    Truncate(rustix::io::Errno),
    /// `mmap` failed.
    Map(rustix::io::Errno),
    /// `F_ADD_SEALS` failed. The segment is not safe to share.
    Seal(rustix::io::Errno),
    /// `F_GET_SEALS` failed, so the seals could not be verified.
    SealQuery(rustix::io::Errno),
    /// The segment is missing `F_SEAL_SHRINK`/`F_SEAL_GROW`, so it could be
    /// truncated under a reader and fault it with `SIGBUS`.
    Unsealed,
    /// `fstat` on the segment failed.
    Stat(rustix::io::Errno),
    /// The fd's size disagrees with the header's `arena_size`.
    SizeMismatch {
        /// Bytes the segment actually has.
        actual: u64,
        /// Bytes the header claims.
        expected: u64,
    },
    /// The first eight bytes are not `TF_TREE_MAGIC` — not a tf_tree arena.
    BadMagic,
    /// The segment was written by a different `FORMAT_VERSION`.
    VersionMismatch {
        /// Version found in the segment.
        found: u32,
        /// Version this build speaks.
        expected: u32,
    },
    /// The segment's record layout differs from this build's. Attaching anyway
    /// would reinterpret every offset.
    LayoutMismatch {
        /// Hash found in the segment.
        found: u32,
        /// Hash this build computes.
        expected: u32,
    },
    /// The segment is smaller than an `ArenaHeader`, so it cannot even be
    /// validated.
    TooSmall,
    /// The header's region offsets do not match the geometry its own capacities
    /// imply, so the regions cannot be trusted to lie within the segment.
    ///
    /// Distinct from [`ShmError::LayoutMismatch`], which compares against a
    /// *build* constant: this catches a header that is internally inconsistent,
    /// whether from a peer bug, a scribbled byte, or a build that shares this
    /// one's record sizes but not its capacities.
    HeaderInconsistent,
}

/// An [`Arena`] backed by a sealed `memfd` mapped `MAP_SHARED`.
///
/// The whole point of this type is that nothing above it knows it exists: the
/// stack is written against [`Arena`], so the same reader code runs unmodified
/// against a [`crate::heap::HeapArena`] and against a segment shared by another
/// process.
pub struct MappedArena {
    base: NonNull<u8>,
    len: usize,
    fd: OwnedFd,
    writable: bool,
}

impl MappedArena {
    /// Create a new sealed segment sized for `layout` and write its header.
    ///
    /// Follows `docs/PHASE2.md` §3.2. Sealing happens **after** the mapping is
    /// established, which is what lets the creator keep write access:
    /// `F_ADD_SEALS SHRINK|GROW` succeeds while a writable mapping is held,
    /// whereas `F_SEAL_WRITE` would return `EBUSY` — so the size is frozen
    /// without freezing the contents.
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
        creator_boot_id: u64,
    ) -> Result<MappedArena, ShmError> {
        const {
            assert!(
                cfg!(target_endian = "little"),
                "tf_tree arenas are little-endian only"
            );
        }
        let len = layout.total_size();

        // `MFD_ALLOW_SEALING` is required for step 5 below; without it
        // `F_ADD_SEALS` returns EPERM and the segment can never be made
        // SIGBUS-safe. `MFD_CLOEXEC` so a segment is never leaked into an
        // unrelated child by accident — sharing is always deliberate.
        let cname = CName::new(name);
        let fd = memfd_create(
            cname.as_cstr(),
            MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
        )
        .map_err(ShmError::Create)?;
        ftruncate(&fd, len as u64).map_err(ShmError::Truncate)?;

        // MAP_POPULATE prefaults the whole arena. `docs/PHASE2.md` §7.1 wants it
        // for a reason that matters more than throughput: without it the *first*
        // touch of each page takes a fault, so the very first lookup after
        // attach — often inside a control loop's first iteration — pays a
        // page-fault storm that never shows up in a steady-state benchmark.
        let base = unsafe_map(len, ProtFlags::READ | ProtFlags::WRITE, &fd)?;

        // SAFETY: `base` addresses `len` freshly zeroed bytes (a memfd is
        // zero-filled and was just sized), page-aligned hence 64-byte aligned,
        // and no other mapping of this fd exists yet, so this call uniquely owns
        // the region.
        unsafe { write_header_at(base.as_ptr(), len, layout, creator_pid, creator_boot_id) };

        // Step 5, the load-bearing one. SEAL itself prevents any future seal
        // being added, so a peer cannot later add F_SEAL_WRITE and freeze the
        // writer out.
        fcntl_add_seals(&fd, REQUIRED_SEALS | SealFlags::SEAL).map_err(ShmError::Seal)?;

        let arena = MappedArena {
            base,
            len,
            fd,
            writable: true,
        };
        arena.advise();
        Ok(arena)
    }

    /// Map an existing segment from a received fd, validating it first.
    ///
    /// Follows `docs/PHASE2.md` §3.3 steps 4-6. Every check here is a refusal to
    /// trust a peer about something this process can verify itself.
    ///
    /// # Errors
    ///
    /// [`ShmError::Unsealed`] if the segment could be truncated under us;
    /// [`ShmError::BadMagic`], [`ShmError::VersionMismatch`],
    /// [`ShmError::LayoutMismatch`] or [`ShmError::SizeMismatch`] if it is not a
    /// segment this build can read.
    pub fn attach(fd: OwnedFd, mode: AttachMode) -> Result<MappedArena, ShmError> {
        // Refuse an unsealed segment *before* mapping it. Once mapped, a
        // truncation by any fd holder turns every subsequent read into SIGBUS,
        // and a library cannot recover from that.
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
            base,
            len,
            fd,
            writable: mode == AttachMode::ReadWrite,
        };
        arena.advise();

        // Validate the header only now that it is mapped. On any failure the
        // `MappedArena` is dropped, unmapping cleanly.
        let h = arena.header();
        if h.magic != u64::from_le_bytes(TF_TREE_MAGIC) {
            return Err(ShmError::BadMagic);
        }
        if h.format_version != FORMAT_VERSION {
            return Err(ShmError::VersionMismatch {
                found: h.format_version,
                expected: FORMAT_VERSION,
            });
        }
        // The layout hash is what makes a mismatched build fail loudly instead
        // of reading every region at the wrong offset.
        if h.layout_hash != layout_hash() {
            return Err(ShmError::LayoutMismatch {
                found: h.layout_hash,
                expected: layout_hash(),
            });
        }
        if h.arena_size != size {
            return Err(ShmError::SizeMismatch {
                actual: size,
                expected: h.arena_size,
            });
        }

        // `layout_hash()` is a *build* constant — it pins the record sizes this
        // binary was compiled against, not this segment's capacities. Two builds
        // can agree on it and disagree about `max_frames`. Since `ArenaView`
        // forms slices straight off these offsets, an inconsistent header would
        // produce out-of-bounds reads rather than an error, so recompute the
        // geometry the header's own counts imply and require it to match.
        //
        // `from_totals` is exact here: the region layout depends only on the
        // *sum* of the per-edge capacities, which is `stamp_slots`.
        let implied = ArenaLayout::from_totals(h.max_frames, h.max_edges, h.stamp_slots)
            .map_err(|_| ShmError::HeaderInconsistent)?;
        let matches = implied.total_size() as u64 == h.arena_size
            && implied.frame_table().offset as u32 == h.frame_table_off
            && implied.frame_hash().offset as u32 == h.frame_hash_off
            && implied.topo_blocks().offset as u32 == h.topo_block_off
            && implied.topo_block_stride() as u32 == h.topo_block_stride
            && implied.claim_table().offset as u32 == h.claim_table_off
            && implied.edge_table().offset as u32 == h.edge_table_off
            && implied.stamp_arena().offset as u32 == h.stamp_arena_off
            && implied.pose_arena().offset as u32 == h.pose_arena_off
            && h.stamp_slots == h.pose_slots;
        if !matches {
            return Err(ShmError::HeaderInconsistent);
        }
        Ok(arena)
    }

    /// Apply the mapping policy from `docs/PHASE2.md` §7. Both calls are
    /// best-effort: a kernel without transparent huge pages, or a mapping the
    /// kernel declines to mark, is not a reason to fail an attach.
    fn advise(&self) {
        // MADV_DONTFORK is the easy one to forget (§7.3) and the consequences
        // are subtle: a forked child would otherwise inherit the mapping and
        // become an invisible participant that no registry knows about, holding
        // the segment alive and potentially writing to it.
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

/// `mmap` `len` bytes of `fd` shared and prefaulted.
fn unsafe_map(len: usize, prot: ProtFlags, fd: &OwnedFd) -> Result<NonNull<u8>, ShmError> {
    // SAFETY: `mmap` with a null hint lets the kernel choose an address, so no
    // existing mapping can be replaced. `len` is the segment's size and `fd`
    // refers to a memfd of at least that size (just `ftruncate`d, or `fstat`ed).
    // The returned pointer is checked for null by `NonNull::new`.
    let raw = unsafe {
        mmap(
            core::ptr::null_mut(),
            len,
            prot,
            MapFlags::SHARED | MapFlags::POPULATE,
            fd,
            0,
        )
    }
    .map_err(ShmError::Map)?;
    NonNull::new(raw.cast::<u8>()).ok_or(ShmError::Map(rustix::io::Errno::NOMEM))
}

impl Drop for MappedArena {
    fn drop(&mut self) {
        // SAFETY: module invariant — `base`/`len` are exactly what `mmap`
        // returned for this arena, unmapped here exactly once.
        let _ = unsafe { munmap(self.base.as_ptr().cast(), self.len) };
    }
}

// SAFETY: `MappedArena` owns its mapping and exposes only the base pointer and
// length. It hands out no interior references that alias the bytes, and all
// concurrent access — within this process or from another — is mediated by the
// atomic protocols in `tf_tree_core`.
unsafe impl Send for MappedArena {}
// SAFETY: see the `Send` impl above.
unsafe impl Sync for MappedArena {}

// SAFETY: `base()`/`len()` describe one live mapping at a fixed page-aligned
// address, valid for `len` bytes until `Drop`. The seals verified in `attach`
// (and applied in `create`) are what make `len` immutable for the fd's lifetime,
// so the region cannot be truncated out from under a reader.
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
/// `memfd_create` wants a `&CStr` and this crate is `no_std + alloc` with a
/// deliberately tiny dependency budget, so building one without pulling in
/// anything is worth 20 lines. The name is debug-only — it shows up in
/// `/proc/<pid>/fd` — so silently truncating an over-long one is the right
/// failure mode.
struct CName {
    buf: [u8; Self::CAP],
    len: usize,
}

impl CName {
    const CAP: usize = 64;

    fn new(name: &str) -> CName {
        let mut buf = [0u8; Self::CAP];
        let src = name.as_bytes();
        // Truncate at the first interior NUL. `from_bytes_with_nul_unchecked`
        // requires exactly one NUL, at the end, and `name` is arbitrary caller
        // input — `build_shared("a\0b")` would otherwise violate that contract.
        // The kernel would stop at the first NUL anyway, so this only makes the
        // Rust-side invariant match what actually happens.
        let end = src.iter().position(|&b| b == 0).unwrap_or(src.len());
        // Leave room for the terminator. The kernel treats the name as opaque
        // bytes, so a truncated multi-byte sequence is harmless — it is a debug
        // label in /proc/<pid>/fd, nothing more.
        let n = core::cmp::min(end, Self::CAP - 1);
        buf[..n].copy_from_slice(&src[..n]);
        CName { buf, len: n }
    }

    fn as_cstr(&self) -> &core::ffi::CStr {
        // SAFETY: `buf` was zero-initialized and only `buf[..len]` was written
        // with `len <= CAP - 1`, so `buf[len]` is a NUL and the slice up to and
        // including it contains exactly one NUL, at the end.
        unsafe { core::ffi::CStr::from_bytes_with_nul_unchecked(&self.buf[..=self.len]) }
    }
}
