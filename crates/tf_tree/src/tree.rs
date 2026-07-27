//! The concrete [`Tree`] (owning a heap arena), its [`TreeBuilder`], the
//! per-edge [`Capacity`]/[`EdgeCfg`] declarations, and the `Display` wrapper
//! [`Described`].
//!
//! Per decision `0004` (`docs/decisions/0004-builder-time-edge-declaration.md`,
//! still authoritative for this API), the tree's topology is declared on the [`TreeBuilder`]
//! *before* [`TreeBuilder::build`]. `build()` sizes the arena from exactly those
//! declarations (via `ArenaLayout` / `from_edges` sizing), so the arena reserves
//! ring slots **only** for dynamic edges, each sized to its own capacity. There
//! is no post-build `declare_*`; the only runtime topology change is
//! [`Tree::reparent`], which reuses an already-declared edge and allocates no new
//! capacity.

use std::collections::HashSet;
use std::fmt;
use std::sync::atomic::Ordering;
use std::sync::Mutex;

use tf_tree_arena::{Arena, ArenaLayout, HeapArena, LayoutError};
#[cfg(all(feature = "shm", target_os = "linux"))]
use tf_tree_arena::{AttachMode, MappedArena, ShmError};
use tf_tree_core::arena_view::{ArenaBuilder, ArenaView};
use tf_tree_core::edge::{claim, EdgeKind, EdgeRecord, Publisher};
use tf_tree_core::frame::blake3_64;
use tf_tree_core::plan::{compile, Domain, EdgeMeta, Guard, InterpPolicy, Stamp, SystemDomain};
use tf_tree_core::topology::{TopoLockError, TopoLockView};
use tf_tree_core::{
    EdgeId, FrameError, FrameId, LookupError, ParticipantError, PushError, TopologyError,
};
use tf_tree_math::Iso3;

use crate::cache;

/// Smallest power of two `>= n`, saturating at the largest `u32` power of two
/// (`1 << 31`). `next_pow2(0) == 1`, so a dynamic ring is never zero-length (a
/// zero capacity is what marks a *static* edge in the arena layout).
fn next_pow2_u32(n: u32) -> u32 {
    let mut p: u64 = 1;
    let target = u64::from(n);
    while p < target {
        p <<= 1;
    }
    if p > u64::from(u32::MAX) {
        1u32 << 31
    } else {
        p as u32
    }
}

/// Ring capacity for one dynamic edge, always a power of two.
///
/// A capacity may be given directly with [`Capacity::slots`] (rounded up to a
/// power of two) or as a retention window with [`Capacity::history`]. The window
/// form is the documented default idiom: it is how operators reason about
/// history depth ("keep 10 s at 1 kHz") and what URDF ingestion will feed.
///
/// ```
/// use tf_tree::Capacity;
/// assert_eq!(Capacity::slots(5000).get(), 8192);      // rounded up to 2^13
/// assert_eq!(Capacity::history(1000.0, 10.0).get(), 16384); // next_pow2(10_000)
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capacity(u32);

impl Capacity {
    /// A ring holding at least `n` samples, rounded **up** to a power of two.
    #[must_use]
    pub fn slots(n: u32) -> Capacity {
        Capacity(next_pow2_u32(n))
    }

    /// A ring sized to retain `secs` seconds of history at `rate_hz`:
    /// `next_pow2(ceil(rate_hz * secs))`. Non-finite or non-positive inputs
    /// collapse to the minimum one-slot ring.
    #[must_use]
    pub fn history(rate_hz: f64, secs: f64) -> Capacity {
        let needed = (rate_hz * secs).ceil();
        let clamped = if needed.is_finite() && needed >= 1.0 {
            if needed > f64::from(u32::MAX) {
                u32::MAX
            } else {
                needed as u32
            }
        } else {
            1
        };
        Capacity(next_pow2_u32(clamped))
    }

    /// The resolved power-of-two slot count.
    #[inline]
    #[must_use]
    pub fn get(self) -> u32 {
        self.0
    }
}

/// Per-edge configuration for [`TreeBuilder::dynamic_edge`].
///
/// `capacity` is required; `interp` and `domain` fall back to the builder
/// defaults ([`TreeBuilder::default_interp`] / [`TreeBuilder::default_domain`])
/// when left `None`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EdgeCfg {
    /// Ring capacity (a power of two; see [`Capacity`]).
    pub capacity: Capacity,
    /// Interpolation policy; `None` uses the builder default.
    pub interp: Option<InterpPolicy>,
    /// Time-domain tag (see [`Domain`]); `None` uses the builder default.
    pub domain: Option<u8>,
}

impl EdgeCfg {
    /// A config with the given `capacity` and builder-default interp/domain.
    #[must_use]
    pub fn new(capacity: Capacity) -> EdgeCfg {
        EdgeCfg {
            capacity,
            interp: None,
            domain: None,
        }
    }

    /// Override the interpolation policy for this edge.
    #[must_use]
    pub fn interp(mut self, interp: InterpPolicy) -> EdgeCfg {
        self.interp = Some(interp);
        self
    }

    /// Override the time-domain tag for this edge.
    #[must_use]
    pub fn domain(mut self, domain: u8) -> EdgeCfg {
        self.domain = Some(domain);
        self
    }
}

/// What kind of edge a declaration describes.
#[derive(Clone, Copy, Debug)]
enum EdgeDeclKind {
    /// A static edge carrying a constant pose `T_parent_child`.
    Static(Iso3),
    /// A dynamic edge backed by a ring of the given configuration.
    Dynamic(EdgeCfg),
}

/// One collected edge declaration, keyed by frame *names* (resolved to ids at
/// [`TreeBuilder::build`]).
#[derive(Clone, Debug)]
struct EdgeDecl {
    parent: String,
    child: String,
    kind: EdgeDeclKind,
}

/// Builder for a [`Tree`].
///
/// Collect the topology — frames and static/dynamic edges — then call
/// [`Self::build`]. `build()` derives the arena's frame and edge budgets from
/// exactly what was declared (plus the reserved id-0 sentinels and any optional
/// headroom) and reserves ring slots only for the dynamic edges.
///
/// ```
/// use tf_tree::{TreeBuilder, Capacity, EdgeCfg, Iso3};
///
/// let tree = TreeBuilder::new()
///     .dynamic_edge("odom", "base_link", EdgeCfg::new(Capacity::history(50.0, 10.0)))
///     .static_edge("base_link", "camera", &Iso3::IDENTITY)
///     .build()
///     .expect("layout");
/// ```
#[derive(Clone, Debug)]
pub struct TreeBuilder {
    default_interp: InterpPolicy,
    default_domain: u8,
    frames: Vec<String>,
    edges: Vec<EdgeDecl>,
    frame_headroom: u32,
    edge_headroom: u32,
}

impl Default for TreeBuilder {
    fn default() -> Self {
        TreeBuilder::new()
    }
}

impl TreeBuilder {
    /// An empty builder: no frames, no edges, `ScLerp` interpolation and the
    /// [`SystemDomain`] time domain as the per-edge defaults.
    #[must_use]
    pub fn new() -> TreeBuilder {
        TreeBuilder {
            default_interp: InterpPolicy::ScLerp,
            default_domain: SystemDomain::TAG,
            frames: Vec::new(),
            edges: Vec::new(),
            frame_headroom: 0,
            edge_headroom: 0,
        }
    }

    /// Default interpolation policy for dynamic edges that do not set their own.
    #[must_use]
    pub fn default_interp(mut self, interp: InterpPolicy) -> TreeBuilder {
        self.default_interp = interp;
        self
    }

    /// Default time-domain tag for dynamic edges that do not set their own.
    #[must_use]
    pub fn default_domain(mut self, domain: u8) -> TreeBuilder {
        self.default_domain = domain;
        self
    }

    /// Register a frame explicitly. Frames referenced by an edge are registered
    /// implicitly, so this is only needed for isolated frames (e.g. an
    /// unattached root used as a lookup endpoint). Registering the same name
    /// twice is harmless.
    #[must_use]
    pub fn frame(mut self, name: &str) -> TreeBuilder {
        self.frames.push(name.to_owned());
        self
    }

    /// Declare a static edge `parent -> child` carrying the constant pose `iso`
    /// (`T_parent_child`). Static edges reserve **zero** ring slots and are
    /// folded into constant plan steps. Referenced frames are registered
    /// implicitly.
    #[must_use]
    pub fn static_edge(mut self, parent: &str, child: &str, iso: &Iso3) -> TreeBuilder {
        self.edges.push(EdgeDecl {
            parent: parent.to_owned(),
            child: child.to_owned(),
            kind: EdgeDeclKind::Static(*iso),
        });
        self
    }

    /// Declare a dynamic edge `parent -> child` backed by a sample ring sized by
    /// `cfg.capacity`. Referenced frames are registered implicitly.
    #[must_use]
    pub fn dynamic_edge(mut self, parent: &str, child: &str, cfg: EdgeCfg) -> TreeBuilder {
        self.edges.push(EdgeDecl {
            parent: parent.to_owned(),
            child: child.to_owned(),
            kind: EdgeDeclKind::Dynamic(cfg),
        });
        self
    }

    /// Reserve extra empty frame slots beyond the declared frames (defaults to
    /// `0`). Only needed if new frame *names* will be interned at runtime.
    #[must_use]
    pub fn frame_headroom(mut self, n: u32) -> TreeBuilder {
        self.frame_headroom = n;
        self
    }

    /// Reserve extra empty (zero-capacity) edge slots beyond the declared edges
    /// (defaults to `0`).
    #[must_use]
    pub fn edge_headroom(mut self, n: u32) -> TreeBuilder {
        self.edge_headroom = n;
        self
    }

    /// Allocate the arena from the declared topology and build the tree.
    ///
    /// The frame budget is `unique_frames + 1` (slot 0 is the root sentinel) and
    /// the edge budget is `edges + 1` (`EdgeId 0` is the "no edge" sentinel), each
    /// plus any headroom. Ring slots are reserved only for dynamic edges, sized to
    /// their own capacities and laid out at cumulative offsets in `EdgeId` order.
    ///
    /// # Errors
    ///
    /// [`BuildError`] if two edges share a child, the declared counts overflow the
    /// `u32` id space, the capacities do not form a valid arena layout (e.g. the
    /// arena would exceed the `u32` offset model), a frame name collides on its
    /// 64-bit hash, or an edge would create a cycle.
    pub fn build(self) -> Result<Tree, BuildError> {
        let arena = self
            .build_with(|layout, pid, start, boot| Ok(HeapArena::new(layout, pid, start, boot)))?;
        let backing = ArenaBacking::Heap(arena);
        let (participant, incarnation) = register_participant(&ArenaView::new(backing.as_dyn()))
            .map_err(BuildError::Participant)?;
        let liveness = liveness_for(ArenaView::new(backing.as_dyn()).header().boot_id);
        #[cfg(all(feature = "shm", target_os = "linux"))]
        let fork_gen = fork_gen_for(&backing);
        Ok(Tree {
            arena: backing,
            participant,
            incarnation,
            liveness,
            decl: Mutex::new(()),
            #[cfg(all(feature = "shm", target_os = "linux"))]
            attachment: None,
            #[cfg(all(feature = "shm", target_os = "linux"))]
            claim_lock: None,
            #[cfg(all(feature = "shm", target_os = "linux"))]
            fork_gen,
        })
    }

