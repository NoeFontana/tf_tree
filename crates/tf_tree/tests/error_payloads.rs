//! The facade's error types, measured from outside the crate: every payload a
//! public variant carries is nameable through `tf_tree` alone, a payload that
//! has a `Display` is printed with it rather than dumped with `Debug`, and the
//! two writer-path refusals name the edge they are about (D11).
//!
//! This target carries no crate-level `#[cfg(feature = ...)]`, so it compiles in
//! the facade's default feature set, and `just test` runs it there. Two items in
//! it are `shm`-gated: [`every_ipc_error_payload_is_nameable_through_the_facade`],
//! whose compile is the whole of what it asserts, and
//! [`shared_memory_wrappers_print_their_display`], which asserts at run time.
//! `just shm-check` runs this target under `shm` for the second one
//! (`cargo nextest run -p tf_tree --features shm --test error_payloads`); before
//! `docs/decisions/0059` that recipe only clippied it, which compiles a runtime
//! assertion and never executes it.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use tf_tree::{
    AwaitError, BuildError, Capacity, ClaimApiError, ClaimError, EdgeCfg, FrameError, FrameId,
    Iso3, PushError, ReparentError, TopologyError, TreeBuilder,
};

/// Every stable error variant's payload type is reachable from `tf_tree`, and
/// it is **the** type the variant carries rather than a same-named stand-in:
/// each line coerces the variant's constructor to a `fn` pointer over the
/// facade path, which is `E0308` if the two are different types and
/// fails to compile if the name is missing.
///
/// **Mutant:** drop `TopologyError` from the facade's `pub use tf_tree_core::{..}`
/// list. Applied: this target does not compile — `error[E0432]: unresolved
/// import `tf_tree::TopologyError``.
#[test]
fn every_public_error_payload_is_nameable_through_the_facade() {
    let _: fn(TopologyError) -> BuildError = BuildError::Topology;
    let _: fn(tf_tree::LayoutError) -> BuildError = BuildError::Layout;
    let _: fn(tf_tree::ParticipantError) -> BuildError = BuildError::Participant;
    let _: fn(FrameError) -> BuildError = BuildError::Frame;
    let _: fn(TopologyError) -> ReparentError = ReparentError::Topology;
    let _: fn(FrameError) -> AwaitError = AwaitError::Frame;
}

/// The same pin for `OpenError::Rendezvous`'s payload, under the feature that
/// exports it. `IpcError` itself by constructor coercion, and each of the eight
/// types its variants carry by a typed binding on the field that holds it, so a
/// name missing from the facade's `pub use tf_tree_ipc::{..}` list is `E0432`
/// and a same-named stand-in is `E0308`. Nothing else named these eight through
/// `tf_tree::` — `tests/rendezvous.rs` matches only two field-less `IpcError`
/// variants — so before this item any of them could be dropped from the list
/// with every gate green.
///
/// **Mutant:** drop `LockRole` from the facade's `pub use tf_tree_ipc::{..}`
/// list. Applied: `cargo clippy -p tf_tree --features shm --all-targets` fails
/// on this target and no other — `error[E0432]: unresolved import
/// `tf_tree::LockRole``, while the library itself still compiles.
#[cfg(all(feature = "shm", target_os = "linux"))]
#[test]
fn every_ipc_error_payload_is_nameable_through_the_facade() {
    use tf_tree::{
        EnvVar, HelloStatus, IpcError, LockRole, NameProblem, OpenError, ProcError, ProcParseError,
        RuntimeDirSource, WireError,
    };

    let _: fn(IpcError) -> OpenError = OpenError::Rendezvous;
    let _: fn(WireError) -> IpcError = IpcError::HandshakeMalformed;
    let _: fn(ProcError) -> IpcError = IpcError::Proc;

    fn fields(e: IpcError) {
        match e {
            IpcError::RuntimeDirNotADirectory { source } => {
                let _: RuntimeDirSource = source;
            }
            IpcError::NameInvalid { var, problem } => {
                let _: EnvVar = var;
                let _: NameProblem = problem;
            }
            IpcError::LockFailed { role, .. } => {
                let _: LockRole = role;
            }
            IpcError::RejectionCarriedFd { status } => {
                let _: HelloStatus = status;
            }
            IpcError::Proc(ProcError::Parse { cause, .. }) => {
                let _: ProcParseError = cause;
            }
            _ => {}
        }
    }
    fields(IpcError::ArenaAbsent);
}

