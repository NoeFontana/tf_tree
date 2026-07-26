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
    /// `getrandom` could not fill the arena's `instance_uuid`.
    ///
    /// Deliberately fatal rather than falling back to a counter or a timestamp:
    /// a *guessable* instance id still compares equal to itself, so the
    /// split-brain check it exists for would keep passing while no longer
    /// meaning anything.
    Random(rustix::io::Errno),
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
    /// Every participant slot is taken, so this process cannot join.
    ParticipantTableFull,
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
    /// The process that established this mapping.
    ///
    /// The mapping is `MADV_DONTFORK` (§7.3), so a `fork` child does not have
    /// it — the address range is a *hole* in the child's address space. An
    /// unguarded `munmap` there is usually a harmless no-op, but "usually" is
    /// doing real work in that sentence: nothing stops the kernel from placing a
    /// later mapping of the child's own into that hole, and the destructor would
    /// then unmap memory belonging to something else entirely, at a distance,
    /// with no diagnostic.
    ///
    /// `getpid` is a syscall, and that is affordable *here* and nowhere else:
    /// this runs once per arena teardown. The equivalent check on the hot path
    /// is a `pthread_atfork` counter (`tf_tree_ipc::fork`), which this crate
    /// deliberately does not depend on — the dependency would point the wrong
    /// way, and a destructor can pay 50 ns.
    ///
    /// **Coverage, stated plainly: no test fails when this check is removed.**
    /// `crates/tf_tree_bench/tests/fork.rs` was run against that mutant and
    /// stayed green, because `munmap` on a hole succeeds and does nothing — the
    /// damage needs the child to have placed a mapping of its own in that hole
    /// first, which the harness does not arrange and cannot arrange without a
    /// public accessor for the arena's base address that exists for no other
    /// reason. Kept anyway: the state is reachable by any child that allocates
    /// enough, the failure is silent and at a distance, and nothing else guards
    /// it.
    owner_pid: rustix::process::Pid,
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
        unsafe {
            write_header_at(
                base.as_ptr(),
                len,
                layout,
                creator_pid,
                owner_start_time,
                boot_id,
                instance_uuid()?,
            )
        };

        // Step 5, the load-bearing one. SEAL itself prevents any future seal
        // being added, so a peer cannot later add F_SEAL_WRITE and freeze the
        // writer out.
        fcntl_add_seals(&fd, REQUIRED_SEALS | SealFlags::SEAL).map_err(ShmError::Seal)?;

        let arena = MappedArena {
            owner_pid: rustix::process::getpid(),
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
            owner_pid: rustix::process::getpid(),
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
            // A6's region. Omitting these was a real hole: `ArenaView::participants`
            // builds a slice from `participant_table_off` and `max_participants`
            // and its SAFETY comment cites *this* check as what bounds them, so a
            // header carrying garbage there produced an out-of-bounds slice —
            // exactly the failure this validation exists to prevent.
            && implied.participant_table().offset as u32 == h.participant_table_off
            && implied.max_participants() == h.max_participants
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

/// Draw 16 random bytes for a new arena's `instance_uuid`.
///
/// `getrandom` is documented not to return a short read for buffers this small
/// **except when interrupted by a signal**, and "except when interrupted" is the
/// entire hazard here: a partially-filled uuid is still 16 bytes that look
/// random, so a short read would not announce itself anywhere downstream. The
/// loop therefore refills from where it stopped rather than assuming one call
/// suffices.
///
/// The call **blocks** (no `GRND_NONBLOCK`), deliberately. Arena creation is a
/// startup operation, so waiting for the entropy pool on a freshly-booted
/// embedded target is correct where spinning on `EAGAIN` would not be — and
/// with blocking flags `EAGAIN` cannot be returned at all, so there is no arm
/// for it to hide in.
fn instance_uuid() -> Result<[u8; 16], ShmError> {
    use rustix::rand::{getrandom, GetRandomFlags};

    let mut uuid = [0u8; 16];
    let mut filled = 0;
    while filled < uuid.len() {
        match getrandom(&mut uuid[filled..], GetRandomFlags::empty()) {
            // A zero-length read with no error would spin forever; there is no
            // legitimate way for `getrandom` to make no progress on a non-empty
            // buffer, so treat it as the I/O failure it is.
            Ok(0) => return Err(ShmError::Random(rustix::io::Errno::IO)),
            Ok(n) => filled += n,
            // A blocking `getrandom` is interruptible; every other errno is a
            // real failure and must not be retried.
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(ShmError::Random(e)),
        }
    }
    Ok(uuid)
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
        // See `owner_pid`. In a `fork` child this range is not ours to unmap.
        if rustix::process::getpid() != self.owner_pid {
            return;
        }
        // SAFETY: module invariant — `base`/`len` are exactly what `mmap`
        // returned for this arena, unmapped here exactly once. The `getpid`
        // guard above additionally establishes that this is the process the
        // mapping was made in, so the range is still ours.
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use alloc::vec;

    fn fixture() -> ArenaLayout {
        ArenaLayout::new(8, 4, vec![16, 0, 4, 64]).unwrap()
    }

    fn create() -> MappedArena {
        MappedArena::create("tf_tree.uuid_test", &fixture(), 1234, 5678, [7; 16]).unwrap()
    }

    /// The point of an instance id is to tell two *different* segments apart.
    ///
    /// A constant would satisfy every other assertion in this file — the field
    /// would round-trip through `attach`, land at the right offset, and survive
    /// sealing — while making the split-brain check (`docs/PHASE2.md` §11.2
    /// scenario 9) compare equal for two unrelated arenas, which is exactly the
    /// answer it must never give.
    #[test]
    fn two_arenas_never_share_an_instance_uuid() {
        let a = create();
        let b = create();
        assert_ne!(a.header().instance_uuid, b.header().instance_uuid);
        // ...and neither is the all-zero "not a shared instance" sentinel that
        // a heap arena writes.
        assert_ne!(a.header().instance_uuid, [0; 16]);
        assert_ne!(b.header().instance_uuid, [0; 16]);
    }

    /// A joiner must read the *creator's* id, not one of its own.
    ///
    /// This is the direction the wire depends on: `HelloResponse` carries the
    /// owner's `instance_uuid` and the client compares it against the header it
    /// just mapped. If `attach` minted a fresh id the comparison would fail for
    /// every legitimate join.
    #[test]
    fn attach_preserves_the_creators_instance_uuid() {
        let created = create();
        let uuid = created.header().instance_uuid;
        // Assert non-zero *before* comparing: if `write_header_at` never wrote
        // the field, both sides would read all-zero and the equality below would
        // hold while proving nothing.
        assert_ne!(uuid, [0; 16], "instance_uuid was never written");

        let fd = rustix::io::fcntl_dupfd_cloexec(created.as_raw_fd(), 0).unwrap();
        let attached = MappedArena::attach(fd, AttachMode::ReadOnly).unwrap();

        assert_eq!(attached.header().instance_uuid, uuid);
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