    /// Build the tree into a **shared-memory** segment instead of the heap.
    ///
    /// The returned [`Tree`] behaves identically — the read path does not know
    /// which backend it has (`docs/PHASE2.md` §4) — but its arena lives in a
    /// sealed `memfd` that other processes can map with [`Tree::attach_shared`].
    /// Hand them [`Tree::shared_fd`].
    ///
    /// `name` is a debug label only; it appears in `/proc/<pid>/fd` and is
    /// truncated past 63 bytes. Segments are **not** discoverable by name — the
    /// fd is the capability, which is what keeps an unrelated process from
    /// attaching by guessing.
    ///
    /// # Errors
    ///
    /// [`BuildError`] as for [`TreeBuilder::build`], plus
    /// [`BuildError::Shm`] if the segment could not be created, sized, mapped or
    /// sealed.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    pub fn build_shared(self, name: &str) -> Result<Tree, BuildError> {
        let arena = self.build_with(|layout, pid, start, boot| {
            MappedArena::create(name, layout, pid, start, boot).map_err(BuildError::Shm)
        })?;
        // **After** `build_with`, not inside `create` (§7.1). Population is at
        // declaration granularity, and at `create` time nothing is declared yet
        // — `frame_count` and `edge_count` are still zero, so `populate_hot`
        // would fault in the header and stop. `build_with` is what interns the
        // frames and declares the edges, so this is the first moment the arena
        // can say what is actually in use.
        arena.populate_hot();
        let backing = ArenaBacking::Mapped(arena);
        let (participant, incarnation) = register_participant(&ArenaView::new(backing.as_dyn()))
            .map_err(BuildError::Participant)?;
        let liveness = liveness_for(ArenaView::new(backing.as_dyn()).header().boot_id);
        #[cfg(all(feature = "shm", target_os = "linux"))]
        let fork_gen = fork_gen_for(&backing);
        Ok(Tree {
            arena: backing,
            participant,
            incarnation,
            liveness,
            decl: Mutex::new(()),
            #[cfg(all(feature = "shm", target_os = "linux"))]
            attachment: None,
            #[cfg(all(feature = "shm", target_os = "linux"))]
            claim_lock: None,
            #[cfg(all(feature = "shm", target_os = "linux"))]
            fork_gen,
        })
    }

    /// The shared body of [`TreeBuilder::build`] and
    /// [`TreeBuilder::build_shared`]: everything except *which* allocation the
    /// bytes land in.
    ///
    /// Generic over the backend rather than duplicated, because the moment the
    /// two paths differ by a line, the heap and shared arenas stop being the
    /// same bytes — and "the same bytes, read by the same code" is the entire
    /// claim of Phase 2.
    fn build_with<A: Arena>(
        self,
        make: impl FnOnce(&ArenaLayout, u32, u64, [u8; 16]) -> Result<A, BuildError>,
    ) -> Result<A, BuildError> {
        // 1. Unique frame names in first-seen order (explicit frames, then edge
        //    endpoints). Order only affects id assignment, not correctness.
        let mut names: Vec<&str> = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        for f in &self.frames {
            if seen.insert(f.as_str()) {
                names.push(f.as_str());
            }
        }
        for e in &self.edges {
            if seen.insert(e.parent.as_str()) {
                names.push(e.parent.as_str());
            }
            if seen.insert(e.child.as_str()) {
                names.push(e.child.as_str());
            }
        }

        // 2. A frame is the child of at most one edge (it is a tree).
        let mut children: HashSet<&str> = HashSet::new();
        for e in &self.edges {
            if !children.insert(e.child.as_str()) {
                return Err(BuildError::DuplicateEdge {
                    child: blake3_64(&e.child),
                });
            }
        }

        // 3. Budgets: declared count + reserved id-0 sentinel + optional headroom.
        let frame_count = names.len() as u64;
        let edge_count = self.edges.len() as u64;
        let max_frames = frame_count + 1 + u64::from(self.frame_headroom);
        let max_edges = edge_count + 1 + u64::from(self.edge_headroom);
        let max_frames = u32::try_from(max_frames).map_err(|_| BuildError::TooManyFrames)?;
        let max_edges = u32::try_from(max_edges).map_err(|_| BuildError::TooManyEdges)?;

        // 4. Per-edge capacities indexed by EdgeId: index 0 (and any headroom
        //    slots) is a zero-capacity sentinel; static edges are zero; dynamic
        //    edges reserve their own capacity.
        let mut caps = std::vec![0u32; max_edges as usize];
        for (i, e) in self.edges.iter().enumerate() {
            if let EdgeDeclKind::Dynamic(cfg) = &e.kind {
                caps[i + 1] = cfg.capacity.get();
            }
        }

        // 5. Size and allocate the arena. Declaration-time edge writes go through
        //    an `ArenaBuilder`, whose `&mut` borrow of the arena is what makes
        //    them sound: no shared `ArenaView` can exist while one happens.
        let layout = ArenaLayout::new(max_frames, max_edges, caps)?;
        let boot_id = boot_id();
        let mut arena = make(&layout, std::process::id(), process_start_time(), boot_id)?;
        // Scoped so the builder's exclusive borrow ends before `arena` moves into
        // the `Tree`; nothing may declare an edge once the tree is shareable.
        {
            let mut builder = ArenaBuilder::new(&mut arena);
            // Record how many edge slots are in use (sentinel + declared); nothing
            // on the read path depends on it, but it keeps diagnostics honest.
            builder
                .view()
                .header()
                .edge_count
                .store(edge_count as u32 + 1, Ordering::Relaxed);

            // 6. Intern every frame name so isolated frames get an id and any
            //    collision / over-capacity surfaces now. Interning is idempotent,
            //    so the edge loop below re-interns endpoints to recover their ids.
            for &n in &names {
                builder.view().intern(n).map_err(BuildError::Frame)?;
            }

            // 7. Declare each edge (real ids start at 1) and wire the topology.
            //    Each dynamic edge's ring occupies a cumulative slot range in
            //    EdgeId order, so `running_off` is the element offset shared by its
            //    stamp and pose sub-arenas (which have equal slot counts).
            let mut running_off: u32 = 0;
            for (i, e) in self.edges.iter().enumerate() {
                let edge_id = (i + 1) as u32;
                // Idempotent: both endpoints were interned in step 6.
                let parent = builder
                    .view()
                    .intern(&e.parent)
                    .map_err(BuildError::Frame)?;
                let child = builder.view().intern(&e.child).map_err(BuildError::Frame)?;
                let record = match &e.kind {
                    EdgeDeclKind::Static(iso) => EdgeRecord::static_edge(
                        parent.get(),
                        child.get(),
                        iso.to_bits(),
                        self.default_domain,
                    ),
                    EdgeDeclKind::Dynamic(cfg) => {
                        let capacity = cfg.capacity.get();
                        let interp = cfg.interp.unwrap_or(self.default_interp);
                        let domain = cfg.domain.unwrap_or(self.default_domain);
                        let record = EdgeRecord::dynamic(
                            parent.get(),
                            child.get(),
                            capacity,
                            running_off,
                            running_off,
                            interp.as_u8(),
                            domain,
                        );
                        running_off += capacity;
                        record
                    }
                };
                builder
                    .declare_edge(EdgeId(edge_id), record)
                    .map_err(BuildError::Topology)?;
                builder
                    .view()
                    .topology()
                    .set_parent(child, parent.get(), edge_id)
                    .map_err(BuildError::Topology)?;
            }
        }

        Ok(arena)
    }
}

/// Which allocation backs a [`Tree`]'s arena.
///
/// An enum rather than `Box<dyn Arena>` so the backend stays a concrete,
/// statically-known type and no allocation is added to construct a tree. The
/// read path is unaffected either way: [`ArenaView`] already takes
/// `&dyn Arena`, and it is built once per [`Tree::guard`], not per lookup.
enum ArenaBacking {
    /// Single-process: one heap allocation (Phase 1).
    Heap(HeapArena),
    /// Multi-process: a sealed `memfd` mapped `MAP_SHARED` (Phase 2).
    #[cfg(all(feature = "shm", target_os = "linux"))]
    Mapped(MappedArena),
    /// Offline: the arena image inside a `.tft`, mapped `PROT_READ`
    /// (`docs/PHASE5.md` §2).
    #[cfg(all(feature = "shm", target_os = "linux"))]
    Frozen(tf_tree_arena::FrozenArena),
}

impl ArenaBacking {
    fn as_dyn(&self) -> &dyn Arena {
        match self {
            ArenaBacking::Heap(a) => a,
            #[cfg(all(feature = "shm", target_os = "linux"))]
            ArenaBacking::Mapped(a) => a,
            #[cfg(all(feature = "shm", target_os = "linux"))]
            ArenaBacking::Frozen(a) => a,
        }
    }

    /// Whether the mapping accepts stores.
    ///
    /// Every mutating entry point on [`Tree`] consults this. A `PROT_READ`
    /// mapping does not fault politely on a `compare_exchange` — it delivers
    /// `SIGSEGV`, killing the consumer process. Turning that into an `Err` is
    /// the difference between read-only being a safety boundary and being a
    /// loaded gun.
    fn is_writable(&self) -> bool {
        match self {
            ArenaBacking::Heap(_) => true,
            #[cfg(all(feature = "shm", target_os = "linux"))]
            ArenaBacking::Mapped(a) => a.is_writable(),
            // `docs/PHASE5.md` §2.4: a frozen arena's `AttachMode` is implicitly
            // and permanently `ReadOnly`. There is no mode in which this can be
            // true — the mapping is `PROT_READ`, so a store through it is a
            // `SIGSEGV` and not an error anything can catch.
            #[cfg(all(feature = "shm", target_os = "linux"))]
            ArenaBacking::Frozen(_) => false,
        }
    }

    /// Whether other processes may be mapping the same arena.
    fn is_shared(&self) -> bool {
        match self {
            #[cfg(all(feature = "shm", target_os = "linux"))]
            ArenaBacking::Mapped(_) => true,
            // Other *processes* may map the same `.tft`, but "shared" here asks
            // whether a peer can mutate it — the participant table, the claim
            // protocol and reaping all hang off this answer. A frozen arena has
            // no writers at all (§2.4), so it is `false` for the same reason a
            // heap arena is.
            #[cfg(all(feature = "shm", target_os = "linux"))]
            ArenaBacking::Frozen(_) => false,
            ArenaBacking::Heap(_) => false,
        }
    }
}

/// A claimed edge: the arena record, and the lease that makes its holder's
/// death observable (`docs/PHASE2.md` §6.1, `docs/decisions/0005` §5).
///
/// Derefs to the [`Publisher`], so `push` and friends work unchanged.
///
/// # Drop order is load-bearing
///
/// The fields below are declared **publisher first, `_lease` second**, and Rust
/// drops struct fields in declaration order. That yields *clear the record,
/// then unlock* — the order `0005` §5 specifies.
///
/// Reversing it is not catastrophic but is wrong: it leaves a window in which
/// the byte is free while the record still says held, which is precisely the
/// signature a reaper reads as "the holder is dead". The reap that follows is
/// harmless (our own release is a CAS and cannot clear a successor's claim) but
/// it is a spurious epoch bump and a `ClaimRevoked` somebody has to explain.
///
/// This cannot be enforced by `ManuallyDrop` here — that needs `unsafe`, and
/// this crate is `#![forbid(unsafe_code)]`. **Do not reorder these fields.**
pub struct EdgeWriter<'a> {
    publisher: Publisher<'a>,
    /// The fork generation this writer was claimed in.
    ///
    /// `Copy`, so it takes no part in the drop order above; it is declared here
    /// only to keep the two drop-ordered fields adjacent.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    fork_gen: Option<u64>,
    /// Held purely for its `Drop`, hence the underscore — nothing reads it, and
    /// the observable effect is releasing the byte when this value dies.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    _lease: Option<ClaimLease>,
}

