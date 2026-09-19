//! The concrete [`Tree`] (owning a heap arena), its [`TreeBuilder`], the
//! per-edge [`Capacity`]/[`EdgeCfg`] declarations, and the `Display` wrapper
//! [`Described`].

use std::cell::Cell;
use std::collections::HashSet;
use std::fmt;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tf_tree_arena::{Arena, ArenaLayout, HeapArena, LayoutError};
#[cfg(all(feature = "shm", target_os = "linux"))]
use tf_tree_arena::{AttachMode, MappedArena, ShmError};
use tf_tree_core::arena_view::{ArenaBuilder, ArenaView};
use tf_tree_core::edge::{claim, ClaimRecord, EdgeKind, EdgeRecord, Publisher};
use tf_tree_core::frame::blake3_64;
use tf_tree_core::plan::{compile, Domain, EdgeMeta, Guard, InterpPolicy, Stamp, SystemDomain};
use tf_tree_core::topology::{TopoLockError, TopoLockView};
use tf_tree_core::{
    EdgeId, FrameError, FrameId, LookupError, ParticipantError, PushError, TopologyError,
};
use tf_tree_math::Iso3;

use crate::cache;

/// First backoff interval for this crate's two waits. Doubles up to
/// [`MAX_BACKOFF`].
pub(crate) const MIN_BACKOFF: Duration = Duration::from_micros(200);
/// Backoff ceiling for this crate's two waits; see [`MIN_BACKOFF`].
pub(crate) const MAX_BACKOFF: Duration = Duration::from_millis(4);

/// Why [`Tree::await_frames`] could not produce ids.
///
/// **Facade-local, and deliberately not a `Timeout` variant on
/// [`LookupError`]** (`docs/decisions/0019`, and `0018` for the reasoning it
/// inherits). A wall-clock concept does not belong in a `no_std` crate that
/// `0018` keeps free of one, and adding the variant there would put an
/// unreachable arm in the return type of every hot-path read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AwaitError {
    /// The budget expired with at least one name still un-interned.
    ///
    /// **A hash, not a name**, for the same reason
    /// [`LookupError::UnknownFrame`] carries one: a name that was never
    /// interned has no [`FrameId`], and D11 keeps `String`s out of error types.
    #[error("no frame with name hash {hash:#018x} appeared before the deadline")]
    Timeout {
        /// The 64-bit BLAKE3 prefix hash of the first name still missing.
        hash: u64,
    },
    /// The tree is writable, and this call refuses to guess what that means.
    ///
    /// "Does this name exist" has two defensible answers on a writable tree,
    /// because [`Tree::frame`] *interns on demand* there — a wait built on it
    /// would return instantly, with a fresh id, for a name nobody declared.
    #[error("await_frames refuses a writable tree; use Tree::frame, which interns on demand and cannot fail for absence")]
    WritableTree,
    /// The tree is a frozen `.tft` image, so no name will ever be interned into
    /// it.
    #[error("await_frames refuses a frozen .tft tree: it has no writers, so no name can appear")]
    FrozenTree,
    /// A name resolved to an error rather than to an id.
    ///
    /// Terminal, not retried. [`FrameError::FrameHashCollision`] is a permanent
    /// property of the two names involved, and [`FrameError::InternContended`]
    /// names a claimant no caller can judge — waiting on either is waiting on
    /// something that will not change on its own.
    #[error("{0}")]
    Frame(FrameError),
    /// This tree belongs to a process that no longer exists — it was opened
    /// before a `fork()` and this is the child. See [`Tree::detached`].
    #[error("this tree was opened before a fork() and is being used in the child")]
    ChildDetached,
}

/// `[Option<FrameId>; N]` → `[FrameId; N]`, or `None` if any slot is empty.
///
/// [`FrameId`] has no `Default` and this crate denies `unwrap`/`expect`, so the
/// array is seeded with a value obtained by `?` *inside* `Option`
/// (`FrameId::new(1)` is `Some` because 1 is not the root sentinel) and every
/// element is then overwritten from `found`. No allocation, and `N == 0`
/// answers `Some([])`.
fn all_interned<const N: usize>(found: &[Option<FrameId>; N]) -> Option<[FrameId; N]> {
    let mut out = [FrameId::new(1)?; N];
    for (dst, src) in out.iter_mut().zip(found.iter()) {
        *dst = (*src)?;
    }
    Some(out)
}

/// A frame record's stored — and therefore possibly truncated — name.
///
/// `FrameRecord` keeps 48 bytes and a length; a longer name was cut at intern
/// time and the cut is not recoverable here. `from_utf8_lossy` rather than a
/// refusal, because a truncation can land mid-codepoint and a frame listing that
/// fails on one bad byte tells the caller nothing about the other ninety frames.
fn stored_name(bytes: &[u8], len: u8) -> String {
    let n = (len as usize).min(bytes.len());
    String::from_utf8_lossy(&bytes[..n]).into_owned()
}

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
#[non_exhaustive]
pub struct EdgeCfg {
    /// Ring capacity (a power of two; see [`Capacity`]).
    pub capacity: Capacity,
    /// Interpolation policy; `None` uses the builder default.
    pub interp: Option<InterpPolicy>,
    /// Time-domain tag (see [`Domain`]); `None` uses the builder default.
    pub domain: Option<u8>,
    /// The rate this edge is *expected* to publish at, in milli-hertz;
    /// `0` means "not declared". Set it through [`EdgeCfg::nominal_rate_hz`]
    /// rather than by hand.
    pub nominal_rate_mhz: u32,
}

impl EdgeCfg {
    /// A config with the given `capacity`, builder-default interp/domain and no
    /// declared nominal rate.
    #[must_use]
    pub fn new(capacity: Capacity) -> EdgeCfg {
        EdgeCfg {
            capacity,
            interp: None,
            domain: None,
            nominal_rate_mhz: 0,
        }
    }

    /// Declare the rate this edge is expected to publish at, in hertz.
    ///
    /// This is the *nominal* rate — what the publisher was configured to do —
    /// and is the only thing that makes "the observed rate is wrong" a
    /// statement anybody can check. Without it a diagnostic can report what a
    /// rate *is* and never that it should have been something else
    /// (`docs/PHASE5.md` §6, `TFT007`).
    #[must_use]
    pub fn nominal_rate_hz(mut self, rate_hz: f64) -> EdgeCfg {
        let mhz = rate_hz * 1000.0;
        self.nominal_rate_mhz = if mhz.is_finite() && mhz >= 1.0 && mhz <= f64::from(u32::MAX) {
            // `round`, not `as`: `as` truncates, so a rate that arrived as
            // 19.9999 Hz through a text round-trip would declare 19_999 mHz for
            // an edge the operator wrote `20.0` for.
            mhz.round() as u32
        } else {
            0
        };
        self
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
            // Before `backing` moves into `arena`.
            cache_scope: cache_scope_for(&backing),
            arena: backing,
            participant,
            incarnation,
            liveness,
            decl: Mutex::new(()),
            #[cfg(all(feature = "shm", target_os = "linux"))]
            attachment: std::sync::Mutex::new(None),
            #[cfg(all(feature = "shm", target_os = "linux"))]
            lock_file: None,
            #[cfg(all(feature = "shm", target_os = "linux"))]
            ofd_probe: None,
            #[cfg(all(feature = "shm", target_os = "linux"))]
            fork_gen,
        })
    }

    /// Build the tree into a **shared-memory** segment instead of the heap.
    ///
    /// The returned [`Tree`] behaves identically — the read path does not know
    /// which backend it has (`docs/PHASE2.md` §4) — but its arena lives in a
    /// sealed `memfd` that other processes can map with [`Tree::attach_shared`].
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
            // Before `backing` moves into `arena`.
            cache_scope: cache_scope_for(&backing),
            arena: backing,
            participant,
            incarnation,
            liveness,
            decl: Mutex::new(()),
            #[cfg(all(feature = "shm", target_os = "linux"))]
            attachment: std::sync::Mutex::new(None),
            #[cfg(all(feature = "shm", target_os = "linux"))]
            lock_file: None,
            #[cfg(all(feature = "shm", target_os = "linux"))]
            ofd_probe: None,
            #[cfg(all(feature = "shm", target_os = "linux"))]
            fork_gen,
        })
    }

    /// The shared body of [`TreeBuilder::build`] and
    /// [`TreeBuilder::build_shared`]: everything except *which* allocation the
    /// bytes land in.
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
        let mut arena = make(
            &layout,
            std::process::id(),
            process_start_time().unwrap_or(UNKNOWN_START_TIME),
            boot_id,
        )?;
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
                        let mut record = EdgeRecord::dynamic(
                            parent.get(),
                            child.get(),
                            capacity,
                            running_off,
                            running_off,
                            interp.as_u8(),
                            domain,
                        );
                        // Assigned rather than passed to `dynamic()`: that
                        // constructor is already at clippy's seven-argument
                        // limit, and the field is plain data with no invariant
                        // tying it to the ring layout the constructor computes.
                        record.nominal_rate_mhz = cfg.nominal_rate_mhz;
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
            // and permanently `ReadOnly`, so there is no mode in which this can
            // be true.
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

    /// Whether this arena is a `.tft` image rather than something a process
    /// could still be writing to.
    fn is_frozen(&self) -> bool {
        match self {
            #[cfg(all(feature = "shm", target_os = "linux"))]
            ArenaBacking::Frozen(_) => true,
            #[cfg(all(feature = "shm", target_os = "linux"))]
            ArenaBacking::Mapped(_) => false,
            ArenaBacking::Heap(_) => false,
        }
    }
}

/// A claimed edge: the arena record, and the lease that makes its holder's
/// death observable (`docs/PHASE2.md` §6.1, `docs/decisions/0005` §5).
pub struct EdgeWriter<'a> {
    publisher: Publisher<'a>,
    /// The fork generation this writer was claimed in.
    ///
    /// `Copy`, so it takes no part in the drop order above — which is why it is
    /// allowed to sit between the two fields that do have one. (This comment
    /// used to claim it was here "to keep the two drop-ordered fields
    /// adjacent"; it is what makes them *not* adjacent.)
    #[cfg(all(feature = "shm", target_os = "linux"))]
    fork_gen: Option<u64>,
    /// Held purely for its `Drop`, hence the underscore — nothing reads it, and
    /// the observable effect is releasing the byte when this value dies.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    _lease: Option<ClaimLease>,
    /// The claim record this writer owns — the one field of it [`Publisher`]
    /// never writes.
    claim: &'a ClaimRecord,
    /// Pushes between clock-offset samples. Fixed for this writer’s life,
    /// derived once at claim time from the edge’s declared nominal rate.
    sample_every: u32,
    /// Pushes remaining before the next sample. Read and written only by
    /// [`EdgeWriter::push`].
    until_sample: Cell<u32>,
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
    pub fn push(&self, stamp: i64, iso: &Iso3) -> Result<(), PushError> {
        #[cfg(all(feature = "shm", target_os = "linux"))]
        if self.detached() {
            return Err(PushError::ChildDetached);
        }
        // **The `?` is load-bearing, not a style choice** (`docs/decisions/0036`
        // plan step 1). It is what places the clock read after the ring write —
        // outside the seqlock window, and skipped entirely on a push that never
        // happened. An offset recorded by a `ClaimRevoked` push would be an
        // offset for nothing, and is the observable form of the read having
        // drifted inside the window.
        self.publisher.push(stamp, iso)?;
        self.sample_clock_offset(stamp);
        Ok(())
    }

    /// Count this push and, once every `sample_every` of them, record the
    /// publisher's clock offset: the host wall clock minus `stamp`.
    #[inline]
    fn sample_clock_offset(&self, stamp: i64) {
        let remaining = self.until_sample.get();
        if remaining != 0 {
            self.until_sample.set(remaining - 1);
            return;
        }
        // **Not a wall-clock edge: never sample.** `sample_every == 0` is that
        // case, and the test is here rather than beside the countdown so the
        // sampling edges — the ones that pay for this method — see an unchanged
        // hot path. `until_sample` stays `0`, so such an edge takes this branch
        // on every push and reaches nothing further.
        if self.sample_every == 0 {
            return;
        }

        // **A clock this process cannot read is not an offset of zero** — see
        // `now_nanos`. Returning leaves the field at whatever it held, and on a
        // fresh claim that is `0`, which reads as *no sample yet*.
        let Some(now) = now_nanos() else {
            return;
        };

        // `sample_every` is at least 1 here, so the reload cannot underflow —
        // `sample_interval` returns either `0` (handled above) or a clamped
        // positive count.
        self.until_sample.set(self.sample_every - 1);

        // `Relaxed`: this orders nothing. It is a diagnostic scalar read by a
        // separate process that is already tolerating a torn view of the whole
        // arena, and giving it a `Release` would put a fence on the publish path
        // to publish a number nothing waits on.
        self.claim
            .clock_offset_nanos
            .store(recorded_offset(now, stamp), Ordering::Relaxed);
    }
}

