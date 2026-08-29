//! `tf_tree::open()` — zero-config rendezvous (`docs/PHASE2.md` §3.2).
//!
//! # The seam
//!
//! `tf_tree_ipc` knows the lock file and the socket; `tf_tree_arena` knows the
//! mapping; neither knows the other, and until this module existed nothing
//! joined them — so no process that was not a child could obtain the arena at
//! all. `docs/decisions/0005` puts the join here because [`Tree`]'s constructor
//! surface is private, and every other placement pays for the seam by widening
//! that API to a shape whose only consumer is the seam.
//!
//! # What the three outcomes owe
//!
//! - **Joined** — the owner handed over a segment fd and a participant slot.
//!   Map the fd, register into *that* slot, and hold the socket open, because
//!   its closure is how the owner learns this process is gone (D17).
//! - **Created** — this process brought the arena into existence, so it owes
//!   the service: bind the socket and answer handshakes for as long as it
//!   lives.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tf_tree_arena::AttachMode;
use tf_tree_ipc::{
    boot_id, self_start_time, AccessMode, ArenaName, EnvVar, HelloRequest, HelloStatus, IpcError,
    OpenOutcome, OwnerServer, Rendezvous, RuntimeDir, SegmentDescriptor, ShutdownHandle,
    SocketProbe, SystemEnv, DEFAULT_OPEN_TIMEOUT,
};

use crate::tree::{BuildError, Tree, TreeBuilder, MAX_BACKOFF, MIN_BACKOFF};

/// Re-exported so a caller does not have to depend on `tf_tree_ipc` directly
/// just to name a policy `open()` already takes.
pub use tf_tree_ipc::CreatePolicy;

/// A third description of the lock file, for claim leases (§6.1).
///
/// Its own description, like the liveness probe's and for the same reason:
/// `F_OFD_SETLK` conflicts are per open-file-description, so sharing one with
/// the `Session` would make this process's participant byte and its claim bytes
/// indistinguishable to itself. Separate descriptions keep each answer about
/// the thing it names.
fn open_claim_lock(rv: &Rendezvous) -> Result<std::sync::Arc<tf_tree_ipc::LockFile>, OpenError> {
    Ok(std::sync::Arc::new(
        tf_tree_ipc::LockFile::open(rv.lock_path()).map_err(OpenError::Rendezvous)?,
    ))
}

/// A kernel-authoritative liveness probe over the lock file (§5.1).
///
/// Holds its **own** open file description, deliberately. The alternative —
/// sharing the `Session`'s — would mean the probe could not see this process's
/// own byte, because `F_OFD_GETLK` reports only *conflicting* locks. That is
/// survivable (the caller guards its own slot anyway) but it makes a subtle
/// property load-bearing; a separate description makes the probe answer the
/// same way about every slot including ours.
///
/// The cost is one extra fd per attached tree, against a syscall that replaces
/// parsing `/proc` — which is an inference with a race in it, where this is the
/// kernel's own answer.
pub(crate) struct LivenessProbe {
    lock: tf_tree_ipc::LockFile,
    /// How many times [`Self::is_held`] has asked the kernel.
    ///
    /// **`#[cfg(feature = "test-hooks")]`, absent from every shipped build**,
    /// and it exists for one reason: it is the only part of
    /// [`reclamation_verdict`]'s third constraint that a test in this workspace
    /// can observe. *Which* of the two reads happened first is not visible in
    /// any sequence of stable slot states — only in an interleaving, which is
    /// `loom`'s job (0028 plan step 1) — but *whether the byte was read at all*
    /// is visible, and a predicate that decides a `FREE` word without a syscall
    /// is a predicate that reached the word first. See
    /// [`reclamation_verdict_for_test`], which renders this, and
    /// `a_free_word_is_decided_without_asking_the_kernel`, which asserts it.
    #[cfg(feature = "test-hooks")]
    probes: std::sync::atomic::AtomicU32,
}

impl LivenessProbe {
    /// Wrap a description this caller already opened.
    ///
    /// One constructor rather than two struct literals, so the `test-hooks`
    /// field above is initialised in one place instead of behind a `#[cfg]` at
    /// every construction site.
    fn from_lock(lock: tf_tree_ipc::LockFile) -> LivenessProbe {
        LivenessProbe {
            lock,
            #[cfg(feature = "test-hooks")]
            probes: std::sync::atomic::AtomicU32::new(0),
        }
    }

    /// Open a second description of the rendezvous lock file.
    fn open(rv: &Rendezvous) -> Result<LivenessProbe, IpcError> {
        Ok(LivenessProbe::from_lock(tf_tree_ipc::LockFile::open(
            rv.lock_path(),
        )?))
    }

    /// Whether `slot`'s byte is held, or `None` if the kernel could not say.
    ///
    /// `None` rather than a guess: §6.2 requires this to fail safe, and the
    /// caller turns "cannot tell" back into the `/proc` inference rather than
    /// into a "dead" verdict that would steal a working process's claim.
    /// The description this probe holds, for the callers that need a lock other
    /// than the participant bytes — the hangup callback reaps *claim* leases
    /// through it.
    pub(crate) fn lock(&self) -> &tf_tree_ipc::LockFile {
        &self.lock
    }

    pub(crate) fn is_held(&self, slot: u32) -> Option<bool> {
        // Counted before the syscall, not after: what a test asks is whether
        // the kernel was *reached*, and an `Err` from the probe is still a
        // read of the byte.
        #[cfg(feature = "test-hooks")]
        self.probes.fetch_add(1, Ordering::Relaxed);
        self.lock.probe_participant(slot).ok().map(|p| p.held)
    }

    /// How many times this probe has asked the kernel. Test scaffolding; see
    /// the field.
    #[cfg(feature = "test-hooks")]
    fn probe_count(&self) -> u32 {
        self.probes.load(Ordering::Relaxed)
    }
}

/// What a reclamation sweep may do with one participant slot.
///
/// `docs/decisions/0028-the-slot-a-killed-participant-keeps.md`, the Decision's
/// piece 2. Three answers, and exactly one of them is destructive.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Reclamation {
    /// The record may be collected, and `observed` is the `state` word this
    /// verdict was formed against.
    ///
    /// **The word is carried rather than left to the caller to re-read.**
    /// `ParticipantTable::reclaim` (0028 piece 1) is a
    /// `compare_exchange(observed, FREE, ..)` against *the word the caller
    /// observed*, and by [`reclamation_verdict`]'s third constraint that has to
    /// be the word observed **before** the byte was probed. A caller that
    /// re-loaded `state` to build the CAS guard would hold a word read *after*
    /// the probe, which is the failing order — so the word travels with the
    /// verdict and there is nothing to re-read.
    Reclaimable { observed: u32 },
    /// Somebody is running here, or the slot is this process's own. Not
    /// collectable.
    Live,
    /// No verdict, so **not collectable** — `docs/PHASE2.md` §6.2's fail-safe
    /// direction. Either the kernel declined to answer, or the slot holds no
    /// record to collect at all (a `FREE` word).
    Unknown,
}