/// A wrapper prints its payload's prose, not the payload's struct literal.
///
/// `ends_with(inner)` is what separates `{0}` from `{0:?}`: a payload's
/// `Display` and `Debug` never coincide (the rendering tests in `tf_tree_core`
/// and `tf_tree_arena` assert that for every variant), so a wrapper that fell
/// back to `Debug` ends with the variant name, or a closing brace, instead.
fn prints_its_payload(outer: &dyn std::fmt::Display, inner: &dyn std::fmt::Display) {
    let shown = outer.to_string();
    let inner = inner.to_string();
    assert!(
        shown.ends_with(&inner),
        "{shown:?} does not end with its payload's Display {inner:?}"
    );
    assert!(!shown.contains('{'), "{shown:?} is a struct dump");
    assert!(!shown.contains("FrameId("), "{shown:?} prints a Debug id");
}

/// **The typo-grade mistake, through the real builder.** Two edges that make a
/// cycle are `BuildError::Topology`, and its message used to read
/// `topology error: WouldCreateCycle { child: FrameId(1) }`.
///
/// **Mutant:** `#[error("topology error: {0}")]` → `{0:?}` on
/// `BuildError::Topology`. Applied: this test fails — `"topology error:
/// WouldCreateCycle { child: FrameId(1) }" does not end with its payload's
/// Display "attaching frame 1 under that parent would create a cycle"`.
#[test]
fn a_cycle_in_the_builder_renders_as_prose() {
    let err = TreeBuilder::new()
        .static_edge("a", "b", &Iso3::IDENTITY)
        .static_edge("b", "a", &Iso3::IDENTITY)
        .build()
        .map(drop)
        .unwrap_err();
    let BuildError::Topology(inner) = err else {
        panic!("a two-edge cycle is a topology error, got {err:?}");
    };
    prints_its_payload(&err, &inner);
}

/// The wrappers that dumped a payload which already had prose, or which gained
/// it in `docs/decisions/0059`.
///
/// Each is constructed rather than provoked: `AwaitError::Frame` needs a 64-bit
/// hash collision or an anonymous claimant stalled mid-intern, and the other two
/// are the same attribute on a sibling enum as the builder test above.
///
/// **Mutants, applied one at a time, each fails this test at its own line:**
/// `AwaitError::Frame`'s `{0}` → `{0:?}` (`"InternContended" does not end with
/// its payload's Display "interning contended past its retry budget; ..."`);
/// `BuildError::Frame`'s `{0}` → `{0:?}` (`"frame error: FrameHashCollision {
/// hash: 7 }" does not end with ...`); `ReparentError::Topology`'s `{0}` →
/// `{0:?}` (`"topology error: WouldCreateCycle { child: FrameId(3) }" does not
/// end with ...`).
///
/// `BuildError::Layout` and `BuildError::Participant` joined in `0059`, when
/// `LayoutError` and `ParticipantError` gained a `Display`.
#[test]
fn wrapped_payloads_print_their_display() {
    let contended = FrameError::InternContended;
    prints_its_payload(&AwaitError::Frame(contended), &contended);

    let collision = FrameError::FrameHashCollision { hash: 7 };
    prints_its_payload(&BuildError::Frame(collision), &collision);

    let cycle = TopologyError::WouldCreateCycle {
        child: FrameId::new(3).unwrap(),
    };
    prints_its_payload(&ReparentError::Topology(cycle), &cycle);

    let too_large = tf_tree::LayoutError::ArenaTooLarge {
        total_size: 5_000_000_000,
    };
    prints_its_payload(&BuildError::Layout(too_large), &too_large);

    let full = tf_tree::ParticipantError::TableFull;
    prints_its_payload(&BuildError::Participant(full), &full);
}