/// Pushes between clock-offset samples for an edge that declares **no** nominal
/// rate (`EdgeRecord::nominal_rate_mhz == 0` — *not declared*, which is the
/// reading `TFT007` already takes of that value).
const DEFAULT_SAMPLE_EVERY: u32 = 1024;

/// The value the sampler stores for a push received at `now` bearing `stamp`.
///
/// Split out of [`EdgeWriter::push`]'s sampler because both of its rules are
/// about values a clock will not produce on demand, and a test that cannot
/// construct its input is a test that does not exist.
fn recorded_offset(now: i64, stamp: i64) -> i64 {
    match now.saturating_sub(stamp) {
        0 => 1,
        offset => offset,
    }
}

/// Pushes between clock-offset samples for an edge, or **`0` for "never"**.
///
/// `nominal_rate_mhz` is **milli**hertz — [`EdgeCfg::nominal_rate_hz`] stores
/// `rate_hz * 1000.0` — so the quotient by 1000 is pushes per second, and *one
/// sample per that many pushes* is exactly the rule `docs/decisions/0036`
/// question 1 ratifies: one offset per second of published data, at any rate.
fn sample_interval(domain: u8, nominal_rate_mhz: u32) -> u32 {
    if domain != <SystemDomain as Domain>::TAG {
        return 0;
    }
    match nominal_rate_mhz {
        0 => DEFAULT_SAMPLE_EVERY,
        mhz => (mhz / 1000).max(1),
    }
}