/// Whether `slot`'s participant record may be reclaimed — from the lock byte,
/// and from nothing else.
///
/// `docs/decisions/0028-the-slot-a-killed-participant-keeps.md` piece 2, plan
/// step 2. **One predicate, named once:** every reclamation decision goes
/// through this function, and a second copy of it is the defect that record was
/// opened about, re-created. `docs/PHASE2.md` §5.1 is NORMATIVE and this is its
/// sentence in code — *"whether it is live is a kernel fact"* — so consulting
/// the byte needs no amendment to it, because consulting the byte is what it
/// prescribes.
///
/// # Scope: a tree that carries a probe, and the **two** steps that buy it
///
/// A [`LivenessProbe`] is installed by [`Open::attempt`] and by nothing else,
/// so this predicate only ever runs against a tree obtained from the
/// rendezvous. That is the scope; what makes the byte a *sound* answer inside
/// it is not one earlier step but two. **0028's open question 6 resolved
/// conditionally, and the condition is the whole answer:** *"it holds only with
/// step 0b (no byte-less writer) **and** step 0c (the correspondence
/// asserted)"*. They buy different halves, neither buys the other's, and both
/// are on `main`.
///
/// **Step 0b buys *every participant that joined through the rendezvous holds a
/// byte*.** [`Tree::attach_shared`] and [`Tree::attach_shared_at`] refuse
/// [`AttachMode::ReadWrite`] (`refuse_a_byteless_writer`), so the byte-less
/// writer they used to produce has no producer left.
///
/// **It does not buy *every participant*, and the difference is a live defect.**
/// `TreeBuilder::build_shared` called directly still registers without a byte.
/// `0028` concluded that this was harmless because such a tree has "no lock file,
/// therefore no probe, and never reaches here" — **and that is wrong, because the
/// probe belongs to the *observer*, not to the subject**. This function is handed
/// `rec`, a record in a shared arena; whether *that* participant has a byte is
/// unrelated to whether the caller has a probe. A `build_shared` creator whose
/// arena is served through `tf_tree_ipc::OwnerServer` is read `Reclaimable` by
/// every peer that joined normally, and [`Tree::reap_participants`] frees its
/// record while it is publishing —
/// `a_byteless_creators_record_reads_dead_and_is_reaped_while_it_publishes`
/// (`crates/tf_tree/tests/rendezvous.rs`) pins it. The predicate is therefore
/// **total only over participants that joined through the rendezvous**, which is
/// a property of the arena's population and not of this caller.
/// `docs/decisions/0031-the-participant-record-with-no-byte.md` is where that is
/// being decided; nothing here changes until it is.
///
/// **Step 0c buys *the byte at index `slot` is the byte of the record at index
/// `slot`***, and nothing else does. The two indices are chosen by code that
/// cannot see the other: the byte by `tf_tree_ipc`, which has no arena
/// dependency, and the record by the first `FREE` slot of a fresh arena. On every ordinary path they agree by construction; under
/// `CreatePolicy::Always`, which skips §3.4 step 4's guard by design, they
/// diverged — *measured*, in
/// `defect_201_release_ownership_strands_a_live_non_owner_on_byte_0`. `0035`
/// has since put the create path on `register_creator`, which takes byte 0
/// atomically, and issue #201 closed the **takeover** arm by deleting it
/// ([`0037`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0037-a-takeover-is-not-a-second-open.md)).
/// What this still buys is
/// hand-rolled `tf_tree_ipc::Open` plus `TreeBuilder::build_shared`
/// construction, where the two numbers are chosen by two callers and nothing
/// pairs them — the route the [`OpenError::ParticipantSlotDiverged`] doc names
/// as reachable. **Do not delete 0c on the strength of the takeover arm being
/// fixed.** This
/// function reads one at the index of the other, so without 0c's assertion
/// every verdict below is about a different process than the one it names, and
/// a `Reclaimable` verdict then frees a live participant's record. The
/// assertion is in [`Open::attempt`]'s `Created` arm, before the owner server
/// is spawned.
///
/// # The three constraints, each of which an earlier revision got wrong
///
/// 1. **It is not built on `record_is_alive`.** That function opens with
///    `state_of(..) != LIVE { return false }`, so composing it would decide
///    liveness from `state` — the thing §5.1 calls a bug — and 0028's open
///    question 1 took the `/proc` half off this path outright: `/proc` maps an
///    unmounted or `hidepid`-hidden entry to `ENOENT`, which `read_start_time`
///    classifies as `NoSuchProcess`, the *proof of death* branch. A second
///    conjunct that inverts to "everyone is dead" on a hardened host is worse
///    than no second conjunct, so there is not one.
/// 2. **This process's own slot is skipped, unconditionally, and first.**
///    `F_OFD_GETLK` answers about *conflicting* locks, so an open file
///    description does not see its own — the rule `Tree::use_ofd_liveness` and
///    `reap_inner` already carry, for claims and for records alike. This
///    probe's separate description happens to make our own byte visible again,
///    but a sweep that leant on that detail would be one refactor away from
///    reclaiming its own live slot.
/// 3. **The state word is observed *before* the byte is probed**, and that is
///    not a stylistic preference. Under word-then-byte the `Acquire` load of a
///    `live_word` **synchronises-with** the publishing `Release` store in
///    `fill_slot`, so a byte probe sequenced after it must see the byte held.
///    Reverse the two reads — or take one up-front holder mask, which
///    `LockFile::held_participants` makes the natural way to write a sweep —
///    and `loom` erases a published record in 0.00 s: the probe sees byte *s*
///    free before the registrant takes it, then the load observes
///    `live_word(1)`, and the CAS frees a live byte-holder's record. Two
///    independently built models, separately (0028 open question 6).
///    `Tree::participant_alive` already has this order, by `&&`'s
///    short-circuit; this is the one place that *states* it.
///
///    **What pins it here, and what does not.** No sequence of *stable* slot
///    states tells the two orders apart: on a `FREE` word both return
///    [`Reclamation::Unknown`] and on a `LIVE` word both consult the byte, so
///    the disagreement lives only in an interleaving — and the interleaving is
///    between two adjacent statements, which no multiprocess test in this
///    workspace can schedule a registrant into. What *is* observable without a
///    race is whether the byte was read **at all**, and only a predicate that
///    reached the word first can decide a `FREE` slot without a syscall: that
///    is `a_free_word_is_decided_without_asking_the_kernel`
///    (`crates/tf_tree/tests/rendezvous.rs`), which the reversal fails.
///    The interleaving itself is `reclaim_races_register`, the `loom` case 0028
///    plan step 1 owes in `crates/tf_tree_core/src/loom_tests.rs`, shipping with
///    the reversed control that erases a published record in 0.00 s. That case
///    is also the **only** thing that pins this constraint's other half — that
///    `observed` is not re-read after the probe, which is why
///    [`Reclamation::Reclaimable`] carries the word instead of leaving the
///    caller to fetch one. A reload yields the same word in every state a test
///    can stage, so nothing in this workspace fails when one is introduced.
///    **Step 1 has landed** (`ParticipantTable::reclaim`, and
///    `reclaim_races_register` beside it), so that half is modelled rather than
///    merely argued — but it is modelled *there* and not tested *here*, and the
///    two are not the same claim: `loom` reasons about the C11 ordering, while
///    nothing in this crate's tests fails when a reload is introduced.
///
/// # The `FREE` word is a live participant, more often than not
///
/// A `FREE` word is reported [`Reclamation::Unknown`] rather than reclaimable,
/// and that branch is **neither dead nor a formality**. It is the ordinary
/// state of a **live read-only joiner**: the rendezvous takes its lock byte in
/// `register_at` during the handshake, and then `attach_joined_at` registers no
/// arena record at all, because the table is in the arena and a `PROT_READ`
/// mapping cannot be written. Read-only is the consumer default (D18) *and* the
/// Python default, so on a real system this is the common slot shape, not a
/// corner — [`Open::attempt`]'s own `assign` closure already has to special-case
/// it for the same reason. `spawn_owner_server`'s comment says it in the
/// negative: *"the table alone reports its slot empty"*.
///
/// So the two wrong answers here are wrong about a running process. Reporting
/// [`Reclamation::Live`] instead — what deleting the branch produces, since the
/// byte is held — is merely imprecise. Reporting [`Reclamation::Reclaimable`]
/// is the corrupting direction this whole record exists to prevent: `reclaim`
/// would CAS `FREE -> FREE`, **succeed**, and report a slot collected that a
/// live joiner is sitting in, which the sweeps of steps 3-5 then hand to
/// somebody else while its byte is still held.
/// `a_live_read_only_joiner_is_unknown_not_reclaimable`
/// (`crates/tf_tree/tests/rendezvous.rs`) stages exactly that participant, and
/// fails for both mutations.
///
/// The rule the branch encodes is narrow: the word answers *is there a record
/// here*, never *is its process alive*. This function does not invent a
/// liveness answer about a slot that holds no record, in either direction.
pub(crate) fn reclamation_verdict(
    probe: &LivenessProbe,
    own_slot: u32,
    slot: u32,
    rec: &tf_tree_core::ParticipantRecord,
) -> Reclamation {
    // Constraint 2, before anything else and with nothing in front of it.
    if slot == own_slot {
        return Reclamation::Live;
    }
    // Constraint 3: the word first. Everything below is sequenced after this
    // load, which is the whole of the argument above.
    let observed = rec.state.load(Ordering::Acquire);
    if tf_tree_core::participant::state_of(observed) == tf_tree_core::participant::FREE {
        return Reclamation::Unknown;
    }
    // Constraint 1: the kernel, and only the kernel. `None` is "would not say",
    // which is not "dead" (§6.2).
    match probe.is_held(slot) {
        Some(true) => Reclamation::Live,
        Some(false) => Reclamation::Reclaimable { observed },
        None => Reclamation::Unknown,
    }
}

/// [`reclamation_verdict`] for `slot`, rendered as one line.
///
/// **Test scaffolding, and present only under `--features test-hooks`.** The
/// predicate is private, and the seam does not become redundant now that the
/// predicate has production callers. There are two, and they are the two the
/// `0028` plan named — `spawn_owner_server`'s slot assigner (plan step 3) and
/// [`Tree::reap_participants`] (plan step 5) — and *both act on a verdict
/// without ever reporting one*. The assigner returns at the **first** grantable
/// slot, so it says nothing at all about the rest of the table; the sweep walks
/// every slot but reports a count, and the three answers this renders are
/// exactly what a count cannot separate — a slot left alone because the byte
/// was held reads the same as one left alone because the kernel would not say,
/// and both read the same as a slot with no record in it. So without this seam
/// the tests step 2 owes could still not name a slot and read back what the
/// predicate says about it, and the same argument that put
/// [`crate::CLAIM_WINDOW_HOOK`] behind this feature applies: a window a test
/// cannot otherwise stand in. (`Open` briefly had a `#[cfg(test)]` takeover
/// seam that rejected a `pub` one for a reason that does not reach here — what
/// it would have published was a route `Open` withholds on purpose, where this
/// publishes a read-only verdict about a slot. That seam went with the builder
/// it drove; `docs/decisions/0037`.)
///
/// `own_slot` is a **parameter rather than `tree.participant_slot()`**, and
/// that is the point of it: with the tree's real slot passed, the own-slot
/// guard is unobservable, because this probe's separate open file description
/// reports our own byte as held and the byte answer agrees with the guard. A
/// test that points `own_slot` at a slot whose byte reads *free* is the only
/// way to see the guard rather than the byte, and deleting the guard has to
/// fail something.
///
/// # The rendered line, and why it carries a syscall count
///
/// `reclaimable word 0x… probes=N`, `live probes=N`, `unknown probes=N`, plus
/// `no-lock-file` and `no-such-slot` for a caller that named neither — those
/// two carry no count because they never build a probe.
///
/// `N` is [`LivenessProbe::probe_count`], and it is here because it is the only
/// part of the predicate's **program order** a test can see. The verdict alone
/// is the same under both read orders for every slot state a test can stage
/// (see the third constraint on [`reclamation_verdict`]); `N` is not. It makes
/// three separate statements assertable:
///
/// - `probes=0` on a `FREE` word — the word was reached first and short-circuited.
/// - `probes=1` on a `LIVE` word — the kernel *was* asked, so the verdict is
///   not being read out of `state` (§5.1's bug).
/// - `probes=0` on our own slot — the guard answered with nothing in front of it.
#[cfg(feature = "test-hooks")]
#[doc(hidden)]
#[must_use]
pub fn reclamation_verdict_for_test(
    tree: &Tree,
    lock_path: &std::path::Path,
    own_slot: u32,
    slot: u32,
) -> String {
    let Ok(lock) = tf_tree_ipc::LockFile::open(lock_path) else {
        return "no-lock-file".to_string();
    };
    let probe = LivenessProbe::from_lock(lock);
    let view = tree.view();
    let Some(rec) = view.participants().get(slot) else {
        return "no-such-slot".to_string();
    };
    let verdict = match reclamation_verdict(&probe, own_slot, slot, rec) {
        Reclamation::Reclaimable { observed } => format!("reclaimable word {observed:#x}"),
        Reclamation::Live => "live".to_string(),
        Reclamation::Unknown => "unknown".to_string(),
    };
    // After the call, so it counts this verdict's syscalls and no others: the
    // probe is built one line above and dropped one line below.
    format!("{verdict} probes={}", probe.probe_count())
}

/// The rendezvous session a `Tree` from [`Open::open`] holds.
pub(crate) type JoinedSession = tf_tree_ipc::Session<tf_tree_ipc::Attached>;

