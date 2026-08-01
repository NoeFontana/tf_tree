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

use crate::check::{validate_arena_header, ShmError};
use crate::header::{ArenaHeader, TOPO_BLOCKS};
use crate::heap::{write_header_at, Arena};
use crate::layout::ArenaLayout;

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

        // **No `MAP_POPULATE`** — see `unsafe_map`, which is where that decision
        // and its measurement live. `docs/PHASE2.md` §7.1 is NORMATIVE that
        // population happens at declaration granularity; mapping the whole arena
        // eagerly charged 66.3 MiB of RSS against 66.1 MiB declared.
        // `MappedArena::populate_hot` puts back exactly the pages that are read.
        let base = unsafe_map(len, ProtFlags::READ | ProtFlags::WRITE, &fd)?;

        // **Take ownership of the mapping before the first fallible step.** Every
        // `?` below returns early, and until this value exists there is no `Drop`
        // to `munmap`: a failed `getrandom` or `F_ADD_SEALS` would strand the
        // segment's address space, and — because the mapping holds its own
        // reference to the memfd inode — its committed pages too, for the life of
        // the process. Dropping `fd` does not release them. The whole reason
        // `MappedArena` owns the mapping is that `Drop` unmaps it exactly once;
        // the construction was simply on the wrong side of the fallible steps.
        let arena = MappedArena {
            owner_pid: rustix::process::getpid(),
            base,
            len,
            fd,
            writable: true,
        };

        let uuid = instance_uuid()?;
        // SAFETY: `arena.base` addresses `len` freshly zeroed bytes (a memfd is
        // zero-filled and was just sized), page-aligned hence 64-byte aligned,
        // and no other mapping of this fd exists yet, so this call uniquely owns
        // the region.
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

        // Step 5, the load-bearing one. SEAL itself prevents any future seal
        // being added, so a peer cannot later add F_SEAL_WRITE and freeze the
        // writer out.
        fcntl_add_seals(&arena.fd, REQUIRED_SEALS | SealFlags::SEAL).map_err(ShmError::Seal)?;

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
        //
        // The checks themselves live in `crate::check` because the frozen-file
        // backend must make exactly the same ones, and a check that exists on
        // one path and not the other is a hole with a filename on it.
        validate_arena_header(arena.header(), size)?;
        Ok(arena)
    }

    /// Fault in `[offset, offset + len)` of this arena, up front.
    ///
    /// # Why this is not `MADV_WILLNEED`
    ///
    /// `docs/PHASE2.md` §7.1: *"`MADV_WILLNEED` does not work here (measured:
    /// zero change in charged pages on a memfd). Do not substitute it."*
    /// `WILLNEED` is a readahead hint for page-cache-backed mappings; a memfd's
    /// pages are already in the page cache, so it has nothing to do. What is
    /// needed is population of the *page tables*, which is `MADV_POPULATE_*`.
    ///
    /// # Read versus write
    ///
    /// `MADV_POPULATE_WRITE` on a `PROT_READ` mapping is `EINVAL`, so the advice
    /// follows the mapping's protection. For a writable mapping `WRITE` is the
    /// right one even though nothing is written yet: `POPULATE_READ` on a
    /// private-writable page would fault in the shared zero page and leave the
    /// first *store* to take a copy-on-write fault, which is the fault this
    /// exists to remove. (`MAP_SHARED` makes that moot here, but the rule is
    /// worth not having to re-derive.)
    ///
    /// # Errors
    ///
    /// Never. `MADV_POPULATE_*` landed in Linux 5.14 and returns `EINVAL` on
    /// anything older; that case falls back to touching the pages by hand, which
    /// is what the kernel would have done anyway. Any other errno means the
    /// pages stay cold and the first access faults — slower, never incorrect —
    /// so this returns `()` rather than an error nobody could act on.
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

    /// Kernels before 5.14: fault the pages in by touching one byte per page.
    ///
    /// A **read** of each page, never a write, on both mapping modes. A write
    /// would be a correctness bug rather than a slow path: this runs on a
    /// segment other processes are already using, and storing anything — even
    /// the byte that is already there — into a live claim record or sample slot
    /// races every reader of it. A read fault populates the page table entry,
    /// which is the entire objective.
    fn populate_by_touch(&self, offset: usize, len: usize) {
        for at in touch_offsets(offset, len) {
            // SAFETY: `at` is within `[offset, offset + len)` — see
            // `touch_offsets`, which is where that is established and tested —
            // and the caller has already clamped that range to this arena's
            // mapping. `read_volatile` is used so the load cannot be optimised
            // away: the fault it takes is the only reason the load exists.
            unsafe {
                core::ptr::read_volatile(self.base.as_ptr().add(at));
            }
        }
    }

    /// The first 256 bytes of the arena, for tests that assert nothing wrote to
    /// it.
    #[cfg(test)]
    fn header_snapshot(&self) -> [u8; 256] {
        let mut out = [0u8; 256];
        // SAFETY: module invariant — the mapping is at least 256 bytes (it is at
        // least `size_of::<ArenaHeader>()`, which is 320 since FORMAT_VERSION 3
        // and was 256 before it), and this only reads. The snapshot deliberately
        // stays 256: it exists to compare the *pinned* header prefix across a
        // remap, and every field it is used to check lives below 256.
        unsafe { core::ptr::copy_nonoverlapping(self.base.as_ptr(), out.as_mut_ptr(), 256) };
        out
    }

    /// Populate every region that is actually read, and nothing else (§7.1).
    ///
    /// # What "actually read" means, and why the header can answer it
    ///
    /// §7.1 says to populate at *declaration* granularity. Decision `0004` moved
    /// declaration to build time, so there is no `declare_dynamic` to hook — but
    /// the arena records what was declared: `frame_count` and `edge_count` are
    /// live counters in the header. So an **attaching** process derives the used
    /// extents itself, with nothing passed in and no agreement to keep in sync
    /// with the builder.
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
    /// | stamp + pose arenas | all — under `0004` they are sized to the declared rings exactly |
    /// | edge counters | `edge_count` records — written by `Guard::drop` on every read batch |
    /// | participant counters | all (8 KiB) — same path, keyed by the reader's own slot |
    ///
    /// The headroom tails are what this leaves cold, and they are the whole
    /// win: on the measured arena above, 66 MiB of it.
    ///
    /// Frames interned *after* this runs fault once, which is correct — that is
    /// a rare path, and pre-faulting a 200 000-frame table on the chance that
    /// one more name shows up is exactly what §7.1 forbids.
    pub fn populate_hot(&self) {
        // SAFETY: module invariant — the mapping is at least `size_of::<ArenaHeader>()`
        // bytes (checked by `attach`, and by construction in `create`), aligned,
        // and the header is only ever read through this shared reference.
        let h = unsafe { &*self.base.as_ptr().cast::<ArenaHeader>() };
        let frames = h.frame_count.load(core::sync::atomic::Ordering::Acquire) as usize;
        let edges = h.edge_count.load(core::sync::atomic::Ordering::Acquire) as usize;

        self.populate(0, core::mem::size_of::<ArenaHeader>());
        self.populate(h.frame_table_off as usize, frames * 64);

        // The four topology blocks are strided, not contiguous, so each one's
        // used prefix is populated separately. Populating `blocks * stride` from
        // the first would pull in three blocks' worth of headroom.
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
        self.populate(h.stamp_arena_off as usize, h.stamp_slots as usize * 8);
        self.populate(h.pose_arena_off as usize, h.pose_slots as usize * 64);

        // v3's counter regions (`docs/PHASE5.md` §5.2). These are not
        // diagnostics-only pages that a `top` invocation happens to touch:
        // `Guard::drop` does a `fetch_add` into `edge_counters` at the end of
        // every read batch and `note_err` writes there on every failure, so they
        // are on the *lookup* path of any read-write participant — exactly the
        // pages §7.1 exists to warm. Left out, an attaching process takes ~34
        // minor faults at 1-3 µs each inside a control loop's first iterations,
        // against a 150 ns p50 budget.
        self.populate(h.edge_counters_off as usize, edges * 128);
        self.populate(
            h.participant_counters_off as usize,
            h.max_participants as usize * 128,
        );
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

/// Byte offsets to touch so that every page overlapping `[offset, offset + len)`
/// is faulted in — one per page, plus the first, and **never past the end**.
///
/// Pulled out of [`MappedArena::populate_by_touch`] as a pure function precisely
/// so its bound can be tested. The kernel side of that function is not
/// observable from inside this crate — residency needs `mincore`, which
/// `rustix` does not have and which is not worth a `libc` dependency here — so a
/// test of the *effect* cannot distinguish "touched every page" from "stopped
/// one page short". A test of the arithmetic can, and the arithmetic is the part
/// that can be wrong.
///
/// Every yielded offset is `< offset + len`, which is what makes the `unsafe`
/// read in the caller in-bounds.
fn touch_offsets(offset: usize, len: usize) -> impl Iterator<Item = usize> {
    const PAGE: usize = 4096;
    // The first page of the range starts at `offset`, which is not necessarily
    // page-aligned; stepping from the *aligned* base instead would touch a page
    // before the range.
    (0..len).step_by(PAGE).map(move |d| offset + d)
}

/// `mmap` `len` bytes of `fd` shared, **without** prefaulting.
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
            // No `MAP_POPULATE`: `docs/PHASE2.md` §7.1 is NORMATIVE that
            // population happens at declaration granularity, not over the whole
            // address space. Measured, on an arena declaring one 1024-slot edge
            // with 200k slots of frame/edge headroom: `MAP_POPULATE` charged
            // **66.3 MiB of RSS against 66.1 MiB declared** — essentially all of
            // it headroom nobody asked for and nothing ever reads.
            //
            // `MappedArena::populate_hot` puts back exactly the pages that are
            // actually touched, and reports failure, which `MAP_POPULATE`
            // cannot: it is best-effort and silent.
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
    use crate::header::{FORMAT_VERSION, TF_TREE_MAGIC};
    use crate::layout::layout_hash;
    use alloc::vec;

    fn fixture() -> ArenaLayout {
        ArenaLayout::new(8, 4, vec![16, 0, 4, 64]).unwrap()
    }

    fn create() -> MappedArena {
        MappedArena::create("tf_tree.uuid_test", &fixture(), 1234, 5678, [7; 16]).unwrap()
    }

    /// **The fallback must not write.** It runs on a segment other processes
    /// are already using, so a store — even of the byte that is already there —
    /// races every reader of a live claim record or sample slot. That would be a
    /// correctness bug, not a slow path.
    ///
    /// `MADV_POPULATE_*` landed in Linux 5.14, so on every machine this is
    /// developed and tested on, [`MappedArena::populate`] takes the `madvise`
    /// branch and this path is dead code that ships anyway; calling it directly
    /// is the only way it is exercised at all.
    ///
    /// What this **cannot** show is that every page was touched: residency is
    /// not observable from inside this crate. That is why the bound lives in
    /// [`touch_offsets`] and is tested there.
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

    /// The fallback's bound, tested where it is observable.
    ///
    /// Residency is not visible from inside this crate, so the effect of
    /// `populate_by_touch` cannot be distinguished from a loop that stops a page
    /// early — which is why the bound lives in a pure function. Mutant:
    /// `while at + PAGE < end` in the original loop shape, i.e. dropping the
    /// final partial page ⇒ the last two cases below fail.
    #[test]
    fn touch_offsets_covers_every_page_and_never_passes_the_end() {
        let v = |o, l| touch_offsets(o, l).collect::<alloc::vec::Vec<_>>();
        assert_eq!(v(0, 0), alloc::vec![]);
        assert_eq!(v(0, 1), alloc::vec![0]);
        assert_eq!(v(0, 4096), alloc::vec![0]);
        assert_eq!(v(0, 4097), alloc::vec![0, 4096]);
        assert_eq!(v(0, 8192), alloc::vec![0, 4096]);
        // A range that starts mid-page must touch *that* page, not the aligned
        // one before it.
        assert_eq!(v(100, 1), alloc::vec![100]);
        assert_eq!(v(4095, 2), alloc::vec![4095]);
        // The last partial page is still a page, and skipping it leaves exactly
        // the fault this code exists to remove.
        assert_eq!(v(0, 4096 * 3 + 1), alloc::vec![0, 4096, 8192, 12288]);
        for (o, l) in [(0usize, 12345usize), (7, 99999), (4095, 4097)] {
            for at in touch_offsets(o, l) {
                assert!(at >= o && at < o + l, "{at} escaped [{o}, {o}+{l})");
            }
        }
    }

    /// `populate` must never walk off the end, whatever it is asked for.
    ///
    /// The clamp is the only thing between a caller's arithmetic slip and an
    /// `madvise` over memory this arena does not own.
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

    /// **The seal check is the whole `memfd`-not-`shm_open` argument**, and it
    /// runs before the segment is mapped: once mapped, any fd holder could
    /// `ftruncate` it and every subsequent read would fault with `SIGBUS` from
    /// inside a lookup, which a library cannot recover from.
    ///
    /// Mutant: delete the `seals.contains(REQUIRED_SEALS)` guard in `attach` ⇒
    /// the unsealed case below maps happily and this fails. Nothing else in the
    /// workspace exercises it — every other test attaches to a segment `create`
    /// has just sealed for it.
    #[test]
    fn an_unsealed_or_undersized_segment_is_refused_before_it_is_mapped() {
        let len = fixture().total_size() as u64;

        // No `ALLOW_SEALING`, so the segment can never be sealed and a peer
        // could shrink it under us.
        let raw = memfd_create(c"tf_tree.unsealed", MemfdFlags::CLOEXEC).unwrap();
        ftruncate(&raw, len).unwrap();
        let refused = MappedArena::attach(raw, AttachMode::ReadOnly).err();
        assert_eq!(refused, Some(ShmError::Unsealed));

        // Sealed, but too small to hold a header — so the header cannot even be
        // read to find out what the segment claims to be.
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
    /// is rejected by value, naming both sides.
    ///
    /// Each case is a single-field edit to an otherwise perfectly good segment,
    /// which is the shape of the real failure: the same binary, rebuilt. Mutant:
    /// drop any one of the three comparisons in `validate_arena_header` ⇒ the
    /// corresponding case here reports `None` or the next error down, and fails.
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
            // no other process holds, and its base is a live, page-aligned
            // (hence 64-byte aligned), initialized `ArenaHeader`. No other
            // reference to it is live across this call.
            unsafe { poke(&mut *owner.base().cast::<ArenaHeader>()) };
            let fd = rustix::io::fcntl_dupfd_cloexec(owner.as_raw_fd(), 0).unwrap();
            let refused = MappedArena::attach(fd, AttachMode::ReadOnly).err();
            assert_eq!(refused, Some(want));
        }
    }

    /// `CName::as_cstr`'s `from_bytes_with_nul_unchecked` requires **exactly
    /// one** NUL, at the end — and `MappedArena::create` takes the name from an
    /// arbitrary caller (`tf_tree::TreeBuilder::build_shared` passes it
    /// straight through), so `create("a\0b")` is reachable public API and this
    /// truncation is the sole guarantor of that precondition.
    ///
    /// Mutant: drop the interior-NUL truncation in `CName::new` ⇒ the buffer
    /// holds two NULs, the `unsafe` becomes unsound, and the `"a\0b"` case
    /// fails. Mutant: use `CAP` instead of `CAP - 1` for the length bound ⇒ the
    /// terminator is overwritten and the long case fails.
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
        // And the truncated name still reaches the kernel, rather than being
        // refused: the whole point of truncating instead of erroring.
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