impl Drop for EdgeWriter<'_> {
    fn drop(&mut self) {
        // `Publisher`'s own `Drop` releases the claim with a `compare_exchange`
        // *into the arena*. In a `fork` child that arena is a hole in the
        // address space, so the destructor faults — and it does so whether or
        // not the child ever called anything, which is what makes it the most
        // dangerous of the four inherited destructors.
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

/// An [`EdgeWriter`] that owns its tree — the claim shape for a writer that is
/// **stored** rather than scoped (`docs/decisions/0017`, `docs/API.md` §2.1).
///
/// # It carries no lifetime, which is the entire point
///
/// ```
/// use std::sync::Arc;
/// use tf_tree::{Capacity, EdgeCfg, Iso3, OwnedWriter, TreeBuilder};
///
/// // No lifetime parameter on the user's type. `EdgeWriter<'a>` cannot do this.
/// struct OdomPublisher {
///     writer: OwnedWriter,
/// }
///
/// let tree = Arc::new(
///     TreeBuilder::new()
///         .dynamic_edge("odom", "base", EdgeCfg::new(Capacity::slots(64)))
///         .build()
///         .expect("layout"),
/// );
/// let base = tree.frame("base").unwrap();
/// let odom = tree.frame("odom").unwrap();
/// let node = OdomPublisher {
///     writer: tree.claim_owned(base, odom).expect("claim"),
/// };
///
/// // The caller's handle goes away; the writer keeps the arena alive by itself.
/// drop(tree);
/// node.writer.push(1_000, &Iso3::IDENTITY).expect("push");
/// ```
///
/// # Auto traits
///
/// `OwnedWriter` is `Send`:
/// ```
/// fn assert_send<T: Send>() {}
/// assert_send::<tf_tree::OwnedWriter>();
/// ```
///
/// but deliberately **not** `Sync` (this must fail to compile):
/// ```compile_fail,E0277
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<tf_tree::OwnedWriter>();
/// ```
pub struct OwnedWriter {
    /// The claim, with its borrow of the tree below extended to `'static`.
    ///
    /// Declared first so it drops first — see the type's doc comment.
    writer: Box<EdgeWriter<'static>>,
    /// The strong reference that makes the field above's `'static` true.
    ///
    /// Never read, hence the `allow` — but **do not delete it**, and do not
    /// replace it with a `PhantomData`. It is the entire safety argument for the
    /// `'static` above; removing it leaves a writer pointing into an arena
    /// nothing is keeping alive, which is a use-after-free that compiles.
    #[allow(dead_code)]
    tree: Arc<Tree>,
}

impl OwnedWriter {
    /// The edge this writer owns.
    ///
    /// The same accessor [`Publisher::edge`] gives a scoped writer through
    /// [`EdgeWriter`]'s `Deref`, forwarded by hand because `OwnedWriter` has no
    /// `Deref`: one to [`Publisher`] would also expose [`Publisher::push`],
    /// which is the copy without the fork check (see [`Self::push`]).
    #[inline]
    #[must_use]
    pub fn edge(&self) -> EdgeId {
        self.writer.edge()
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
    #[inline]
    pub fn push(&self, stamp: i64, iso: &Iso3) -> Result<(), PushError> {
        self.writer.push(stamp, iso)
    }

    /// Release the claim now, instead of at the end of the enclosing scope.
    ///
    /// Identical to dropping the value — which is the point of naming it. A
    /// stored writer's scope is often a whole process, so "drop it" is advice
    /// with nowhere to land, and `let _ = writer;` is the spelling that
    /// silently does *not* release.
    pub fn release(self) {
        drop(self);
    }
}

/// Fired inside [`Tree::claim`], after the arena CAS and before the lease
/// `SETLK`.
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

/// How many times [`Tree::reparent`] re-attempts A2's topology byte before it
/// reports contention.
#[cfg(all(feature = "shm", target_os = "linux"))]
const TOPO_BYTE_ATTEMPTS: u32 = 32;

/// Holds A2's topology byte for as long as this value lives
/// (`docs/decisions/0029`).
#[cfg(all(feature = "shm", target_os = "linux"))]
struct TopologyLease<'a> {
    lock: &'a tf_tree_ipc::LockFile,
}

#[cfg(all(feature = "shm", target_os = "linux"))]
impl Drop for TopologyLease<'_> {
    fn drop(&mut self) {
        // Best effort, for `ClaimLease`'s reason.
        let _ = self.lock.release_topology();
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
pub struct Tree {
    arena: ArenaBacking,
    /// Which *arena* this tree reads, for the per-thread plan cache's key
    /// (`crate::cache`).
    cache_scope: u64,
    /// This process's slot in the arena's participant table.
    ///
    /// Claims name this slot rather than a PID (`docs/PHASE2.md` §1, A3), which
    /// is what lets a claim publish its owner and its identity in one store.
    participant: u32,
    /// The incarnation this process's participant slot carried when it
    /// registered.
    incarnation: u64,
    /// Decides whether a participant slot's owner is still running.
    ///
    /// Boxed and stored rather than built per call because it must outlive the
    /// [`ArenaView`] that borrows it, and because the reboot check is a property
    /// of the *arena*, not of any one record: if the segment predates this boot,
    /// every pid it names belongs to a dead world and no `/proc` lookup can tell
    /// you so. That comparison is made once, here, and collapses into the
    /// closure.
    liveness: BoxedLiveness,
    /// Serializes *this process's* threads through [`Self::reparent`].
    ///
    /// Not the real lock and never was — a `Mutex` is per-process, so it
    /// serializes nothing against a peer that mapped the same segment
    /// (`docs/PHASE2.md` §1, A2). The arena's `TopoLock` is what makes the
    /// mutation exclusive; this one is **also load-bearing**, and not merely an
    /// optimisation, so do not remove it as redundant.
    decl: Mutex<()>,
    /// What keeps this process attached, for a tree obtained from
    /// [`crate::open`].
    #[cfg(all(feature = "shm", target_os = "linux"))]
    /// The rendezvous attachment, behind a lock so recovery needs only `&self`.
    ///
    /// **`Mutex` and not a bare `Option`:**
    /// [`0044`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0044-recovery-the-languages-a-robot-is-written-in-cannot-reach.md)
    /// §1 and *Why a `Mutex` and not a `RefCell` or an atomic swap*.
    attachment: std::sync::Mutex<Option<crate::open::Attachment>>,
    /// This tree's open file description on the rendezvous lock file.
    ///
    /// **Two roles, one description, and the sharing is the point.** It carries
    /// this tree's claim leases (§6.1) *and* A2's topology byte
    /// (`docs/decisions/0029`), and both want the same lifetime: the byte a
    /// description holds is released by the kernel when that description closes,
    /// which is exactly at process death by any means. A second description
    /// would cost an `open(2)` and decide nothing.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    lock_file: Option<std::sync::Arc<tf_tree_ipc::LockFile>>,
    /// The kernel-authoritative liveness probe, kept as a *probe* and not only
    /// as the closure it is folded into.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    ofd_probe: Option<std::sync::Arc<crate::open::LivenessProbe>>,
    /// The fork generation this tree was opened in, or `None` for a backing that
    /// survives a `fork` intact.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    fork_gen: Option<u64>,
}

impl Tree {
    /// Whether this tree belongs to a process that no longer exists — it was
    /// opened before a `fork()` and this is the child.
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

    /// The crate-internal view, and **the one this crate's own modules use**.
    ///
    /// [`Tree::arena_view`] is the public spelling of it and is gated on the
    /// `unstable` feature (`docs/API.md` §2.6); `frozen.rs` and `open.rs` are
    /// inside the facade and must not reach a public surface to do their work,
    /// or the feature would be load-bearing for a default build.
    pub(crate) fn view(&self) -> ArenaView<'_> {
        // The detached case first, and unconditionally: every accessor below
        // funnels through here, so this is the one place that has to be right
        // for a read of the vanished mapping to be impossible rather than
        // merely unlikely.
        #[cfg(all(feature = "shm", target_os = "linux"))]
        if self.detached() {
            return ArenaView::new(poison_arena());
        }
        // Both builders are load-bearing for A8: `as_participant` lets a rescuer take over a
        // stalled interner, `with_liveness` makes a dead claimant's entry recoverable.
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
    /// **[`FrameError::ReadOnly`] if this tree is a read-only attachment and
    /// `name` is not already interned** — which is the most common failure on
    /// the default attach (`Open::new` is read-only, D18), and the one
    /// this section used not to mention at all. It is not a permissions
    /// complaint about a name that exists: it means *this name is not declared
    /// and I cannot declare it*. A consumer racing its publisher wants
    /// [`Tree::await_frames`]; a consumer that will never see the name declared
    /// wants the creator to declare it, or `frame_headroom` and a writable
    /// attach.
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

    /// Every interned frame's name, in [`FrameId`] order.
    ///
    /// The stable answer to "what is in this tree", and the plural of
    /// [`Tree::frame`]. It is on the *stable* surface deliberately: an embedder
    /// must not have to enable the `unstable` feature to ask what their own tree
    /// contains, which is what gating `Tree::arena_view` would otherwise have
    /// forced (`docs/API.md` §2.6, §7 check 1). Python has had `tree.frames()`
    /// since §3.2 and this is its mirror.
    ///
    /// # Errors
    ///
    /// [`LookupError::ChildDetached`] on a tree inherited across a `fork()`.
    /// Such a tree reads a one-frame poison arena, so answering would hand back
    /// a plausible-looking short list instead of naming the fork.
    pub fn frames(&self) -> Result<Vec<String>, LookupError> {
        if self.detached() {
            return Err(LookupError::ChildDetached);
        }
        let view = self.view();
        // **`Relaxed`, and that is the justified ordering rather than the cheap
        // one.** `tf_tree_core::frame`'s `finish` does `frame_count.fetch_add`
        // *first*, then `write_record`, then the Release publish into the intern
        // table. An `Acquire` load here would therefore order this thread
        // against everything the interner did *before* it took its id — and
        // against nothing it did after, which is precisely the record about to
        // be read. Acquire would read like a guarantee and buy none.
        let count = view.header().frame_count.load(Ordering::Relaxed);
        let mut out = Vec::with_capacity(count as usize);
        for raw in 1..=count {
            // The strictest set of checks any copy of this walk applied.
            let Some(id) = FrameId::new(raw) else {
                continue;
            };
            let Some(rec) = view.frame_record(id) else {
                continue;
            };
            // **The count is bumped before the record is written**, so an
            // interner in another process can be counted here one instant
            // before its name exists and the slot still reads as zeros. A
            // written record's `name_hash` is BLAKE3 of the name — non-zero
            // even for `""` — so a zero hash means "not written yet". Skipping
            // it lists that frame one call later; taking it prints a frame with
            // an empty name, which reads as our bug rather than as a race lost
            // by a microsecond.
            if rec.name_hash == 0 {
                continue;
            }
            out.push(stored_name(&rec.name, rec.name_len));
        }
        Ok(out)
    }

    /// Wait until every name in `names` is interned, and return their ids.
    ///
    /// **The second of `docs/decisions/0019` §2b's two waits.**
    /// `Open::await_open` waits for the *arena* to exist; this waits
    /// for *names* to be interned into an arena that already does. They are two
    /// different absences, and a consumer that started before its publisher
    /// meets both.
    ///
    /// # Errors
    ///
    /// [`AwaitError::WritableTree`] immediately on a writable tree;
    /// [`AwaitError::FrozenTree`] immediately on a frozen `.tft`;
    /// [`AwaitError::ChildDetached`] on a tree inherited across a `fork()`;
    /// [`AwaitError::Frame`] if a name resolves to a hash collision or a
    /// contended interner; [`AwaitError::Timeout`] carrying the hash of the
    /// **first** name still missing when the budget ran out — first in `names`
    /// order, not first probed, so a request whose leading names resolved names
    /// the earliest one that did not.
    ///
    /// # Examples
    ///
    /// The refusal, which is the part of the contract a caller most needs to
    /// know and the only part reachable without a shared arena:
    ///
    /// ```
    /// use std::time::Duration;
    /// use tf_tree::{AwaitError, Iso3, TreeBuilder};
    ///
    /// let tree = TreeBuilder::new()
    ///     .static_edge("map", "odom", &Iso3::IDENTITY)
    ///     .build()
    ///     .expect("layout");
    ///
    /// // A heap tree is writable, so `Tree::frame` would intern "no_such_frame"
    /// // on demand and hand back an id for a frame nobody declared. This says so
    /// // instead — and says it in microseconds, not after the five seconds.
    /// let started = std::time::Instant::now();
    /// assert_eq!(
    ///     tree.await_frames(["map", "no_such_frame"], Duration::from_secs(5)),
    ///     Err(AwaitError::WritableTree),
    /// );
    /// assert!(started.elapsed() < Duration::from_millis(100));
    ///
    /// // On that tree the right call is `Tree::frame`, which cannot fail for
    /// // absence.
    /// assert!(tree.frame("map").is_ok());
    /// ```
    pub fn await_frames<const N: usize>(
        &self,
        names: [&str; N],
        timeout: Duration,
    ) -> Result<[FrameId; N], AwaitError> {
        // Before any sleep, and before any arena read: this is a property of
        // the handle, and burning a five-second budget to report it would be
        // the worst of both answers.
        if self.is_writable() {
            return Err(AwaitError::WritableTree);
        }
        // The other statically-futile handle: distinct condition, distinct
        // answer. See `AwaitError::FrozenTree`.
        if self.arena.is_frozen() {
            return Err(AwaitError::FrozenTree);
        }
        let start = std::time::Instant::now();
        let mut found: [Option<FrameId>; N] = [None; N];
        let mut backoff = MIN_BACKOFF;
        loop {
            // **Per iteration, and before `view()`** — see
            // `AwaitError::ChildDetached`.
            if self.detached() {
                return Err(AwaitError::ChildDetached);
            }
            let view = self.view();
            for (slot, name) in found.iter_mut().zip(names.iter()) {
                if slot.is_some() {
                    // Memoized; see this method's doc.
                    continue;
                }
                match view.find_frame(name) {
                    Ok(id) => *slot = id,
                    // Terminal — see `AwaitError::Frame`.
                    Err(e) => return Err(AwaitError::Frame(e)),
                }
            }
            if let Some(ids) = all_interned(&found) {
                return Ok(ids);
            }
            // Deadline **after** the work and **before** the sleep, so a name
            // interned during the last iteration is reported rather than napped
            // past. `saturating_*` throughout: `Duration` subtraction panics.
            if start.elapsed() >= timeout {
                let hash = names
                    .iter()
                    .zip(found.iter())
                    .find(|(_, slot)| slot.is_none())
                    // Unreachable: `all_interned` returned `None`, so some slot
                    // is empty. `map_or` rather than `unwrap` because this crate
                    // denies both, and a wrong hash is a worse answer than a
                    // panic only if somebody matches on it, which R5 forbids.
                    .map_or(0, |(name, _)| blake3_64(name));
                return Err(AwaitError::Timeout { hash });
            }
            // Never sleep past the caller's deadline.
            let left = timeout.saturating_sub(start.elapsed());
            std::thread::sleep(core::cmp::min(backoff, left));
            backoff = core::cmp::min(backoff * 2, MAX_BACKOFF);
        }
    }

    /// Every declared edge as a `(parent, child)` name pair, in [`EdgeId`]
    /// order.
    ///
    /// # Errors
    ///
    /// [`LookupError::ChildDetached`] on a tree inherited across a `fork()`.
    /// The poison arena such a tree reads has zero edges, so without this the
    /// answer is a silent empty list.
    pub fn edges(&self) -> Result<Vec<(String, String)>, LookupError> {
        if self.detached() {
            return Err(LookupError::ChildDetached);
        }
        let view = self.view();
        // `edge_count` is stored as (declared edges + 1 sentinel), so the real
        // ids are `1..edge_count` — `tf_tree_core::EdgeId`'s own doc comment,
        // and the off-by-one that cost `tf_tree_c::unstable` a test.
        let count = view.header().edge_count.load(Ordering::Relaxed);
        let mut out = Vec::with_capacity(count.saturating_sub(1) as usize);
        for raw in 1..=count {
            // One observation of the record, not two: re-reading `view.edge`
            // for the child could name a parent and a child that never belonged
            // to the same edge.
            let Some(rec) = view.edge(EdgeId(raw)) else {
                continue;
            };
            // `None` from either endpoint means a slot whose record is still
            // zeros, because a zeroed record names frame 0 and `FrameId::new(0)`
            // declines. **That is what keeps the sentinel and any headroom slot
            // out of this list**, not the loop bound, so the tempting "never
            // drop an entry" fallback to a `<root>` placeholder would put
            // `("", "")`-shaped noise in the answer instead.
            let name = |f: u32| -> Option<String> {
                let r = view.frame_record(FrameId::new(f)?)?;
                Some(stored_name(&r.name, r.name_len))
            };
            let (Some(parent), Some(child)) = (name(rec.parent), name(rec.child)) else {
                continue;
            };
            out.push((parent, child));
        }
        Ok(out)
    }

    /// Re-parent an existing `child` frame under `new_parent`, reusing the child's
    /// already-declared edge (no new capacity is allocated). This is the only
    /// runtime topology mutation; it bumps the topology generation, invalidating
    /// compiled [`crate::Plan`]s so they recompile against the new shape.
    ///
    /// # Errors
    ///
    /// [`ReparentError::NoEdge`] if `child` has no incoming edge to reuse,
    /// [`ReparentError::Topology`] if the move would create a cycle or references
    /// an out-of-range frame, [`ReparentError::LockContended`] if a live peer
    /// holds the topology lock (retry), or [`ReparentError::TopologyLease`] if
    /// the lock file could not be asked at all.
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

        // **The byte first, and the guards drop the other way round.** `_lease`
        // is declared before `_topo`, so Rust's reverse-declaration drop order
        // releases the word and only then the byte. Reversing that would leave a
        // window in which the byte is free and the word still names this
        // process — which is exactly the signature a peer reads as "the holder
        // is dead or has no lock file", so it would spin out its budget and then
        // consult `/proc` about a process that is merely finishing. The order is
        // enforced by scope rather than by this comment; the comment says why
        // the scope is shaped that way.
        #[cfg(all(feature = "shm", target_os = "linux"))]
        let _lease = self.take_topology_lease(&lock)?;

        let participants = view.participants();
        let arena_boot = header.boot_id;
        let is_alive = move |slot: u32| participant_is_alive(&participants, slot, &arena_boot);
        let _topo = lock.acquire(self.participant, now_nanos().unwrap_or(0), &is_alive)?;

        // `docs/PHASE2.md` §11.3: **`topo.holding_lock`**. Both locks held and
        // the topology not yet mutated — the state that row is about, and the
        // one instruction where a death leaves *the kernel* holding the repair.
        #[cfg(feature = "crash-points")]
        tf_tree_core::crash::maybe_abort(crate::open::CRASH_SITES[1]);

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

    /// Take A2's topology byte, where there is a lock file to take it from.
    ///
    /// `Ok(None)` for a tree that went through no rendezvous — a heap tree, a
    /// directly-called `TreeBuilder::build_shared`, an `attach_shared` over an
    /// inherited fd. Those have no lock file, so the arena word alone is the
    /// exclusion exactly as it was before this byte existed, and the `/proc`
    /// predicate is the whole of what decides a steal. That residual is
    /// `docs/decisions/0029`'s T3 and it is stated there rather than narrowed
    /// here: narrowing it means refusing those callers, which is a breaking
    /// change on a public path and a separate decision.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    fn take_topology_lease(
        &self,
        topo: &TopoLockView<'_>,
    ) -> Result<Option<TopologyLease<'_>>, ReparentError> {
        let Some(file) = self.lock_file.as_ref() else {
            return Ok(None);
        };
        // Bounded, for `TOPO_LOCK_SPIN_LIMIT`'s reason: an unbounded wait here
        // would wedge every mutator behind a holder that is merely slow, and the
        // whole point of putting this lock in the kernel is that a holder which
        // *died* needs nobody's patience at all.
        for _ in 0..TOPO_BYTE_ATTEMPTS {
            match file.try_take_topology() {
                Ok(tf_tree_ipc::LockAttempt::Acquired) => {
                    return Ok(Some(TopologyLease { lock: file }))
                }
                Ok(tf_tree_ipc::LockAttempt::Contended) => {}
                // **Not folded into `LockContended`.** Refusing is the safe
                // direction either way, but a refusal that names a live peer
                // when the real cause is `EBADF` sends an operator to look for
                // a process that is not there — the shape of wrong diagnosis
                // `TFT014` and `TopoLockView::finish` were both fixed for. Also
                // **not retried**: the loop is waiting out a peer, and there is
                // no peer.
                Err(tf_tree_ipc::IpcError::LockFailed { errno, .. }) => {
                    return Err(ReparentError::TopologyLease {
                        raw_os_error: errno.raw_os_error(),
                    })
                }
                // `try_take_topology` produces no other variant. Mapping it to a
                // zero errno rather than `unreachable!()`: this is a refusal
                // path, and a panic here would turn a diagnosable failure into a
                // crash inside a mutation protocol.
                Err(_) => return Err(ReparentError::TopologyLease { raw_os_error: 0 }),
            }
        }
        Err(ReparentError::LockContended {
            owner_slot: topo.holder(),
        })
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
        let (Some(ring), Some(claim_rec), Some(edge_rec)) =
            (view.ring(eid), view.claim(eid), view.edge(eid))
        else {
            return Err(ClaimApiError::NotDynamic { child, edge: eid });
        };
        // Two-phase acquire (`docs/decisions/0005` §5).
        let (epoch, owner) = claim(claim_rec, self.participant)
            .map_err(|cause| ClaimApiError::AlreadyClaimed { edge: eid, cause })?;

        // The CAS has landed and the lease has not been taken: the one place a
        // reaper can be placed inside `take_claim_lease`'s window on purpose.
        #[cfg(all(feature = "test-hooks", feature = "shm", target_os = "linux"))]
        if let Some(hook) = CLAIM_WINDOW_HOOK.get() {
            hook();
        }

        #[cfg(all(feature = "shm", target_os = "linux"))]
        let lease = self.take_claim_lease(eid, claim_rec, epoch, owner)?;

        // **The recorded offset is cleared, because a claim inherits the edge and
        // not the writer** (`docs/decisions/0036`). Nothing else in the system
        // resets `clock_offset_nanos` — not `release`, not the reaper, not
        // `tf_tree_core::edge::claim` — so without this line a fresh writer
        // publishes under the *previous* writer's offset until its own first
        // sample lands. `TFT004`'s "nothing sampled yet" skip is
        // `clock_offset_nanos == 0`, which would not fire, so the check would
        // report an hours-wide clock skew against a publisher whose clock is
        // perfect. `Relaxed` for the same reason the sampling store is: it
        // orders nothing, and the CAS above has already made this edge ours.
        claim_rec.clock_offset_nanos.store(0, Ordering::Relaxed);

        // §7.1's per-edge population, writer half. This is the moment the edge
        // is taken up, and it is off the publish path — `push` is what must not
        // fault, and it now cannot, for the first lap and every lap after.
        #[cfg(all(feature = "shm", target_os = "linux"))]
        self.populate_edge_rings(eid);

        // `docs/decisions/0036` step 1: one division, here, per claim — never on
        // the push path.
        let sample_every = sample_interval(edge_rec.domain, edge_rec.nominal_rate_mhz);

        Ok(EdgeWriter {
            publisher: Publisher::new(ring, claim_rec, epoch, owner),
            #[cfg(all(feature = "shm", target_os = "linux"))]
            fork_gen: self.fork_gen,
            #[cfg(all(feature = "shm", target_os = "linux"))]
            _lease: lease,
            claim: claim_rec,
            sample_every,
            // **Zero, so the claim's *first* push samples**, and every
            // `sample_every`-th one after it. Starting a full interval away
            // instead would leave `clock_offset_nanos == 0` — indistinguishable
            // from the pre-`0036` state where nothing wrote it — for 102 s on a
            // 10 Hz edge with no declared rate and about 85 minutes on a 0.2 Hz
            // one, which are exactly the slow publishers §6.4 is about. It costs
            // nothing: the same countdown, entered at its end instead of its
            // start.
            until_sample: Cell::new(0),
        })
    }

    /// Claim `child`'s edge, keeping the tree alive for as long as the writer
    /// lives (`docs/decisions/0017`).
    ///
    /// # Errors
    ///
    /// [`ClaimApiError`], exactly as [`Tree::claim`] — this is that call with
    /// the tree's own handle stapled to the result.
    pub fn claim_owned(
        self: &Arc<Tree>,
        child: FrameId,
        parent: FrameId,
    ) -> Result<OwnedWriter, ClaimApiError> {
        // The one `unsafe` in this crate (`docs/decisions/0017`). `deny` rather
        // than `forbid` at the crate root exists so this `allow` is greppable;
        // `rg 'allow\(unsafe_code\)' crates/tf_tree/src` must return this line
        // and nothing else.
        #[allow(unsafe_code)]
        // SAFETY: the `Tree` lives inside the `Arc`'s heap allocation, and the
        // `Arc::clone` stored beside the writer below is what keeps that
        // allocation alive for as long as the writer exists. Three facts, and
        // all three are needed:
        //
        // 1. An `Arc`'s contents never move — the `Tree` is *in* the allocation
        //    the `Arc` points at, so a reference into it stays valid across
        //    every clone, move and send of the handle. This is a shared
        //    reference into a shared-only allocation: `Arc` hands out `&mut`
        //    only through `get_mut`/`try_unwrap`, and fact 2 makes both fail.
        // 2. The clone is a *strong* reference, so there is no safe way for a
        //    caller to move the `Tree` out from under the writer, or to drop the
        //    allocation while it lives.
        // 3. `OwnedWriter`'s two fields drop writer-then-`Arc` (declaration
        //    order), so the claim release lands while the arena is still mapped.
        //
        // This extends exactly one reference (the borrow of the `Tree`), and the claim is taken
        // through it below, so the writer provably borrows the `Tree` the `Arc::clone` refers to.
        let tree: &'static Tree = unsafe { &*Arc::as_ptr(self) };

        // Deliberately the same `claim` every other caller uses, rather than a
        // second copy of its body: the two-phase acquire above — CAS, hook
        // window, lease, epoch re-check — is the part of this file most likely
        // to be edited and least likely to survive being written twice.
        let writer = tree.claim(child, parent)?;

        Ok(OwnedWriter {
            // Boxed for soundness — `OwnedWriter`'s *Why the writer is behind a
            // `Box`* section, and `just miri` if it is ever un-boxed. The
            // allocation is paid once, at claim time, on a path that already
            // does two syscalls.
            writer: Box::new(writer),
            tree: Arc::clone(self),
        })
    }

    /// Phase two: take the lease, then prove the record is still ours.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    fn take_claim_lease(
        &self,
        eid: EdgeId,
        claim_rec: &tf_tree_core::edge::ClaimRecord,
        epoch: u64,
        owner: u64,
    ) -> Result<Option<ClaimLease>, ClaimApiError> {
        let Some(lock) = self.lock_file.as_ref() else {
            // No rendezvous (a heap tree, or an `attach_shared` over an
            // inherited fd). The CAS alone is the claim, exactly as before —
            // the lease adds observability, not correctness.
            return Ok(None);
        };
        match lock.try_take_claim(eid.0) {
            Ok(tf_tree_ipc::LockAttempt::Acquired) => {}
            Ok(tf_tree_ipc::LockAttempt::Contended) => {
                // The record was free but a live process holds the lease.
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
        // §7.1's per-edge population, reader half, done from `compile`'s own
        // edge callback — which `compile` invokes for exactly the edges it
        // walks. Compilation is off the query path by D3 (`Plan::at` is the hot
        // tier and this is not it), so the guarantee that matters — no fault
        // *inside* a lookup — is preserved by warming here rather than by
        // warming every ring in the arena at attach.
        #[cfg(all(feature = "shm", target_os = "linux"))]
        let edge_meta = |eid| {
            self.populate_edge_rings(eid);
            edge_meta(&view, eid)
        };
        #[cfg(not(all(feature = "shm", target_os = "linux")))]
        let edge_meta = |eid| edge_meta(&view, eid);

        compile(&topo, edge_meta, target, source)
    }

    /// Fault in one dynamic edge's two rings (`docs/PHASE2.md` §7.1).
    #[cfg(all(feature = "shm", target_os = "linux"))]
    fn populate_edge_rings(&self, eid: EdgeId) {
        let ArenaBacking::Mapped(arena) = &self.arena else {
            return;
        };
        if let Some(extents) = self.view().ring_extents(eid) {
            for (off, len) in extents {
                arena.populate(off, len);
            }
        }
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
        #[cfg(all(feature = "shm", target_os = "linux"))]
        let g = if self.is_shared() {
            g.with_fork_check(tf_tree_ipc::fork::generation)
        } else {
            g
        };
        g
    }

    /// Convenience lookup by name at a stamp: **resolves** the names, compiles
    /// (or reuses a cached) [`crate::Plan`], and evaluates it.
    ///
    /// # Errors
    ///
    /// [`LookupError::UnknownFrame`] if a name does not resolve to a frame, or
    /// any compilation / evaluation error. `UnknownFrame` also covers
    /// [`FrameError`]'s read-side refusals.
    pub fn lookup<D: Domain>(
        &self,
        target: &str,
        source: &str,
        stamp: Stamp<D>,
    ) -> Result<Iso3, LookupError> {
        self.lookup_tagged(target, source, stamp.nanos(), D::TAG)
    }

    /// [`Self::lookup`], with the query's domain carried as a runtime tag.
    ///
    /// The convenience tier's tagged sibling, for the reason
    /// `docs/decisions/0038-the-domain-a-binding-cannot-name.md` gives: [`Domain`]
    /// is an open trait, so a foreign binding cannot name the type the typed
    /// form needs and carries the tag as data instead. The check, the cache and
    /// the evaluation are all [`Self::lookup`]'s, unchanged.
    ///
    /// # Errors
    ///
    /// As [`Self::lookup`].
    pub fn lookup_tagged(
        &self,
        target: &str,
        source: &str,
        nanos: i64,
        domain: u8,
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
        // `.0` then `?`: the outer `Result` is the *compile*, which the cache
        // now answers for on a hit whether it succeeded or not (#259), and the
        // inner one is the evaluation, which it never answers for.
        cache::with_plan(self, t, s, generation, |plan| {
            let g = self.guard();
            plan.at_tagged(&g, nanos, domain)
        })
        .0?
    }

    /// A read-only [`ArenaView`] over the backing arena, for diagnostics and
    /// inspection (the CLI `tree` and `doctor` commands).
    #[must_use]
    #[cfg(feature = "unstable")]
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
    /// # Errors
    ///
    /// [`ShmError::ReadWriteNeedsRendezvous`] for a [`AttachMode::ReadWrite`]
    /// `mode`, before the segment is even mapped. Otherwise [`ShmError`] if the
    /// segment is unsealed (and so could be truncated under a reader, faulting it
    /// with `SIGBUS`), is not a tf_tree arena, or was written by a build with a
    /// different `FORMAT_VERSION` or record layout.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    pub fn attach_shared(fd: std::os::fd::OwnedFd, mode: AttachMode) -> Result<Tree, ShmError> {
        refuse_a_byteless_writer(mode)?;
        Tree::attach_shared_inner(fd, mode, None)
    }

    /// Attach into the participant slot an owner granted (`docs/PHASE2.md` §3.7).
    ///
    /// [`Tree::attach_shared`] is the fd-inheritance path, where there is no
    /// owner to ask and the slot is self-assigned.
    ///
    /// # Errors
    ///
    /// [`ShmError::ReadWriteNeedsRendezvous`] for a [`AttachMode::ReadWrite`]
    /// `mode`. Otherwise as [`Tree::attach_shared`], plus
    /// [`ShmError::ParticipantTableFull`] if the named slot is not free — which
    /// a read-only attach cannot reach either, because it registers nothing.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    pub fn attach_shared_at(
        fd: std::os::fd::OwnedFd,
        mode: AttachMode,
        slot: u32,
    ) -> Result<Tree, ShmError> {
        refuse_a_byteless_writer(mode)?;
        Tree::attach_shared_inner(fd, mode, Some(slot))
    }

    /// [`Tree::attach_shared_at`] for the one caller that has already taken the
    /// participant lock byte for `slot`.
    ///
    /// # Errors
    ///
    /// As [`Tree::attach_shared_at`], minus the refusal.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    pub(crate) fn attach_joined_at(
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
            // Before `backing` moves into `arena`.
            cache_scope: cache_scope_for(&backing),
            arena: backing,
            participant,
            incarnation,
            liveness,
            decl: Mutex::new(()),
            #[cfg(all(feature = "shm", target_os = "linux"))]
            attachment: std::sync::Mutex::new(None),
            #[cfg(all(feature = "shm", target_os = "linux"))]
            lock_file: None,
            #[cfg(all(feature = "shm", target_os = "linux"))]
            ofd_probe: None,
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
            // Before `backing` moves into `arena`.
            cache_scope: cache_scope_for(&backing),
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
            attachment: std::sync::Mutex::new(None),
            lock_file: None,
            ofd_probe: None,
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
        rendezvous: tf_tree_ipc::Rendezvous,
    ) {
        self.put_attachment(Some(crate::open::Attachment::Joined {
            session,
            socket,
            rendezvous,
        }));
    }

    /// Whether the process that owns this arena has gone away (§3.5).
    ///
    /// A participant holds its attach socket for the lifetime of the attachment
    /// and the owner reads that socket's closure as death (D17). This is the
    /// same fact from the other end — the owner's death closes it too, and the
    /// kernel reports `POLLHUP` exactly and with no timeout to tune — at the end
    /// of the owner's exit, not at its signal, which the last section below
    /// says in full.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    #[must_use]
    pub fn owner_lost(&self) -> bool {
        use std::os::fd::AsFd;
        match &*self.attachment.lock().unwrap_or_else(|e| e.into_inner()) {
            Some(crate::open::Attachment::Joined {
                socket, session, ..
            }) => {
                // A `poll` failure is not a hangup. Reporting one would send a
                // survivor to take ownership of an arena whose owner is alive
                // and serving, which is the split-brain §3.4 exists to prevent.
                if !tf_tree_ipc::peer_hung_up(socket.as_fd()).unwrap_or(false) {
                    return false;
                }
                // Hung up says **our channel** is dead, not that the role is
                // vacant — this method's three-state table, and `0043`.
                !session.ownership_held().unwrap_or(true)
            }
            _ => false,
        }
    }

    /// Is this a joined rendezvous attachment — the only shape §3.5 can inherit
    /// from?
    #[cfg(all(feature = "shm", target_os = "linux"))]
    pub(crate) fn is_joined(&self) -> bool {
        matches!(
            *self.attachment.lock().unwrap_or_else(|e| e.into_inner()),
            Some(crate::open::Attachment::Joined { .. })
        )
    }

    /// Take the parked attachment out.
    ///
    /// **The caller owes a matching [`Tree::put_attachment`] on every path**,
    /// including error paths: what is taken here holds this process's
    /// participant lock byte and its attach socket, and dropping it releases
    /// both. It exists so `Tree::inherit_ownership` can hand `&self` to
    /// `spawn_owner_server` while it owns the session.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    pub(crate) fn take_attachment(&self) -> Option<crate::open::Attachment> {
        self.attachment
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// Put back what [`Tree::take_attachment`] removed.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    pub(crate) fn put_attachment(&self, a: Option<crate::open::Attachment>) {
        *self.attachment.lock().unwrap_or_else(|e| e.into_inner()) = a;
    }

    /// Park the owner's session and serving thread.
    ///
    /// **This is not where the byte/record correspondence is checked**, though
    /// it is the function that first has both numbers in one place and
    /// `docs/PHASE2.md` §0.0 named it for that reason. The check is at the sole
    /// call site, `crate::open::Open::attempt`, two statements earlier — *before*
    /// `spawn_owner_server` binds the rendezvous socket, so a refusal happens
    /// while the arena is still private (`docs/decisions/0028` plan step 0c). By
    /// the time this is called, `session.slot() == self.participant` has already
    /// been established.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    pub(crate) fn hold_ownership(
        &mut self,
        session: crate::open::JoinedSession,
        server: crate::open::OwnerThread,
    ) {
        self.put_attachment(Some(crate::open::Attachment::Owner {
            _server: server,
            _session: session,
        }));
    }

    /// Replace the `/proc` liveness heuristic with the kernel's answer (§5.1).
    ///
    /// `/proc` parsing is an *inference* with a race in it: between reading a
    /// pid and acting on it the process can exit and the number be reused, and
    /// an unreadable `/proc` entry is indistinguishable from a permission
    /// problem. So it fails safe — unknown means alive — which is right but
    /// means a dead participant is never *proven* dead.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    pub(crate) fn use_ofd_liveness(&mut self, probe: crate::open::LivenessProbe) {
        let own_slot = self.participant;
        // **One description, two holders — and the `Arc` is exactly the
        // lifetime convenience.** The closure below must own what it captures,
        // and `Self::reap_participants` needs the same object for the
        // three-valued answer this closure collapses. See the `ofd_probe`
        // field for why nothing here can rebuild a probe, and why a second one
        // would decide nothing.
        let probe = std::sync::Arc::new(probe);
        self.ofd_probe = Some(std::sync::Arc::clone(&probe));
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
        self.lock_file = Some(lock);
    }

    /// Reclaim edges whose holder is provably dead (`docs/PHASE2.md` §6.3).
    ///
    /// Returns how many claims were reaped.
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
        let Some(lock) = self.lock_file.as_ref() else {
            return 0; // no rendezvous, no leases, nothing provable
        };
        // A read-only tree cannot reap and cannot form an owner word.
        if self.participant == u32::MAX || !self.arena.is_writable() {
            return 0;
        }
        let own_slot = self.participant;

        reap_claims(&self.view(), lock, only_slot, own_slot)
    }

    /// Reclaim the participant records of processes the kernel says are gone
    /// (`docs/PHASE2.md` §3.9 and §6.3, `docs/decisions/0028` plan step 5).
    #[cfg(all(feature = "shm", target_os = "linux"))]
    #[must_use]
    pub fn reap_participants(&self) -> usize {
        // R6, and first.
        if !self.arena.is_writable() {
            return 0;
        }
        // No rendezvous, no lock file, no kernel fact to act on. This scopes the
        // *sweeper*, and only the sweeper: it says nothing about whether the
        // records it will judge have bytes of their own. A `build_shared`
        // participant in this same arena has none, and is read dead — but a
        // sweeper can only *be* in such an arena if somebody served it through a
        // hand-bound `tf_tree_ipc::OwnerServer`, which
        // `docs/decisions/0031-the-participant-record-with-no-byte.md` answered
        // **out of contract** on 2026-09-18. The supported create path takes the
        // byte before it builds, so over the population it produces every record
        // this sweep judges is byte-paired.
        let Some(probe) = self.ofd_probe.as_ref() else {
            return 0;
        };

        let view = self.view();
        let table = view.participants();
        let slots = view.header().max_participants;
        let mut reaped = 0;

        for slot in 0..slots {
            let Some(rec) = table.get(slot) else {
                continue;
            };
            // The word, then the byte, then the CAS against the word — all
            // three inside the predicate and `reclaim`, so this loop chooses no
            // ordering of its own and cannot get one wrong.
            if let crate::open::Reclamation::Reclaimable { observed } =
                crate::open::reclamation_verdict(probe, self.participant, slot, rec)
            {
                // `docs/PHASE2.md` §11.3: **`reclaim.after_probe_before_cas`**.
                #[cfg(feature = "crash-points")]
                tf_tree_core::crash::maybe_abort(crate::open::CRASH_SITES[4]);

                // `false` when the slot moved under us — reclaimed by a racing
                // sweeper, or re-occupied. Not counted, because nothing was
                // collected: racing reclaimers are harmless and at most one
                // CAS succeeds.
                if table.reclaim(slot, observed) {
                    reaped += 1;
                }
            }
        }
        reaped
    }

    /// This process's own participant slot in the arena's table.
    ///
    /// `u32::MAX` for a read-only attachment, which takes a lock-file byte but
    /// writes no arena record — it cannot, the mapping is `PROT_READ`.
    #[must_use]
    pub fn participant_slot(&self) -> u32 {
        self.participant
    }

    /// Whether the participant in `slot` is still running.
    ///
    // `tf_tree::open` is deliberately *not* an intra-doc link here: it is
    // `#[cfg(all(feature = "shm", target_os = "linux"))]`, so on the default
    // feature set — which is what a `cargo add tf_tree` consumer renders, and
    // what `just stable-tier-check` renders — the link has no target and
    // `RUSTDOCFLAGS="-D warnings"` is an error rather than a broken anchor.
    /// The kernel's answer for a tree obtained from `tf_tree::open`, a `/proc`
    /// inference otherwise (`docs/PHASE2.md` §5.1). Exposed because `doctor`
    /// and the reaper both need it, and because it is the one predicate whose
    /// two implementations differ in a way a test can see: a `SIGSTOP`ped
    /// holder still holds its lock byte.
    #[must_use]
    pub fn participant_alive(&self, slot: u32) -> bool {
        match self.view().participants().get(slot) {
            None => false,
            Some(rec) => {
                // **The word first, then the liveness source** — here that
                // order comes from `&&`'s short-circuit rather than from a
                // statement, and it is not free to reverse: under
                // word-then-byte the `Acquire` load of a live word
                // synchronises-with `fill_slot`'s publishing `Release` store,
                // so a probe sequenced after it must see the byte held.
                tf_tree_core::participant::state_of(rec.state.load(Ordering::Acquire))
                    == tf_tree_core::participant::LIVE
                    && (self.liveness)(slot, rec)
            }
        }
    }

    /// This tree's arena identity, the first component of the per-thread plan
    /// cache's key (`crate::cache`, [`cache_scope_for`]).
    pub(crate) fn cache_scope(&self) -> u64 {
        self.cache_scope
    }

    /// Which arena *instance* this tree is attached to (A7, §3.7).
    ///
    /// All-zero for a heap tree, which is single-process by construction and so
    /// has no second attacher to disambiguate against.
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
    ///
    /// Through [`stored_name`] because `name_len` is a `u8` over 48 bytes and a
    /// corrupt arena panics an unclamped slice — on the error-display path, which
    /// is exactly where a bad arena is being looked at. Nothing validates
    /// per-record fields on read.
    fn frame_name(&self, id: FrameId) -> String {
        let Some(rec) = self.view().frame_record(id) else {
            return std::format!("frame#{}", id.get());
        };
        stored_name(&rec.name, rec.name_len)
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

/// Look up a frame by name for the read path, mapping "not found", hash
/// collisions and intern contention to [`LookupError::UnknownFrame`] — see
/// [`Tree::lookup`]'s `# Errors` for why that is three remedies behind one
/// variant.
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

impl Drop for Tree {
    fn drop(&mut self) {
        // Release the participant slot on a clean exit. A slot leaked by a
        // *crash* is the reaper's problem (`docs/PHASE2.md` §6); this is the
        // orderly path, and skipping it would exhaust the table across
        // repeated attach/detach cycles in a long-lived arena.
        if self.participant != u32::MAX && self.arena.is_writable() {
            self.view()
                .participants()
                .release(self.participant, self.incarnation);
        }
    }
}

/// Refuse a read-write attach that arrives over a bare file descriptor.
///
/// One function rather than a check repeated at each entry point, because there
/// are two `pub` entry points and closing one of them closes nothing.
#[cfg(all(feature = "shm", target_os = "linux"))]
fn refuse_a_byteless_writer(mode: AttachMode) -> Result<(), ShmError> {
    match mode {
        AttachMode::ReadOnly => Ok(()),
        AttachMode::ReadWrite => Err(ShmError::ReadWriteNeedsRendezvous),
    }
}

/// The fork generation to poison a tree against, or `None` for a backing that
/// survives a `fork` intact.
#[cfg(all(feature = "shm", target_os = "linux"))]
fn fork_gen_for(backing: &ArenaBacking) -> Option<u64> {
    match backing {
        ArenaBacking::Heap(_) => None,
        // A frozen mapping is `MAP_PRIVATE | PROT_READ` and deliberately *not*
        // `MADV_DONTFORK`, so a `fork` child inherits it intact and every
        // reference into it stays valid — the same situation as a heap arena,
        // and the one §2.2's sixteen dataloader workers depend on.
        ArenaBacking::Frozen(_) => None,
        ArenaBacking::Mapped(_) => {
            tf_tree_ipc::fork::arm();
            let _ = poison_arena();
            Some(tf_tree_ipc::fork::generation())
        }
    }
}

/// Which arena a [`Tree`] reads, as one `u64`, for the plan cache's key.
///
/// **The arena, not the handle.** Two `Tree`s mapping one shared segment see
/// one topology and one set of static transforms, so a plan compiled through
/// either is correct through the other; giving them separate identities would
/// cost every second handle a recompile and buy no safety. Two *processes*
/// mapping that segment have separate caches already — the cache is
/// `thread_local!` — so nothing here is shared between them but the value.
fn cache_scope_for(backing: &ArenaBacking) -> u64 {
    // A match on the variants, not a predicate: no existing one (`is_shared` means "a peer can
    // mutate it") answers "does this backing carry an identity that outlives the handle?".
    let uuid: Option<[u8; 16]> = match backing {
        // Single-process by construction: the handle *is* the arena.
        ArenaBacking::Heap(_) => None,
        #[cfg(all(feature = "shm", target_os = "linux"))]
        ArenaBacking::Mapped(_) => Some(ArenaView::new(backing.as_dyn()).header().instance_uuid),
        // The header's uuid identifies the arena this image was taken from, not
        // the image. See the closing paragraph of this function's doc.
        #[cfg(all(feature = "shm", target_os = "linux"))]
        ArenaBacking::Frozen(_) => None,
    };
    // `create` always draws one, so the all-zero arm is defence rather than a
    // reachable branch: an all-zero id read as an identity would make every
    // shared arena the same arena, which is the defect this function exists to
    // remove.
    let Some(uuid) = uuid.filter(|u| *u != [0u8; 16]) else {
        return next_local_scope();
    };
    let lo = u64::from_le_bytes([
        uuid[0], uuid[1], uuid[2], uuid[3], uuid[4], uuid[5], uuid[6], uuid[7],
    ]);
    let hi = u64::from_le_bytes([
        uuid[8], uuid[9], uuid[10], uuid[11], uuid[12], uuid[13], uuid[14], uuid[15],
    ]);
    // The halves are independent uniform bytes, so their xor is uniform in 64
    // bits; forcing the top bit costs one bit of that and buys a space disjoint
    // from the counter's, so a heap tree and a shared tree can never collide by
    // arithmetic accident and no argument about the odds is needed.
    (lo ^ hi) | (1 << 63)
}

/// A process-unique id for an arena that carries no `instance_uuid`.
///
/// Process-*local* is the whole requirement: the cache it keys is
/// `thread_local!`, so a value only ever meets values minted by this process.
fn next_local_scope() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    // Relaxed: uniqueness is `fetch_add`'s own guarantee and nothing is
    // published through this counter, so there is no other thread's writes for
    // it to order.
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    // Stays out of the shared half of the space: 2^63 trees at one per
    // nanosecond is 292 years.
    debug_assert!(n < 1 << 63);
    n
}