impl EdgeWriter<'_> {
    /// Whether this writer belongs to the pre-`fork` process. One relaxed load.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    fn detached(&self) -> bool {
        self.fork_gen
            .is_some_and(|g| g != tf_tree_ipc::fork::generation())
    }

    /// Publish `iso` at `stamp` on the claimed edge.
    ///
    /// # Errors
    ///
    /// [`PushError::NonMonotonicStamp`] if `stamp` predates the edge's newest;
    /// [`PushError::ClaimRevoked`] if a reaper judged this writer dead and took
    /// the edge away (`docs/PHASE2.md` §1, A4);
    /// [`PushError::ChildDetached`] if this writer was claimed before a `fork()`
    /// and is being used in the child.
    ///
    /// # Why this shadows the `Deref` target
    ///
    /// An inherent method wins over a `Deref` one, so `writer.push(..)` lands
    /// here and gets the fork check. Reaching the inner [`Publisher`] explicitly
    /// — `(&*writer).push(..)` — skips it, and after a `fork` that is a write
    /// through a dangling reference into an unmapped page. The `Deref` is kept
    /// because it is what makes every pre-existing caller compile unchanged, but
    /// **do not route `push` around it.**
    ///
    /// # Cost, measured
    ///
    /// `+0.195 ns` on a shared-arena push — 9.041 ns against 8.846 ns for the
    /// same benchmark with the branch forced not-taken (`benches/push.rs`,
    /// `--features shm`). One relaxed load of a process-local static plus a
    /// predictable branch. A heap tree carries `None` and pays only the
    /// discriminant test, so the single-process path is unchanged (8.71 ns).
    ///
    /// That is the price of a `fork` child not writing through a dangling
    /// pointer into an unmapped page, which is not a trade this hot path gets
    /// to decline.
    pub fn push(&self, stamp: i64, iso: &Iso3) -> Result<(), PushError> {
        #[cfg(all(feature = "shm", target_os = "linux"))]
        if self.detached() {
            return Err(PushError::ChildDetached);
        }
        self.publisher.push(stamp, iso)
    }
}

impl Drop for EdgeWriter<'_> {
    fn drop(&mut self) {
        // `Publisher`'s own `Drop` releases the claim with a `compare_exchange`
        // *into the arena*. In a `fork` child that arena is a hole in the
        // address space, so the destructor faults — and it does so whether or
        // not the child ever called anything, which is what makes it the most
        // dangerous of the four inherited destructors.
        //
        // Found by `a_forked_child_runs_its_destructors_without_touching_the_parent`,
        // which failed with `child=signalled 11` while every API-level check in
        // the same child passed. Guarding `Tree::drop` is not sufficient: a
        // `Drop` impl's early return does not stop the struct's *fields* from
        // being dropped afterwards, so every owned resource has to stand itself
        // down.
        #[cfg(all(feature = "shm", target_os = "linux"))]
        if self.detached() {
            self.publisher.abandon();
        }
    }
}

impl<'a> core::ops::Deref for EdgeWriter<'a> {
    type Target = Publisher<'a>;

    fn deref(&self) -> &Publisher<'a> {
        &self.publisher
    }
}

/// Fired inside [`Tree::claim`], after the arena CAS and before the lease
/// `SETLK`.
///
/// **Test scaffolding, and present only under `--features test-hooks`.** The
/// window between those two operations is one syscall wide; a reaper that runs
/// inside it sees `record held ∧ lease free`, which is its exact "the holder is
/// dead" signature, and clears a claim that was in the middle of being taken.
/// `take_claim_lease` recovers by re-reading the epoch, and there is no way to
/// demonstrate that recovery — or to catch its removal — without putting a
/// reaper in the window deliberately.
///
/// Set it once, from a test, before the `claim` under test. `fn()` rather than a
/// boxed closure so this is a bare function pointer with no allocation and no
/// `Sync` bound to reason about.
#[cfg(all(feature = "test-hooks", feature = "shm", target_os = "linux"))]
#[doc(hidden)]
pub static CLAIM_WINDOW_HOOK: std::sync::OnceLock<fn()> = std::sync::OnceLock::new();

/// Holds an edge's claim byte for as long as this value lives.
#[cfg(all(feature = "shm", target_os = "linux"))]
pub(crate) struct ClaimLease {
    lock: std::sync::Arc<tf_tree_ipc::LockFile>,
    edge: u32,
    /// The fork generation this lease was taken in.
    ///
    /// An OFD lock belongs to the **open file description**, which a `fork`
    /// child inherits — so an unlock issued by the child releases the *parent's*
    /// byte. Dropping an inherited `EdgeWriter` in a child would therefore hand
    /// the parent's live edge to the next reaper, from another process, with
    /// nothing in the parent's logs to show for it.
    fork_gen: u64,
}

#[cfg(all(feature = "shm", target_os = "linux"))]
impl Drop for ClaimLease {
    fn drop(&mut self) {
        // Never unlock from a `fork` child: the byte is the parent's (see
        // `fork_gen`). Leaking it here leaks nothing — the description stays
        // open in the parent, which still owns and will still release it.
        if self.fork_gen != tf_tree_ipc::fork::generation() {
            return;
        }
        // Best effort: if the unlock fails the process is in no state to react,
        // and the kernel releases the byte at exit regardless — which is the
        // property the lease exists for.
        let _ = self.lock.release_claim(self.edge);
    }
}

/// This process's answer to "is the participant in slot `n` still running?".
///
/// Boxed and owned by the [`Tree`] because it must outlive every [`ArenaView`]
/// that borrows it, and because which implementation applies is decided once —
/// `/proc` inference for a heap or fd-inherited tree, the kernel's `F_OFD_GETLK`
/// answer for one that came through [`crate::open`] (`docs/PHASE2.md` §5.1).
type BoxedLiveness = Box<dyn Fn(u32, &tf_tree_core::ParticipantRecord) -> bool + Send + Sync>;

/// A transform tree: a fixed-capacity arena plus the ergonomic operations for
/// publishing samples and looking up transforms. Build one with [`TreeBuilder`].
///
/// `Send + Sync`: the arena's interior mutation is all atomic, the single runtime
/// topology mutation ([`Self::reparent`]) is serialized by the in-arena topology
/// lock (`docs/PHASE2.md` §1, A2) behind a process-local mutex, and lookups are
/// read-only. Share one `Tree` across threads; each reader compiles or caches its
/// own [`crate::Plan`]s.
pub struct Tree {
    arena: ArenaBacking,
    /// This process's slot in the arena's participant table.
    ///
    /// Claims name this slot rather than a PID (`docs/PHASE2.md` §1, A3), which
    /// is what lets a claim publish its owner and its identity in one store.
    /// The topology lock names it the same way, and for the same reason.
    participant: u32,
    /// The incarnation this process's participant slot carried when it
    /// registered.
    ///
    /// Checked on release so a slot that was reaped and handed to another
    /// process is not freed by *this* process's `Drop` — see
    /// `ParticipantTable::release`.
    incarnation: u64,
    /// Decides whether a participant slot's owner is still running.
    ///
    /// Boxed and stored rather than built per call because it must outlive the
    /// [`ArenaView`] that borrows it, and because the reboot check is a property
    /// of the *arena*, not of any one record: if the segment predates this boot,
    /// every pid it names belongs to a dead world and no `/proc` lookup can tell
    /// you so. That comparison is made once, here, and collapses into the
    /// closure.
    ///
    /// This is the seam `docs/PHASE2.md` §5.1 replaces with the OFD lock file,
    /// which is authoritative where `/proc` is inference.
    liveness: BoxedLiveness,
    /// Serializes *this process's* threads through [`Self::reparent`].
    ///
    /// Not the real lock and never was — a `Mutex` is per-process, so it
    /// serializes nothing against a peer that mapped the same segment
    /// (`docs/PHASE2.md` §1, A2). The arena's `TopoLock` is what makes the
    /// mutation exclusive; this one is **also load-bearing**, and not merely an
    /// optimisation, so do not remove it as redundant.
    ///
    /// `TopoGuard`'s release compares only the owner word, which is
    /// `participant_slot + 1` and therefore identical for every thread of this
    /// process. Two overlapping guards on one slot — thread A stolen from,
    /// thread B re-acquiring, then A dropping — would let A's release free B's
    /// lock. Distinct processes have distinct slots, so the only way to build
    /// that overlap is two threads of *this* process in `reparent` at once,
    /// which this mutex makes unrepresentable. Removing it would require a
    /// per-acquisition token in the owner word instead.
    ///
    /// It also stops two threads of one process spending the arena lock's spin
    /// budget on each
    /// other before one of them gets to do any work.
    decl: Mutex<()>,
    /// What keeps this process attached, for a tree obtained from
    /// [`crate::open`].
    ///
    /// `None` for a heap tree, for `build_shared`, and for `attach_shared` over
    /// an inherited fd — none of those went through a rendezvous.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    attachment: Option<crate::open::Attachment>,
    /// The lock file this tree takes claim leases against (§6.1).
    ///
    /// `None` for a heap tree and for `attach_shared` over an inherited fd —
    /// neither went through a rendezvous, so there is no lock file, and the
    /// arena CAS alone is the claim exactly as it was before leases existed.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    claim_lock: Option<std::sync::Arc<tf_tree_ipc::LockFile>>,
    /// The fork generation this tree was opened in, or `None` for a backing that
    /// survives a `fork` intact.
    ///
    /// See [`fork_gen_for`]. A mismatch means this value belongs to a process
    /// that no longer exists, and every reference it holds into the arena is
    /// dangling — the mapping is `MADV_DONTFORK`.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    fork_gen: Option<u64>,
}

impl Tree {
    /// Whether this tree belongs to a process that no longer exists — it was
    /// opened before a `fork()` and this is the child.
    ///
    /// One relaxed load of a process-local counter; see `tf_tree_ipc::fork`
    /// for why it is not `getpid()`.
    ///
    /// A detached tree is not repairable. Open a new one in the child, or
    /// `exec`.
    #[must_use]
    pub fn detached(&self) -> bool {
        #[cfg(all(feature = "shm", target_os = "linux"))]
        {
            self.fork_gen
                .is_some_and(|g| g != tf_tree_ipc::fork::generation())
        }
        #[cfg(not(all(feature = "shm", target_os = "linux")))]
        {
            false
        }
    }