/// What keeps a rendezvous-obtained [`Tree`] attached.
///
/// Held purely for its `Drop`: the session releases the participant lock byte,
/// the socket's closure tells the owner this process is gone (D17), and the
/// owner variant additionally stops the serving thread. Naming the fields with
/// a leading underscore is deliberate — nothing reads them, and the point is
/// that dropping them is the observable effect.
pub(crate) enum Attachment {
    /// This process joined somebody else's arena.
    ///
    /// **`rendezvous` is not held for `Drop`, unlike its two siblings**, and is
    /// what [`0037`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0037-a-takeover-is-not-a-second-open.md)
    /// question 1 found missing: a survivor inheriting the owner role has to
    /// bind *this* rendezvous, and nothing in scope at the old takeover arm
    /// named it. `session` and `socket` lose their underscores for the same
    /// reason — the session takes byte 0 and the socket is how this process
    /// learns the owner died (`Tree::owner_lost`).
    Joined {
        session: JoinedSession,
        socket: std::os::fd::OwnedFd,
        rendezvous: Rendezvous,
    },
    /// This process owns the arena and serves it.
    ///
    /// **`_server` is declared before `_session`, and the order is the
    /// correctness property — do not reorder these two fields.** Rust drops an
    /// enum variant's fields in declaration order (RFC 1857), and
    /// [`OwnerThread`]'s own `Drop` stops the serving thread, so declaring it
    /// first means serving stops *before* the session releases the ownership
    /// byte. The reverse order leaves a window in which a successor takes
    /// ownership and binds while this process is still answering handshakes from
    /// the old socket — two servers on one path, with clients split between
    /// them.
    ///
    /// This used to be an explicit `impl Drop for Attachment` calling
    /// `server.stop()`. That impl made the whole enum un-destructurable, which
    /// is what `Tree::inherit_ownership` needs to do to hand a surviving session
    /// its new role — and it was redundant besides, since `OwnerThread::drop`
    /// already routes through the same `stop()`. The guarantee is the same one,
    /// expressed structurally instead of imperatively.
    Owner {
        _server: OwnerThread,
        _session: JoinedSession,
    },
}

/// Every `docs/PHASE2.md` §11.3 crash point compiled into **this crate**.
///
/// The companion to [`tf_tree_core::crash::SITES`], which is scoped to that
/// crate for the same reason: §11.3's table spans three crates, and a harness
/// that arms a site at random (§11.4) must read the literals from wherever they
/// are placed rather than re-spelling them, because a typo silently arms nothing
/// and the run then looks clean.
///
/// `attach.after_slot_assigned_before_publish` is in
/// [`tf_tree_core::crash::SITES`] instead: the window it names is inside
/// `participant::fill_slot`, and the arena record is that crate's even though
/// `tf_tree_ipc` takes the byte. `reclaim.probe_then_reoccupied` is argued at
/// its row not to be an abort site at all — it names an *interleaving* between two live processes, and a
/// mechanism that kills one of them cannot produce it.
#[cfg(feature = "crash-points")]
pub const CRASH_SITES: &[&str] = &[
    "takeover.after_ownership_lock_before_bind",
    "topo.holding_lock",
    "open.after_ownership_lock_before_bind",
    "open.after_create_before_bind",
    "reclaim.after_probe_before_cas",
    "hangup.after_probe_before_cas",
];

/// How [`Tree::inherit_ownership`] resolved (§3.5).
///
/// Anything but [`Inheritance::Inherited`] means this process is **not** the
/// owner and should keep behaving as a plain participant. It is never a reason
/// to stop reading: lookups are unaffected by ownership in every one of these
/// states.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Inheritance {
    /// This process is now the owner and is serving the rendezvous.
    Inherited,
    /// The owner is alive. Nothing was attempted.
    OwnerAlive,
    /// Another survivor won the ownership byte and is binding.
    ///
    /// Not an error, and **this process kept its slot** — which is the property
    /// the deleted takeover arm could not provide, because it went looking for a
    /// byte instead of keeping the one it held
    /// ([`0037`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0037-a-takeover-is-not-a-second-open.md)).
    /// **This is a transient, and it took
    /// [`0043`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0043-owner-lost-is-a-question-about-the-owner.md)
    /// to make it one.** The loser keeps its slot, its byte and its mapping, so
    /// it keeps reading with nothing lost — but its attach socket still points
    /// at the dead owner and stays hung up for the life of the process, and
    /// while `Tree::owner_lost` read only that socket it answered `true`
    /// forever, so §3.5's recommended loop re-attempted the ownership lock every
    /// control cycle to be told each time that somebody else had it. `owner_lost`
    /// now asks the kernel whether byte 0 is held before answering, so a loser
    /// stops calling on its own — **and starts again by itself if the new owner
    /// dies too**, which is why latching a flag was never the fix.
    ///
    /// §3.5's `retry connect with backoff` is deliberately still not
    /// implemented: it would need a new wire message, because §3.5 requirement 2
    /// forbids a survivor from registering a second time. What it would buy over
    /// the probe is prompt reclamation of *this* process's slot by the new
    /// owner's hangup callback, since that owner never learns this process
    /// exists; the byte-keyed collectors — the slot assigner and
    /// `Tree::reap_participants` — still reclaim it, a grant or a sweep later.
    /// `0043` records that residue.
    Contended,
    /// This tree is a read-only attachment, so it cannot serve.
    ///
    /// An owner writes the participant table on every grant, and a `PROT_READ`
    /// mapping cannot — which is D18 working, not failing. A read-only consumer
    /// keeps reading from the arena it has; some read-write participant must be
    /// the heir.
    ReadOnly,
    /// This tree has no owner to inherit from: a heap tree, a frozen `.tft`, an
    /// `attach_shared` over an inherited fd, or a tree this process already owns.
    NotApplicable,
}

impl Tree {
    /// Inherit the owner role from a departed owner, and begin serving (§3.5).
    ///
    /// **Ownership is a role, not a property of the arena.** The arena is the
    /// memfd and lives as long as any mapping does; the owner is merely whichever
    /// participant holds byte 0 and the listening socket. When the owner dies the
    /// kernel releases byte 0, and any surviving read-write participant may take
    /// it — the heir is chosen by an uncontended `F_OFD_SETLK` with no message
    /// exchanged and no quorum, which is why this is inheritance rather than the
    /// election D16 rejects.
    ///
    /// # What this closes
    ///
    /// Until this existed, owner death was **terminal for new joiners**. Lookups
    /// kept working for every already-attached process exactly as §3.5 promises,
    /// but a new process could not join at all: it won the ownership byte, met
    /// §3.4's split-brain check against the survivors' held participant bytes,
    /// backed off, and timed out with `ArenaHeldButUnreachable` for as long as
    /// any survivor lived. That is the shape a supervised robot has every time it
    /// restarts one node, and `docs/PHASE2.md` §0.0 recorded it, measured with
    /// `shm_torture`.
    ///
    /// # The trigger is the caller's, on purpose
    ///
    /// This does nothing until somebody calls it. Pair it with
    /// [`Tree::owner_lost`], which is a non-blocking `poll` of the attach
    /// socket:
    ///
    /// ```ignore
    /// if tree.owner_lost() {
    ///     let _ = tree.inherit_ownership()?;   // Contended is fine: somebody won
    /// }
    /// ```
    ///
    /// There is no background thread and no daemon, because
    /// [`0019`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0019-one-binary-and-topology-you-can-wait-for.md)
    /// holds that every process a user is *required* to run is a place adoption
    /// dies — and a thread per attachment is the library-shaped version of the
    /// same cost. A survivor that never calls this simply never becomes owner,
    /// and the arena stays ownerless exactly as it does today.
    ///
    /// # Lookups do not pause
    ///
    /// Not during the poll, not during the lock, not during the bind. `Plan::at`
    /// touches the mapping and nothing else; ownership lives entirely in the
    /// control plane. That is the answer to the first question any integrator
    /// asks about owner death, and it is unchanged by this method existing.
    ///
    /// # Errors
    ///
    /// [`OpenError::Rendezvous`] if the `fcntl` fails, or whatever binding the
    /// rendezvous socket fails with. **On every error path this process keeps its
    /// participant slot, its byte and its mapping**, and gives back the ownership
    /// byte if it had taken one — so a failed inheritance leaves a plain
    /// participant and lets another survivor try, rather than an arena with an
    /// owner that is not serving.
    ///
    /// # It takes `&self`, and that is what makes it reachable at all
    ///
    /// It took `&mut self` until 2026-08-29, which cost two things
    /// ([`0044`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0044-recovery-the-languages-a-robot-is-written-in-cannot-reach.md)).
    /// `docs/PHASE2.md` §3.5 had to carry a caller-side qualification — the
    /// inheriting handle's own `Guard<'_>` could not be outstanding across the
    /// call, so a control loop had to arrange recovery *between* cycles. And
    /// **both bindings could not call it at all**: `tft_tree` holds an
    /// `Arc<TreeShare>` and `PyTree` the `Arc<Tree>` that `Tree::claim_owned`
    /// requires, and `Arc::get_mut` fails the moment any plan or publisher holds
    /// a clone — which, in a binding, is always. So §3.5's recovery was
    /// unreachable from the languages a robot's nodes are written in.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    pub fn inherit_ownership(&self) -> Result<Inheritance, OpenError> {
        if !self.is_joined() {
            return Ok(Inheritance::NotApplicable);
        }
        if !self.owner_lost() {
            return Ok(Inheritance::OwnerAlive);
        }
        if !self.is_writable() {
            return Ok(Inheritance::ReadOnly);
        }
        // Taken out so `spawn_owner_server` can borrow `self` immutably below.
        // **Every exit from here on puts it back.** Dropping it would release
        // this process's participant byte and close the socket, which is the
        // one thing worse than failing to inherit.
        let (mut session, socket, rendezvous) = match self.take_attachment() {
            Some(Attachment::Joined {
                session,
                socket,
                rendezvous,
            }) => (session, socket, rendezvous),
            other => {
                self.put_attachment(other);
                return Ok(Inheritance::NotApplicable);
            }
        };

        // Taken on the description that already holds our participant byte, so
        // the slot cannot move and nothing needs verifying — `0037`'s whole
        // argument for this being a method rather than a second `open()`.
        let acquired = match session.take_over_ownership() {
            Ok(v) => v,
            Err(e) => {
                self.put_attachment(Some(Attachment::Joined {
                    session,
                    socket,
                    rendezvous,
                }));
                return Err(OpenError::Rendezvous(e));
            }
        };
        if !acquired {
            self.put_attachment(Some(Attachment::Joined {
                session,
                socket,
                rendezvous,
            }));
            return Ok(Inheritance::Contended);
        }

        // `docs/PHASE2.md` §11.3: **`takeover.after_ownership_lock_before_bind`**.
        // The row's repair claim is "ownership released; another participant
        // takes over; joiners retry", and this is the instruction it names — byte
        // 0 held, nothing listening. A process killed here has its byte released
        // by the kernel with no cooperation, which is what makes that claim true
        // rather than hopeful, and `a_killed_heir_leaves_the_role_for_the_next_survivor`
        // is what executes it.
        #[cfg(feature = "crash-points")]
        tf_tree_core::crash::maybe_abort(CRASH_SITES[0]);