/// The process-wide empty arena a detached [`Tree`] reads instead of the
/// mapping that went away.
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
    view.participants().register(
        std::process::id(),
        process_start_time().unwrap_or(UNKNOWN_START_TIME),
        // `0` reads as *unknown attach time* in a participant record, which is what
        // an unreadable clock is. Only the offset sampler cannot use it.
        now_nanos().unwrap_or(0),
    )
}

/// Register into the slot the arena's owner named (`docs/PHASE2.md` §3.7).
///
/// A joiner does not get to choose: the slot in the `HelloResponse` is also the
/// lock-file byte it took, and §5.1's liveness predicate asks the kernel about
/// that byte and then reads the record it indexes. Two independently-chosen
/// numbers would make every liveness answer be about somebody else.
#[cfg(all(feature = "shm", target_os = "linux"))]
fn register_participant_at(view: &ArenaView, slot: u32) -> Result<u64, ParticipantError> {
    view.participants().register_at(
        slot,
        std::process::id(),
        process_start_time().unwrap_or(UNKNOWN_START_TIME),
        // `0` reads as *unknown attach time*; see `register_participant`.
        now_nanos().unwrap_or(0),
    )
}

/// Wall-clock nanoseconds since the epoch, saturating, or `None` if the host
/// clock is **before** the epoch. Diagnostics only — nothing
/// correctness-critical reads it.
fn now_nanos() -> Option<i64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