/// `docs/decisions/0059` part (c) under `shm`: the three wrappers whose payloads
/// only exist with shared memory print that payload's `Display`, and a
/// `ShmError` returned bare by `Tree::attach_shared` leaves a function through
/// `?` into `Box<dyn Error>`, which before `0059` was `E0277`.
///
/// Every payload here is struct-shaped or a unit variant, and either shape
/// fails `prints_its_payload` under `{0:?}`: a struct dump carries a brace, and
/// a bare variant name does not end with the prose it is the last word of.
///
/// **Mutant (M7):** `OpenError::Map`'s `#[error("{0}")]` → `{0:?}`. Applied:
/// `just shm-check`'s `--test error_payloads` line fails at this test —
/// `"LayoutMismatch { found: 1, expected: 2 }" does not end with its payload's
/// Display "arena layout hash 0x00000001 is not this build's 0x00000002
/// (LayoutMismatch)"` — while the same target in default features, which is
/// all `just test` runs, stays green.
#[cfg(all(feature = "shm", target_os = "linux"))]
#[test]
fn shared_memory_wrappers_print_their_display() {
    use std::os::fd::OwnedFd;

    use tf_tree::{AttachMode, FrozenError, FrozenFileError, OpenError, ShmError, Tree};

    let mismatch = ShmError::LayoutMismatch {
        found: 1,
        expected: 2,
    };
    prints_its_payload(&OpenError::Map(mismatch), &mismatch);

    let size = ShmError::SizeMismatch {
        actual: 4096,
        expected: 8192,
    };
    prints_its_payload(&BuildError::Shm(size), &size);

    let arena = FrozenError::Arena(ShmError::BadMagic);
    prints_its_payload(&FrozenFileError::Frozen(arena), &arena);
    let hash = FrozenError::LayoutMismatch {
        found: 1,
        expected: 2,
    };
    prints_its_payload(&FrozenFileError::Frozen(hash), &hash);

    // A descriptor that is not a memfd: `F_GET_SEALS` refuses it, so the
    // attach fails on its first syscall with an error nobody constructed.
    fn attach(fd: OwnedFd) -> Result<Tree, Box<dyn std::error::Error>> {
        Ok(Tree::attach_shared(fd, AttachMode::ReadOnly)?)
    }
    let not_a_memfd: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
    let err = attach(not_a_memfd).map(drop).unwrap_err();
    let shown = err.to_string();
    assert!(shown.ends_with("(SealQuery)"), "{shown:?}");
    assert!(shown.contains("errno "), "{shown:?}");
    assert!(!shown.contains('{'), "{shown:?} is a struct dump");
}

/// Two dynamic edges, so the edge under test is not the first one declared and
/// a producer that named "edge 1" or "edge 0" regardless could not pass.
fn two_edge_tree() -> (Arc<tf_tree::Tree>, FrameId, FrameId) {
    let cfg = EdgeCfg::new(Capacity::slots(8));
    let tree = Arc::new(
        TreeBuilder::new()
            .dynamic_edge("map", "odom", cfg)
            .dynamic_edge("odom", "base", cfg)
            .build()
            .unwrap(),
    );
    let odom = tree.frame("odom").unwrap();
    let base = tree.frame("base").unwrap();
    (tree, base, odom)
}

/// **D11 on the claim path.** `ClaimApiError::AlreadyClaimed` was a tuple
/// variant around the core `ClaimError` alone; the facade knew the edge and
/// dropped it on the `?` that converted one into the other.
///
/// **Mutant:** in `Tree::claim`, `AlreadyClaimed { edge: eid, cause }` →
/// `AlreadyClaimed { edge: EdgeId(0), cause }`. Applied: this test fails —
/// `left: EdgeId(0)`, `right: EdgeId(2)`.
#[test]
fn a_refused_claim_names_its_edge() {
    let (tree, base, odom) = two_edge_tree();
    let held = tree.claim_owned(base, odom).expect("the edge is free");
    let err = tree.claim(base, odom).map(drop).unwrap_err();
    let ClaimApiError::AlreadyClaimed { edge, cause } = err else {
        panic!("a held edge refuses a second claim, got {err:?}");
    };
    assert_eq!(edge, held.edge());
    assert!(matches!(cause, ClaimError::EdgeAlreadyClaimed { .. }));
    assert!(
        err.to_string()
            .starts_with(&format!("edge {}: ", held.edge().get())),
        "{err}"
    );
}

/// **D11 on the push path.** `PushError::NonMonotonicStamp` is the core type
/// the facade re-exports, so the edge is filled where the ring raises it, and
/// this is that edge arriving through the facade's writer.
///
/// **Mutant:** in `SampleRing::push`, `edge: self.edge` → `edge: EdgeId(0)`.
/// Applied: this test fails — `left: EdgeId(0)`, `right: EdgeId(2)`.
#[test]
fn a_backwards_stamp_names_its_edge() {
    let (tree, base, odom) = two_edge_tree();
    let writer = tree.claim_owned(base, odom).unwrap();
    writer.push(1_000, &Iso3::IDENTITY).unwrap();
    let err = writer.push(999, &Iso3::IDENTITY).unwrap_err();
    let PushError::NonMonotonicStamp { edge, last, got } = err else {
        panic!("a regressing stamp is NonMonotonicStamp, got {err:?}");
    };
    assert_eq!(edge, writer.edge());
    assert_eq!((last, got), (1_000, 999));
}