        match spawn_owner_server(&rendezvous, self) {
            Ok(server) => {
                // The old socket goes with the old owner. It is already hung up —
                // that is what brought us here — and holding it would keep a
                // descriptor open on a connection to a dead process.
                drop(socket);
                self.put_attachment(Some(Attachment::Owner {
                    _server: server,
                    _session: session,
                }));
                Ok(Inheritance::Inherited)
            }
            Err(e) => {
                // Hand the role back rather than hold a byte we cannot serve
                // behind. A survivor that owns byte 0 and is not listening is
                // exactly the state that makes an arena unjoinable, which is the
                // failure this method exists to end.
                let _ = session.release_ownership();
                self.put_attachment(Some(Attachment::Joined {
                    session,
                    socket,
                    rendezvous,
                }));
                Err(e)
            }
        }
    }
}

/// The arena and lock-file participant tables index the same slot space, and
/// nothing but this crate can see both constants to check it.
///
/// A disagreement would not fail loudly: the owner would grant a slot the arena
/// has no record for, or a reaper would walk past records no byte covers.
const _: () = assert!(
    tf_tree_ipc::MAX_PARTICIPANTS == tf_tree_arena::DEFAULT_MAX_PARTICIPANTS,
    "the lock file and the arena must agree on the participant slot space"
);

/// Why [`Open::open`] could not produce a [`Tree`].
///
/// `Copy` and `String`-free like every other error here, and the single place
/// the three previously-unrelated families meet: the rendezvous
/// ([`IpcError`]), the mapping ([`tf_tree_arena::ShmError`]) and construction
/// ([`BuildError`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum OpenError {
    /// The rendezvous failed — no runtime directory, no arena, a stuck peer,
    /// or an owner that refused this build.
    #[error("{0}")]
    Rendezvous(IpcError),
    /// The segment was handed over but could not be mapped.
    #[error("{0:?}")]
    Map(tf_tree_arena::ShmError),
    /// This process had to create the arena and could not.
    #[error("{0}")]
    Build(BuildError),
    /// `create` was not `Never`, but no layout was supplied, so there is
    /// nothing to create the arena *from*.
    ///
    /// Decision `0004` sizes an arena from its declared edges, so a creator must
    /// bring a [`TreeBuilder`]. A consumer that only ever joins should say so
    /// with `create = Never` and get [`IpcError::ArenaAbsent`] instead of this.
    #[error("no layout was supplied and the arena had to be created")]
    NoLayoutToCreate,
    /// [`AttachMode::ReadOnly`] was combined with a `create` policy other than
    /// [`CreatePolicy::Never`] (`docs/decisions/0019` §2a).
    ///
    /// A read-only creator is incoherent on its face: it asks to bring an arena
    /// into existence that it then cannot write, which is by definition the
    /// empty arena a consumer publishes nothing into while looking healthy.
    /// Refusing the combination removes that class by construction rather than
    /// by advice — which is `docs/API.md` R6 carried one step further, from
    /// *read-only is the default* to *read-only is the thing that cannot
    /// create*.
    ///
    /// Reported **before** the runtime directory is resolved, so a
    /// misconfiguration reports as itself rather than as whatever the machine
    /// happens to be missing.
    #[error("a read-only attach cannot create an arena: use CreatePolicy::Never, or AttachMode::ReadWrite")]
    ReadOnlyCannotCreate,
    /// [`Open::require_create`] was set and the rendezvous resolved to
    /// [`OpenOutcome::Joined`] — somebody else's arena is already live.
    ///
    /// The refusal `docs/decisions/0019` §3 question 1 settles for the ROS
    /// bridge, and `0015` depends on it: [`CreatePolicy`] has no
    /// "create, or refuse if one is already live" variant, so a second bridge
    /// would take the *join* path and start claiming edges in an arena it did
    /// not size. The session is dropped before this is returned, so the
    /// participant lock byte is released and the socket closed — a refused
    /// attach leaves nothing behind.
    #[error("an arena is already live at this rendezvous and require_create was set")]
    ArenaAlreadyLive,
    /// This process's participant **lock byte** and its arena participant
    /// **record** came out at different indices, so the arena was not published
    /// (`docs/decisions/0028` plan step 0c, issue #201).
    ///
    /// [`Tree::participant_slot`] calls that integer "the one number that
    /// indexes both tables", and `docs/PHASE2.md` §5.1's liveness predicate
    /// spends it that way: it asks the kernel about the byte at slot *i* and
    /// then reads the arena record at slot *i*. When the two disagree every
    /// answer is about somebody else — reproduced through published API as
    /// `participant_alive(0) == false` about a process that holds record 0 and
    /// is still pushing samples.
    ///
    /// **No create path can reach it, and that is recent.** Until `0035` a
    /// creator scanned for the first free byte, so a byte 0 that changed hands
    /// mid-scan left it holding byte *n* against arena record 0 — and
    /// [`CreatePolicy::Always`], which skips §3.4 step 4's guard by design, was
    /// the policy that met that state most often. A creator now takes byte 0
    /// with a single `F_OFD_SETLK`, so the acquire *is* the check and the
    /// divergence is unrepresentable there; a contended byte 0 is refused with
    /// [`IpcError::ArenaHeldButUnreachable`] instead. This doc used to say
    /// "only `CreatePolicy::Always` can reach it" — true when it was written,
    /// and no longer.
    ///
    /// What remains reachable is hand-rolled
    /// `tf_tree_ipc::Open` plus `TreeBuilder::build_shared` construction, which
    /// registers a record with no byte to pair it against. Whether the §3.5 heir
    /// should reuse its slot is **`0028` question 3** — RESOLVED 2026-08-20, the
    /// heir keeps its existing slot, byte and arena — which is why `0035` left
    /// this guard standing rather than deleting it. This comment cited `0029`
    /// question 3 and called it open; that record's question 3 was about the
    /// socket-hangup callback, and `0029` no longer has open questions at all.
    ///
    /// # What a caller does about it
    ///
    /// Not retry through the takeover arm — there is none; issue #201 closed it
    /// by deletion, and `docs/decisions/0037` records why a takeover cannot be an
    /// `Open::open` call at all. Either stop the process still holding the byte
    /// (`tf_tree
    /// participants` names it from the lock file's identity records), or open
    /// with [`CreatePolicy::IfAbsent`] and let the wedge be diagnosed rather
    /// than created over.
    ///
    /// # Why an error and not an assertion
    ///
    /// The engine's liveness bias, stated on `record_is_alive`: a false
    /// "dead" lets a rescuer take an entry from a running process, which is
    /// corruption, while a false "alive" only delays recovery. A diverged
    /// pairing manufactures the corrupting direction, and `0028`'s plan step 1
    /// is the first code that acts on such a verdict destructively. A
    /// `debug_assert!` would compile out of exactly the builds that ship, so it
    /// would be a comment with a test harness attached.
    ///
    /// The refusal leaves nothing behind: the arena is dropped before the owner
    /// server binds, so no peer ever saw it, and the session is dropped with it
    /// so both the ownership and participant bytes are released. Neither index
    /// is carried in this value — `docs/API.md` R5 keeps errors `Copy` and
    /// prose in a separate layer, and the two numbers are readable from the
    /// lock file by anything entitled to see them.
    #[error("this process's participant lock byte and its arena participant record are different slots; the arena was not published")]
    ParticipantSlotDiverged,
}

impl From<IpcError> for OpenError {
    fn from(e: IpcError) -> OpenError {
        OpenError::Rendezvous(e)
    }
}
impl From<tf_tree_arena::ShmError> for OpenError {
    fn from(e: tf_tree_arena::ShmError) -> OpenError {
        OpenError::Map(e)
    }
}
impl From<BuildError> for OpenError {
    fn from(e: BuildError) -> OpenError {
        OpenError::Build(e)
    }
}

/// Join the running arena, read-only.
///
/// Zero configuration: domain and name come from `$TF_TREE_DOMAIN` (else
/// `$ROS_DOMAIN_ID`, else 0) and `$TF_TREE_NAME` (else `default`), and the
/// runtime directory from `$TF_TREE_RUNTIME_DIR`, `$XDG_RUNTIME_DIR`, `/run`, or
/// `/tmp` in that order.
///
/// **This never creates anything.** [`Open::new`]'s defaults are the *consumer*
/// (`docs/decisions/0019` §2a), so a process that means to bring the arena into
/// existence says so with [`Open::create`] and [`Open::layout_if_creating`].
///
/// # Errors
///
/// See [`OpenError`]. On a machine where nothing is serving this is
/// [`IpcError::ArenaAbsent`], fast — see [`Open::await_open`] for the consumer
/// that would rather wait for the publisher to start.
pub fn open() -> Result<Tree, OpenError> {
    Open::new().open()
}

/// The `docs/PHASE2.md` §3.2 builder.
pub struct Open {
    domain: Option<u32>,
    name: Option<ArenaName>,
    mode: AttachMode,
    create: CreatePolicy,
    timeout: Duration,
    layout: Option<TreeBuilder>,
    require_create: bool,
}

impl Default for Open {
    fn default() -> Open {
        Open::new()
    }
}

impl Open {
    /// Defaults: read-only, **never create**, the §3.4 timeout, env discovery.
    ///
    /// **`ReadOnly` is deliberate and differs from the Rust in-process default**
    /// (D18). A `PROT_READ` mapping makes a buggy consumer *incapable* of
    /// corrupting a robot's transform tree, enforced by the MMU rather than by
    /// convention, and most processes that open a tree are consumers.
    ///
    /// # Why `create` is [`CreatePolicy::Never`]
    ///
    /// It used to be [`CreatePolicy::IfAbsent`], which paired with the
    /// `ReadOnly` above into a configuration that asks to create an arena it
    /// cannot write — the one [`OpenError::ReadOnlyCannotCreate`] now rejects.
    /// A builder whose own documented defaults are an error is a defect on its
    /// own terms, so the defaults moved instead of the rule
    /// (`docs/decisions/0019` §2a and its plan's step 1, which supersedes
    /// `docs/decisions/0005` §3.2 and `docs/PHASE2.md` §3.2 on this one point).
    ///
    /// The defaults are therefore the *consumer*, and the error is reachable
    /// only by writing both halves out explicitly. A creator names both:
    /// [`Open::mode`] with [`AttachMode::ReadWrite`], [`Open::create`], and the
    /// [`TreeBuilder`] that decision `0004` sizes the arena from.
    #[must_use]
    pub fn new() -> Open {
        Open {
            domain: None,
            name: None,
            mode: AttachMode::ReadOnly,
            create: CreatePolicy::Never,
            timeout: DEFAULT_OPEN_TIMEOUT,
            layout: None,
            require_create: false,
        }
    }