/// **The liveness predicate** — the single seam where "is participant `slot`
/// still running?" is answered, injected into
/// [`tf_tree_core::topology::TopoLockView::acquire`].
fn record_is_alive(rec: &tf_tree_core::ParticipantRecord) -> bool {
    use core::sync::atomic::Ordering;
    if tf_tree_core::participant::state_of(rec.state.load(Ordering::Acquire))
        != tf_tree_core::participant::LIVE
    {
        return false;
    }
    let pid = rec.pid.load(Ordering::Relaxed);
    let start_time = rec.start_time.load(Ordering::Relaxed);
    alive_given(start_time, read_start_time(pid), proc_answers_here())
}

/// Turn a record's stored `start_time` and what `/proc` said into a verdict.
///
/// Both host facts arrive as parameters rather than as reads, because both are
/// things a test cannot arrange: whether `/proc` answers is a property of the
/// machine the suite runs on, and staging pid reuse means exhausting the pid
/// space. Passing them in is what makes the bias below assertable instead of
/// merely stated.
fn alive_given(stored_start_time: u64, probe: ProcStartTime, proc_answers: bool) -> bool {
    match probe {
        // The registrant could not read its own start time and stored
        // `UNKNOWN_START_TIME`, so there is nothing here to compare against.
        ProcStartTime::Known(_) if stored_start_time == UNKNOWN_START_TIME => true,
        // PID reuse: same number, different process. Not our participant.
        ProcStartTime::Known(st) => st == stored_start_time,
        // Death, but only as read from a host that would have shown us the
        // entry. On one that answers `ENOENT` for every pid, a missing entry
        // says nothing and every participant in the arena would read dead.
        ProcStartTime::NoSuchProcess => !proc_answers,
        ProcStartTime::Unreadable => true,
    }
}