    fn view(&self) -> ArenaView<'_> {
        // The detached case first, and unconditionally: every accessor below
        // funnels through here, so this is the one place that has to be right
        // for a read of the vanished mapping to be impossible rather than
        // merely unlikely.
        #[cfg(all(feature = "shm", target_os = "linux"))]
        if self.detached() {
            return ArenaView::new(poison_arena());
        }
        // Both builders are load-bearing for A8, and neither is optional:
        //
        // * `as_participant` — a rescuer publishes *itself* into `claiming`, so
        //   an anonymous view can wait on a stalled interner but may never take
        //   the entry over. Without this, A8's recovery is inert.
        // * `with_liveness` — the default is "believed alive", the right
        //   fail-safe but one that never fires. This is what makes a dead
        //   claimant's entry actually recoverable.
        ArenaView::new(self.arena.as_dyn())
            .as_participant(self.participant)
            .with_liveness(&*self.liveness)
            // Third builder, and as load-bearing as the other two: without it
            // the diagnostic counters (`docs/PHASE5.md` §5) write through a
            // read-only mapping and the process dies with SIGSEGV.
            .writable(self.is_writable())
    }

    /// Resolve a frame name to its stable id.
    ///
    /// A name declared at build time resolves to its existing id without
    /// consuming a slot. A name that was never declared is interned on demand,
    /// which needs a free frame slot (see [`TreeBuilder::frame_headroom`]).
    ///
    /// # Errors
    ///
    /// [`FrameError`] if the frame table is full or a name hash collides.
    pub fn frame(&self, name: &str) -> Result<FrameId, FrameError> {
        // Explicitly, ahead of `view()`: the poison arena is a *writable* heap
        // arena, so interning into it would succeed and hand back a `FrameId`
        // that names nothing. A wrong answer is worse than an error.
        if self.detached() {
            return Err(FrameError::ChildDetached);
        }
        if !self.arena.is_writable() {
            // Interning a *new* name publishes into the hash table with a
            // `compare_exchange`; through a `PROT_READ` mapping that is a
            // `SIGSEGV`, not an error. Resolving an existing name is a pure
            // read, so a read-only participant can still do that — which is
            // every name the creator declared.
            return self.view().find_frame(name)?.ok_or(FrameError::ReadOnly);
        }
        self.view().intern(name)
    }

    /// Re-parent an existing `child` frame under `new_parent`, reusing the child's
    /// already-declared edge (no new capacity is allocated). This is the only
    /// runtime topology mutation; it bumps the topology generation, invalidating
    /// compiled [`crate::Plan`]s so they recompile against the new shape.
    ///
    /// # Serialization (`docs/PHASE2.md` §1, A2)
    ///
    /// Two locks, doing two different jobs:
    ///
    /// * `self.decl`, a plain `Mutex`, keeps *this* process's threads out of
    ///   each other's way. It serializes nothing across a process boundary and
    ///   never did; it is kept because it is free and stops two threads of one
    ///   process burning the arena lock's spin budget against each other.
    /// * The **in-arena** [`TopoLockView`] is the real one. Its word lives in
    ///   the header, so every participant that mapped the segment contends on
    ///   the same bytes, and it is stealable from a participant that died
    ///   holding it — which is why this is no longer refused on a shared arena.
    ///
    /// # Errors
    ///
    /// [`ReparentError::NoEdge`] if `child` has no incoming edge to reuse,
    /// [`ReparentError::Topology`] if the move would create a cycle or references
    /// an out-of-range frame, or [`ReparentError::LockContended`] if a live peer
    /// holds the topology lock (retry).
    pub fn reparent(&self, child: FrameId, new_parent: FrameId) -> Result<(), ReparentError> {
        if self.detached() {
            return Err(ReparentError::ChildDetached);
        }
        if !self.arena.is_writable() {
            return Err(ReparentError::ReadOnly);
        }
        let _local = self.decl.lock().unwrap_or_else(|e| e.into_inner());
        let view = self.view();
        let header = view.header();
        let lock = TopoLockView::new(&header.topo_lock.owner, &header.topo_lock.acquired_at_nanos);
        let participants = view.participants();
        let arena_boot = header.boot_id;
        let is_alive = move |slot: u32| participant_is_alive(&participants, slot, &arena_boot);
        let _topo = lock.acquire(self.participant, now_nanos(), &is_alive)?;

        let (_p, _depth, edge, _gen) =
            view.topology()
                .read_frame(child)
                .ok_or(ReparentError::Topology(TopologyError::UnknownFrame {
                    frame: child.get(),
                }))?;
        if edge == 0 {
            return Err(ReparentError::NoEdge { child });
        }
        view.topology().set_parent(child, new_parent.get(), edge)?;
        Ok(())
    }

    /// Claim exclusive write access to the dynamic edge whose child is `child`
    /// (and whose parent must be `parent`). Returns a [`Publisher`]; dropping it
    /// releases the claim.
    ///
    /// # Errors
    ///
    /// [`ClaimApiError`] if `child` is unknown, no edge attaches it, the parent
    /// does not match, the edge carries no sample ring (it is static or
    /// tombstoned), or the edge is already claimed.
    pub fn claim(&self, child: FrameId, parent: FrameId) -> Result<EdgeWriter<'_>, ClaimApiError> {
        if self.detached() {
            return Err(ClaimApiError::ChildDetached);
        }
        if !self.arena.is_writable() {
            return Err(ClaimApiError::ReadOnly);
        }
        let view = self.view();
        let (p, _depth, edge, _gen) = view
            .topology()
            .read_frame(child)
            .ok_or(ClaimApiError::UnknownFrame { child })?;
        if edge == 0 {
            return Err(ClaimApiError::NoEdge { child });
        }
        if p != parent.get() {
            return Err(ClaimApiError::ParentMismatch {
                child,
                expected: parent.get(),
                actual: p,
            });
        }
        let eid = EdgeId(edge);
        // A static or tombstoned edge has no ring (`capacity == 0`); publishing to
        // it is a typed error, not a panic on an empty slot slice.
        let (Some(ring), Some(claim_rec)) = (view.ring(eid), view.claim(eid)) else {
            return Err(ClaimApiError::NotDynamic { child, edge: eid });
        };
        // ---- Two-phase acquire (`docs/decisions/0005` §5) --------------
        //
        // The arena CAS is the *decision*; the lock-file byte is a *lease* that
        // makes the holder's death observable. §6.1's literal "the lock file is
        // authoritative" is not implementable — two files, no atomic
        // cross-update, and a `HeapArena` has no lock file at all — so exactly
        // one of them is the linearization point, and it is this CAS.
        let (epoch, owner) = claim(claim_rec, self.participant)?;

        // The CAS has landed and the lease has not been taken: this is the
        // one-syscall window `take_claim_lease`'s epoch re-check exists to
        // recover from, and the only place a reaper can be placed inside it on
        // purpose. Compiled out entirely without `test-hooks`.
        #[cfg(all(feature = "test-hooks", feature = "shm", target_os = "linux"))]
        if let Some(hook) = CLAIM_WINDOW_HOOK.get() {
            hook();
        }

        #[cfg(all(feature = "shm", target_os = "linux"))]
        let lease = self.take_claim_lease(eid, claim_rec, epoch, owner)?;

        Ok(EdgeWriter {
            publisher: Publisher::new(ring, claim_rec, epoch, owner),
            #[cfg(all(feature = "shm", target_os = "linux"))]
            fork_gen: self.fork_gen,
            #[cfg(all(feature = "shm", target_os = "linux"))]
            _lease: lease,
        })
    }

    /// Phase two: take the lease, then prove the record is still ours.
    ///
    /// # The epoch re-check, which §6.3 does not mention and which is required
    ///
    /// Between the CAS above and the `SETLK` here there is a one-syscall
    /// window. A reaper that runs inside it sees `record held ∧ lock free` —
    /// its exact "the holder is dead" signature — and clears the record. This
    /// process would then hold a lease on an edge the arena reports free, and a
    /// third process could claim it: two writers on one edge, which is what D7
    /// and A4 exist to prevent.
    ///
    /// `edge::reap` bumps the epoch *before* clearing the owner, which is what
    /// makes the window recoverable at all: re-reading the epoch after taking
    /// the lease detects the reap, and this backs out and lets the caller
    /// retry.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    fn take_claim_lease(
        &self,
        eid: EdgeId,
        claim_rec: &tf_tree_core::edge::ClaimRecord,
        epoch: u64,
        owner: u64,
    ) -> Result<Option<ClaimLease>, ClaimApiError> {
        let Some(lock) = self.claim_lock.as_ref() else {
            // No rendezvous (a heap tree, or an `attach_shared` over an
            // inherited fd). The CAS alone is the claim, exactly as before —
            // the lease adds observability, not correctness.
            return Ok(None);
        };
        match lock.try_take_claim(eid.0) {
            Ok(tf_tree_ipc::LockAttempt::Acquired) => {}
            Ok(tf_tree_ipc::LockAttempt::Contended) => {
                // The record was free but a live process holds the lease.
                // Reachable only through a reaper bug or `--force-new` byte
                // aliasing; back the CAS out rather than publish beside them.
                tf_tree_core::edge::release(claim_rec, owner);
                return Err(ClaimApiError::LeaseContended { edge: eid });
            }
            Err(_) => {
                tf_tree_core::edge::release(claim_rec, owner);
                return Err(ClaimApiError::LeaseUnavailable { edge: eid });
            }
        }

        // Verified by `the_acquire_window_backs_out`, which places a reaper
        // inside the window through `CLAIM_WINDOW_HOOK` — the window is one
        // syscall wide and cannot be hit by racing. Mutant: `if false` here ⇒
        // `claim` returns `Ok` and the writer publishes onto a reaped record.
        if claim_rec.epoch.load(Ordering::Acquire) != epoch {
            // A reaper ran inside the window. Give everything back.
            tf_tree_core::edge::release(claim_rec, owner);
            let _ = lock.release_claim(eid.0);
            return Err(ClaimApiError::ReapedDuringClaim { edge: eid });
        }

        Ok(Some(ClaimLease {
            lock: std::sync::Arc::clone(lock),
            edge: eid.0,
            fork_gen: tf_tree_ipc::fork::generation(),
        }))
    }

    /// Compile a `lookup(target, source)` path into a reusable [`crate::Plan`].
    ///
    /// # Errors
    ///
    /// [`LookupError::Disconnected`] / [`LookupError::TreeTooDeep`] as
    /// [`compile`].
    pub fn plan(
        &self,
        target: FrameId,
        source: FrameId,
    ) -> Result<tf_tree_core::Plan, LookupError> {
        if self.detached() {
            return Err(LookupError::ChildDetached);
        }
        let view = self.view();
        let topo = view.topology();
        compile(&topo, |eid| edge_meta(&view, eid), target, source)
    }

    /// A fresh [`Guard`] pinning the current topology generation for a batch of
    /// lookups.
    #[must_use]
    pub fn guard(&self) -> Guard<'_> {
        // `Guard::new` reads the topology generation *immediately*, so a
        // detached tree cannot build one even to throw away. A poisoned guard
        // answers `ChildDetached` to every evaluation instead — which is why
        // this method can stay infallible for the several dozen call sites that
        // will never fork.
        if self.detached() {
            return Guard::detached(self.view());
        }
        let g = Guard::new(self.view());
        // The counter flush is a write from a *destructor*, and a shared
        // mapping is `MADV_DONTFORK` — so a guard created here and dropped in a
        // `fork` child would write into a hole in the address space. The check
        // above catches a guard *created* after the fork; this catches one that
        // crossed it, which is the case `EdgeWriter::drop` already guards and
        // which `Guard` walked straight into.
        //
        // Only for a shared arena: a heap one is ordinary memory the child
        // inherits intact, and there is nothing to protect against.
        #[cfg(all(feature = "shm", target_os = "linux"))]
        let g = if self.is_shared() {
            g.with_fork_check(tf_tree_ipc::fork::generation)
        } else {
            g
        };
        g
    }

    /// Convenience lookup by name at a stamp: interns the names, compiles (or
    /// reuses a cached) [`crate::Plan`], and evaluates it. Keeps a small per-thread plan
    /// cache keyed by `(target, source, generation)`.
    ///
    /// # Errors
    ///
    /// [`LookupError::UnknownFrame`] if a name was never declared, or any
    /// compilation / evaluation error.
    pub fn lookup<D: Domain>(
        &self,
        target: &str,
        source: &str,
        stamp: Stamp<D>,
    ) -> Result<Iso3, LookupError> {
        if self.detached() {
            return Err(LookupError::ChildDetached);
        }
        let view = self.view();
        let t = find(&view, target)?;
        let s = find(&view, source)?;
        // Always a stable generation since A1: there is no torn value, and
        // caching a plan under it would key the cache on a value `compile` never
        // stamps a plan with — so every lookup during a mutation would miss the
        // cache and then fail with `TopologyChanged`.
        let generation = view.topology().stable_generation();
        let (plan, _hit) = cache::get_or_compile(self, t, s, generation)?;
        let g = self.guard();
        plan.at(&g, stamp)
    }

    /// A read-only [`ArenaView`] over the backing arena, for diagnostics and
    /// inspection (the CLI `tree` and `doctor` commands).
    ///
    /// The view exposes only the core read surface — frame/edge/claim records and
    /// the topology seqlock — and holds no mutation capability of its own (edges
    /// are still only mutated through [`Self::claim`]/[`Self::reparent`]). It is
    /// the one accessor Phase 1 tooling needs to walk the tree without a running
    /// writer.
    ///
    /// # One method on the returned view is not read-only
    ///
    /// [`ArenaView::intern`] publishes into the arena's hash table with a
    /// `compare_exchange`. Against a read-only backing — a `ReadOnly`
    /// `attach_shared`, or a `.tft` opened with [`Tree::open_frozen`] — that
    /// store reaches a `PROT_READ` page and the process takes `SIGSEGV`, which
    /// no `Result` can catch. Use [`Tree::frame`], which checks
    /// [`Tree::is_writable`] first and returns [`FrameError::ReadOnly`].
    ///
    /// `intern` does not make the check itself because `ArenaView`'s `writable`
    /// flag defaults to `false` and this crate is not the only thing that builds
    /// one; moving the guard down is a change to `tf_tree_core`'s contract (it
    /// also gates the §5 counters) and belongs in its own commit with its own
    /// `just loom` run.
    #[must_use]
    pub fn arena_view(&self) -> ArenaView<'_> {
        self.view()
    }

    /// Total size of the backing arena, in bytes.
    ///
    /// Because the arena is sized from the declared edges (decision `0004`), this
    /// reflects the dynamic edges' capacities only: static edges reserve no ring
    /// slots, so a tree that is mostly static is far smaller than a uniform
    /// per-edge reservation would be.
    #[must_use]
    pub fn arena_size_bytes(&self) -> usize {
        self.view().header().arena_size as usize
    }

    /// Attach to a shared arena another process created, over its file
    /// descriptor.
    ///
    /// The arena arrives **already built** — topology, frames and edges were all
    /// declared by the creator — so there is no builder here and none is
    /// possible: `docs/PROJECT.md` §5 D4 forbids growth, and a second process
    /// declaring edges into a fixed layout is exactly the growth that is
    /// forbidden. An attached process reads, and publishes to edges it claims.
    ///
    /// Use [`AttachMode::ReadOnly`] unless this process actually publishes. It
    /// maps `PROT_READ`, which makes corruption impossible rather than merely
    /// impolite — the only real safety boundary in the trust model
    /// (`docs/PHASE2.md` §0), and enforced by the MMU rather than by convention.
    ///
    /// # Errors
    ///
    /// [`ShmError`] if the segment is unsealed (and so could be truncated under
    /// a reader, faulting it with `SIGBUS`), is not a tf_tree arena, or was
    /// written by a build with a different `FORMAT_VERSION` or record layout.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    pub fn attach_shared(fd: std::os::fd::OwnedFd, mode: AttachMode) -> Result<Tree, ShmError> {
        Tree::attach_shared_inner(fd, mode, None)
    }

    /// Attach into the participant slot an owner granted (`docs/PHASE2.md` §3.7).
    ///
    /// Used by [`crate::open`]; [`Tree::attach_shared`] is the fd-inheritance
    /// path, where there is no owner to ask and the slot is self-assigned.
    ///
    /// # Errors
    ///
    /// As [`Tree::attach_shared`], plus [`ShmError::ParticipantTableFull`] if
    /// the named slot is not free.
    ///
    /// That last case means the arena still holds a record for a participant
    /// whose lock byte is already gone — the owner grants only slots its table
    /// reports free, and this process only got here after taking the byte. A
    /// **stale record with a free byte is exactly what reaping is for**
    /// (`docs/PHASE2.md` §6.3, `docs/decisions/0005` step 8), and until that
    /// lands there is nothing useful to retry: the owner would name the same
    /// slot again. It is surfaced rather than papered over.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    pub fn attach_shared_at(
        fd: std::os::fd::OwnedFd,
        mode: AttachMode,
        slot: u32,
    ) -> Result<Tree, ShmError> {
        Tree::attach_shared_inner(fd, mode, Some(slot))
    }

    #[cfg(all(feature = "shm", target_os = "linux"))]
    fn attach_shared_inner(
        fd: std::os::fd::OwnedFd,
        mode: AttachMode,
        slot: Option<u32>,
    ) -> Result<Tree, ShmError> {
        let arena = MappedArena::attach(fd, mode)?;
        // The attacher derives the used extents from the arena's own
        // `frame_count`/`edge_count`, so nothing has to be passed across the
        // handshake and there is no agreement with the creator to keep in sync
        // (§7.1, `docs/decisions/0005` step 10).
        arena.populate_hot();
        let backing = ArenaBacking::Mapped(arena);
        // A read-only peer cannot register — the table is in the arena and
        // registration writes to it. It takes the sentinel slot instead, and
        // every mutating entry point already refuses before reaching a claim.
        let (participant, incarnation) = if backing.is_writable() {
            let view = ArenaView::new(backing.as_dyn());
            match slot {
                Some(s) => (
                    s,
                    register_participant_at(&view, s)
                        .map_err(|_| ShmError::ParticipantTableFull)?,
                ),
                None => register_participant(&view).map_err(|_| ShmError::ParticipantTableFull)?,
            }
        } else {
            (u32::MAX, 0)
        };
        let liveness = liveness_for(ArenaView::new(backing.as_dyn()).header().boot_id);
        #[cfg(all(feature = "shm", target_os = "linux"))]
        let fork_gen = fork_gen_for(&backing);
        Ok(Tree {
            arena: backing,
            participant,
            incarnation,
            liveness,
            decl: Mutex::new(()),
            #[cfg(all(feature = "shm", target_os = "linux"))]
            attachment: None,
            #[cfg(all(feature = "shm", target_os = "linux"))]
            claim_lock: None,
            #[cfg(all(feature = "shm", target_os = "linux"))]
            fork_gen,
        })
    }

    /// The shared segment's file descriptor, to hand to another process.
    ///
    /// `None` for a heap-backed tree. Pass it over a unix socket with
    /// `SCM_RIGHTS`, or let a child inherit it — the fd *is* the capability to
    /// attach, so whoever holds it is a participant.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    #[must_use]
    pub fn shared_fd(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        match &self.arena {
            ArenaBacking::Mapped(a) => Some(a.as_raw_fd()),
            // A `.tft` is not shareable *as a segment*: handing its fd to a peer
            // and letting it `attach` would map the container header, not the
            // arena. Peers open the path.
            ArenaBacking::Frozen(_) | ArenaBacking::Heap(_) => None,
        }
    }

    /// Whether this tree's arena is shared with other processes.
    #[must_use]
    pub fn is_shared(&self) -> bool {
        self.arena.is_shared()
    }

    /// Construct a permanently read-only [`Tree`] over a frozen `.tft` image.
    ///
    /// `pub(crate)`: [`crate::frozen`] owns the file half of `docs/PHASE5.md`
    /// §2, but `Tree`'s fields are private to this module, so the constructor
    /// has to live here.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    pub(crate) fn from_frozen(arena: tf_tree_arena::FrozenArena) -> Tree {
        let backing = ArenaBacking::Frozen(arena);
        let fork_gen = fork_gen_for(&backing);
        Tree {
            arena: backing,
            // The read-only sentinel, exactly as a `PROT_READ` `attach_shared`
            // takes: registering would write to the participant table, which is
            // inside the mapping.
            participant: u32::MAX,
            incarnation: 0,
            // **Nobody is alive in a frozen arena.** Its participant and claim
            // records name processes of whatever run produced the file, and the
            // usual `/proc` inference would answer about *this* host's current
            // pids — so a recycled pid would resurrect a participant that has
            // been dead since before the file existed. `false` is not a
            // conservative guess here, it is the fact.
            liveness: Box::new(|_, _| false),
            decl: Mutex::new(()),
            attachment: None,
            claim_lock: None,
            fork_gen,
        }
    }

    /// The arena bytes behind this tree, for [`crate::frozen`]'s freeze path.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    pub(crate) fn backing(&self) -> &dyn Arena {
        self.arena.as_dyn()
    }

    /// The boot id of the host that created this arena, all 16 bytes.
    ///
    /// Read from the header rather than cached: a `Tree` that attached to
    /// somebody else's segment must report the *creator's* boot id, which is
    /// what makes a segment surviving a reboot detectable (`docs/PHASE2.md` §1,
    /// A7).
    #[must_use]
    pub fn boot_id(&self) -> [u8; 16] {
        self.view().header().boot_id
    }

    /// Whether this tree may publish — false for a read-only attachment.
    ///
    /// Every mutating method checks this and returns an error rather than
    /// letting the store reach a `PROT_READ` page, so callers do not have to;
    /// it is exposed so a consumer can branch on capability instead of on an
    /// error it was going to get.
    #[must_use]
    pub fn is_writable(&self) -> bool {
        self.arena.is_writable()
    }

    /// Park what keeps a joined process attached (`docs/decisions/0005`).
    ///
    /// The session holds this process's participant lock byte and the socket is
    /// the owner's liveness signal for it (D17). Both must live exactly as long
    /// as the `Tree`, and there is nowhere else with that lifetime.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    pub(crate) fn hold_attachment(
        &mut self,
        session: crate::open::JoinedSession,
        socket: std::os::fd::OwnedFd,
    ) {
        self.attachment = Some(crate::open::Attachment::Joined {
            _session: session,
            _socket: socket,
        });
    }

    /// Park the owner's session and serving thread.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    pub(crate) fn hold_ownership(
        &mut self,
        session: crate::open::JoinedSession,
        server: crate::open::OwnerThread,
    ) {
        self.attachment = Some(crate::open::Attachment::Owner {
            _session: session,
            server,
        });
    }

    /// Replace the `/proc` liveness heuristic with the kernel's answer (§5.1).
    ///
    /// `/proc` parsing is an *inference* with a race in it: between reading a
    /// pid and acting on it the process can exit and the number be reused, and
    /// an unreadable `/proc` entry is indistinguishable from a permission
    /// problem. So it fails safe — unknown means alive — which is right but
    /// means a dead participant is never *proven* dead.
    ///
    /// `F_OFD_GETLK` on the participant's lock byte is authoritative instead.
    /// A process that dies for any reason has its byte released by the kernel,
    /// with no cooperation and no timeout; a process that is merely `SIGSTOP`ped
    /// or GC-stalled still holds it, which is exactly the distinction
    /// `docs/PROJECT.md` §5 D17 forbids a heartbeat from making.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    pub(crate) fn use_ofd_liveness(&mut self, probe: crate::open::LivenessProbe) {
        let own_slot = self.participant;
        self.liveness = Box::new(move |slot, rec| {
            // **Never report ourselves dead.** `F_OFD_GETLK` answers about
            // *conflicting* locks, so a description does not see its own — a
            // property `tf_tree_ipc`'s `a_holder_does_not_see_its_own_lock`
            // proves. This probe uses a second open file description, which
            // happens to make our byte visible again, but relying on that would
            // be relying on a detail a future refactor could remove by sharing
            // one description. The guard is explicit so the correctness does
            // not depend on which description asked.
            if slot == own_slot {
                return true;
            }
            probe.is_held(slot).unwrap_or_else(|| record_is_alive(rec))
        });
    }

    /// Take claim leases against `lock` from now on (§6.1).
    #[cfg(all(feature = "shm", target_os = "linux"))]
    pub(crate) fn use_claim_leases(&mut self, lock: std::sync::Arc<tf_tree_ipc::LockFile>) {
        self.claim_lock = Some(lock);
    }

    /// Reclaim edges whose holder is provably dead (`docs/PHASE2.md` §6.3).
    ///
    /// Returns how many claims were reaped.
    ///
    /// # The predicate
    ///
    /// `record held and lease free` means the holder is dead — **or** is inside
    /// the one-syscall window between its CAS and its `SETLK`. That second case
    /// is closed from the *claimer's* side by the epoch re-check in
    /// [`Self::claim`], not by this loop backing off, which is strictly better
    /// than a grace period because there is no timing constant to tune.
    ///
    /// # Two skips that are not optional
    ///
    /// **Never judge ourselves.** `F_OFD_GETLK` reports only *conflicting*
    /// locks, so a description does not see its own — every edge this process
    /// holds reads lease-free. A literal §6.3 loop therefore revokes its own
    /// live writers, and A4 then correctly reports `ClaimRevoked` on the next
    /// push: a self-inflicted outage presenting as a spurious reap. §6.3 does
    /// not mention this.
    ///
    /// **Only a read-write participant may reap**, because a read-only tree
    /// never registered and its `participant` is the `u32::MAX` sentinel, where
    /// `sentinel + 1` overflows. It also has nothing to reap *with*: reaping
    /// writes to the arena.
    ///
    /// An unreadable lease is treated as held, which is the fail-safe direction
    /// (§6.2) — a false "alive" postpones recovery, a false "dead" steals an
    /// edge from a working process.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    #[must_use]
    pub fn reap_dead(&self) -> usize {
        self.reap_inner(None)
    }

    /// Reap only the edges a *named* participant held — the D17 fast path.
    ///
    /// The owner learns a participant died from `EPOLLHUP` on its socket, and
    /// therefore knows *which slot* went away. Passing it turns an `O(edges)`
    /// sweep of `fcntl` calls into `O(edges)` relaxed loads plus one syscall per
    /// edge that slot actually held, which matters because `probe_claim` is a
    /// syscall and an arena can hold thousands of edges.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    #[must_use]
    pub fn reap_participant(&self, slot: u32) -> usize {
        self.reap_inner(Some(slot))
    }

    #[cfg(all(feature = "shm", target_os = "linux"))]
    fn reap_inner(&self, only_slot: Option<u32>) -> usize {
        let Some(lock) = self.claim_lock.as_ref() else {
            return 0; // no rendezvous, no leases, nothing provable
        };
        // A read-only tree cannot reap and cannot form an owner word.
        if self.participant == u32::MAX || !self.arena.is_writable() {
            return 0;
        }
        // Compare the *slot*, not the word. `pack_owner` is
        // `(epoch << 16) | (slot + 1)`, so a whole-word comparison against
        // `slot + 1` matches only at epoch 0 — which `claim` never produces.
        // A reaper making that mistake does not recognise its own claims and
        // revokes them.
        let own_slot = self.participant;

        let view = self.view();
        let max_edges = view.header().max_edges;
        let mut reaped = 0;

        for edge in 0..max_edges {
            let Some(rec) = view.claim(EdgeId(edge)) else {
                continue;
            };
            // The cheap filter that keeps this from being one syscall per edge:
            // an unclaimed edge costs a relaxed load and nothing else.
            let owner = rec.owner.load(Ordering::Acquire);
            if owner == 0 {
                continue;
            }
            // `u32::MAX` for a claim still in flight (the `CLAIMING` sentinel),
            // which is never ours and *should* be reaped: it is distinguishable
            // garbage a killed claimer leaves behind. A live claimer caught in
            // that few-instruction window is protected from the other side, by
            // the epoch re-check in `claim`.
            let owner_slot = tf_tree_core::edge::slot_of(owner);
            if owner_slot == own_slot {
                continue;
            }
            if only_slot.is_some_and(|s| owner_slot != s) {
                continue;
            }
            // Unreadable reads as held (§6.2): fail safe.
            if lock.probe_claim(edge).map_or(true, |p| p.held) {
                continue;
            }
            tf_tree_core::edge::reap(rec);
            reaped += 1;
        }
        reaped
    }

    /// This process's own participant slot in the arena's table.
    ///
    /// `u32::MAX` for a read-only attachment, which takes a lock-file byte but
    /// writes no arena record — it cannot, the mapping is `PROT_READ`.
    ///
    /// The one number that indexes both tables (`docs/PHASE2.md` §3.7): the
    /// arena record and the lock-file byte are deliberately the same integer, so
    /// this is what a caller passes to [`Self::participant_alive`] to ask about
    /// itself.
    #[must_use]
    pub fn participant_slot(&self) -> u32 {
        self.participant
    }

    /// Whether the participant in `slot` is still running.
    ///
    /// The kernel's answer for a tree obtained from [`crate::open`], a `/proc`
    /// inference otherwise (`docs/PHASE2.md` §5.1). Exposed because `doctor`
    /// and the reaper both need it, and because it is the one predicate whose
    /// two implementations differ in a way a test can see: a `SIGSTOP`ped
    /// holder still holds its lock byte.
    #[must_use]
    pub fn participant_alive(&self, slot: u32) -> bool {
        match self.view().participants().get(slot) {
            None => false,
            Some(rec) => {
                tf_tree_core::participant::state_of(rec.state.load(Ordering::Acquire))
                    == tf_tree_core::participant::LIVE
                    && (self.liveness)(slot, rec)
            }
        }
    }

    /// Which arena *instance* this tree is attached to (A7, §3.7).
    ///
    /// All-zero for a heap tree, which is single-process by construction and so
    /// has no second attacher to disambiguate against.
    ///
    /// Distinct from the arena *name*: two processes that both resolved
    /// `<runtime_dir>/<domain>/<name>` can still hold different segments if the
    /// owner died and was replaced between their `open()` calls. Comparing
    /// names cannot detect that; comparing this can, which is why it appears in
    /// a `Tree`'s `__repr__` and in `doctor`.
    #[must_use]
    pub fn instance_uuid(&self) -> [u8; 16] {
        self.view().header().instance_uuid
    }

    /// Wrap a [`LookupError`] so its `Display` resolves ids to frame names.
    #[must_use]
    pub fn describe(&self, err: LookupError) -> Described<'_> {
        Described(err, self)
    }

    /// Resolve a frame id to its stored (truncated) name.
    fn frame_name(&self, id: FrameId) -> String {
        let Some(rec) = self.view().frame_record(id) else {
            return std::format!("frame#{}", id.get());
        };
        let n = rec.name_len as usize;
        std::str::from_utf8(&rec.name[..n])
            .unwrap_or("<invalid-utf8>")
            .to_owned()
    }

    /// Resolve an edge id to a `"parent->child"` label.
    fn edge_name(&self, id: EdgeId) -> String {
        let view = self.view();
        // An edge id could be out of range for a wildly stale error; guard it.
        let Some(rec) = view.edge(id) else {
            return std::format!("edge#{}", id.get());
        };
        let parent = FrameId::new(rec.parent)
            .map(|f| self.frame_name(f))
            .unwrap_or_else(|| "<root>".to_owned());
        let child = FrameId::new(rec.child)
            .map(|f| self.frame_name(f))
            .unwrap_or_else(|| "<root>".to_owned());
        std::format!("{parent}->{child} (edge#{})", id.get())
    }
}