    /// Override the domain (default: `$TF_TREE_DOMAIN`, `$ROS_DOMAIN_ID`, 0).
    #[must_use]
    pub fn domain(mut self, domain: u32) -> Open {
        self.domain = Some(domain);
        self
    }

    /// Override the arena name (default: `$TF_TREE_NAME`, else `default`).
    ///
    /// # Errors
    ///
    /// [`IpcError::NameInvalid`] if the name is empty, over 64 bytes, or has a
    /// path separator in it.
    pub fn name(mut self, name: &str) -> Result<Open, OpenError> {
        self.name = Some(ArenaName::new(name, EnvVar::Name).map_err(OpenError::Rendezvous)?);
        Ok(self)
    }

    /// Read-only (default) or read-write.
    #[must_use]
    pub fn mode(mut self, mode: AttachMode) -> Open {
        self.mode = mode;
        self
    }

    /// Whether to create the arena when none exists.
    ///
    /// Anything other than [`CreatePolicy::Never`] needs [`Open::mode`] set to
    /// [`AttachMode::ReadWrite`]; the pair is [`OpenError::ReadOnlyCannotCreate`]
    /// otherwise.
    #[must_use]
    pub fn create(mut self, create: CreatePolicy) -> Open {
        self.create = create;
        self
    }

    /// Refuse to *join*: this process must be the one that creates the arena.
    ///
    /// [`CreatePolicy`] has three settings and none of them is "create, or
    /// refuse if one is already live" — `IfAbsent` silently joins and `Always`
    /// silently replaces. A process that owns an arena's topology needs the
    /// missing fourth answer: `docs/decisions/0019` §3 question 1 settles it for
    /// the ROS bridge of `0015`, where taking the join path would mean claiming
    /// edges in an arena somebody else sized.
    ///
    /// With this set, [`OpenOutcome::Joined`] becomes
    /// [`OpenError::ArenaAlreadyLive`] and the session is dropped before the
    /// error is returned — so the participant lock byte is released and the
    /// socket closed, and a refused attach is indistinguishable from one that
    /// never happened.
    ///
    /// It does **not** change what `create` means. `Never` plus this is a
    /// contradiction that reports as [`IpcError::ArenaAbsent`]: nothing to join
    /// and nothing permitted to create.
    #[must_use]
    pub fn require_create(mut self, require: bool) -> Open {
        self.require_create = require;
        self
    }

    /// How long to wait for a live-but-unreachable arena to resolve (§3.4).
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Open {
        self.timeout = timeout;
        self
    }

    /// The topology to create the arena *with*, if this process has to create it.
    ///
    /// A [`TreeBuilder`] rather than a raw layout, because decision `0004` sizes
    /// the arena from its declared edges and the creator also has to write the
    /// topology. A joiner never uses this — it maps what it is given.
    #[must_use]
    pub fn layout_if_creating(mut self, builder: TreeBuilder) -> Open {
        self.layout = Some(builder);
        self
    }

    /// Run §3.4 and produce a [`Tree`].
    ///
    /// One attempt. A consumer that starts before its publisher wants
    /// [`Open::await_open`].
    ///
    /// # Errors
    ///
    /// See [`OpenError`].
    pub fn open(mut self) -> Result<Tree, OpenError> {
        let per_attempt = self.timeout;
        self.attempt(per_attempt)
    }

    /// Run §3.4 repeatedly until an arena is there, or `timeout` runs out.
    ///
    /// **The first of `docs/decisions/0019` §2b's two waits**, and it exists
    /// because [`CreatePolicy::Never`] against an absent arena fails *fast* by
    /// design: a consumer racing its publisher's process *start* never obtains a
    /// [`Tree`] at all, so it cannot reach [`Tree::await_frames`]. Two absences,
    /// two names.
    ///
    /// # What is retried, and what is not
    ///
    /// Only [`IpcError::ArenaAbsent`] and [`IpcError::ArenaHeldButUnreachable`]
    /// — "never started" and "not yet", which `docs/decisions/0018` puts on one
    /// branch because a waiter cannot tell them apart and does the same thing
    /// about both. **Every other error is terminal and returned verbatim.**
    /// Retrying cannot change a `FORMAT_VERSION` disagreement, a layout hash
    /// mismatch or a missing runtime directory, and burning the budget against
    /// one would replace a precise message with a timeout.
    ///
    /// # There is no `Timeout` variant
    ///
    /// On expiry this returns the last retryable error it saw.
    /// [`IpcError::ArenaHeldButUnreachable`] already names the holder slots and
    /// the pid, and already means "I waited and it never resolved"; a second
    /// spelling would carry strictly less. [`IpcError::ArenaAbsent`] likewise
    /// says exactly what was true for the whole budget.
    ///
    /// # Granularity
    ///
    /// A bounded poll — `MIN_BACKOFF` doubling to `MAX_BACKOFF`, this crate's
    /// own pair, shared with [`Tree::await_frames`] and defined once in
    /// `crate::tree` — not a notification. `docs/decisions/0018`
    /// records why there is no arena-resident primitive to wake on, and it
    /// applies here with more force: topology settles once, at startup. So this
    /// returns *later* than the arena appeared, by up to one backoff interval
    /// plus scheduler granularity.
    ///
    /// [`Open::timeout`] is clamped to what is left of `timeout` on every
    /// attempt, or a default `Open` would let one held-but-unreachable attempt
    /// run the full [`DEFAULT_OPEN_TIMEOUT`] past the caller's deadline. That
    /// clamp is then floored at one backoff interval and truncated to whole
    /// microseconds, because the handshake's `SO_RCVTIMEO` rejects a value
    /// outside either bound and reports it as a *terminal* error; the comment on
    /// the loop has the two failures in full.
    ///
    /// # Errors
    ///
    /// See [`OpenError`].
    pub fn await_open(mut self, timeout: Duration) -> Result<Tree, OpenError> {
        let start = std::time::Instant::now();
        let mut backoff = MIN_BACKOFF;
        loop {
            // Clamp to what the *caller's* budget has left. `Open::timeout`
            // defaults to 5 s, so an unclamped attempt turns `await_open(1s)`
            // into a five-second call.
            //
            // **Floored at `MIN_BACKOFF`, and that is not tidiness.** The
            // handshake sets `SO_RCVTIMEO`/`SO_SNDTIMEO` from this value, and a
            // zero `Duration` there is `EINVAL` — reported as
            // `IpcError::ClientSocketSetup`, which is *terminal*, so a budget
            // that ran out exactly on an iteration boundary would replace the
            // rendezvous' real answer with a local socket error. The overrun
            // this permits is 200 µs, well inside one scheduler tick.
            //
            // **And truncated to whole microseconds, which is the same hazard
            // from the other end.** `SO_RCVTIMEO_NEW` carries a
            // `(tv_sec, tv_usec)` pair, and the conversion rounds the
            // sub-microsecond tail *up* without carrying into `tv_sec`: a
            // `Duration` in the last microsecond of a second becomes
            // `tv_usec == 1_000_000`, which the kernel rejects with `EDOM` —
            // again `IpcError::ClientSocketSetup`, again terminal. This loop is
            // the only place that *manufactures* a `Duration` by subtraction,
            // so it is the only place that produces one nobody wrote: a plain
            // `await_open(Duration::from_secs(1))` reaches here as
            // `999.999_9xx ms` on the first iteration and failed outright.
            // Observed as `setsockopt(4, SOL_SOCKET, SO_RCVTIMEO_NEW,
            // {tv_sec=0, tv_usec=1000000}) = -1 EDOM` under `strace`. Dropping
            // up to 999 ns costs nothing, and the floor above is a whole number
            // of microseconds so this cannot undercut it.
            let left = timeout.saturating_sub(start.elapsed());
            let per_attempt = core::cmp::max(core::cmp::min(self.timeout, left), MIN_BACKOFF);
            let per_attempt =
                Duration::new(per_attempt.as_secs(), per_attempt.subsec_micros() * 1_000);
            let err = match self.attempt(per_attempt) {
                Ok(tree) => return Ok(tree),
                Err(e) if is_retryable(e) => e,
                // Terminal: a version, layout or configuration disagreement no
                // amount of waiting alters.
                Err(e) => return Err(e),
            };
            // Deadline **after** the work and **before** the sleep: an attempt
            // that consumed the whole budget must report, not nap first.
            if start.elapsed() >= timeout {
                return Err(err);
            }
            // Never sleep past the caller's deadline.
            let left = timeout.saturating_sub(start.elapsed());
            std::thread::sleep(core::cmp::min(backoff, left));
            backoff = core::cmp::min(backoff * 2, MAX_BACKOFF);
        }
    }