/// Build the liveness predicate for an arena, folding in the one-time reboot
/// check.
fn liveness_for(arena_boot: [u8; 16]) -> BoxedLiveness {
    let host = *host_boot_id();
    if arena_boot != [0u8; 16] && host != [0u8; 16] && arena_boot != host {
        return Box::new(|_, _| false);
    }
    Box::new(|_slot, rec| record_is_alive(rec))
}

/// May the topology word held by `slot` be stolen? — the residual, and *only*
/// the residual (`docs/decisions/0029` T3).
fn participant_is_alive(
    participants: &tf_tree_core::ParticipantTable<'_>,
    slot: u32,
    arena_boot: &[u8; 16],
) -> bool {
    // The arena outlived a reboot: every pid it records belongs to a previous
    // boot and means nothing now. Decided only when *both* ids are known — see
    // `liveness_for` for why "unknown" must not read as "different".
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

    alive_given(start_time, read_start_time(pid), proc_answers_here())
}

/// The `start_time` a participant record carries when this host would not say
/// what it is.
const UNKNOWN_START_TIME: u64 = 0;

/// Would this host tell us that some other process exists?
///
/// A `/proc` that is not mounted — a `chroot` without one, a stripped container
/// — fails **every** open with `ENOENT`, the same errno a genuinely dead pid
/// produces and indistinguishable from it at the call site. Reading our own
/// entry settles which it is: this process is running by construction, so if
/// `/proc/self/stat` is not there then `/proc` is not there, and an `ENOENT`
/// about anybody else proves nothing at all.
fn proc_answers_here() -> bool {
    /// Not yet asked, or asked and answered indecisively.
    const UNASKED: u8 = 0;
    /// `/proc/self/stat` was readable: an `ENOENT` about anyone else is real.
    const ANSWERS: u8 = 1;
    /// `/proc/self/stat` was absent: no `ENOENT` here proves anything.
    const SILENT: u8 = 2;

    static HOST: AtomicU8 = AtomicU8::new(UNASKED);
    match HOST.load(Ordering::Relaxed) {
        ANSWERS => true,
        SILENT => false,
        _ => match latch_for(&std::fs::metadata("/proc/self/stat")) {
            Some(true) => {
                HOST.store(ANSWERS, Ordering::Relaxed);
                true
            }
            Some(false) => {
                HOST.store(SILENT, Ordering::Relaxed);
                false
            }
            None => false,
        },
    }
}

/// Which way a probe of `/proc/self/stat` latches, and whether it latches at all.
///
/// Split out from [`proc_answers_here`] so the three-way classification can be
/// tested without an unmounted `/proc` or an induced `ENOMEM`. `None` is the
/// indecisive case and is the whole point of the split: it must not latch.
fn latch_for(probe: &std::io::Result<std::fs::Metadata>) -> Option<bool> {
    match probe {
        Ok(_) => Some(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
    }
}

/// The outcome of asking `/proc` when a process started.
///
/// Three cases, not two: "no such process" is the only one that can prove death,
/// and collapsing it with "could not read" is what turns a hardened `/proc`, a
/// container without `hidepid` access, or an `EMFILE` into a false report of
/// death (`docs/PHASE2.md` §6.2).
#[derive(Clone, Copy)]
enum ProcStartTime {
    /// Field 22 of `/proc/<pid>/stat`, in clock ticks since boot.
    Known(u64),
    /// There was no entry — `ENOENT`.
    NoSuchProcess,
    /// There might be; `/proc` would not say.
    Unreadable,
}

/// Read another process's start time (`/proc/<pid>/stat` field 22).
fn read_start_time(pid: u32) -> ProcStartTime {
    let stat = match std::fs::read_to_string(std::format!("/proc/{pid}/stat")) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ProcStartTime::NoSuchProcess,
        Err(_) => return ProcStartTime::Unreadable,
    };
    parse_start_time(&stat).map_or(ProcStartTime::Unreadable, ProcStartTime::Known)
}