// SAFETY note: `Tree` is `Send + Sync` by auto-derivation — `HeapArena` is
// `Send + Sync`, `Mutex` is `Send + Sync`, and the remaining field is a plain
// `Copy` scalar. No manual `unsafe impl` is needed (and none is allowed here).

/// Look up a frame by name for the read path, mapping "not found" and hash
/// collisions to [`LookupError::UnknownFrame`].
fn find(view: &ArenaView, name: &str) -> Result<FrameId, LookupError> {
    match view.find_frame(name) {
        Ok(Some(id)) => Ok(id),
        Ok(None) | Err(_) => Err(LookupError::UnknownFrame {
            hash: blake3_64(name),
        }),
    }
}

/// Read an edge's folding metadata (kind / domain / static pose) from the arena.
/// `None` for an edge id this arena has no record for — `compile` turns that into
/// [`LookupError::UnknownEdge`] rather than reading past the edge table.
fn edge_meta(view: &ArenaView, eid: EdgeId) -> Option<EdgeMeta> {
    let e = view.edge(eid)?;
    Some(EdgeMeta {
        kind: EdgeKind::from_u8(e.kind),
        domain: e.domain,
        static_pose: Iso3::from_bits(&e.static_pose),
    })
}

/// Best-effort Linux boot id folded to a `u64` (Phase 2 staleness input); `0` if
/// unavailable. Phase 1 stores it and does nothing else with it.
impl Drop for Tree {
    fn drop(&mut self) {
        // Release the participant slot on a clean exit. A slot leaked by a
        // *crash* is the reaper's problem (`docs/PHASE2.md` §6); this is the
        // orderly path, and skipping it would exhaust the table across
        // repeated attach/detach cycles in a long-lived arena.
        //
        // Read-only attachments never registered (they cannot write the table),
        // so they have nothing to release.
        // **A `fork` child must not release the parent's slot** — and what stops
        // it is `view()`, not a check here. A detached tree's `view()` returns
        // the poison arena, so this releases a slot in a throwaway heap arena
        // that names nothing. The parent's record is untouched, and the child
        // never stores into the unmapped page.
        //
        // An explicit `if self.detached() { return }` was written here first and
        // then deleted: with the poison in place it is unreachable in any way a
        // test can demonstrate — removing it left
        // `a_forked_child_runs_its_destructors_without_touching_the_parent`
        // green, while removing the poison turns that test's child into
        // `signalled 11`. Two mechanisms for one invariant, only one of them
        // load-bearing, is how the load-bearing one eventually gets deleted as
        // redundant. **Do not add the check back; keep the poison.**
        if self.participant != u32::MAX && self.arena.is_writable() {
            self.view()
                .participants()
                .release(self.participant, self.incarnation);
        }
    }
}