    /// One pass of §3.4.
    ///
    /// `&mut self` so [`Open::await_open`] can call it repeatedly, and **the
    /// layout is cloned rather than taken** — which is the difference between an
    /// audit and a guarantee.
    ///
    /// The audit this replaces read: the only path that consumes the layout
    /// returns either `Ok` (no retry) or [`OpenError::NoLayoutToCreate`], which
    /// `is_retryable` classifies as terminal, so a second attempt never finds it
    /// missing where the first found it present. That was **wrong**. On the
    /// same arm — `Created` —
    /// `spawn_owner_server` returns
    /// `OpenError::Rendezvous(IpcError::ArenaAbsent)` when `tree.shared_fd()` is
    /// `None`, and `is_retryable` calls that **retryable** — so `await_open`
    /// would loop with the layout already gone and report `NoLayoutToCreate`, a
    /// diagnostic pointing at the caller for an internal invariant break.
    ///
    /// Unreachable today (a freshly `build_shared`-ed arena always has an fd),
    /// which is precisely why a comment is the wrong instrument: nothing would
    /// fail if the audit went stale again. Cloning removes the class instead of
    /// re-auditing it, and it is what `&mut self` cost in the first place —
    /// before `await_open` existed this method took `self` by value and simply
    /// moved the layout out.
    ///
    /// The price is one [`TreeBuilder`] clone per *creating* attempt, next to a
    /// `memfd_create`, an `mmap` and a socket bind. Consumers (`create =
    /// Never`) never reach it, and a creating caller reaches it at most once —
    /// `Created` either succeeds or fails terminally.
    fn attempt(&mut self, per_attempt: Duration) -> Result<Tree, OpenError> {
        // **Before `RuntimeDir::resolve()`, deliberately** (`docs/decisions/0019`
        // plan step 1). This is a property of the arguments alone; checking it
        // after would report a misconfigured builder as whatever the machine
        // happens to be missing, which is a diagnostic pointing at the wrong
        // process.
        if self.mode == AttachMode::ReadOnly && self.create != CreatePolicy::Never {
            return Err(OpenError::ReadOnlyCannotCreate);
        }
        let rd = RuntimeDir::resolve().map_err(OpenError::Rendezvous)?;
        let domain = match self.domain {
            Some(d) => d,
            None => tf_tree_ipc::domain_from_env(&SystemEnv).map_err(OpenError::Rendezvous)?,
        };
        let name = match self.name {
            Some(n) => n,
            None => tf_tree_ipc::name_from_env(&SystemEnv).map_err(OpenError::Rendezvous)?,
        };
        let rv = Rendezvous::new(rd, domain, name);

        let request = HelloRequest {
            format_version: tf_tree_arena::FORMAT_VERSION,
            layout_hash: tf_tree_arena::layout_hash(),
            mode: match self.mode {
                AttachMode::ReadOnly => AccessMode::ReadOnly,
                AttachMode::ReadWrite => AccessMode::ReadWrite,
            },
            client_pid: std::process::id(),
            client_start_time: self_start_time().unwrap_or(0),
            client_boot_id: boot_id().unwrap_or([0; 16]),
            client_name: name_bytes(),
        };
        let mut probe = SocketProbe::new(request, per_attempt);

        let ipc_open = tf_tree_ipc::Open::new(rv.clone())
            .mode(request.mode)
            .create(self.create)
            .timeout(per_attempt);
        let mut session = ipc_open.open(&mut probe).map_err(OpenError::Rendezvous)?;

        match session.outcome() {
            OpenOutcome::Joined => {
                if self.require_create {
                    // Drop the whole session before returning. It holds this
                    // process's participant lock byte and the connection whose
                    // closure tells the owner we are gone (D17), so returning
                    // the error while it lived would leave a slot taken and a
                    // client the owner still counts.
                    drop(session);
                    return Err(OpenError::ArenaAlreadyLive);
                }
                let attached = session
                    .take_attached()
                    .ok_or(OpenError::Rendezvous(IpcError::ArenaAbsent))?;
                let slot = attached.response.participant_slot;
                // `attach_joined_at`, not the `pub` `attach_shared_at`: that
                // one refuses `ReadWrite` because a caller holding a raw
                // descriptor holds no lock byte. Here the byte is already
                // taken — `session` is holding it, taken by `register_at`
                // during the handshake, before this record is written — which
                // is exactly the crate-private path's precondition
                // (`docs/decisions/0028` plan step 0b).
                let mut tree = Tree::attach_joined_at(attached.segment, self.mode, slot)?;
                tree.use_ofd_liveness(LivenessProbe::open(&rv)?);
                tree.use_claim_leases(open_claim_lock(&rv)?);
                // The socket and the lock file must outlive the handshake: the
                // first is how the owner learns we died (D17), the second is
                // what holds our participant byte. Parking them in the `Tree`
                // ties both to the lifetime a caller actually manages.
                tree.hold_attachment(session, attached.socket, rv);
                Ok(tree)
            }
            OpenOutcome::Created => {
                // `docs/PHASE2.md` §11.3: **`open.after_ownership_lock_before_bind`**.
                // The ownership byte is held — `Open::open` took it to reach this
                // arm — and nothing has been created or bound. The row's repair
                // claim is "ownership lock released by the kernel; the next
                // `open()` proceeds; **no arena created twice**", and this is the
                // instruction it names.
                //
                // What makes the claim true rather than hopeful is that the byte
                // is an OFD lock: a killed process gives it back with no
                // cooperation, and the next `open()` finds byte 0 free, no socket
                // to connect to, and no participant byte held — so §3.4 step 4's
                // split-brain check does not fire and it creates. The *second*
                // half of the claim is the interesting one: nothing was created
                // here, so there is no first arena for a second to race.
                #[cfg(feature = "crash-points")]
                tf_tree_core::crash::maybe_abort(CRASH_SITES[2]);

                // `clone`, not `take` — see this method's doc comment. A retry
                // must find the layout exactly as the first attempt found it.
                let builder = self.layout.clone().ok_or(OpenError::NoLayoutToCreate)?;
                let mut tree = builder.build_shared(rv.name().as_str())?;

                // `docs/PHASE2.md` §11.3: **`open.after_create_before_bind`**.
                // The arena exists, the ownership byte is held, and nothing is
                // serving. The row: "arena exists, nothing serving, no
                // participant byte held → next `open()` finds nothing alive and
                // creates fresh; **the orphan memfd is freed with its last
                // mapping**."
                //
                // That last clause is the one this placement exists to exercise,
                // and it is the reason `memfd` was chosen over `shm_open` (§3.9):
                // the segment has no name in any filesystem, so when this
                // process's mapping goes with it there is no stale object for
                // anybody to find, clean up, or accidentally attach to. A
                // `shm_open` arena killed here would leave a file.
                //
                // **Before `use_ofd_liveness`/`use_claim_leases`**, so the tree
                // this abort abandons holds only the segment — which is exactly
                // what the row's "no participant byte held" asserts.
                #[cfg(feature = "crash-points")]
                tf_tree_core::crash::maybe_abort(CRASH_SITES[3]);

                tree.use_ofd_liveness(LivenessProbe::open(&rv)?);
                tree.use_claim_leases(open_claim_lock(&rv)?);

                // **The byte/record correspondence, asserted where the two are
                // paired** (`docs/decisions/0028` plan step 0c, issue #201).
                // `session.slot()` is the participant lock byte this process
                // holds; `tree.participant_slot()` is the arena record
                // `build_shared` just registered it at. Nothing between them
                // reconciles the two — the byte is chosen inside `tf_tree_ipc`,
                // which has no arena dependency and cannot see the record index,
                // and the record is chosen by the first `FREE` slot in a fresh
                // arena. On every ordinary path they agree by
                // construction; under `CreatePolicy::Always`, which skips §3.4
                // step 4's guard by design, they need not — as written. `0035`
                // then put the create path on `register_creator`, which takes
                // byte 0 atomically, so this comparison is an assertion on that
                // arm and a filter only on the takeover one.
                //
                // §5.1's predicate reads one at the index of the other, so a
                // disagreement makes every liveness verdict about somebody else.
                // That is not hypothetical: `participant_alive(0) == false` was
                // measured about a live, publishing process whose record is 0
                // and whose byte is 1.
                //
                // **Before `spawn_owner_server`, not after**, which is the whole
                // of why the check sits on this line rather than inside
                // `hold_ownership`. The server binds `rv.sock_path()` and starts
                // answering handshakes, so one line later a joiner could already
                // hold this segment, and refusing then would tear an arena out
                // from under a process that did nothing wrong. Here the arena is
                // still private: nobody but this process has ever seen it.
                //
                // **That ordering is an argument, not a tested property, and the
                // tests say so.** Moving this block below `spawn_owner_server`
                // leaves both `defect_201` tests in
                // `crates/tf_tree/tests/rendezvous.rs` green: the socket is bound
                // and published, and then `impl Drop for OwnerServer`
                // (`crates/tf_tree_ipc/src/server.rs:475`) unlinks the path it
                // published, so nothing on disk tells the two placements apart.
                // What separates them is a joiner scheduled inside these two
                // statements, which no test in this workspace can arrange.
                if session.slot() != tree.participant_slot() {
                    // Record first, then byte — the order a healthy participant
                    // leaves in (`Tree`'s `Drop` releases the record, and only
                    // then does the `Session` release the byte). Explicit rather
                    // than left to scope order, for the same reason the
                    // `require_create` refusal above is.
                    drop(tree);
                    drop(session);
                    return Err(OpenError::ParticipantSlotDiverged);
                }

                let server = spawn_owner_server(&rv, &tree)?;
                tree.hold_ownership(session, server);
                Ok(tree)
            }
        }
    }
}

/// Whether [`Open::await_open`] should try again, or report this verbatim.
///
/// **Exactly two, and the list is the decision rather than a heuristic**
/// (`docs/decisions/0019` plan step 2). Both describe the publisher-mid-start
/// window: [`IpcError::ArenaAbsent`] is "nothing is there", and
/// [`IpcError::ArenaHeldButUnreachable`] is "something took the ownership byte
/// and has not begun serving". `docs/decisions/0018` puts "not yet" and "never
/// started" on one branch because a waiter cannot distinguish them and does the
/// same thing about both.
///
/// Everything else — a `FORMAT_VERSION` or layout-hash disagreement, a missing
/// runtime directory, a mapping failure, a builder misconfiguration — is
/// terminal. Retrying cannot change any of them, and burning the budget would
/// hand the caller a timeout where it had a precise message.
fn is_retryable(err: OpenError) -> bool {
    matches!(
        err,
        OpenError::Rendezvous(IpcError::ArenaAbsent)
            | OpenError::Rendezvous(IpcError::ArenaHeldButUnreachable { .. })
    )
}

/// The owner's serving thread, and the handle that stops it.
pub(crate) struct OwnerThread {
    shutdown: ShutdownHandle,
    join: Option<std::thread::JoinHandle<()>>,
    /// The fork generation the thread was spawned in.
    ///
    /// `fork` copies the address space but **not the threads**: the child gets
    /// an `OwnerThread` value describing a thread that does not exist there. Its
    /// `JoinHandle` names nothing, and — worse — `ShutdownHandle` is an
    /// `eventfd` whose *description* the child inherited, so a write from the
    /// child is delivered to the **parent's** serving loop. Dropping an
    /// inherited `Tree` in a child would therefore shut down the parent's owner
    /// server, and every subsequent joiner would find an unreachable arena.
    fork_gen: u64,
    /// Set by the loop when it returns, so `Drop` can tell "still serving" from
    /// "already stopped" without blocking on a thread that has gone.
    running: Arc<AtomicBool>,
}

impl OwnerThread {
    /// Stop the server and wait for the thread.
    ///
    /// A no-op in a `fork` child — see [`OwnerThread::fork_gen`]. Both callers
    /// (this type's `Drop` and [`Attachment`]'s) route through here, so the
    /// check lives here rather than at each of them.
    pub(crate) fn stop(&mut self) {
        if self.fork_gen != tf_tree_ipc::fork::generation() {
            // Do not join a thread that was never forked, and do not signal the
            // parent's eventfd. Drop the handle so `Drop` does not try again.
            self.join = None;
            return;
        }
        let _ = self.shutdown.stop();
        if let Some(h) = self.join.take() {
            let _ = h.join();
        }
        self.running.store(false, Ordering::Release);
    }
}