/// Field 22 out of one `/proc/<pid>/stat` line, in clock ticks since boot.
///
/// Field 2 is `comm`, parenthesised and free to contain spaces *and*
/// parentheses, so the scan starts after the **last** `)` — the parsing trap
/// `docs/PHASE2.md` §5.1 calls out by name. After it the fields are state(3),
/// ppid(4), … starttime(22), so starttime is the 20th token from there.
fn parse_start_time(stat: &str) -> Option<u64> {
    let after_comm = &stat[stat.rfind(')')? + 1..];
    after_comm.split_whitespace().nth(19)?.parse().ok()
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
/// 22), or `None` if this host would not say.
fn process_start_time() -> Option<u64> {
    parse_start_time(&std::fs::read_to_string("/proc/self/stat").ok()?)
}

/// A [`LookupError`] paired with the [`Tree`] that can resolve its ids to names.
///
/// `Display` produces a human-readable message (the error itself stays `Copy` and
/// allocation-free). Obtain one with [`Tree::describe`].
pub struct Described<'a>(LookupError, &'a Tree);

impl fmt::Display for Described<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tree = self.1;
        match self.0 {
            // **It cannot name the frame that was asked for, and it can name
            // the ones that exist.** The error carries a BLAKE3 prefix and
            // BLAKE3 does not invert, so the hash is all this arm has of the
            // *request* — but `Described` holds the `&Tree`, and "what is
            // actually in here" is the question an operator reading this is
            // about to ask next. Naming that costs one walk of a table this
            // process has already mapped.
            LookupError::UnknownFrame { hash } => {
                write!(f, "unknown frame (name hash {hash:#018x})")?;
                const SHOWN: usize = 8;
                match tree.frames() {
                    // A tree with no frames is a different situation and gets a
                    // different sentence: "known frames: (none)" reads as a
                    // broken lookup, when what happened is that no publisher has
                    // interned anything yet — the case the wait exists for.
                    Ok(names) if names.is_empty() => write!(
                        f,
                        "; this tree has no frames yet, so no publisher has \
                         declared anything into it. Wait for one with \
                         Tree::await_frames, or declare the frame on the \
                         TreeBuilder that creates the arena"
                    ),
                    Ok(mut names) => {
                        let total = names.len();
                        names.sort_unstable();
                        names.truncate(SHOWN);
                        f.write_str("; this tree has ")?;
                        for (i, n) in names.iter().enumerate() {
                            if i > 0 {
                                f.write_str(", ")?;
                            }
                            f.write_str(n)?;
                        }
                        if total > SHOWN {
                            write!(f, ", … ({total} total)")?;
                        }
                        write!(
                            f,
                            ". If the name is spelled right, its publisher has \
                             most likely not declared it yet: wait with \
                             Tree::await_frames, or declare it on the \
                             TreeBuilder that creates the arena. Two rarer \
                             causes read the same: the name's hash collides \
                             with a frame already interned (rename one), or \
                             another participant is interning it right now \
                             (retry) — Tree::frame on a read-only tree says \
                             which"
                        )
                    }
                    // `Tree::frames` fails only for `ChildDetached`, and that is
                    // worth saying: every name would read absent in a fork
                    // child, so the frame list would be a lie rather than a
                    // short answer.
                    Err(_) => write!(
                        f,
                        "; this tree was opened before a fork() and is being \
                         used in the child, so it can name nothing"
                    ),
                }
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
            // `docs/PHASE1.md` §7.1 ("Two bounds, and they price different
            // slots") and `LookupError::TreeTooDeep`'s field docs.
            LookupError::TreeTooDeep { depth } => {
                if usize::from(depth) > tf_tree_core::MAX_PATH_EDGES {
                    write!(
                        f,
                        "the path between these frames is longer than the {MAX} \
                         edges a lookup walks: re-parent so the two frames \
                         share a nearer ancestor",
                        MAX = tf_tree_core::MAX_PATH_EDGES,
                    )
                } else {
                    write!(
                        f,
                        "this path compiles to {depth} steps and a plan holds \
                         {MAX}: declare the rigid links on it with \
                         TreeBuilder::static_edge so each adjacent run folds to \
                         one step, or re-parent so the two frames share a \
                         nearer ancestor",
                        MAX = tf_tree_core::MAX_DEPTH,
                    )
                }
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
            // Two more that name an edge, and they reached the catch-all until
            // a review caught it. `docs/decisions/0040`'s comment claimed every
            // remaining variant "carries no frame or edge this wrapper could
            // name"; these two carry one, so they were rendering the core's
            // `edge 3` from the one layer whose whole purpose is resolving it.
            LookupError::DerivativesUnavailable { edge, interp } => write!(
                f,
                "{} interpolates under policy {interp}, which has no derivative to report",
                tree.edge_name(edge),
            ),
            LookupError::NoSegment { edge } => write!(
                f,
                "{} has no bracketing segment at that stamp, so there is no twist",
                tree.edge_name(edge),
            ),
            // **The arms above are the ones that resolve a *name*, which is the
            // whole reason this wrapper exists. Everything else delegates.**
            other => write!(f, "{other}"),
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
    #[error("arena layout error: {0}")]
    Layout(LayoutError),
    /// A frame name could not be interned (table full or 64-bit hash collision).
    #[error("frame error: {0}")]
    Frame(FrameError),
    /// Wiring an edge into the topology failed (cycle or out-of-range frame).
    #[error("topology error: {0}")]
    Topology(TopologyError),
    /// The shared-memory segment could not be created, sized, mapped or sealed.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    #[error("shared memory error: {0}")]
    Shm(ShmError),
    /// The participant table is full, so this process cannot join the arena.
    // Bare, with no "participant table full" prefix (`docs/decisions/0059`
    // decision 4): both producers go through `ParticipantTable::register`, whose
    // only error is `TableFull`, and that payload's own text already says so.
    #[error("{0}")]
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
    #[error("topology error: {0}")]
    Topology(TopologyError),
    /// The arena is mapped read-only; it cannot be mutated.
    #[error("arena is mapped read-only")]
    ReadOnly,
    /// The topology mutation lock (`docs/PHASE2.md` §1, A2) is held by another
    /// participant that is still alive.
    #[error("the topology lock is held by a live peer{owner_slot}", owner_slot = HolderSuffix(*owner_slot))]
    LockContended {
        /// Participant slot of the holder observed when the attempt gave up, or
        /// `None` if the observation could not name one.
        owner_slot: Option<u32>,
    },
    /// The lock file's topology byte could not be asked about at all — an
    /// `fcntl` failure that is not contention.
    #[error("the topology lock byte could not be taken: fcntl failed with errno {raw_os_error}")]
    TopologyLease {
        /// `errno`, or `0` if the OS did not supply one.
        raw_os_error: i32,
    },
}

impl From<TopologyError> for ReparentError {
    fn from(e: TopologyError) -> ReparentError {
        ReparentError::Topology(e)
    }
}

/// The `Display` half of [`ReparentError::LockContended`]'s holder, kept out of
/// the error type so the type stays a `Copy` identifier (`docs/API.md` R5).
struct HolderSuffix(Option<u32>);

impl core::fmt::Display for HolderSuffix {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            Some(slot) => write!(f, " (participant slot {slot})"),
            None => f.write_str(" that has not yet published its slot"),
        }
    }
}