/// The fork generation to poison a tree against, or `None` for a backing that
/// survives a `fork` intact.
///
/// A [`HeapArena`] is ordinary anonymous memory: `fork` gives the child a
/// copy-on-write duplicate, every reference into it stays valid, and the child's
/// tree keeps working — divergently from the parent's, which is what `fork`
/// means. Poisoning that would break `multiprocessing` for single-process users
/// to defend against a hazard they do not have.
///
/// A mapped arena is the opposite: `MADV_DONTFORK` means the child has **no
/// mapping** there at all (`docs/PHASE2.md` §7.3).
///
/// Arming here rather than lazily is the load-bearing part. The handler must be
/// installed before any `fork` can happen, and "a shared mapping now exists" is
/// the earliest moment at which a `fork` could do damage. Arming on first *use*
/// would install it after the fork that mattered, and both processes would then
/// read generation 0 and agree they were the parent.
///
/// This also forces [`poison_arena`] to be built, in the parent, while
/// allocating is still safe. A `fork` child may have inherited a locked
/// allocator from a thread that no longer exists, so the detached path must not
/// be the thing that first allocates.
#[cfg(all(feature = "shm", target_os = "linux"))]
fn fork_gen_for(backing: &ArenaBacking) -> Option<u64> {
    match backing {
        ArenaBacking::Heap(_) => None,
        // A frozen mapping is `MAP_PRIVATE | PROT_READ` and deliberately *not*
        // `MADV_DONTFORK`, so a `fork` child inherits it intact and every
        // reference into it stays valid — the same situation as a heap arena,
        // and the one §2.2's sixteen dataloader workers depend on. Poisoning it
        // would break `multiprocessing` for offline users to defend against a
        // hazard they do not have.
        ArenaBacking::Frozen(_) => None,
        ArenaBacking::Mapped(_) => {
            tf_tree_ipc::fork::arm();
            let _ = poison_arena();
            Some(tf_tree_ipc::fork::generation())
        }
    }
}

