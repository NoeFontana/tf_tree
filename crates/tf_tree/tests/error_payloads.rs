//! The facade's error types from outside the crate: every payload is nameable
//! through `tf_tree`, wrappers print their payload's `Display`, and the two
//! writer-path refusals name their edge (D11). `shm`-gated items run under
//! `just shm-check` (`docs/decisions/0059`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use tf_tree::{
    AwaitError, BuildError, Capacity, ClaimApiError, ClaimError, EdgeCfg, FrameError, FrameId,
    Iso3, PushError, ReparentError, TopologyError, TreeBuilder,
};

/// Every stable error variant's payload type is reachable from `tf_tree` and is
/// the type the variant carries (a `fn`-pointer coercion: `E0308`/`E0432`).
#[test]
fn every_public_error_payload_is_nameable_through_the_facade() {
    let _: fn(TopologyError) -> BuildError = BuildError::Topology;
    let _: fn(tf_tree::LayoutError) -> BuildError = BuildError::Layout;
    let _: fn(tf_tree::ParticipantError) -> BuildError = BuildError::Participant;
    let _: fn(FrameError) -> BuildError = BuildError::Frame;
    let _: fn(TopologyError) -> ReparentError = ReparentError::Topology;
    let _: fn(FrameError) -> AwaitError = AwaitError::Frame;
}

/// The same pin for `OpenError::Rendezvous`'s payload under `shm`: `IpcError` by
/// constructor coercion, its eight field types by typed bindings.
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

/// A wrapper prints its payload's prose, not its struct literal (`ends_with`
/// separates `{0}` from `{0:?}`).
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

/// A cycle through the real builder is `BuildError::Topology` and prints the
/// payload's `Display`.
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

/// The wrappers constructed rather than provoked (`AwaitError::Frame`,
/// `BuildError::Frame`, `ReparentError::Topology`, `BuildError::Layout`,
/// `BuildError::Participant`; `0059`) print their payload's `Display`.
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
/// need shared memory print their `Display`, and a bare `ShmError` converts into
/// `Box<dyn Error>`.
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

    // A non-memfd descriptor: `F_GET_SEALS` refuses it on the first syscall.
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

/// Two dynamic edges, so the edge under test is not the first declared.
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

/// D11 on the claim path: `ClaimApiError::AlreadyClaimed` names the edge.
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

/// D11 on the push path: `PushError::NonMonotonicStamp` names the edge through
/// the facade's writer.
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