impl From<TopoLockError> for ReparentError {
    fn from(e: TopoLockError) -> ReparentError {
        match e {
            // **The one place the core's sentinel is translated.**
            // `TopoLockView` reports `u32::MAX` for a holder its observation
            // could not name, because it is `no_std` and its own callers are
            // engine code that reads its doc comment; a user of this crate is
            // not. Translating here rather than there keeps the sentinel
            // knowledge in one function instead of on every caller.
            TopoLockError::Contended { owner_slot } => ReparentError::LockContended {
                owner_slot: (owner_slot != u32::MAX).then_some(owner_slot),
            },
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
    #[error("edge {}: the claim record was free but its lease is held", edge.get())]
    LeaseContended {
        /// The edge.
        edge: EdgeId,
    },
    /// The lock file could not be asked about the edge's lease.
    #[error("edge {}: the claim lease could not be taken", edge.get())]
    LeaseUnavailable {
        /// The edge.
        edge: EdgeId,
    },
    /// A reaper cleared this claim inside the CAS-to-lease window.
    ///
    /// Everything is given back before this returns, so the correct response is
    /// simply to claim again.
    #[error("edge {}: reaped while being claimed; retry", edge.get())]
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
    #[error("edge {}: {cause}", edge.get())]
    AlreadyClaimed {
        /// The edge whose claim was refused.
        edge: EdgeId,
        /// The engine's refusal, which names the owning participant slot.
        cause: tf_tree_core::ClaimError,
    },
    /// The arena is mapped read-only, so no edge can be claimed for writing.
    #[error("arena is mapped read-only")]
    ReadOnly,
}

/// Revoke every claim whose holder the kernel says is gone.
///
/// The body [`Tree::reap_inner`] used to inline, lifted out because **the owner's
/// socket-hangup callback needs the same walk and had no way to reach a
/// `Tree`** — it runs on the serving thread, which deliberately holds its own
/// mapping and its own lock-file description rather than borrowing the handle a
/// caller owns. `docs/PHASE2.md` §3.9 says a dead participant's "arena-side
/// records" are the owner's to reap; until this was callable from there, the
/// callback freed the participant *record* and left every claim that participant
/// held, forever.
#[cfg(all(feature = "shm", target_os = "linux"))]
pub(crate) fn reap_claims(
    view: &tf_tree_core::arena_view::ArenaView<'_>,
    lock: &tf_tree_ipc::LockFile,
    only_slot: Option<u32>,
    own_slot: u32,
) -> usize {
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// A start time a host could report, one that is not it, and the sentinel a
    /// registration writes when the host reports nothing.
    const REAL: u64 = 4321;
    const OTHER: u64 = 4322;
    const UNSET: u64 = UNKNOWN_START_TIME;

    /// Hand `f` a participant record carrying the identity asked for.
    ///
    /// Through `register_at` rather than field by field: `ParticipantRecord`'s
    /// fields are public but its `Default` is `#[cfg(test)]` inside
    /// `tf_tree_core`, and a record assembled here by hand would not be the one
    /// the publication protocol produces.
    fn with_record(pid: u32, start_time: u64, f: impl FnOnce(&tf_tree_core::ParticipantRecord)) {
        let arena = HeapArena::new(&ArenaLayout::minimal(), 0, 0, [0u8; 16]);
        let view = ArenaView::new(&arena);
        let table = view.participants();
        table
            .register_at(0, pid, start_time, 0)
            .expect("slot 0 of a fresh arena is free");
        f(table.get(0).expect("slot 0 is within every layout's table"));
    }

    /// **The two rules of [`recorded_offset`] that a clock will not produce on
    /// demand.**
    #[test]
    fn a_recorded_offset_is_never_the_no_sample_sentinel_and_never_wraps() {
        // A publisher whose clock reads exactly its own stamp — the coarse-clock
        // case on a platform whose `SystemTime::now()` is coarser than a push.
        assert_eq!(
            super::recorded_offset(1_787_000_000_000_000_000, 1_787_000_000_000_000_000),
            1,
            "an exact-zero offset was stored as the arena's no-sample-yet \
             sentinel: that publisher reads as never sampled, forever"
        );
        // Ordinary skew keeps its sign and magnitude, both ways round.
        assert_eq!(super::recorded_offset(1_000, 400), 600);
        assert_eq!(super::recorded_offset(400, 1_000), -600);
        // A stamp near the end of the range clamps rather than wrapping into a
        // plausible-looking offset.
        assert_eq!(super::recorded_offset(-1, i64::MAX), i64::MIN);
        assert_eq!(super::recorded_offset(i64::MAX, -1), i64::MAX);
    }

    /// **Only the wall-clock domain samples**, `docs/decisions/0036`.
    ///
    /// `sample_interval` is the seam, so this is a table over the tags rather
    /// than a tree built per domain: the fact under test is a mapping, and a
    /// mapping is best asserted as one.
    #[test]
    fn only_a_wall_clock_edge_samples_an_offset() {
        use tf_tree_core::plan::{Domain, SensorDomain, SimDomain, SteadyDomain, SystemDomain};

        assert_eq!(
            super::sample_interval(<SystemDomain as Domain>::TAG, 10_000),
            10,
            "a 10 Hz wall-clock edge must sample every ten pushes"
        );
        assert_eq!(
            super::sample_interval(<SystemDomain as Domain>::TAG, 0),
            super::DEFAULT_SAMPLE_EVERY,
            "an undeclared-rate wall-clock edge must sample at the default"
        );
        for (tag, name) in [
            (<SensorDomain as Domain>::TAG, "SensorDomain"),
            (<SimDomain as Domain>::TAG, "SimDomain"),
            (<SteadyDomain as Domain>::TAG, "SteadyDomain"),
            (200, "a user-declared domain"),
        ] {
            assert_eq!(
                super::sample_interval(tag, 10_000),
                0,
                "a {name} edge sampled: `wall clock - stamp` is not an offset \
                 when the two do not share an epoch, and this one would record \
                 a fifty-six-year skew against a publisher that is fine"
            );
        }
    }

    /// **The documented bias, as an assertion rather than a comment.**
    ///
    /// Every combination of the two facts the predicate has, with each verdict
    /// written out rather than derived — a table that recomputed the
    /// implementation would pass against any implementation. Death is provable
    /// in four of the sixteen, and a fifth appearing here owes an argument.
    #[test]
    fn every_ambiguity_resolves_to_alive() {
        // stored start time, what /proc said, does this host answer, alive?
        let cases = [
            (REAL, ProcStartTime::Known(REAL), true, true),
            (REAL, ProcStartTime::Known(REAL), false, true),
            // Both start times known and different: pid reuse. The one shape of
            // death that does not depend on the host answering at all.
            (REAL, ProcStartTime::Known(OTHER), true, false),
            (REAL, ProcStartTime::Known(OTHER), false, false),
            // No entry, from a host whose entries mean something.
            (REAL, ProcStartTime::NoSuchProcess, true, false),
            (REAL, ProcStartTime::NoSuchProcess, false, true),
            (REAL, ProcStartTime::Unreadable, true, true),
            (REAL, ProcStartTime::Unreadable, false, true),
            // Nothing was recorded, so there is nothing to compare against: no
            // live `/proc` entry can make this record dead, whatever it says.
            (UNSET, ProcStartTime::Known(REAL), true, true),
            (UNSET, ProcStartTime::Known(REAL), false, true),
            (UNSET, ProcStartTime::Known(OTHER), true, true),
            (UNSET, ProcStartTime::Known(OTHER), false, true),
            (UNSET, ProcStartTime::NoSuchProcess, true, false),
            (UNSET, ProcStartTime::NoSuchProcess, false, true),
            (UNSET, ProcStartTime::Unreadable, true, true),
            (UNSET, ProcStartTime::Unreadable, false, true),
        ];
        let mut dead = 0;
        for (row, (stored, probe, answers, alive)) in cases.into_iter().enumerate() {
            dead += usize::from(!alive);
            assert_eq!(
                alive_given(stored, probe, answers),
                alive,
                "row {row}: stored={stored}, proc_answers={answers}"
            );
        }
        assert_eq!(dead, 4, "the table itself grew or lost a verdict of death");
    }

    /// `ENOENT` is proof of death only where `/proc` would have shown the entry.
    ///
    /// The host fact is a parameter precisely so this needs no unmounted
    /// `/proc`: on a host with none, *every* pid reads `NoSuchProcess`, running
    /// ones included, and the whole participant table resolves to dead at once.
    #[test]
    fn enoent_proves_death_only_on_a_host_that_answers() {
        assert!(!alive_given(REAL, ProcStartTime::NoSuchProcess, true));
        assert!(
            alive_given(REAL, ProcStartTime::NoSuchProcess, false),
            "a host that cannot see its own /proc entry reported another \
             process dead on the strength of an ENOENT that means nothing"
        );
    }

    /// A record whose `start_time` is the sentinel is **unknown**, not a
    /// mismatch.
    #[test]
    fn a_sentinel_start_time_reads_unknown_rather_than_mismatched() {
        for answers in [true, false] {
            assert!(alive_given(UNSET, ProcStartTime::Known(REAL), answers));
        }
    }

    /// The same inversion end to end, through the real predicate.
    ///
    /// The pid is this process's, so `/proc` answers `Known` with a start time
    /// that is certainly not the sentinel: the exact shape that used to invert.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_running_process_with_no_recorded_start_time_reads_alive() {
        with_record(std::process::id(), UNSET, |rec| {
            assert!(
                record_is_alive(rec),
                "a record carrying no start time was reported dead about the \
                 very process asking"
            );
        });
    }

    /// The fix must not buy its safety by making death unprovable.
    ///
    /// `pid_max` is at most 2^22, so `u32::MAX` is a number no process holds and
    /// no reuse can hand back.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_impossible_pid_is_still_dead() {
        with_record(u32::MAX, REAL, |rec| assert!(!record_is_alive(rec)));
    }

    /// Pid reuse is still caught: our own number, a start time that is not ours.
    ///
    /// Derived from the real one rather than picked, because any literal is a
    /// start time some process could genuinely have — `REAL` is 4321 ticks,
    /// which is a process launched 43 seconds into the boot.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_recycled_pid_is_dead() {
        let not_ours = process_start_time().expect("this host answers about itself") + 1;
        with_record(std::process::id(), not_ours, |rec| {
            assert!(
                !record_is_alive(rec),
                "the start-time comparison stopped happening"
            );
        });
    }

    // The host probe answers here, which is what makes the two tests above mean
    // what they say — on a host that answered `false` both would read alive.
    #[cfg(target_os = "linux")]
    /// The probe latches a decisive answer and refuses to latch anything else.
    ///
    /// The indecisive arm is the one that matters: an `ENOMEM`, an LSM denial or
    /// a bind-mount race during startup must cost this process one call's worth
    /// of caution, not its ability to prove a death for the rest of its life.
    #[test]
    fn only_a_decisive_proc_probe_latches() {
        let present = std::fs::metadata(".");
        assert!(
            present.is_ok(),
            "the test's own working directory must exist"
        );
        assert_eq!(
            latch_for(&present),
            Some(true),
            "a readable entry latches yes"
        );

        let absent = std::fs::metadata("/proc/self/tf-tree-no-such-entry");
        assert_eq!(
            absent.as_ref().err().map(std::io::Error::kind),
            Some(std::io::ErrorKind::NotFound),
            "fixture must actually produce NotFound"
        );
        assert_eq!(
            latch_for(&absent),
            Some(false),
            "a genuine absence latches no"
        );

        for kind in [
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::OutOfMemory,
            std::io::ErrorKind::Interrupted,
        ] {
            let indecisive: std::io::Result<std::fs::Metadata> =
                Err(std::io::Error::new(kind, "induced"));
            assert_eq!(
                latch_for(&indecisive),
                None,
                "{kind:?} is indecisive and must not latch"
            );
        }
    }

    #[test]
    fn this_host_answers_about_its_own_processes() {
        assert!(proc_answers_here());
        assert!(process_start_time().is_some());
    }

    /// `docs/PHASE2.md` Appendix B's fixture, against this crate's copy of the
    /// parser: for a process named `evil) proc` the naive whitespace split
    /// returns field 12 where field 22 was meant, silently and plausibly.
    #[test]
    fn the_last_paren_is_the_only_safe_anchor() {
        let raw = "1234 (evil) proc) S 1 1234 1234 0 -1 4194304 1 2 3 4 5 6 7 8 9 10 11 12 13";
        assert_eq!(parse_start_time(raw), Some(13));
        assert_eq!(
            raw.split_whitespace().nth(21).map(str::to_owned),
            Some("12".to_owned()),
            "the fixture stopped demonstrating the trap it was chosen for"
        );
    }

    /// **`Tree::attach_shared(fd, ReadWrite)` returns an error, not a `Tree`.**
    ///
    /// `docs/decisions/0028` plan step 0b: a read-write attach registers a
    /// participant record, and over a bare descriptor there is no lock file in
    /// which to take the byte that record's liveness is decided by. Both `pub`
    /// entry points refuse; closing one would close nothing, because the other
    /// is byte-less in exactly the same way.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    #[test]
    fn a_read_write_attach_over_a_descriptor_is_refused_on_both_entry_points() {
        let owner = TreeBuilder::new()
            .static_edge("a", "b", &Iso3::IDENTITY)
            .build_shared("tf_tree-attach-refusal-test")
            .expect("build a shared arena");
        let dup = || {
            owner
                .shared_fd()
                .expect("a shared tree has a segment fd")
                .try_clone_to_owned()
                .expect("dup the segment fd")
        };

        assert_eq!(
            Tree::attach_shared(dup(), AttachMode::ReadWrite).err(),
            Some(ShmError::ReadWriteNeedsRendezvous),
            "attach_shared handed out a byte-less writer"
        );
        // Slot 1 rather than 0: the owner holds 0, so a build that skipped the
        // refusal would get past registration here and return a `Tree`, which
        // is the failure this asserts against. `ParticipantTableFull` would be
        // the *wrong* error and is not accepted.
        assert_eq!(
            Tree::attach_shared_at(dup(), AttachMode::ReadWrite, 1).err(),
            Some(ShmError::ReadWriteNeedsRendezvous),
            "attach_shared_at handed out a byte-less writer"
        );

        // And the reader path is untouched on both. A read-only attach registers
        // no record at all, so it can strand no slot and has nothing to refuse.
        let ro = Tree::attach_shared(dup(), AttachMode::ReadOnly)
            .expect("a read-only fd attach still works");
        assert!(!ro.is_writable());
        assert_eq!(ro.arena_size_bytes(), owner.arena_size_bytes());

        let ro_at = Tree::attach_shared_at(dup(), AttachMode::ReadOnly, 1)
            .expect("a read-only fd attach at a named slot still works");
        assert!(!ro_at.is_writable());
        // The slot argument was ignored, as it always was for a read-only
        // attach: there is no record to put anywhere.
        assert_eq!(ro_at.participant, u32::MAX);
    }
}