/// The process-wide empty arena a detached [`Tree`] reads instead of the
/// mapping that went away.
///
/// [`Tree::guard`] and [`Tree::arena_view`] cannot report an error — the first
/// is the `let g = tree.guard();` idiom used by every reader, the second is a
/// diagnostic accessor — so a detached tree has to hand back *something*, and
/// that something must be memory this process actually has mapped. One frame, no
/// edges: every query answers "not here".
///
/// Nothing is published into it and every tree shares the one instance, so the
/// cost is a few kilobytes per process that ever opens a shared arena.
///
/// Reads that *can* report an error do not come here; they check
/// [`Tree::detached`] first and return [`LookupError::ChildDetached`] and its
/// siblings, which say what actually happened.
#[cfg(all(feature = "shm", target_os = "linux"))]
fn poison_arena() -> &'static HeapArena {
    static POISON: std::sync::OnceLock<HeapArena> = std::sync::OnceLock::new();
    POISON.get_or_init(|| {
        // `minimal()` is infallible precisely so this can be written without an
        // `unwrap` in a crate that denies them.
        HeapArena::new(&tf_tree_arena::ArenaLayout::minimal(), 0, 0, [0u8; 16])
    })
}

/// Register this process in the arena's participant table.
///
/// Every `Tree` — created or attached — takes a slot, because a claim names a
/// slot and there is no other way to be named. The slot is released in
/// [`Tree`]'s `Drop`.
fn register_participant(view: &ArenaView) -> Result<(u32, u64), ParticipantError> {
    view.participants()
        .register(std::process::id(), process_start_time(), now_nanos())
}

/// Register into the slot the arena's owner named (`docs/PHASE2.md` §3.7).
///
/// A joiner does not get to choose: the slot in the `HelloResponse` is also the
/// lock-file byte it took, and §5.1's liveness predicate asks the kernel about
/// that byte and then reads the record it indexes. Two independently-chosen
/// numbers would make every liveness answer be about somebody else.
#[cfg(all(feature = "shm", target_os = "linux"))]
fn register_participant_at(view: &ArenaView, slot: u32) -> Result<u64, ParticipantError> {
    view.participants()
        .register_at(slot, std::process::id(), process_start_time(), now_nanos())
}

/// Wall-clock nanoseconds since the epoch, saturating; `0` if the clock is
/// before the epoch. Diagnostics only — nothing correctness-critical reads it.
fn now_nanos() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

/// **The liveness predicate** — the single seam where "is participant `slot`
/// still running?" is answered, injected into
/// [`tf_tree_core::topology::TopoLockView::acquire`].
///
/// `tf_tree_core` deliberately does not answer this: it is `no_std` and
/// `docs/PHASE2.md` §2 forbids it a syscall dependency, so the arena lock takes
/// the answer as a parameter and this is what supplies it.
///
/// # This is the interim implementation, and it is meant to be replaced
///
/// `docs/PHASE2.md` §5.1 is explicit: **the OFD lock file is authoritative and
/// these records are advisory** — "any code deciding liveness from `state` or
/// `heartbeat` is a bug". §6.1 explains why: a `SIGSTOP`ped or GC-stalled
/// process still holds its kernel lock, so `F_OFD_GETLK` reporting the byte free
/// is a *fact* about death rather than a heuristic about it, and the whole
/// zombie class disappears. The lock file is not built yet.
///
/// Until it is, this implements §6.2's `/proc` predicate, which is the same
/// question asked less reliably. When the lock file lands, replacing the body of
/// this function with an `F_OFD_GETLK` on `PARTICIPANT_BASE + slot` is the entire
/// change: nothing in `tf_tree_core` and nothing else in this crate knows how
/// the answer is reached.
///
/// # It fails safe, in every branch
///
/// Every path that cannot *prove* death returns `true`. A false negative steals
/// the topology lock from a live mutator and lets two writers race one scratch
/// block; a false positive only makes the caller retry. §6.2 states the
/// asymmetry and the default it demands.
/// Is the process that owns this participant record still running?
///
/// The per-record half of [`participant_is_alive`], in the shape A8's rescue
/// path needs: it is handed a record, not a slot index.
///
/// A slot that is not `LIVE` is reported dead — that covers a released slot and
/// one whose registrant died partway through filling it in, since `RESERVED` is
/// held for a handful of instructions by a healthy process
/// (`docs/PHASE2.md` §11.3, `attach.after_slot_assigned_before_publish`).
///
/// `Unreadable` resolves to **alive**, and every branch is chosen the same way:
/// a false "dead" lets a rescuer take an entry from a running process, which is
/// corruption; a false "alive" only delays recovery.
fn record_is_alive(rec: &tf_tree_core::ParticipantRecord) -> bool {
    use core::sync::atomic::Ordering;
    if tf_tree_core::participant::state_of(rec.state.load(Ordering::Acquire))
        != tf_tree_core::participant::LIVE
    {
        return false;
    }
    let pid = rec.pid.load(Ordering::Relaxed);
    let start_time = rec.start_time.load(Ordering::Relaxed);
    match read_start_time(pid) {
        // PID reuse: same number, different process. Not our participant.
        ProcStartTime::Known(st) => st == start_time,
        ProcStartTime::NoSuchProcess => false,
        ProcStartTime::Unreadable => true,
    }
}

/// Build the liveness predicate for an arena, folding in the one-time reboot
/// check.
///
/// If the arena's boot id is known and differs from this host's, the segment
/// outlived a reboot: every pid in it refers to a process from a previous boot,
/// so nothing in it is alive and no per-record check could discover that. When
/// either id is unknown the comparison is skipped — treating "unknown" as
/// "different" would declare every participant dead, the false negative this
/// must never produce.
fn liveness_for(arena_boot: [u8; 16]) -> BoxedLiveness {
    let host = *host_boot_id();
    if arena_boot != [0u8; 16] && host != [0u8; 16] && arena_boot != host {
        return Box::new(|_, _| false);
    }
    Box::new(|_slot, rec| record_is_alive(rec))
}

fn participant_is_alive(
    participants: &tf_tree_core::ParticipantTable<'_>,
    slot: u32,
    arena_boot: &[u8; 16],
) -> bool {
    // The arena outlived a reboot: every pid it records belongs to a previous
    // boot and means nothing now. Only decided when *both* ids are known — an
    // unreadable boot id is stored as all-zeros, and treating "unknown" as
    // "different" would declare every participant dead, which is precisely the
    // false negative this must never produce.
    let host_boot = host_boot_id();
    if *arena_boot != [0u8; 16] && *host_boot != [0u8; 16] && arena_boot != host_boot {
        return false;
    }

    // `identity` returns `None` unless the slot is `LIVE`, so a slot that was
    // released, or that a registrant died halfway through filling in, resolves
    // to no participant at all — held by nobody, and therefore reclaimable.
    let Some((pid, start_time, _incarnation)) = participants.identity(slot) else {
        return false;
    };

    match read_start_time(pid) {
        // PID reuse: same number, different process. Not our participant.
        ProcStartTime::Known(st) => st == start_time,
        ProcStartTime::NoSuchProcess => false,
        ProcStartTime::Unreadable => true,
    }
}

/// The outcome of asking `/proc` when a process started.
///
/// Three cases, not two: "no such process" is the only one that proves death,
/// and collapsing it with "could not read" is what turns a hardened `/proc`, a
/// container without `hidepid` access, or an `EMFILE` into a false report of
/// death (`docs/PHASE2.md` §6.2).
enum ProcStartTime {
    /// Field 22 of `/proc/<pid>/stat`, in clock ticks since boot.
    Known(u64),
    /// The process does not exist.
    NoSuchProcess,
    /// It might; `/proc` would not say.
    Unreadable,
}

/// Read another process's start time (`/proc/<pid>/stat` field 22).
///
/// Field 2 is `comm`, parenthesised and free to contain spaces *and*
/// parentheses, so the scan starts after the **last** `)` — the parsing trap
/// `docs/PHASE2.md` §5.1 calls out by name.
fn read_start_time(pid: u32) -> ProcStartTime {
    let stat = match std::fs::read_to_string(std::format!("/proc/{pid}/stat")) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ProcStartTime::NoSuchProcess,
        Err(_) => return ProcStartTime::Unreadable,
    };
    let Some(after_comm) = stat.rfind(')').map(|i| &stat[i + 1..]) else {
        return ProcStartTime::Unreadable;
    };
    after_comm
        .split_whitespace()
        .nth(19)
        .and_then(|v| v.parse().ok())
        .map_or(ProcStartTime::Unreadable, ProcStartTime::Known)
}

/// This host's boot id, read once per process.
///
/// Constant for the life of the machine, so caching it keeps the contended
/// topology-lock path off the filesystem.
fn host_boot_id() -> &'static [u8; 16] {
    static ID: std::sync::OnceLock<[u8; 16]> = std::sync::OnceLock::new();
    ID.get_or_init(boot_id)
}

