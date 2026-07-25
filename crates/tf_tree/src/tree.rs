//! The concrete [`Tree`] (owning a heap arena), its [`TreeBuilder`], the
//! per-edge [`Capacity`]/[`EdgeCfg`] declarations, and the `Display` wrapper
//! [`Described`].
//!
//! Per decision `0004`, the tree's topology is declared on the [`TreeBuilder`]
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

use tf_tree_arena::{ArenaLayout, HeapArena, LayoutError};
use tf_tree_core::arena_view::{ArenaBuilder, ArenaView};
use tf_tree_core::edge::{claim, EdgeKind, EdgeRecord, Publisher};
use tf_tree_core::frame::blake3_64;
use tf_tree_core::plan::{compile, Domain, EdgeMeta, Guard, InterpPolicy, Stamp, SystemDomain};
use tf_tree_core::{EdgeId, FrameError, FrameId, LookupError, TopologyError};
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
        let mut arena = HeapArena::new(&layout, std::process::id(), boot_id);
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

        Ok(Tree {
            arena,
            boot_id,
            decl: Mutex::new(()),
        })
    }
}

/// A transform tree: a fixed-capacity arena plus the ergonomic operations for
/// publishing samples and looking up transforms. Build one with [`TreeBuilder`].
///
/// `Send + Sync`: the arena's interior mutation is all atomic, the single runtime
/// topology mutation ([`Self::reparent`]) is serialized by an internal mutex, and
/// lookups are read-only. Share one `Tree` across threads; each reader compiles or
/// caches its own [`crate::Plan`]s.
pub struct Tree {
    arena: HeapArena,
    boot_id: u64,
    /// Serializes runtime topology mutations (the single seqlock writer).
    decl: Mutex<()>,
}

impl Tree {
    fn view(&self) -> ArenaView<'_> {
        ArenaView::new(&self.arena)
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
        self.view().intern(name)
    }

    /// Re-parent an existing `child` frame under `new_parent`, reusing the child's
    /// already-declared edge (no new capacity is allocated). This is the only
    /// runtime topology mutation; it bumps the topology generation, invalidating
    /// compiled [`crate::Plan`]s so they recompile against the new shape.
    ///
    /// # Errors
    ///
    /// [`ReparentError::NoEdge`] if `child` has no incoming edge to reuse, or
    /// [`ReparentError::Topology`] if the move would create a cycle or references
    /// an out-of-range frame.
    pub fn reparent(&self, child: FrameId, new_parent: FrameId) -> Result<(), ReparentError> {
        let _guard = self.decl.lock().unwrap_or_else(|e| e.into_inner());
        let view = self.view();
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
    pub fn claim(&self, child: FrameId, parent: FrameId) -> Result<Publisher<'_>, ClaimApiError> {
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
        let epoch = claim(claim_rec, std::process::id(), self.boot_id)?;
        Ok(Publisher::new(ring, claim_rec, epoch))
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
        let view = self.view();
        let topo = view.topology();
        compile(&topo, |eid| edge_meta(&view, eid), target, source)
    }

    /// A fresh [`Guard`] pinning the current topology generation for a batch of
    /// lookups.
    #[must_use]
    pub fn guard(&self) -> Guard<'_> {
        Guard::new(self.view())
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
        let view = self.view();
        let t = find(&view, target)?;
        let s = find(&view, source)?;
        // Must be the *stable* generation: an odd one names a torn topology, and
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
fn boot_id() -> u64 {
    match std::fs::read_to_string("/proc/sys/kernel/random/boot_id") {
        Ok(s) => {
            let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a offset basis
            for b in s.trim().bytes() {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
            h
        }
        Err(_) => 0,
    }
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
}

impl From<TopologyError> for ReparentError {
    fn from(e: TopologyError) -> ReparentError {
        ReparentError::Topology(e)
    }
}

/// Failure claiming an edge for writing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ClaimApiError {
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
    #[error("edge already claimed by pid {}", .0.owner_pid())]
    AlreadyClaimed(tf_tree_core::ClaimError),
}

impl From<tf_tree_core::ClaimError> for ClaimApiError {
    fn from(e: tf_tree_core::ClaimError) -> ClaimApiError {
        ClaimApiError::AlreadyClaimed(e)
    }
}

// Small accessor so the `#[error]` attribute above can read the owner pid.
trait ClaimErrorExt {
    fn owner_pid(&self) -> u32;
}
impl ClaimErrorExt for tf_tree_core::ClaimError {
    fn owner_pid(&self) -> u32 {
        match self {
            tf_tree_core::ClaimError::EdgeAlreadyClaimed { owner_pid } => *owner_pid,
            _ => 0,
        }
    }
}