impl Drop for OwnerThread {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Bind the §3.7 socket and serve it from a thread for this arena's lifetime.
///
/// A thread rather than a daemon: §3.5 makes ownership a role a survivor
/// inherits, which is only possible if any participant can bind.
fn spawn_owner_server(rv: &Rendezvous, tree: &Tree) -> Result<OwnerThread, OpenError> {
    // The assigner's own view of the lock file — see the closure below.
    let lock_probe = LivenessProbe::open(rv)?;
    let view = tree.view();
    let header = view.header();
    let desc = SegmentDescriptor {
        format_version: header.format_version,
        layout_hash: header.layout_hash,
        arena_size: header.arena_size,
        instance_uuid: header.instance_uuid,
        boot_id: header.boot_id,
    };

    let server = OwnerServer::bind_at(rv.sock_path(), desc, std::process::id())
        .map_err(OpenError::Rendezvous)?;
    let shutdown = server.shutdown_handle().map_err(OpenError::Rendezvous)?;

    // The serving thread needs the segment fd and the participant table for as
    // long as it runs. `try_clone` gives it an independent descriptor, so the
    // thread's lifetime is not tied to the `Tree`'s internals.
    let segment = tree
        .shared_fd()
        .ok_or(OpenError::Rendezvous(IpcError::ArenaAbsent))?;
    let segment = rustix_dup(segment).map_err(OpenError::Rendezvous)?;

    // A second mapping so the serving thread can reach the participant table
    // without borrowing the `Tree`. One extra mapping of an already-resident
    // segment costs page-table entries and nothing else, and it keeps the
    // thread's lifetime independent of the handle a caller holds.
    //
    // **Read-write, and it used to be read-only.** §3.9 says that when a
    // participant dies "the owner reaps its arena-side records"; the hangup
    // callback below is where that happens, and freeing a record is a CAS on the
    // arena. An owner always has a writable segment — it either created it or
    // took over by building one — so this cannot demote a read-only attachment.
    let table_fd = {
        use std::os::fd::AsFd;
        rustix_dup(segment.as_fd()).map_err(OpenError::Rendezvous)?
    };
    let table_arena = tf_tree_arena::MappedArena::attach(table_fd, AttachMode::ReadWrite)?;

    // **The owner's own slot, which no sweep may collect.**
    // `reclamation_verdict`'s second constraint skips it unconditionally and
    // first — `F_OFD_GETLK` reports only *conflicting* locks, so a description
    // does not see its own byte, and a sweep that leant on this probe's
    // separate description happening to make ours visible would be one refactor
    // away from reclaiming its own live record. One integer indexes both tables
    // (§3.7), and `Open::attempt`'s `Created` arm is what asserts that here
    // (`docs/decisions/0028` plan step 0c) rather than assuming it.
    let own_slot = tree.participant_slot();

    let running = Arc::new(AtomicBool::new(true));
    let flag = Arc::clone(&running);
    let join = std::thread::Builder::new()
        .name("tf_tree-owner".into())
        .spawn(move || {
            use std::os::fd::AsFd;

            // Slots granted but not yet hung up. The arena table alone is not
            // enough: a joiner registers *after* it takes its lock byte, so
            // between the response and that registration the slot still reads
            // free — and re-granting it would hand two clients the same slot,
            // which is exactly what `register_at` exists to make impossible.
            // Shared between the two closures below, which `serve` holds
            // simultaneously — so a plain `u64` cannot be borrowed by both.
            // Single-threaded within this loop, hence `Cell` and not a mutex.
            let granted = std::rc::Rc::new(std::cell::Cell::new(0u64));
            let granted_assign = std::rc::Rc::clone(&granted);
            let granted_hangup = std::rc::Rc::clone(&granted);

            let _ = server.serve(
                segment.as_fd(),
                |_req| {
                    let view = tf_tree_core::arena_view::ArenaView::new(&table_arena);
                    let table = view.participants();
                    // `granted` is a u64, so the shift below is defined only
                    // for the first 64 slots. The const assert at the top of
                    // this file ties the table to exactly 64; this bound keeps
                    // that assert's failure a compile error rather than a
                    // shift overflow at runtime.
                    let n = table.capacity().min(64) as u32;
                    for slot in 0..n {
                        let bit = 1u64 << slot;
                        if granted_assign.get() & bit != 0 {
                            continue; // granted, not yet hung up
                        }
                        let Some(rec) = table.get(slot) else {
                            // `n` came from `capacity()`, so this cannot fire;
                            // it is the `Option` and not a bound check.
                            continue;
                        };
                        // **Is there a record here at all** — a question about
                        // the *record*, not about its process. `docs/PHASE2.md`
                        // §5.1 forbids deciding *liveness* from `state`, and
                        // this decides something else: whether `fill_slot`'s
                        // `FREE -> RESERVED` CAS could succeed at this index.
                        // Granting a slot whose word is not `FREE` hands the
                        // joiner `ShmError::ParticipantTableFull` — *"Every
                        // participant slot is taken, so this process cannot
                        // join"* — about the very slot this loop just decided
                        // was free, and there is nothing it can usefully retry.
                        let word = rec.state.load(Ordering::Acquire);
                        if tf_tree_core::participant::state_of(word)
                            != tf_tree_core::participant::FREE
                        {
                            // **This is `docs/decisions/0028`'s defect, at the
                            // line it was filed against.** The test used to be
                            // `if table.identity(slot).is_some() { continue }`,
                            // and `identity` returns `Some` iff the word reads
                            // `LIVE` — so a participant that was `SIGKILL`ed,
                            // and therefore never ran `Tree`'s `Drop`, kept its
                            // slot for the life of the segment and the assigner
                            // skipped it for ever. Sixty-four abnormal
                            // read-write exits wedged the arena (#184: 63 of 64
                            // slots holding records for dead pids, every
                            // subsequent attach refused). §5.1 is normative
                            // that this is a bug in as many words: *"Any code
                            // deciding liveness from `state` or `heartbeat` is
                            // a bug."*
                            //
                            // The verdict comes from the kernel instead, and
                            // through the **one** predicate — a second copy of
                            // it is the defect that record was opened about,
                            // re-created.
                            match reclamation_verdict(&lock_probe, own_slot, slot, rec) {
                                Reclamation::Reclaimable { observed } => {
                                    // **Deciding correctly is not enough**, and
                                    // 0028's candidate A is the record of why:
                                    // `fill_slot` CASes from `FREE`, so a slot
                                    // judged collectable and left `LIVE` is
                                    // refused to the very joiner this grant is
                                    // for. Reclaim first, grant second.
                                    //
                                    // `observed` comes from the verdict rather
                                    // than from a fresh load, which is what
                                    // `Reclamation::Reclaimable` carries it
                                    // for: it is the word read *before* the
                                    // byte was probed, and `reclaim`'s ordering
                                    // obligation is about that word. A reload
                                    // here would build the CAS guard out of a
                                    // word read *after* the probe — the failing
                                    // order, which erases a published record.
                                    //
                                    // **And nothing in this crate would catch
                                    // one**, which is why it is written down
                                    // here rather than left to the reviewer who
                                    // finds `observed` redundant. Measured:
                                    // binding the arm as `Reclaimable { .. }`
                                    // and opening it with
                                    // `let observed = rec.state.load(Acquire);`
                                    // compiles clean under `-D warnings` — no
                                    // `unused variable`, because the arm stops
                                    // binding the field — and all 29 rendezvous
                                    // tests pass. A reload yields the same word
                                    // in every state a test can stage, so the
                                    // property lives in the model and not here:
                                    // `tf_tree_core::loom_tests`'
                                    // `reclaim_races_register`, which
                                    // [`reclamation_verdict`]'s third
                                    // constraint routes to, ships the reversed
                                    // control that erases a published record.
                                    if !table.reclaim(slot, observed) {
                                        // The word moved between the
                                        // observation and the CAS, so this
                                        // verdict is about an occupancy that no
                                        // longer exists. Leave the slot; the
                                        // next handshake forms a fresh verdict.
                                        //
                                        // **Unpinned, and written down rather
                                        // than left to be "simplified".**
                                        // Dropping this `continue` and ignoring
                                        // the return passes the whole
                                        // rendezvous suite — measured, not
                                        // assumed.
                                        //
                                        // **An earlier revision of this comment
                                        // predicted that plan step 5's
                                        // `reap_participants` would make it
                                        // load-bearing, by putting a second
                                        // reclaimer in the workspace. Step 5
                                        // landed; the prediction was wrong, and
                                        // re-measured rather than re-asserted:**
                                        // with the guard deleted the tree is
                                        // green at 148 of 148
                                        // (`-p tf_tree --features
                                        // shm,test-hooks,unstable`) and over
                                        // five consecutive runs of the 31-test
                                        // rendezvous target. Two reasons, and
                                        // the second is the durable one. The
                                        // sweep runs when a participant *calls*
                                        // it, and nothing schedules one against
                                        // a grant in flight — but more to the
                                        // point, the byte probe below already
                                        // catches the case a lost CAS is
                                        // frightening for: the only way to lose
                                        // it to an occupancy rather than to a
                                        // peer sweeper's `FREE` is for somebody
                                        // to have registered here, and by plan
                                        // step 0b a registrant holds this
                                        // slot's byte across the whole of
                                        // `fill_slot` and keeps it for its
                                        // life, so `is_held` reports it and the
                                        // slot is skipped anyway.
                                        //
                                        // What the guard still bounds is the
                                        // residue neither of those covers: a
                                        // slot reclaimed under us, re-registered
                                        // by a third process, and *abandoned* by
                                        // it before the probe below — free byte,
                                        // occupied word, and a grant that hands
                                        // the joiner a slot `fill_slot` will
                                        // refuse it. The guard turns that into a
                                        // skipped slot. Nothing in this
                                        // workspace can stage it, so it is kept
                                        // on the argument and not on a test —
                                        // which is what "unpinned" means here.
                                        continue;
                                    }
                                }
                                // A live participant holds the byte, or the
                                // slot is our own. Not ours to collect.
                                Reclamation::Live => continue,
                                // The kernel would not say. §6.2's fail-safe
                                // direction is to leave it alone: a slot nobody
                                // can use costs one participant, a wrong grant
                                // costs a running one its record. **`Unknown`
                                // cannot mean "no record here" on this line** —
                                // that is the predicate's `FREE` branch, and
                                // the `if` above has already excluded it.
                                Reclamation::Unknown => continue,
                            }
                        }
                        // **And the lock byte must be free too.** A read-only
                        // participant takes its byte but writes no arena
                        // record — `attach_shared` cannot register a
                        // `PROT_READ` mapping — so the table alone reports its
                        // slot empty. `mode="ro"` is the consumer default
                        // (D18) *and* the Python default, so this is the
                        // common case, not a corner. Granting such a slot
                        // hands the joiner a byte it cannot take; the
                        // `granted` bitmask hides that until an owner restart
                        // or a takeover clears it, after which the owner names
                        // the same slot forever and the joiner loops.
                        //
                        // **A slot just reclaimed is probed twice**, and that
                        // is deliberate rather than overlooked. The verdict
                        // above reported the byte free, but the byte is not
                        // ours between the two reads: `tf_tree_ipc`'s
                        // `hold-participant` helper is a `[[bin]]` of a
                        // published crate and can take any byte at any moment,
                        // without asking an owner. Losing that race costs a
                        // skipped slot whose dead record has already been
                        // collected, which is the harmless direction.
                        if lock_probe.is_held(slot).unwrap_or(false) {
                            continue;
                        }
                        granted_assign.set(granted_assign.get() | bit);
                        return Ok(slot);
                    }
                    Err(HelloStatus::NoParticipantSlots)
                },
                |slot| {
                    // **§3.9: "the owner reaps its arena-side records".** This
                    // is that sentence, and until it was written here nothing in
                    // the workspace performed it.
                    //
                    // A participant that is `SIGKILL`ed never runs `Tree`'s
                    // `Drop`, so its record stays `LIVE` until somebody else
                    // clears it — and when this was written nobody did: `assign`
                    // above skipped a `LIVE` slot while `register_at` fills only
                    // a `FREE` one, so that slot could never be granted to
                    // anybody again. (`assign` no longer skips it; that is plan
                    // step 3, and this callback is now the O(1) fast path for
                    // the same collection rather than the only one.)
                    // Measured before this existed, with
                    // `shm_torture --kill-hz 6`: 63 of the 64 slots held records
                    // for dead pids after thirty seconds, every subsequent
                    // attach was refused `NoParticipantSlots`, and the arena ran
                    // the remaining 29 minutes with no writer at all while the
                    // observer read four frozen rings and scored a perfect 256
                    // composed lookups per round out of them.
                    //
                    // **The observed word is what makes this safe**, and it
                    // has to give the same bound the incarnation guard gave.
                    // This callback used to read `identity(slot)` for the
                    // incarnation and call `release(slot, incarnation)`; it now
                    // loads the `state` word once and hands *that* to
                    // `reclaim`, which is one `compare_exchange(observed, FREE)`
                    // — the load and the compare fused rather than a load and
                    // then a separately-built guard (`docs/decisions/0028` plan
                    // step 4).
                    //
                    // *For a `live_word(inc)`* the bound is identical, because
                    // the word **is** the incarnation: `live_word` packs it into
                    // the high 30 bits, so "still live and still the occupancy
                    // this hangup is about" is the same single comparison
                    // `release` made. A participant that detached cleanly has
                    // already stored `FREE`, so the `!= FREE` test below skips
                    // it; one whose slot was re-granted carries a different
                    // incarnation and the CAS fails.
                    //
                    // *For a `RESERVED` word* — which this collects and
                    // `release` never could, because `identity` returns `None`
                    // for it — the guard is weaker and the bound is not. A
                    // `RESERVED` word is the bare constant `1` and carries no
                    // incarnation, so against it the CAS degenerates to an ABA:
                    // if the slot were freed, re-granted and driven back to
                    // `RESERVED` between this load and this CAS, the CAS would
                    // succeed against the *new* occupancy. What bounds that is
                    // the byte and not the word. By plan step 0b every process
                    // that writes a record holds the matching lock byte across
                    // the whole of `fill_slot`, and by step 0c that byte is the
                    // byte at the record's own index — so the record such a CAS
                    // erases belongs to a joiner that is holding the byte and is
                    // about to publish `live_word` over it, and is *entitled*
                    // to, because it really does own the slot. **The outcome is
                    // a spurious free, never a second occupant**, which is the
                    // same bound the incarnation guard bought.
                    // `ParticipantTable::reclaim`'s doc comment carries that
                    // precondition, and says what to narrow it back to if
                    // either half stops holding.
                    //
                    // **The single-thread argument that used to close the
                    // ABA's first leg no longer holds, and the paragraph above
                    // is why that costs nothing.** Reaching a second `RESERVED`
                    // needs the slot to pass through `FREE` first, and **the
                    // only operation in this workspace that can drive a
                    // `RESERVED` word to `FREE` is `reclaim` itself**: the other
                    // writer of `FREE` is `ParticipantTable::release`, whose CAS
                    // names `live_word(inc)` and therefore cannot match the bare
                    // constant `1`. When plan step 4 landed, `reclaim` had
                    // exactly two call sites outside `tf_tree_core`'s own unit
                    // and `loom` tests — this callback and the `assign` closure
                    // above — and `serve` calls both from its one `epoll` loop,
                    // on this thread, so nothing could free this word while this
                    // callback held it. **Plan step 5 added the third**:
                    // `Tree::reap_participants` sweeps the whole table from any
                    // surviving read-write participant, in another process, on a
                    // thread this loop knows nothing about. So the first leg is
                    // open now, and what bounds the interleaving is the byte
                    // argument above and not this thread — a spurious free,
                    // never a second occupant. (The `granted` bit below is still
                    // cleared after this CAS, which closes the *re-grant* leg
                    // for slots this owner granted; a §3.5 heir serving an arena
                    // whose participants it never granted would not have even
                    // that.)
                    //
                    // Before the `granted` bit, so no `assign` can hand the slot
                    // out between the two — they run on this one thread, but the
                    // ordering costs nothing and does not rely on that.
                    let view = tf_tree_core::arena_view::ArenaView::new(&table_arena);

                    // **`docs/PHASE2.md` §3.9's other half, and it had no
                    // caller.** The section says a dead participant's *"arena-side
                    // records"* are the owner's to reap; this callback freed the
                    // participant record and left every **claim** that
                    // participant held. `Tree::reap_participant` was written for
                    // exactly this site — its doc says "the owner learns a
                    // participant died from `EPOLLHUP` … and therefore knows
                    // which slot went away" — and nothing in the workspace
                    // called it outside a benchmark and a test helper.
                    //
                    // What that cost is the ordinary supervised restart: a
                    // killed publisher's edges stay claimed, the supervisor
                    // restarts the node, the assigner hands it **its
                    // predecessor's slot** — and `reap_claims` skips
                    // `own_slot`, so the one process that needs those edges is
                    // the one that cannot repair them. `tf_tree_cli`'s
                    // `checks.rs` states the same gap from the detection side:
                    // "once a later joiner is granted that slot, the stale claim
                    // joins to a live participant and the edge reads healthy
                    // while no process is writing it."
                    //
                    // **Before the record is freed**, so the slot cannot be
                    // re-granted with its predecessor's claims still standing.
                    // `own_slot` is passed for the same reason every other
                    // caller passes it: `F_OFD_GETLK` does not report a
                    // description's own byte, so an owner sweeping without it
                    // would revoke its own live claims.
                    let revoked =
                        crate::tree::reap_claims(&view, lock_probe.lock(), Some(slot), own_slot);
                    let _ = revoked;

                    let table = view.participants();
                    if let Some(rec) = table.get(slot) {
                        let observed = rec.state.load(Ordering::Acquire);
                        if tf_tree_core::participant::state_of(observed)
                            != tf_tree_core::participant::FREE
                        {
                            // The return is dropped deliberately: `false` means
                            // the word moved under this verdict, which is the
                            // case where there is nothing left to do.
                            // `docs/PHASE2.md` §11.3:
                            // **`hangup.after_probe_before_cas`**. The owner has
                            // loaded `state` and not yet CASed, and this is the
                            // instruction that row names.
                            //
                            // **Its repair claim is the weakest in the table and
                            // the row says so**: one CAS, so there is no torn
                            // intermediate — the reclamation either happened or
                            // it did not, and what is lost is the reclamation
                            // rather than consistency. Under the hangup callback
                            // *alone* the honest entry would be "not repairable".
                            // What makes it repairable is that two other
                            // collectors form the same verdict later: the
                            // assigner at the next grant, and any surviving
                            // read-write participant's `Tree::reap_participants`.
                            #[cfg(feature = "crash-points")]
                            tf_tree_core::crash::maybe_abort(CRASH_SITES[5]);

                            let _ = table.reclaim(slot, observed);
                        }
                    }
                    // D17: the socket closed, so that participant is gone and
                    // its slot can be handed out again.
                    granted_hangup.set(granted_hangup.get() & !(1u64 << slot));
                },
            );
            flag.store(false, Ordering::Release);
        })
        .map_err(|_| OpenError::Rendezvous(IpcError::ArenaAbsent))?;

    Ok(OwnerThread {
        shutdown,
        join: Some(join),
        running,
        fork_gen: tf_tree_ipc::fork::generation(),
    })
}

/// `dup` a borrowed fd into an owned one.
///
/// Reported as [`IpcError::ClientSocketSetup`] — a local resource failure of
/// this process, which is what running out of descriptors is. Naming it after
/// the lock file, as an earlier version did, would point an operator at a file
/// that is not involved.
fn rustix_dup(fd: std::os::fd::BorrowedFd<'_>) -> Result<std::os::fd::OwnedFd, IpcError> {
    fd.try_clone_to_owned()
        .map_err(|e| IpcError::ClientSocketSetup {
            raw_os_error: e.raw_os_error().unwrap_or(0),
        })
}

/// This process's name, NUL-padded, for the handshake's diagnostic field.
///
/// **The 32 here and the 32 `docs/decisions/0033` narrowed are different
/// numbers, and this is the one site in the workspace where they meet.**
/// `HelloRequest::client_name` is **wire** bytes `56..88` of an 88-byte
/// datagram (`tf_tree_ipc::wire`, pinned by `the_byte_layout_is_pinned` and by
/// `docs/PHASE2.md` §3.7) and it did not move; the lock file's identity record
/// is a different structure whose `name` went to `[u8; 16]` so that
/// `pid_ns_inode` could have `48..56`. So `self_comm` narrowed and this pads,
/// which reads like a redundancy and is not one: collapsing the two back
/// together changes a pinned wire layout.
fn name_bytes() -> [u8; 32] {
    let mut out = [0u8; 32];
    let comm = tf_tree_ipc::self_comm();
    out[..comm.len()].copy_from_slice(&comm);
    out
}