/// The host's Linux boot id, all 16 bytes; zeros if unavailable.
///
/// **Not hashed to 64 bits** (`docs/PHASE2.md` §1, A7). A boot id is a 128-bit
/// UUID, and folding it into a `u64` throws away exactly the property that makes
/// it useful — that two hosts, or one host across a reboot, do not collide. A
/// stale segment surviving a reboot is precisely what this detects.
fn boot_id() -> [u8; 16] {
    let Ok(text) = std::fs::read_to_string("/proc/sys/kernel/random/boot_id") else {
        return [0u8; 16];
    };
    let mut out = [0u8; 16];
    let mut nibbles = text.trim().bytes().filter_map(|b| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None, // the dashes
    });
    for byte in &mut out {
        let (Some(hi), Some(lo)) = (nibbles.next(), nibbles.next()) else {
            return [0u8; 16]; // malformed: report "unknown" rather than partial
        };
        *byte = (hi << 4) | lo;
    }
    out
}

/// This process's start time in clock ticks since boot (`/proc/self/stat` field
/// 22); `0` if unavailable.
///
/// Paired with the PID this is a **reuse-proof** process identity
/// (`docs/PHASE2.md` §1, A7 and §5.1): PIDs wrap, and a reaper that trusted a
/// bare PID could conclude a long-dead participant is alive because an unrelated
/// process now holds its number.
///
/// Field 22 is counted after the comm field, which is parenthesised and may
/// itself contain spaces and parentheses — so the scan starts after the *last*
/// `)`, not at a naive whitespace split.
fn process_start_time() -> u64 {
    let Ok(stat) = std::fs::read_to_string("/proc/self/stat") else {
        return 0;
    };
    let Some(after_comm) = stat.rfind(')').map(|i| &stat[i + 1..]) else {
        return 0;
    };
    // After `)` the fields are state(3), ppid(4), ... starttime(22), so
    // starttime is the 20th whitespace-separated token here.
    after_comm
        .split_whitespace()
        .nth(19)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// A [`LookupError`] paired with the [`Tree`] that can resolve its ids to names.
///
/// `Display` produces a human-readable message (the error itself stays `Copy` and
/// allocation-free). Obtain one with [`Tree::describe`].
pub struct Described<'a>(pub LookupError, pub &'a Tree);

impl fmt::Display for Described<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tree = self.1;
        match self.0 {
            LookupError::UnknownFrame { hash } => {
                write!(f, "unknown frame (name hash {hash:#018x})")
            }
            LookupError::Disconnected {
                target,
                source,
                cut_at,
            } => write!(
                f,
                "no path from {} to {}: disconnected at {}",
                tree.frame_name(target),
                tree.frame_name(source),
                tree.frame_name(cut_at),
            ),
            LookupError::TreeTooDeep { depth } => {
                write!(f, "path depth {depth} exceeds the maximum of {MAX}", MAX = tf_tree_core::MAX_DEPTH)
            }
            LookupError::NoData { edge } => {
                write!(f, "no samples on {}", tree.edge_name(edge))
            }
            LookupError::Extrapolation {
                edge,
                requested,
                oldest,
                newest,
            } => write!(
                f,
                "lookup on {} would extrapolate: requested {requested} ns, history [{oldest}, {newest}] ns",
                tree.edge_name(edge),
            ),
            LookupError::SlotRecycled { edge } => {
                write!(f, "the ring on {} lapped the reader mid-read", tree.edge_name(edge))
            }
            LookupError::SlotContended { edge } => {
                write!(f, "a slot on {} stayed contended too long", tree.edge_name(edge))
            }
            LookupError::TopologyChanged { plan, current } => write!(
                f,
                "plan is stale: compiled at topology generation {plan}, current is {current} (re-plan)",
            ),
            LookupError::TimeDomainMismatch { expected, got } => write!(
                f,
                "time-domain mismatch: plan expects domain {expected}, query supplied {got}",
            ),
            LookupError::MixedTimeDomains {
                edge,
                expected,
                got,
            } => write!(
                f,
                "path crosses time domains: {} is in domain {got}, the rest of the path is in domain {expected}",
                tree.edge_name(edge),
            ),
            LookupError::UnknownEdge { edge } => {
                write!(f, "{} names no usable edge in this tree", tree.edge_name(edge))
            }
            LookupError::FrameOutOfRange { frame } => write!(
                f,
                "frame id {} is out of range for this tree",
                frame.get(),
            ),
            LookupError::MissingEdge { child } => write!(
                f,
                "frame {} has a parent but no edge records the link",
                tree.frame_name(child),
            ),
            other => write!(f, "{other:?}"),
        }
    }
}

/// Failure building a [`Tree`] from a [`TreeBuilder`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum BuildError {
    /// Two edges declared the same child frame (a frame has at most one parent).
    /// Carries the child name's 64-bit hash (the declaration is name-keyed).
    #[error("two edges declare the same child (name hash {child:#018x})")]
    DuplicateEdge {
        /// 64-bit hash of the duplicated child name.
        child: u64,
    },
    /// The declared frames exceed the `u32` id space.
    #[error("too many frames for the u32 id space")]
    TooManyFrames,
    /// The declared edges exceed the `u32` id space.
    #[error("too many edges for the u32 id space")]
    TooManyEdges,
    /// The arena layout was rejected (e.g. it would exceed the `u32` offset model).
    #[error("arena layout error: {0:?}")]
    Layout(LayoutError),
    /// A frame name could not be interned (table full or 64-bit hash collision).
    #[error("frame error: {0:?}")]
    Frame(FrameError),
    /// Wiring an edge into the topology failed (cycle or out-of-range frame).
    #[error("topology error: {0:?}")]
    Topology(TopologyError),
    /// The shared-memory segment could not be created, sized, mapped or sealed.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    #[error("shared memory error: {0:?}")]
    Shm(ShmError),
    /// The participant table is full, so this process cannot join the arena.
    #[error("participant table full: {0:?}")]
    Participant(ParticipantError),
}

impl From<LayoutError> for BuildError {
    fn from(e: LayoutError) -> BuildError {
        BuildError::Layout(e)
    }
}

/// Failure re-parenting a frame at runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ReparentError {
    /// This handle was created before a `fork()` and is being used in the child.
    ///
    /// The shared mapping is `MADV_DONTFORK`, so the child has none — see
    /// [`Tree::detached`]. Not retryable: open a new tree, or `exec`.
    #[error("this handle belongs to the pre-fork process; open a new tree in the child")]
    ChildDetached,
    /// The child has no incoming edge to reuse; only frames declared with an edge
    /// can be re-parented (re-parenting allocates no new edge/capacity).
    #[error("frame {} has no edge to re-parent", child.get())]
    NoEdge {
        /// The child frame with no incoming edge.
        child: FrameId,
    },
    /// The topology mutation failed (cycle or out-of-range frame).
    #[error("topology error: {0:?}")]
    Topology(TopologyError),
    /// The arena is mapped read-only; it cannot be mutated.
    #[error("arena is mapped read-only")]
    ReadOnly,
    /// The in-arena topology mutation lock (`docs/PHASE2.md` §1, A2) is held by
    /// another participant that is still alive.
    ///
    /// Not a fault and not a wedge: the lock is bounded-spin and steals from a
    /// dead holder, so this only ever means a live peer is mid-mutation. Retry.
    #[error("the topology lock is held by live participant slot {owner_slot}")]
    LockContended {
        /// Participant slot of the holder observed when the attempt gave up.
        owner_slot: u32,
    },
}

impl From<TopologyError> for ReparentError {
    fn from(e: TopologyError) -> ReparentError {
        ReparentError::Topology(e)
    }
}

impl From<TopoLockError> for ReparentError {
    fn from(e: TopoLockError) -> ReparentError {
        match e {
            TopoLockError::Contended { owner_slot } => ReparentError::LockContended { owner_slot },
        }
    }
}

/// Failure claiming an edge for writing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ClaimApiError {
    /// This handle was created before a `fork()` and is being used in the child.
    ///
    /// The shared mapping is `MADV_DONTFORK`, so the child has none — see
    /// [`Tree::detached`]. Not retryable: open a new tree, or `exec`.
    #[error("this handle belongs to the pre-fork process; open a new tree in the child")]
    ChildDetached,
    /// The arena record was free but a live process holds the edge's lease.
    ///
    /// Reachable only through a reaper bug or `CreatePolicy::Always` byte
    /// aliasing (`docs/decisions/0005` §5). The CAS is backed out before this
    /// returns, so retrying is safe.
    #[error("edge {edge:?}: the claim record was free but its lease is held")]
    LeaseContended {
        /// The edge.
        edge: EdgeId,
    },
    /// The lock file could not be asked about the edge's lease.
    #[error("edge {edge:?}: the claim lease could not be taken")]
    LeaseUnavailable {
        /// The edge.
        edge: EdgeId,
    },
    /// A reaper cleared this claim inside the CAS-to-lease window.
    ///
    /// Everything is given back before this returns, so the correct response is
    /// simply to claim again.
    #[error("edge {edge:?}: reaped while being claimed; retry")]
    ReapedDuringClaim {
        /// The edge.
        edge: EdgeId,
    },
    /// `child` is not a frame of this tree (out of range for its frame table).
    #[error("frame {} is not a frame of this tree", child.get())]
    UnknownFrame {
        /// The out-of-range child frame.
        child: FrameId,
    },
    /// No edge attaches `child` to a parent.
    #[error("no edge attaches child frame {}", child.get())]
    NoEdge {
        /// The child frame with no incoming edge.
        child: FrameId,
    },
    /// The edge attaching `child` carries no sample ring — it is a static or
    /// tombstoned edge, and there is nothing to publish to.
    #[error("edge#{} attaching frame {} is not a dynamic edge", edge.get(), child.get())]
    NotDynamic {
        /// The child frame.
        child: FrameId,
        /// The non-dynamic edge that attaches it.
        edge: EdgeId,
    },
    /// The edge attaching `child` has a different parent than requested.
    #[error("child frame {} is attached to {actual}, not the requested {expected}", child.get())]
    ParentMismatch {
        /// The child frame.
        child: FrameId,
        /// The requested parent index.
        expected: u32,
        /// The actual parent index.
        actual: u32,
    },
    /// The edge is already claimed by a live writer.
    ///
    /// The message names the owning **participant slot**, not a pid: A3 made the
    /// claim word an indirection into the participant table, and resolving it
    /// needs the arena. `tf_tree doctor` prints both.
    #[error("edge already claimed by participant slot {}", .0.owner_slot())]
    AlreadyClaimed(tf_tree_core::ClaimError),
    /// The arena is mapped read-only, so no edge can be claimed for writing.
    #[error("arena is mapped read-only")]
    ReadOnly,
}

impl From<tf_tree_core::ClaimError> for ClaimApiError {
    fn from(e: tf_tree_core::ClaimError) -> ClaimApiError {
        ClaimApiError::AlreadyClaimed(e)
    }
}

// Small accessor so the `#[error]` attribute above can read the owning slot.
trait ClaimErrorExt {
    fn owner_slot(&self) -> u32;
}
impl ClaimErrorExt for tf_tree_core::ClaimError {
    fn owner_slot(&self) -> u32 {
        match self {
            tf_tree_core::ClaimError::EdgeAlreadyClaimed { owner_slot } => *owner_slot,
            _ => 0,
        }
    }
}
