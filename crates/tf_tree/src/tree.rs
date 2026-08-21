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
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

/// First backoff interval for this crate's two waits. Doubles up to
/// [`MAX_BACKOFF`].
///
/// **One definition, and it is the facade's own** (`docs/decisions/0019` §2b).
/// Both poll loops the record adds use it: [`Tree::await_frames`] here, and
/// `crate::open::Open::await_open` one module over.
///
/// An earlier revision of this branch had it *twice* — widened to `pub` on
/// `tf_tree_ipc` for `await_open`, and restated here behind a
/// `const _: () = assert!(…)` equality check for `await_frames`, which cannot
/// name `tf_tree_ipc` at all in a default build. That is two mechanisms for one
/// pair of numbers, and the cost of the first is permanent public API on a crate
/// that had no reason to grow any.
///
/// What the deleted assertion guaranteed was that these matched
/// `tf_tree_ipc`'s *internal* handshake backoff, and that coupling was
/// decorative rather than required: `await_open` retries whole `attempt()`
/// calls, each of which runs the rendezvous' own retry loop inside it. They are
/// nested loops over different work and nothing breaks if they disagree. The
/// numbers are chosen for the same reason in both places — small enough that a
/// takeover completing in a millisecond is joined promptly — not because one
/// derives from the other.
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
///
/// `Copy`, `String`-free and `#[non_exhaustive]`, in the shape of
/// `OpenError` — `docs/API.md` R5. (No intra-doc link: `OpenError` is
/// behind the `shm` feature, and this type is not.)
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AwaitError {
    /// The budget expired with at least one name still un-interned.
    ///
    /// **A hash, not a name**, for the same reason
    /// [`LookupError::UnknownFrame`] carries one: a name that was never
    /// interned has no [`FrameId`], and D11 keeps `String`s out of error types.
    /// The caller still holds the `[&str; N]` it passed in, so it can recover
    /// the name by hashing its own inputs — or read it out of
    /// [`Tree::describe`]'s prose layer.
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
    /// `docs/decisions/0019` §3 forbids picking one silently, so this refuses
    /// and points at [`Tree::frame`], which on a writable tree cannot fail for
    /// absence and needs no wait at all.
    ///
    /// `#[non_exhaustive]` is what keeps relaxing this available later:
    /// refusing now is the reversible direction.
    #[error("await_frames refuses a writable tree; use Tree::frame, which interns on demand and cannot fail for absence")]
    WritableTree,
    /// The tree is a frozen `.tft` image, so no name will ever be interned into
    /// it.
    ///
    /// The sibling of [`AwaitError::WritableTree`] and refused for the same
    /// reason: a wait that cannot mean what the caller thinks should say so
    /// rather than sleep. `docs/PHASE5.md` §2.4 makes a frozen arena
    /// permanently read-only *and* writer-free, so this wait is futile by
    /// construction — polling it would burn the caller's whole budget and then
    /// report [`AwaitError::Timeout`], which is technically honest and
    /// practically a lie: nothing was late, and waiting longer would not help.
    ///
    /// A frozen tree's frame table is complete the moment it opens, so the
    /// right call is [`Tree::frames`] or a direct [`Tree::lookup`] — and
    /// [`Tree::describe`] on the resulting [`LookupError::UnknownFrame`] lists
    /// what the file actually contains.
    #[error("await_frames refuses a frozen .tft tree: it has no writers, so no name can appear")]
    FrozenTree,
    /// A name resolved to an error rather than to an id.
    ///
    /// Terminal, not retried. [`FrameError::FrameHashCollision`] is a permanent
    /// property of the two names involved, and [`FrameError::InternContended`]
    /// names a claimant no caller can judge — waiting on either is waiting on
    /// something that will not change on its own.
    #[error("{0:?}")]
    Frame(FrameError),
    /// This tree belongs to a process that no longer exists — it was opened
    /// before a `fork()` and this is the child. See [`Tree::detached`].
    ///
    /// Checked **every iteration**, before the arena is read: a detached tree
    /// answers with the poison arena, whose `find_frame` says `Ok(None)` for
    /// every name. Without the check a fork victim would wait out the whole
    /// budget and then report a timeout for something that is not one.
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
///
/// `#[non_exhaustive]` because this struct is *designed* to grow — `interp`,
/// `domain` and `nominal_rate_mhz` all arrived after `capacity`, and each would
/// have been a breaking change for any out-of-repo `EdgeCfg { .. }` literal.
/// Construction goes through [`EdgeCfg::new`] and the builder methods, which
/// stay source-compatible across every such addition. Reading the fields is
/// unaffected.
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
    ///
    /// Stored in `EdgeRecord::nominal_rate_mhz`, where `tf_tree doctor`'s
    /// `TFT007` reads it. Nothing on the read path consults it: an edge that
    /// publishes at half its declared rate still resolves normally, and saying
    /// so is a diagnostic's job, not the lookup's.
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
    ///
    /// Stored as milli-hertz because the rates that matter span 0.1 Hz (a map
    /// update) to 1 kHz (an IMU), which integer hertz cannot express at the low
    /// end. A rate that is not finite, not positive, or beyond `u32::MAX` mHz
    /// (~4.29 MHz) is **not** a rate any robot publishes at, so it is dropped
    /// back to "not declared" rather than clamped: a clamp would invent a
    /// 4.29 MHz nominal out of an `f64::INFINITY` typo and then report every
    /// real sample as a deviation from it.
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
            attachment: None,
            #[cfg(all(feature = "shm", target_os = "linux"))]
            claim_lock: None,
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
            // Before `backing` moves into `arena`.
            cache_scope: cache_scope_for(&backing),
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
            ofd_probe: None,
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
                        // Declaration time is the only moment it can be written
                        // — `ArenaBuilder`'s `&mut` borrow ends with this scope,
                        // and after that the arena may be shared.
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

    /// Whether this arena is a `.tft` image rather than something a process
    /// could still be writing to.
    ///
    /// **Not derivable from the other two.** `is_writable() == false` is also
    /// true of a live `PROT_READ` attachment, and `is_shared() == false` is also
    /// true of a heap arena; only the pair identifies a frozen one, and a
    /// predicate spelled as a conjunction of two unrelated answers is one
    /// backing variant away from being wrong. [`Tree::await_frames`] is the
    /// caller: a wait on a frozen tree is futile *by construction* — §2.4 says
    /// a frozen arena has no writers at all — and futility that is statically
    /// known should be an answer, not a nap.
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
/// This is not enforced by `ManuallyDrop`, and deliberately still is not.
/// Doing so needs `unsafe`, and this crate's `unsafe` budget is one block, in
/// [`OwnedWriter`], spent on the lifetime extension `docs/decisions/0017`
/// authorised — an earlier revision of this sentence said the crate was
/// `#![forbid(unsafe_code)]` and could not have a `ManuallyDrop` at any price,
/// which stopped being true when that record landed. **Do not reorder these
/// fields.**
///
/// # Storing one
///
/// You cannot: the lifetime is the point. [`OwnedWriter`] is the shape for a
/// claim that outlives its scope (`docs/API.md` §2.1).
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

/// An [`EdgeWriter`] that owns its tree — the claim shape for a writer that is
/// **stored** rather than scoped (`docs/decisions/0017`, `docs/API.md` §2.1).
///
/// # Which one to use
///
/// [`Tree::claim`] is the default and stays it. Where the claim's scope is
/// lexical the borrow checker enforces the claim's lifetime for free, and that
/// is worth more than the `Arc` this type costs. Reach for [`Tree::claim_owned`]
/// only when the writer outlives the scope that made it: a node that publishes
/// for the life of the process, or a binding whose handle type cannot carry a
/// lifetime at all. Two ways to claim an edge now exist, and if this one becomes
/// the copy-pasted default the compile-time claim-scope check is lost for
/// everybody.
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
/// `Send + !Sync`, exactly as [`Publisher`] is: single-writer-per-edge stays a
/// *type-level* property (`docs/PROJECT.md` §5 D7), not a convention this type
/// relaxes. Both are inherited from the [`EdgeWriter`] field — there is no
/// `unsafe impl Send` here and there must never be one, because an `unsafe impl`
/// would still compile after somebody replaced the field with something that had
/// no business crossing a thread.
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
///
/// The error code is pinned on purpose. A bare `compile_fail` passes when the
/// snippet fails to compile for *any* reason — including `OwnedWriter` being
/// renamed, moved or un-exported — so it would keep reporting success while
/// testing nothing. `E0277` is the unsatisfied-trait-bound failure that is
/// actually under test.
///
/// **rustdoc enforces the code on nightly only** — measured: mutating it to
/// `E0599` fails `cargo +nightly test --doc -p tf_tree` with *"Some expected
/// error codes were not found: \["E0599"\]"* and still reports `ok` on stable.
/// So `just test-doc` (stable, `--workspace`) is **not** the gate for this line;
/// `just test-doc-error-codes` is — one nightly command, run by CI's `miri` job,
/// which exists so this pin is a check rather than something that reads like
/// one.
///
/// The *renamed-or-unexported* half is covered on stable regardless, by
/// `assert_send::<tf_tree::OwnedWriter>()` in
/// `crates/tf_tree/tests/owned_writer.rs`, which stops compiling if the path
/// moves. The three together leave no way for this to pass while testing
/// nothing.
///
/// # Every guard is reproduced, and not by copying one
///
/// `EdgeWriter::drop` does three things a hand-rolled owned writer has to do
/// too: [`Publisher::abandon`] in a `fork` child, the `ClaimLease` release, and
/// the fork-generation compare that decides both. This type gets all three by
/// **containing the `EdgeWriter` whole** rather than by restating them, which is
/// what makes the count unable to drift. The defect `0017` exists to remove was
/// exactly a restatement that dropped two of the three: a
/// `transmute::<EdgeWriter, Publisher>` in `tf_tree_py` that kept the first
/// field, leaked the lease for the life of every Python publisher — so no reaper
/// would ever collect the edge either — and bypassed the fork guard.
///
/// # Drop order is load-bearing, for a second reason
///
/// The fields are declared **writer first, `tree` second**, and Rust drops them
/// in declaration order. The writer releases the claim by writing *into the
/// arena*; the `Arc` is what keeps that arena mapped. Reversing them is a
/// use-after-free on the last handle rather than the merely-wrong ordering
/// [`EdgeWriter`]'s own field order guards against. **Do not reorder these
/// fields.**
///
/// # Why the writer is behind a `Box`
///
/// Not for size, and not for the `Arc`: **an inline `EdgeWriter<'static>` here
/// is Undefined Behavior on drop**, under Stacked Borrows *and* Tree Borrows.
/// `just miri` reports it, the rest of the suite does not, and the box is what
/// `just miri` goes green on.
///
/// The mechanism, because it is not obvious. An `EdgeWriter` holds `&`
/// references into the arena. When a value is passed to a function **by
/// value**, the reference-typed fields inside it are retagged with a *strong
/// protector* for the whole call — that is what makes a reference argument
/// dereferenceable for the duration of a call. Both `drop(writer)` and
/// [`Self::release`] are exactly that: a by-value pass whose callee then drops
/// the last `Arc`, and freeing the arena while a strong protector points into
/// it is UB. It is not hypothetical model-lawyering — Miri's default
/// (`-Zmiri-retag-fields=all`) reports
/// *"deallocating while item \[SharedReadOnly …\] is strongly protected"*, and
/// `-Zmiri-tree-borrows` reports the Tree Borrows equivalent.
///
/// A `Box` field is *weakly* protected — deallocating through it is exactly
/// what a `Box` is for — and retagging does not reach through a pointer into
/// the boxed value, so the arena references are never protected across the
/// `Arc`'s release. Dropping through the box is then an ordinary in-place drop,
/// which was already sound: an `OwnedWriter` that falls out of scope, or that
/// lives in somebody's struct, never tripped this at all. Only the by-value
/// spellings did — which are the two this type documents as the way to release.
///
/// **The cost is one pointer chase in [`Self::push`], and it buys soundness on
/// the shape this type exists to provide.** [`EdgeWriter::push`],
/// [`Publisher::push`] and every scoped claim are untouched: this is a new path
/// that pays it, not an existing one that regressed. The alternative that would
/// not pay it is `Publisher` holding raw pointers instead of `&` — a change to
/// the `no_std` core's hot struct, which is a decision record, not a tidy-up.
pub struct OwnedWriter {
    /// The claim, with its borrow of the tree below extended to `'static`.
    ///
    /// Declared first so it drops first — see the type's doc comment.
    ///
    /// **Boxed for soundness, not for size, and it must stay boxed.** See the
    /// type's *Why the writer is behind a `Box`* section: with the `EdgeWriter`
    /// inline, `drop(writer)` and [`Self::release`] are both Undefined Behavior
    /// under Stacked *and* Tree Borrows, and `just miri` says so.
    writer: Box<EdgeWriter<'static>>,
    /// The strong reference that makes the field above's `'static` true.
    ///
    /// Never read, hence the `allow` — but **do not delete it**, and do not
    /// replace it with a `PhantomData`. It is the entire safety argument for the
    /// `'static` above; removing it leaves a writer pointing into an arena
    /// nothing is keeping alive, which is a use-after-free that compiles.
    ///
    /// Spelled without a leading underscore on purpose: the underscore
    /// convention in this file means "held only for its `Drop`" (see
    /// `EdgeWriter::_lease`), and this is held for its *refcount* — it has to be
    /// alive for the writer's whole life, not merely torn down in a particular
    /// order at the end of it.
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
    ///
    /// It is not decoration. An `EdgeId` is what names a claim to everything
    /// outside the arena's frame table — the lock file's byte, a counter row, a
    /// diagnostic — and a caller that cannot ask the writer has to re-derive it
    /// from a seqlock topology read that can fail, and then has no cross-check
    /// left that the two agree.
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
    ///
    /// # Why this forwards to [`EdgeWriter::push`] and not to [`Publisher::push`]
    ///
    /// [`EdgeWriter::push`] is the one that carries the fork check; the
    /// [`Publisher`] underneath it does not, and cannot — it is `no_std` and has
    /// no notion of a process. Routing this around it would put a store into an
    /// unmapped page one refactor away.
    ///
    /// `#[inline]` because this is a pure forwarder: the whole body is a call
    /// this crate can see through, and without the attribute a downstream
    /// embedder pays a real stack frame for the privilege of owning its tree.
    /// [`EdgeWriter::push`] itself deliberately carries no such attribute — it
    /// is not a forwarder, and changing that is a benchmark question, not a
    /// tidying one.
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
    /// Which *arena* this tree reads, for the per-thread plan cache's key
    /// (`crate::cache`).
    ///
    /// A compiled [`crate::Plan`] is a list of edge indices plus the static
    /// transforms folded along the way, so it is only meaningful against the
    /// arena it was compiled from. The cache is `thread_local!` and shared by
    /// every `Tree` on the thread, so without this the key
    /// `(target, source, generation)` matches across trees: ids are handed out
    /// in interning order and a freshly built tree's generation is its declared
    /// edge count (one tick per link — *not* zero, which an earlier revision of
    /// this comment claimed), which makes the collision the *normal* case for
    /// two similarly-built trees, not an adversarial one.
    ///
    /// See [`cache_scope_for`] for why this is the arena's identity rather than
    /// the handle's, and for what it is derived from.
    cache_scope: u64,
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
    /// The kernel-authoritative liveness probe, kept as a *probe* and not only
    /// as the closure it is folded into.
    ///
    /// `None` for every tree that did not come from [`crate::open`], which is
    /// the same set as `claim_lock`'s and for the same reason: no rendezvous,
    /// no lock file, no byte to ask about.
    ///
    /// **Why the same object is held twice.** [`Self::use_ofd_liveness`] boxes
    /// it into `liveness`, which answers one question — *is the participant in
    /// this slot running* — and answers it as a `bool`, folding `None` back
    /// into the `/proc` inference. [`Self::reap_participants`] needs the
    /// unfolded answer: `crate::open::reclamation_verdict` distinguishes
    /// *the kernel says free* from *the kernel would not say*, because only the
    /// first may be acted on destructively (`docs/PHASE2.md` §6.2).
    ///
    /// The `Arc` is shared because there is nothing here to rebuild a probe
    /// *from*. `crate::open::LivenessProbe::open` takes a `Rendezvous`, and no
    /// field of a `Tree` carries one: `attachment` holds a
    /// `tf_tree_ipc::Session`, which is a `LockFile`, a slot and an outcome,
    /// and a `LockFile` is a `File` with no path. The probe can only be built
    /// at open time, so reaching a second consumer means one object with two
    /// holders.
    ///
    /// **It buys reach, not agreement.** A rebuilt probe would be one more open
    /// file description of the same file — this one already is one, which is
    /// [`crate::open::LivenessProbe`]'s own documented choice — so the two
    /// would answer *identically* about every byte, ours included. Sharing
    /// costs an `open(2)` and an fd less; it decides nothing. What keeps either
    /// consumer off its own slot is the explicit guard each one writes, and
    /// neither guard depends on which description asked.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    ofd_probe: Option<std::sync::Arc<crate::open::LivenessProbe>>,
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
    /// **[`FrameError::ReadOnly`] if this tree is a read-only attachment and
    /// `name` is not already interned** — which is the most common failure on
    /// the default attach (`Open::new` is read-only, D18), and the one
    /// this section used not to mention at all. It is not a permissions
    /// complaint about a name that exists: it means *this name is not declared
    /// and I cannot declare it*. A consumer racing its publisher wants
    /// [`Tree::await_frames`]; a consumer that will never see the name declared
    /// wants the creator to declare it, or `frame_headroom` and a writable
    /// attach.
    ///
    /// Also [`FrameError::CapacityExceeded`] if the frame table is full,
    /// [`FrameError::FrameHashCollision`] if a name hash collides,
    /// [`FrameError::InternContended`] if another interner holds the name's
    /// slot and cannot be judged, and [`FrameError::ChildDetached`] on a tree
    /// inherited across a `fork()`.
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
    /// # Names only
    ///
    /// No rate, no jitter, no sample count. That is `docs/PHASE5.md` §4.2's
    /// `ds.edges()` and it stays held back until §3's counting pass exists —
    /// the same line the Python surface draws.
    ///
    /// # The snapshot is a snapshot
    ///
    /// On a live shared arena another process may intern a frame while this
    /// walk runs, so the list is what was true at some instant inside the call,
    /// exactly as [`crate::Plan::latest`] already is. Frames are append-only, so
    /// what it *does* promise is that nothing in it will be removed or renamed;
    /// a later call can only be longer.
    ///
    /// A slot whose id has been counted but whose record is not yet written is
    /// skipped rather than reported with an empty name — see the walk's own
    /// comments for what that filter is and, more importantly, what it is not.
    ///
    /// `frame_count` over-counts by one when a frame is abandoned mid-intern
    /// (`tf_tree_core::frame`'s `finish`: the record "stays written but
    /// unreferenced"), and that abandoned record carries a real name and a
    /// non-zero hash, so it lands here at a second id. `len()` is therefore an
    /// upper bound on the tree's frames, not the frame count.
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
        //
        // What no ordering available here buys is a race-free read.
        // `FrameRecord`'s fields are plain integers and its publication edge is
        // keyed by *name*, through the intern table; an enumeration keyed by id
        // has no edge to acquire. The `name_hash` filter below is a filter on
        // the value read, not the missing edge, and is not claimed to be one.
        let count = view.header().frame_count.load(Ordering::Relaxed);
        let mut out = Vec::with_capacity(count as usize);
        for raw in 1..=count {
            // Three checks — the strictest set any copy of this walk applied
            // before this method existed, which is the point of the method:
            //
            //  1. `FrameId::new` rejects 0, the root sentinel.
            //  2. `raw <= frame_count` — *this loop's bound*, and load-bearing
            //     rather than incidental: `frame_record` bounds against
            //     `max_frames`, which is `frame_count + 1 + frame_headroom`, so
            //     walking further hands back zeroed headroom slots as frames.
            //  3. `name_hash != 0`, below.
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
    /// Array in, array out, and **no allocation**: `N` is a const generic, and
    /// ids found on an early iteration are memoized rather than re-probed —
    /// which is legal because frames are append-only (D10), so a name once found
    /// cannot become unfound. That memoization is a *cost* property only:
    /// `find_frame` is idempotent, so dropping it would return the same ids,
    /// just after re-hashing every already-resolved name on every iteration. No
    /// assertion anywhere observes it, and `tests/rendezvous.rs` says so rather
    /// than claiming a guard it does not have.
    ///
    /// # The predicate is `find_frame`, and two handles are refused outright
    ///
    /// The wait polls the arena's intern table directly, never [`Tree::frame`],
    /// which is the wrong predicate in *both* modes for opposite reasons: on a
    /// read-only arena it answers [`FrameError::ReadOnly`] for an absent name —
    /// so the wait would never resolve — and on a writable one it **interns and
    /// succeeds immediately**. The second is the dangerous one: it is a
    /// confident wrong answer, an id for a name nobody declared.
    ///
    /// So a writable tree gets [`AwaitError::WritableTree`] rather than a
    /// silently-chosen meaning, before any sleep. A publisher already has
    /// [`Tree::frame`], which on its tree cannot fail for absence. A frozen
    /// `.tft` gets [`AwaitError::FrozenTree`] for the sibling reason: it has no
    /// writers, so the poll is futile by construction.
    ///
    /// **Which gate pins which half**, because they are not the same gate. The
    /// `WritableTree` refusal is `tests/await_frames.rs` under plain
    /// `just test`; `FrozenTree` is `tests/frozen.rs` under `just shm-check`;
    /// and the choice of `find_frame` over [`Tree::frame`] can only be observed
    /// on a *read-only* handle, which a default build cannot construct at all —
    /// it is pinned by `a_consumer_waits_for_a_frame_interned_after_the_arena_exists`
    /// in `tests/rendezvous.rs`, under `just shm-rendezvous`, and nowhere else.
    ///
    /// # Granularity
    ///
    /// A bounded poll — `MIN_BACKOFF` 200 µs doubling to `MAX_BACKOFF` 4 ms,
    /// this crate's pair, shared with `Open::await_open` — not a notification.
    /// `docs/decisions/0018` records
    /// why there is no arena-resident primitive to wake on (a `PROT_READ`
    /// consumer cannot register on one without giving up D18's boundary), and
    /// the argument applies here with more force because topology settles once,
    /// at startup. This therefore returns *later* than the name appeared, by up
    /// to one backoff interval plus scheduler granularity.
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
    ///
    /// And the consumer this method exists for — `docs/decisions/0019` §2b's
    /// startup sequence. **`text`, not `rust`, and that is the record's own
    /// deliberate choice**: the three calls yield `OpenError`, `AwaitError` and
    /// [`LookupError`], and `LookupError` implements neither `Display` nor
    /// `Error`, so no single `?`-chain unifies them — not even into
    /// `Box<dyn Error>`. It also needs a live arena and `--features shm`.
    ///
    /// ```text
    /// // Two waits, because they are two different absences.
    /// let tree = tf_tree::Open::new()
    ///     .mode(AttachMode::ReadOnly)                      // implies CreatePolicy::Never
    ///     .await_open(Duration::from_secs(5))?;            // wait for the arena
    /// let [target, source] =
    ///     tree.await_frames(["map", "base_link"], Duration::from_secs(5))?;
    /// let plan = tree.plan(target, source)?;
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
        // The other statically-futile handle. A frozen arena is read-only *and*
        // writer-free (`docs/PHASE5.md` §2.4), so the poll below would run the
        // caller's whole budget and report a timeout for something that was
        // never coming. Distinct condition, distinct answer.
        if self.arena.is_frozen() {
            return Err(AwaitError::FrozenTree);
        }
        let start = std::time::Instant::now();
        let mut found: [Option<FrameId>; N] = [None; N];
        let mut backoff = MIN_BACKOFF;
        loop {
            // **Per iteration, and before `view()`.** `view()` answers a fork
            // victim with the poison arena, whose `find_frame` returns
            // `Ok(None)` for every name — so without this a detached tree waits
            // out the whole budget and then reports a timeout for something
            // that is not one.
            if self.detached() {
                return Err(AwaitError::ChildDetached);
            }
            let view = self.view();
            for (slot, name) in found.iter_mut().zip(names.iter()) {
                if slot.is_some() {
                    // Memoized. Frames are append-only (D10), so a name once
                    // found cannot become unfound and re-probing it would only
                    // pay for the hash again.
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
    /// The stable mirror of Python's `tree.edges()` (`docs/API.md` §3.2), and
    /// on the stable surface for [`Tree::frames`]'s reason.
    ///
    /// `(parent, child)` is [`TreeBuilder::dynamic_edge`]'s argument order, so
    /// the list reads back the way it was written — but it rebuilds the *graph*
    /// only: the pair does not report the edge's kind, and telling a static edge
    /// from a dynamic one is `tf_tree::unstable::EdgeKind`'s job, which is
    /// arena-shaped and therefore unstable (`docs/API.md` §2.6).
    ///
    /// The pair is read out of the [`EdgeId`]'s own record, which is the
    /// *declared* topology. [`Tree::reparent`] moves a child under a new parent
    /// without rewriting that record, so on a reparented tree this names the
    /// edge's declaration and the live topology block names its current parent.
    /// They can disagree, and every other listing in this workspace makes the
    /// same choice: one answer that is wrong after a reparent beats two answers
    /// that disagree with each other.
    ///
    /// # Names only
    ///
    /// See [`Tree::frames`] — the statistics half is `docs/PHASE5.md` §4.2's and
    /// is held back on every surface, not just this one.
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
        //
        // `Relaxed` needs no argument beyond `frames`': unlike `frame_count`
        // there is no window at all. The edge table is sized and filled by
        // `TreeBuilder` and `edge_count` is stored exactly once, before the
        // arena is ever shared; nothing declares an edge at runtime.
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

        // §7.1's per-edge population, writer half. This is the moment the edge
        // is taken up, and it is off the publish path — `push` is what must not
        // fault, and it now cannot, for the first lap and every lap after.
        #[cfg(all(feature = "shm", target_os = "linux"))]
        self.populate_edge_rings(eid);

        Ok(EdgeWriter {
            publisher: Publisher::new(ring, claim_rec, epoch, owner),
            #[cfg(all(feature = "shm", target_os = "linux"))]
            fork_gen: self.fork_gen,
            #[cfg(all(feature = "shm", target_os = "linux"))]
            _lease: lease,
        })
    }

    /// Claim `child`'s edge, keeping the tree alive for as long as the writer
    /// lives (`docs/decisions/0017`).
    ///
    /// The scoped [`Tree::claim`] is preferable where the claim's scope is
    /// lexical — the borrow checker then enforces the claim's lifetime for free.
    /// Use this where the writer is *stored*: a node that publishes for the life
    /// of the process, or a binding whose handle type cannot carry a lifetime.
    /// See [`OwnedWriter`] for the full argument, and `docs/API.md` §2.1 for the
    /// rule this exists to satisfy.
    ///
    /// # Why `self: &Arc<Self>` and not `Arc<Tree>` by value
    ///
    /// By value would force a clone at every call site that already holds the
    /// handle, and it reads as though the tree were consumed. This spelling
    /// makes the refcount bump an implementation detail while keeping the real
    /// requirement — that the tree is *already* shared — visible in the
    /// signature. It also means the method is simply unavailable on a `Tree` a
    /// caller owns outright, which is the correct answer: they should be using
    /// [`Tree::claim`].
    ///
    /// # Cost
    ///
    /// Exactly [`Tree::claim`] plus one `Arc` strong-count increment, paid once
    /// at claim time. Nothing is added to `push`.
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
        // **This extends exactly one reference — the borrow of the `Tree` — and
        // nothing else.** An earlier revision transmuted a whole `EdgeWriter`
        // instead: a composite holding a `Drop` type and an `Option<ClaimLease>`
        // whose field set can grow, and reinterpreting a composite is precisely
        // how the defect this record exists to delete happened
        // (`transmute::<EdgeWriter, Publisher>`, which was not a lifetime
        // extension at all). This form cannot express that mistake.
        //
        // **And the claim is taken *through* the extended reference**, three
        // lines below, which makes the pairing unstateable rather than merely
        // stated: the writer provably borrows the same `Tree` the `Arc::clone`
        // refers to, so no caller — here or later — can pair a writer claimed
        // from tree A with an `Arc` for tree B. That is why this is written
        // inline rather than as a constructor taking a writer and an `Arc`
        // separately, which would be a safe fn with an unchecked precondition.
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
                // Reachable only through a reaper bug or `CreatePolicy::Always`
                // byte aliasing; back the CAS out rather than publish beside
                // them.
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
        //
        // `Tree::lookup` reaches this through `cache::with_plan`, which calls
        // `plan` on a miss and nothing on a hit, so a reader pays once per path
        // per process and the cached path is untouched.
        //
        // # Why this hangs off the callback instead of walking the returned plan
        //
        // Not style, and it was not predicted. `Plan` is a `[Step; MAX_DEPTH]`,
        // about 2 KiB by value, and this method is a tail expression so the
        // compiler builds the result straight into the caller's slot. Binding it
        // to a local in order to iterate `plan.steps()` costs a copy of all of
        // it, and that copy is worth **80 ns on `first lookup after attach`**:
        // 210 ns p50 against a 130 ns baseline, five runs to three, on the one
        // row §7.1 exists to protect. The cause was isolated by applying the
        // restructure *with the old population behaviour*, where it reproduced
        // in full — so it is the binding, not the populating. `Result::inspect`
        // is not an escape: it takes `self` by value and moves the same 2 KiB.
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
    ///
    /// # Why this is per-edge and not per-arena
    ///
    /// §7.1 is NORMATIVE that population happens at *declaration* granularity,
    /// **per-edge**. `populate_hot` used to over-approximate that for the rings
    /// by warming the whole stamp and pose arenas, on the true-but-irrelevant
    /// grounds that `0004` sizes them to the declared rings exactly. The rings
    /// are 99.8% of a large arena, so that over-approximation was very nearly
    /// the entire resident cost: a process attached to a 200-edge arena that
    /// reads five edges was charged for two hundred, permanently.
    ///
    /// This is the correction, and the win does not expire the way a
    /// *used-prefix* scheme's would. A prefix scheme saves only until the ring
    /// laps — every slot becomes used once `head` reaches `capacity`, so at
    /// 10 Hz into a 16 384-slot ring the saving lasts 27 minutes and is zero
    /// after. Keying on *which edges this process touches* saves forever,
    /// because a process that never plans an edge never plans it.
    ///
    /// # Only a shared mapping
    ///
    /// A `HeapArena` is ordinary anonymous memory with no populate concept, and
    /// a `.tft` deliberately populates nothing at all — [`Tree::open_frozen`]
    /// states that case: a dataloader worker seeks to the four pages its batch
    /// needs and the win is precisely that the rest costs nothing across sixteen
    /// workers. Matching on the backing keeps that decision intact rather than
    /// quietly reversing it for every frozen reader that compiles a plan.
    ///
    /// Idempotent and cheap to repeat: `madvise(MADV_POPULATE_*)` over pages
    /// that are already resident is a walk, not a fault.
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
    /// reuses a cached) [`crate::Plan`], and evaluates it. Keeps a small
    /// per-thread plan cache keyed by
    /// `(arena, target, source, generation)`.
    ///
    /// The `arena` component identifies the arena this tree reads, and is what
    /// keeps two trees on one thread from answering for each other (#196): the
    /// cache is `thread_local!` and the other three components agree across two
    /// similarly-built trees as a matter of course. Two handles onto one shared
    /// segment deliberately share it — one segment is one arena — so a plan
    /// compiled through either is reused by the other.
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
        cache::with_plan(self, t, s, generation, |plan| {
            let g = self.guard();
            plan.at(&g, stamp)
        })?
        .0
    }

    /// A read-only [`ArenaView`] over the backing arena, for diagnostics and
    /// inspection (the CLI `tree` and `doctor` commands).
    ///
    /// # Stability
    ///
    /// **Unstable: behind the `unstable` feature, with [`ArenaView`] itself**
    /// (`docs/API.md` §2.6, [`crate::unstable`]). It is gated rather than merely
    /// documented because it is the *door*: a caller can reach every accessor on
    /// the returned value through inference without ever naming the type, so
    /// leaving this method on the stable surface would have made the tier split
    /// a spelling convention instead of a promise.
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
    /// The arena arrives **already built** — topology, frames and edges were all
    /// declared by the creator — so there is no builder here and none is
    /// possible: `docs/PROJECT.md` §5 D4 forbids growth, and a second process
    /// declaring edges into a fixed layout is exactly the growth that is
    /// forbidden. **An attached process reads.** It does not publish: this path
    /// takes [`AttachMode::ReadOnly`] and nothing else, and the paragraph below
    /// is why.
    ///
    /// The read-only mapping is `PROT_READ`, which makes corruption impossible
    /// rather than merely impolite — the only real safety boundary in the trust
    /// model (`docs/PHASE2.md` §0), and enforced by the MMU rather than by
    /// convention.
    ///
    /// # A writer joins through [`crate::Open`], not through a descriptor
    ///
    /// **This is a refusal, not advice:** [`AttachMode::ReadWrite`] here returns
    /// [`ShmError::ReadWriteNeedsRendezvous`]
    /// (`docs/decisions/0028-the-slot-a-killed-participant-keeps.md`, plan step
    /// 0b). A read-write attach *registers a participant record*; the rendezvous
    /// takes an OFD lock byte for its slot before writing that record, and a bare
    /// descriptor has no lock file to take one in. A record with a permanently
    /// free byte is indistinguishable, by the byte alone, from a slot leaked by a
    /// killed process — so allowing one here would mean no reclaimer could ever
    /// key on the byte.
    ///
    /// A read-only attach registers nothing (the table is in the arena and
    /// writing it needs a writable mapping), so it can leak no slot and is
    /// unaffected.
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
    /// **This entry point refuses [`AttachMode::ReadWrite`] for
    /// [`Tree::attach_shared`]'s reason, and the slot argument does not change
    /// it.** A caller holding a raw descriptor and a slot number holds no lock
    /// byte for that slot: the byte is taken inside the rendezvous, by the
    /// handshake that also produced the number. Being told a slot index is not
    /// the same as having been granted one, and a `pub` entry point cannot tell
    /// the two apart. [`crate::open`]'s joiner registers through a crate-private
    /// path (`Tree::attach_joined_at`) that carries the byte as its
    /// precondition.
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
    /// `pub(crate)`, with the byte as its precondition: the only caller is
    /// [`crate::open`]'s `Joined` arm, which reaches here holding the
    /// `tf_tree_ipc` session that took byte `slot` during the handshake
    /// (`tf_tree_ipc/src/open.rs`'s `register_at`, before the arena record is
    /// written). That is the whole difference between this and the `pub` entry
    /// point above, and it is why this one may register a read-write
    /// participant.
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
            attachment: None,
            #[cfg(all(feature = "shm", target_os = "linux"))]
            claim_lock: None,
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
            attachment: None,
            claim_lock: None,
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
    ) {
        self.attachment = Some(crate::open::Attachment::Joined {
            _session: session,
            _socket: socket,
        });
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
        // **One description, two holders — and the `Arc` is exactly the
        // lifetime convenience.** The probe arrives by value, the closure below
        // must own what it captures, and `Self::reap_participants` needs the
        // same object for the three-valued answer this closure collapses.
        // Nothing in a `Tree` can build a second one: `LivenessProbe::open`
        // wants a `Rendezvous` and a `LockFile` keeps no path. Sharing is not a
        // correctness property — this probe is *already* a description of its
        // own, which the closure's own comment below and `LivenessProbe`'s doc
        // comment both say, so a second one would agree about every byte
        // including ours; it would cost an `open(2)` and an fd and decide
        // nothing. See the `ofd_probe` field.
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

    /// Reclaim the participant records of processes the kernel says are gone
    /// (`docs/PHASE2.md` §3.9 and §6.3, `docs/decisions/0028` plan step 5).
    ///
    /// Returns how many records were collected. Sweeps every slot in the
    /// arena's participant table, applies the one reclamation predicate to
    /// each, and frees the record where — and only where — that predicate says
    /// the byte is free.
    ///
    /// # This is deliberately not owner-only
    ///
    /// `docs/PROJECT.md` §6 lists "reaping from the owner only" as a design
    /// smell by name and `docs/PHASE2.md` §6.3 forbids it outright, and the
    /// reason is concrete rather than stylistic. The owner's socket-hangup
    /// callback is the O(1) fast path and it cannot see five things (`0028`,
    /// candidate B), of which **the owner's own slot** is the one no `HUP` can
    /// ever cover: the owner registers itself, no socket of its own closes, and
    /// nothing hangs up on it. A `SIGKILL`ed owner therefore leaves a `LIVE`
    /// record over a byte the kernel has released — #184's wedge — and any
    /// *surviving* read-write participant calling this is what collects it.
    ///
    /// # One slot per process, so the sweep never reasons about two
    ///
    /// `0028` open question 3 settled that a taking-over heir keeps the slot,
    /// byte and record it already holds — takeover is byte 0 plus a `bind` —
    /// because its participant slot is baked into every claim (A3) and every
    /// topology guard (A2) it holds. So one process is one slot, always, and
    /// nothing here has to reconcile a process occupying two.
    ///
    /// # What it refuses, and how you can tell
    ///
    /// **A read-only tree reaps nothing**, and returns `0` rather than an
    /// error, which is [`Self::reap_dead`]'s shape and `docs/API.md` R6's
    /// substance taken through the door that rule's own text opens: reclaiming
    /// is a `compare_exchange` and a `PROT_READ` mapping answers one with
    /// `SIGSEGV`, so the check is what makes read-only a safety boundary; and
    /// [`Self::is_writable`] is public *"so a consumer can branch on capability
    /// instead of on an error it was going to get"*. Read-only is the consumer
    /// default (D18) and the Python default, so this is the ordinary case, not
    /// a corner. A caller that needs to tell "refused" from "nothing to
    /// collect" asks `is_writable()`; the two are otherwise the same `0`.
    ///
    /// **A tree with no lock file reaps nothing**, for the reason the predicate
    /// documents: liveness is a kernel fact about a byte (§5.1), and a heap
    /// tree or a directly-built `build_shared` tree has no byte to ask about.
    /// The probe is installed by `Open::attempt` and by nothing else.
    ///
    /// **This process's own slot is never collected.** The predicate's own
    /// second constraint, unconditional and evaluated first: `F_OFD_GETLK`
    /// reports only *conflicting* locks, so a description does not see its own,
    /// and a sweep that judged itself from the byte would reclaim its own live
    /// record the moment a refactor shared one description.
    ///
    /// **A tree detached by `fork` reaps nothing**, and what stops it is the
    /// crate-internal `view()` answering with the poison arena rather than a
    /// check here — the same single mechanism `Drop` relies on, kept single on
    /// purpose. The poison arena's table is empty, so every slot reads `FREE`
    /// and the sweep collects nothing without a syscall.
    ///
    /// # Cost
    ///
    /// Nothing on the hot path — `Plan::at` is untouched, so `docs/API.md` R2
    /// is unaffected. One `F_OFD_GETLK` per non-`FREE` slot that is not ours,
    /// and nothing at all for the others: a `FREE` word costs one `Acquire`
    /// load and no syscall, which is what makes the common shape free — a
    /// read-only joiner holds a byte and writes no record, so most slots on a
    /// real system are exactly that. For scale, 64 probes were measured at
    /// ~23-28 µs and a single probe at ~0.4 µs, against a 97.5 µs p50 attach
    /// (`0028` open question 4); a sweep that finds nothing to ask about costs
    /// neither.
    ///
    /// # The read order is a correctness property
    ///
    /// Every slot goes through `crate::open::reclamation_verdict`, which
    /// observes the `state` word **before** it probes the byte and carries the
    /// observed word into the verdict so this loop has nothing to re-read.
    /// `ParticipantTable::reclaim` is a `compare_exchange` against *that* word,
    /// so a slot that changed between the verdict and the CAS is not collected.
    /// Reversed — or written from `LockFile::held_participants`, which returns
    /// all 64 bytes in one call and makes the wrong order the natural one —
    /// `loom` erases a published record in 0.00 s. That is why this sweeps
    /// through the predicate rather than taking a holder mask first, and why
    /// there is exactly one predicate to sweep through.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    #[must_use]
    pub fn reap_participants(&self) -> usize {
        // R6, and first: a `PROT_READ` mapping does not fault politely on a
        // `compare_exchange`, it delivers `SIGSEGV`.
        //
        // **One check, not two**, and unlike `reap_inner`'s pair that is a
        // choice with a measurement behind it. The `participant == u32::MAX`
        // half would catch every read-only tree here too — a read-only
        // attachment registers no record, so the sentinel and unwritability
        // arrive together — and deleting it alone leaves
        // `a_read_only_tree_reaps_no_participant_records` green. It is the
        // proxy; this is the hazard. `reap_inner` needs the sentinel for a
        // second reason this has not got — `slot + 1` overflows when it forms
        // an owner word — and a guard kept for a reason that does not apply is
        // the one a later reader deletes as redundant, having deleted the wrong
        // one.
        if !self.arena.is_writable() {
            return 0;
        }
        // No rendezvous, no lock file, no kernel fact to act on. The predicate
        // is scoped to a tree carrying a probe, and this is that scope.
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
                // `crate::open`'s `reclamation_verdict` is where that is stated
                // and argued (`docs/decisions/0028` piece 2, third constraint);
                // this is the same order, and splitting it into two statements
                // that probe first is what a model erases a published record
                // with.
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

/// Refuse a read-write attach that arrives over a bare file descriptor.
///
/// One function rather than a check repeated at each entry point, because there
/// are two `pub` entry points and closing one of them closes nothing:
/// [`Tree::attach_shared`] and [`Tree::attach_shared_at`] differ only in where
/// the slot index comes from, and neither has a lock file to take a byte in
/// (`docs/decisions/0028-the-slot-a-killed-participant-keeps.md` plan step 0b).
///
/// **Before the segment is mapped**, deliberately: the refusal is a property of
/// the arguments alone, and a caller who gets it after a `SizeMismatch` would be
/// told about the wrong thing.
///
/// [`AttachMode::ReadOnly`] passes through untouched. It registers no
/// participant record at all — `attach_shared_inner` gives a non-writable
/// backing the `u32::MAX` sentinel instead — so it can strand no slot.
#[cfg(all(feature = "shm", target_os = "linux"))]
fn refuse_a_byteless_writer(mode: AttachMode) -> Result<(), ShmError> {
    match mode {
        AttachMode::ReadOnly => Ok(()),
        AttachMode::ReadWrite => Err(ShmError::ReadWriteNeedsRendezvous),
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

/// Which arena a [`Tree`] reads, as one `u64`, for the plan cache's key.
///
/// **The arena, not the handle.** Two `Tree`s mapping one shared segment see
/// one topology and one set of static transforms, so a plan compiled through
/// either is correct through the other; giving them separate identities would
/// cost every second handle a recompile and buy no safety. Two *processes*
/// mapping that segment have separate caches already — the cache is
/// `thread_local!` — so nothing here is shared between them but the value.
///
/// The two backings answer "which arena" from different places:
///
/// * A shared segment already carries an identity: `instance_uuid`, drawn once
///   per `MappedArena::create` and preserved by every attach, which is exactly
///   the "this arena instance, as distinct from this arena *name*" question
///   `docs/decisions/0005` made it answer. Every handle onto one segment
///   agrees on it, which is the property this key wants.
/// * A heap arena's `instance_uuid` is all-zero on purpose (`HeapArena` is
///   single-process by construction, and drawing randomness there would put an
///   RNG in the no-`shm` dependency budget), and it **must not grow a field to
///   fix that**: `FORMAT_VERSION = 3` already happened. It does not need one —
///   a `HeapArena` is owned by exactly one `Tree`, so for that backing handle
///   identity *is* arena identity — and a process-local counter supplies it.
///
/// **Not the base pointer**, which is the tempting answer that adds no state:
/// the allocator hands the same address straight back to the next arena of the
/// same layout, measured here as 8 of 8 build-drop-build cycles returning one
/// address, so a base-pointer key would leave the entire defect standing for
/// sequential trees — which is what a test suite and any rebuild loop does.
///
/// A frozen `.tft` takes a counter id too, even though its header **does** carry
/// the `instance_uuid` of the live arena it was frozen from — measured, by
/// freezing one arena to two paths and reading both back: same uuid, non-zero.
/// Two files frozen from one arena at different times therefore share that id
/// and *differ in content*, so it is not an identity for the image. Two
/// `open_frozen` calls on one path also compile separately, which costs a
/// compile and cannot be wrong.
fn cache_scope_for(backing: &ArenaBacking) -> u64 {
    // **A match on the variants, not a predicate.** The question here — "does
    // this backing carry an identity that outlives the handle?" — is one no
    // existing predicate answers. `is_shared` is the one it was first spelled
    // as, and its own doc says it means "whether a peer can *mutate* it": the
    // participant table, the claim protocol and reaping hang off that answer,
    // and this does not. `is_frozen`'s doc, three lines below it, is the
    // warning that applies verbatim — a predicate answering a question next to
    // the one it was asked is one backing variant away from being wrong. Two
    // concrete ways, both live today:
    //
    // * Give `Frozen` the *other* defensible `is_shared` answer — other
    //   processes really may map the same `.tft`, which is what that method's
    //   summary line says — and every frozen tree starts keying on the header's
    //   `instance_uuid`. Measured: two files frozen from one live arena carry
    //   the **same non-zero** uuid, so two `.tft`s that differ in content would
    //   share a cache scope. That is issue #196 again, in the offline path.
    // * Add a mapped-but-immutable backing (a file-backed image, a sealed
    //   snapshot). `is_shared` false hands every handle its own id, silently
    //   costing the recompile that
    //   `cache::tests::two_handles_on_one_shared_arena_share_their_plans`
    //   exists to prevent; `is_shared` true reads a uuid that may not identify
    //   the *contents*, which is the bug above.
    //
    // Matched here rather than behind a new `ArenaBacking::instance_identity`
    // method because there is exactly one caller and the compile error is
    // identical either way — but written this way it lands on the paragraph
    // that says what the new variant has to answer, instead of on a signature
    // three hundred lines away.
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
///
/// The same shape as `tf_tree_c::publisher`'s `NEXT_TOKEN`, and for the same
/// reason: an identity that can be recycled turns the comparison that uses it
/// into a coin flip, and a `u64` counter is the cheapest thing that cannot be.
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
    view.participants().register(
        std::process::id(),
        process_start_time().unwrap_or(UNKNOWN_START_TIME),
        now_nanos(),
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
        now_nanos(),
    )
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
/// corruption; a false "alive" only delays recovery. [`alive_given`] is where
/// the bias is applied, and two of its arms exist because two branches once
/// produced the other direction.
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
        // Comparing anyway is false for every real start time, so the first
        // reader that *can* read `/proc` reports a running process dead — and it
        // does not even degrade to a bare-pid check, which would at least be
        // conservative. It inverts.
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

    alive_given(start_time, read_start_time(pid), proc_answers_here())
}

/// The `start_time` a participant record carries when this host would not say
/// what it is.
///
/// **Zero, and not a new field.** `FORMAT_VERSION = 3` already happened and
/// CLAUDE.md forbids adding an arena field opportunistically; a fresh arena's
/// participant region is zero anyway, so zero is already the value that means
/// "nothing written here". What changes is the *reading*: [`alive_given`] treats
/// a stored zero as unknown rather than as a start time to compare against.
///
/// A process genuinely started in tick 0 of the boot collides with the sentinel
/// and reads as unknown — that is, as **alive**, the direction the predicate is
/// biased towards, and only for a process in pid 1's neighbourhood.
const UNKNOWN_START_TIME: u64 = 0;

/// Would this host tell us that some other process exists?
///
/// A `/proc` that is not mounted — a `chroot` without one, a stripped container
/// — fails **every** open with `ENOENT`, the same errno a genuinely dead pid
/// produces and indistinguishable from it at the call site. Reading our own
/// entry settles which it is: this process is running by construction, so if
/// `/proc/self/stat` is not there then `/proc` is not there, and an `ENOENT`
/// about anybody else proves nothing at all.
///
/// **Latched on a decisive answer only**, because the answer is a property of the
/// host rather than of the pid being asked about, and the callers are the
/// liveness predicate — run under the topology lock, and once per claim per reap
/// sweep. `Ok` latches "this host answers"; `ENOENT` latches "it does not",
/// which is the genuine no-`/proc` host and is correct forever there. **Any
/// other error is indecisive and is deliberately not latched**: `ENOMEM`, an LSM
/// or seccomp denial, or a bind-mount race during startup would otherwise make
/// one transient failure permanent, and permanently answering "cannot answer"
/// means this process can never prove a death again — the reap sweep stops
/// reclaiming slots and the topology lock stops being stealable, so a momentary
/// error becomes a lasting loss of recovery. It is the safe direction and it is
/// still the wrong trade. An indecisive call resolves to alive and re-probes
/// next time.
///
/// The steady-state cost is unchanged and is zero syscalls: one `stat` on the
/// first liveness question, then a relaxed atomic load, on every host that has a
/// `/proc` and equally on every host that has none.
///
/// The residual is a `/proc` unmounted *after* a successful first question, which
/// leaves a latched `true` and the misreading this exists to prevent. That is the
/// behaviour before the probe existed, on a host doing something no supported
/// deployment does, and covering it would put a syscall back on the predicate's
/// path.
///
/// **`hidepid=2` is out of scope by dependency, not by accident.** It hides
/// another user's entries behind the same `ENOENT`, which nothing here can
/// distinguish — but `docs/PHASE2.md` §3.10 makes participants same-user by
/// construction, so a hidden entry cannot belong to one. That is the trust model
/// carrying weight on a correctness path, and it is named here because it is
/// named nowhere else.
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
///
/// *Can* prove it, and does not on its own. `NoSuchProcess` is what the read
/// saw, not a verdict: [`proc_answers_here`] is the second fact it needs, and
/// [`alive_given`] is where the two meet.
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
///
/// `tf_tree_ipc::parse_start_time` is the same parser behind a richer error
/// type; this copy exists because that crate is a dependency only under the
/// `shm` feature and the predicate above is not.
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
///
/// Paired with the PID this is a **reuse-proof** process identity
/// (`docs/PHASE2.md` §1, A7 and §5.1): PIDs wrap, and a reaper that trusted a
/// bare PID could conclude a long-dead participant is alive because an unrelated
/// process now holds its number.
///
/// **`Option`, where this used to return `0` on failure.** A registrant that
/// cannot read its own start time has no identity to publish, and making the
/// caller name what it writes instead — [`UNKNOWN_START_TIME`] — is what stops
/// the sentinel being mistaken for a start time. It had been one: a `0` written
/// here compares unequal to every real start time, so the first reader that
/// could read `/proc` declared the registrant dead while it was running.
///
/// Not cached. It is constant for a process, but a `fork`ed child's differs from
/// its parent's, and a cache would hand the child the parent's value to register
/// under — reintroducing exactly the mismatch above.
fn process_start_time() -> Option<u64> {
    parse_start_time(&std::fs::read_to_string("/proc/self/stat").ok()?)
}

/// A [`LookupError`] paired with the [`Tree`] that can resolve its ids to names.
///
/// `Display` produces a human-readable message (the error itself stays `Copy` and
/// allocation-free). Obtain one with [`Tree::describe`].
///
/// # Both fields are private, and that is the change rather than the status quo
///
/// They were `pub`, which promised that this wrapper is *exactly* the pair
/// `(error, tree)` forever — a promise nothing needed: the only construction
/// site in the workspace is [`Tree::describe`], and the only use is `Display`.
/// `docs/API.md` §R5 makes the prose layer explicitly separate from the error
/// type, so a caller who wants the [`LookupError`] back holds the one it passed
/// in; it is `Copy`.
///
/// The `'a` is correct under §2.1 and is not the [`EdgeWriter`] case: this is a
/// per-format-call borrow that lives to the end of the `write!` it appears in,
/// like a `std::fmt::Arguments`. Storing one in a struct is not something a
/// user has any reason to do, and `Tree::describe`'s signature is what stops
/// them trying.
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
            //
            // **Bounded at eight, and sorted — and the bound is on the *text*,
            // not on the allocation.** An earlier revision of this comment
            // justified the truncation with "unbounded, a `Display` on a
            // 10 000-frame tree would allocate 10 000 `String`s", which is
            // false: `Tree::frames` allocates one `String` per frame and sorts
            // all of them *before* the `truncate` below ever runs, so the
            // allocation is 10 000 either way. What eight buys is a readable
            // error line — an operator scanning a log wants a sample of the
            // namespace, not a dump of it.
            //
            // Paying that allocation is a deliberate accept, not an oversight:
            // this is `Display` on an error, reached once per failed lookup by a
            // process that is already about to log or exit, and the frame table
            // is memory this process has mapped. Making it genuinely bounded
            // means a bounded-`k` selection over borrowed `&str`s out of the
            // arena rather than `Tree::frames`' owned `Vec<String>`, which is a
            // different method with different unsafe-free borrow plumbing; it is
            // recorded as a follow-up rather than smuggled into an error-message
            // change. Sorted, so two runs of the same failure print the same
            // list — `Tree::frames`' id order does not promise that across
            // processes that interned in different orders.
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
                             not declared it yet: wait with Tree::await_frames, \
                             or declare it on the TreeBuilder that creates the \
                             arena"
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
    ///
    /// The inversion: `process_start_time` used to return `0` on failure, and
    /// that `0` went into the record. A reader that *could* read `/proc` then
    /// got `Known(st)` with `st != 0`, so the comparison was false and the
    /// verdict was death about a running process.
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

    /// The host probe answers here, which is what makes the two tests above mean
    /// what they say — on a host that answered `false` both would read alive.
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
    ///
    /// `tf_tree_ipc` pins the same case against its own copy. This one is
    /// reachable in a build with no `shm` feature, where that crate is not a
    /// dependency at all.
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
    ///
    /// **`ReadOnly` is checked in the same test, on both**, because a refusal
    /// that also broke the reader path would pass an assertion that only looked
    /// for the error.
    ///
    /// Mutant: delete either `refuse_a_byteless_writer` call ⇒ that arm returns
    /// `Ok` and its `expect_err` fails.
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
