//! `open()` — the NORMATIVE §3.4 decision algorithm.
//!
//! ```text
//! deadline = now + open_timeout (default 5 s)
//! loop {
//!     // 1. Someone is already serving. Join.
//!     if connect(sock) succeeds { ...; return Joined }
//!
//!     // 2. Nobody is serving. Try to become the owner.
//!     if F_OFD_SETLK(byte 0, exclusive) fails { backoff; continue }
//!
//!     // 3. We hold ownership. Do we already have an arena?
//!     if we hold an arena fd { goto 5 }          // reuse it -- never create a second one
//!
//!     // 4. SPLIT-BRAIN CHECK. Is any participant byte locked?
//!     if any participant byte is held { release byte 0; backoff; continue }
//!
//!     // 5. Serve.
//!     ...
//!     return Created | TookOver
//! }
//! on timeout -> Err(ArenaHeldButUnreachable { holder_slots, identities })
//! ```
//!
//! **Step 4 is the whole design.** Without it this sequence is possible: the
//! owner dies; a fresh process starts, finds no socket, wins the ownership lock
//! before any surviving participant has noticed the `HUP`, and creates a
//! *second* arena. The survivors keep using the first. Two arenas, both live,
//! silently diverging — worse than any failure to start, because nothing reports
//! an error and the robot's transform tree is quietly inconsistent between
//! nodes.
//!
//! The check is **deterministic, not a grace period.** If any participant byte
//! is locked, a live arena exists, and a fresh process must not create one, full
//! stop. There is no timing assumption and no window to tune. The timeout case
//! is likewise correct rather than a limitation: if a participant is
//! `SIGSTOP`ped and never takes over, no new process can join, and that is the
//! right answer, because the alternative is divergence.
//!
//! # What this module leaves to its caller
//!
//! Steps 1 and 5 are injected rather than performed here, and that is what keeps
//! this module free of both the socket and the arena. The socket lives one
//! module over, in [`crate::OwnerServer`] and [`crate::attach`]; the arena is
//! not reachable from this crate at all — its `Cargo.toml` lists `rustix` and
//! `libc` and nothing else, so `memfd` creation cannot happen here even in
//! principle. "Is someone serving?" is therefore a [`ServerProbe`] the caller
//! injects — [`crate::SocketProbe`] runs the real §3.7 handshake, [`NoServer`]
//! is the test one — and "serve" means taking the locks and returning an
//! [`OpenOutcome`] that tells the caller which of bind/create it now owes. That
//! split is not a stub: the lock-file half is where every race in §3.4 lives,
//! and it is testable to the last branch without a socket.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::error::IpcError;
use crate::identity::{AccessMode, Identity};
use crate::lockfile::LockFile;
use crate::ofd::LockAttempt;
use crate::rendezvous::Rendezvous;

/// What `open()` should do when no arena exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CreatePolicy {
    /// Create one if nothing is there. The default, and what makes the
    /// zero-argument case work.
    #[default]
    IfAbsent,
    /// Never create; fail with [`IpcError::ArenaAbsent`] instead.
    ///
    /// Worth recommending for anything in a supervised deployment: a consumer
    /// that creates an empty arena because the estimator has not started yet
    /// looks healthy and publishes nothing.
    Never,
    /// Create over an arena that exists but cannot be reached, abandoning it.
    ///
    /// **`docs/PHASE2.md` §3.4 calls this `--force-new`, and the flag has never
    /// existed** — the escape hatch is this policy and nothing else, on the
    /// process that creates the arena (§0.0's row, #189). It **skips the
    /// split-brain check**, which is to say it deliberately does the thing §3.4
    /// exists to prevent, and it is here only because an operator staring at
    /// [`IpcError::ArenaHeldButUnreachable`] on a wedged robot needs one. Never
    /// take this path automatically.
    ///
    /// "Unreachable", not "unconditionally": step 1 of the loop still runs, so a
    /// rendezvous with a server answering on its socket is *joined*. What this
    /// abandons is an arena whose **owner is gone** and whose non-owner holders
    /// are alive — §3.4's stranded-participant case, and the only shape it can
    /// abandon.
    ///
    /// The sentence that stood here said "an arena whose holders are alive and
    /// whose owner is not serving", which is precisely the state it *cannot*
    /// abandon: this policy skips step 4's participant scan and nothing else, so
    /// it still takes the ownership byte at step 2 and still takes participant
    /// byte 0 at step 5 — and byte 0 is the owner's slot for the owner's whole
    /// life (`CREATOR_SLOT`, `docs/decisions/0035`; joiners are
    /// assigned slots `>= 1`). An owner that is alive holds one or both, so it
    /// refuses exactly like [`CreatePolicy::IfAbsent`]; the two bytes being free
    /// is when this creates, and an owner that is gone is the ordinary — not the
    /// only — way they get that way. [`Session::release_ownership`] frees the
    /// ownership byte and keeps byte 0, so a live non-owner can sit on the
    /// creator's slot with no owner anywhere and this still refuses. Read the
    /// bytes, not the role. Measured both ways by
    /// `a_live_byte_0_refuses_both_policies_and_says_no_force_can_pass` and
    /// `a_held_ownership_byte_refuses_the_hatch_and_freeing_it_lets_one_through`
    /// (`crates/tf_tree/tests/rendezvous.rs`); the error's own `Display` now
    /// names which of the three states a caller is in.
    ///
    /// # What it leaves behind (§3.9, §11.3)
    ///
    /// - **The abandoned arena.** Its survivors keep their mappings and keep
    ///   publishing; §3.9 frees the segment only when the last one drops. Two
    ///   arenas, two `instance_uuid`s, diverging — chosen, not suffered.
    /// - **Their lock bytes**, in the same lock file. The new owner's slot
    ///   assigner skips a byte the kernel reports held, so those slot indices
    ///   are gone from the replacement's table until the survivors exit. The
    ///   escape hatch spends participant slots; it never recovers one.
    /// - **Their claim leases**, at [`crate::CLAIM_BASE`]` + edge_id` in that same
    ///   file (§6.1). The replacement numbers its edges from zero again, so a
    ///   writer claiming an id a survivor still holds wins the arena CAS on a
    ///   record that is genuinely free and then loses the lease — the
    ///   [`LockAttempt::Contended`] arm, which `tf_tree` surfaces as
    ///   `ClaimApiError::LeaseContended`. Retrying cannot clear it while the
    ///   survivor runs; this is the byte aliasing `docs/decisions/0005` §5 of
    ///   *Consequences* names.
    /// - **A crash mid-force is the original wedge again.** §11.3's
    ///   `open.after_create_before_bind` row promises the next `open()` finds
    ///   nothing alive and creates fresh, but that row assumes no participant
    ///   byte is held — the one state this policy is reached from. The orphan
    ///   memfd is still freed with its last mapping; the rendezvous is back
    ///   where it started, and only another `Always` gets past it.
    Always,
}

/// How `open()` resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenOutcome {
    /// A server was reachable and this process joined it.
    Joined,
    /// Nothing existed; this process won ownership and must now create the
    /// arena (§3.6), unlink any stale socket path, bind and listen.
    ///
    /// A stale socket path is expected rather than exceptional: it is what an
    /// owner that died leaves behind, it holds no state, and §3.9 makes it the
    /// job of whoever wins ownership to remove it.
    Created,
    /// This process already had the arena mapped and has now inherited the
    /// owner role: unlink the stale socket, bind, and serve its *existing* fd.
    /// It must not create a second arena (§3.4 step 3, §3.5).
    TookOver,
}

/// Whether a server is reachable at the socket path, and what it gave us.
///
/// Generic over what a successful probe yields, because the real probe does not
/// merely *observe* a server — it completes the §3.7 handshake and comes back
/// holding the segment fd. Splitting "is anyone there?" from "attach to them"
/// would mean connecting twice and re-running the race in between.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach<T> {
    /// The handshake succeeded.
    ///
    /// Carries whatever the probe obtained **and the slot the owner granted**.
    /// The slot lives in the variant rather than behind a separate accessor so
    /// that a probe cannot report success without saying which slot it was
    /// given — the two are one fact, and splitting them lets them disagree.
    Serving {
        /// What the probe obtained; for the real one, the attachment.
        attached: T,
        /// The participant slot the owner granted, which is also the lock-file
        /// byte this client must take.
        slot: u32,
    },
    /// `ECONNREFUSED`, no socket at all, or a server that died mid-handshake.
    ///
    /// **Not an error.** §3.9 makes a stale socket path an expected state and
    /// the ownership byte is the real discriminator, so all three collapse to
    /// the same verdict and the §3.4 loop carries on.
    Absent,
    /// The owner answered, and refused.
    ///
    /// Terminal, unlike [`Reach::Absent`]. §3.4's loop has no exit for a
    /// rejection, so a `LayoutMismatch` would otherwise be retried until the
    /// deadline and then reported as [`IpcError::ArenaHeldButUnreachable`] —
    /// the exact multi-hour debugging session §3.7 says the message exists to
    /// prevent.
    Rejected(IpcError),
}

/// Step 1 of the algorithm, injected.
///
/// The real implementation connects a `SOCK_SEQPACKET` and performs the §3.7
/// handshake. Modelling it as a trait keeps every §3.4 branch reachable from a
/// test — including the ones that only occur when a *live* arena is
/// simultaneously unreachable, which is precisely the split-brain race and is
/// otherwise a matter of winning a timing window on purpose.
pub trait ServerProbe {
    /// What a successful probe yields — for the real one, the attachment.
    type Attached;

    /// Try to reach a server bound at `sock`.
    ///
    /// # Errors
    ///
    /// Only for failures that are neither "nobody is listening" nor "the owner
    /// refused" — those are [`Reach::Absent`] and [`Reach::Rejected`].
    fn probe(&mut self, sock: &Path) -> Result<Reach<Self::Attached>, IpcError>;
}

/// A probe that always reports nothing listening.
///
/// The state of the world after an owner dies, and the state during the
/// split-brain race. Also the honest stand-in until §3.7 exists.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoServer;

impl ServerProbe for NoServer {
    type Attached = ();

    fn probe(&mut self, _sock: &Path) -> Result<Reach<()>, IpcError> {
        Ok(Reach::Absent)
    }
}

/// The builder from `docs/PHASE2.md` §3.2, minus the parts that need an arena.
#[derive(Clone, Debug)]
pub struct Open {
    rendezvous: Rendezvous,
    mode: AccessMode,
    create: CreatePolicy,
    timeout: Duration,
    already_attached: Option<u32>,
}

/// Default `open_timeout` (§3.4).
pub const DEFAULT_OPEN_TIMEOUT: Duration = Duration::from_secs(5);

/// First backoff interval for *this* crate's handshake retry. Doubles up to
/// [`MAX_BACKOFF`].
///
/// **Private, and deliberately so.** A revision of `docs/decisions/0019` §2b's
/// branch widened this pair to `pub` so the facade's `Open::await_open` could
/// share it. That is permanent public API on this crate bought for two numbers,
/// and it bought no guarantee: `await_open` retries whole rendezvous
/// *attempts*, each of which runs this loop inside it, so the two are nested
/// loops over different work and are free to disagree. The facade now keeps its
/// own pair in `tf_tree::tree`, and this one went back to being an
/// implementation detail.
const MIN_BACKOFF: Duration = Duration::from_micros(200);
/// Backoff ceiling — small enough that a 5 s timeout still gives hundreds of
/// attempts, so a takeover that completes in a millisecond is joined promptly.
const MAX_BACKOFF: Duration = Duration::from_millis(4);

/// The participant slot a creator takes, and the reason it is a named constant
/// rather than a `0` in one expression.
///
/// It is the *same integer* as the arena record `TreeBuilder::build_shared`
/// gives the creator, and the facade's liveness predicates index both with it —
/// `LivenessProbe::is_held(slot)` probes the lock byte while
/// `ParticipantTable` indexes the record. Two spellings of one number is how
/// they drifted (#201).
const CREATOR_SLOT: u32 = 0;

impl Open {
    /// Start from an already-resolved rendezvous.
    #[must_use]
    pub fn new(rendezvous: Rendezvous) -> Open {
        Open {
            rendezvous,
            mode: AccessMode::ReadOnly,
            create: CreatePolicy::IfAbsent,
            timeout: DEFAULT_OPEN_TIMEOUT,
            already_attached: None,
        }
    }

    /// Resolve the rendezvous from the environment (§3.1, §3.2).
    ///
    /// # Errors
    ///
    /// Anything [`Rendezvous::from_env`] can fail with.
    pub fn from_env() -> Result<Open, IpcError> {
        Ok(Open::new(Rendezvous::from_env()?))
    }

    /// Attach mode. [`AccessMode::ReadOnly`] is the consumer default (§8).
    #[must_use]
    pub fn mode(mut self, mode: AccessMode) -> Open {
        self.mode = mode;
        self
    }

    /// What to do when no arena exists.
    #[must_use]
    pub fn create(mut self, create: CreatePolicy) -> Open {
        self.create = create;
        self
    }

    /// How long to keep retrying before giving up with
    /// [`IpcError::ArenaHeldButUnreachable`].
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Open {
        self.timeout = timeout;
        self
    }

    /// Declare that this process **already has the arena mapped at participant
    /// slot `slot`** — it is an existing participant, and this call is a
    /// takeover after the owner died (§3.4 step 3, §3.5).
    ///
    /// This is what makes step 3 short-circuit past the split-brain check: a
    /// participant that already holds the arena is not at risk of creating a
    /// second one, it is the one thing that *cannot* be. Declaring it without
    /// actually holding an arena fd would defeat step 4, which is why it is an
    /// explicit, separately-named builder method rather than an inferred flag.
    ///
    /// # Why it carries the slot, and used to carry a `bool`
    ///
    /// Because the takeover arm has to return one, and the only correct value is
    /// the one the caller already holds.
    ///
    /// While this was `already_attached(true)` the arm called `register_any`,
    /// which takes the first **free** byte — so a survivor holding byte 5 and
    /// arena record 5 was handed a session on byte 0. **Executed, not derived:**
    /// `outcome=TookOver  session slot=0  but the caller's arena record is 5`.
    /// Every liveness predicate in the facade indexes the lock byte and the
    /// arena record with one integer (`docs/PHASE2.md` §5.1), so that session
    /// reports a running process as dead — issue #201's corrupting direction,
    /// and the half [`0035`] left open.
    ///
    /// **The correct value was already decided.** `0028` question 3, resolved
    /// 2026-08-20: *"the heir keeps its existing slot, byte and arena record,
    /// and takeover is byte 0 plus a `bind` and nothing else"* — byte 0 being
    /// the **ownership** byte. A heir that acquired a second slot "would arrange
    /// for its own live claims to be reaped", since the slot is baked into every
    /// claim it already holds. So the arm registers no participant at all, and
    /// this parameter is where its slot comes from.
    ///
    /// # The declaration is checked, and the first revision of this did not check it
    ///
    /// Taking a `u32` rather than a `bool` is not on its own enough, and review
    /// executed all three ways it was not. `open()` therefore **probes the
    /// declared byte once, before anything else**, and refuses with
    /// [`IpcError::NotAttachedAt`] if nobody holds it:
    ///
    /// * `already_attached_at(0)` from a process holding nothing returned
    ///   `TookOver slot=0` with byte 0 **free** — a session naming a record
    ///   whose byte nobody holds, which `docs/PHASE2.md` §0.0 calls the class
    ///   that reads dead to every probe-carrying observer.
    /// * `already_attached_at(u32::MAX)` returned `Ok(4294967295)`. The same
    ///   probe range-checks, so this now refuses too.
    /// * Against a **serving** owner, `already_attached_at(5)` returned
    ///   `Joined slot=1`: the join path assigned its own slot and ignored the
    ///   declaration. It no longer does — the §3.5 race where the other
    ///   survivor binds first is exactly when this arm matters most.
    ///
    /// With the check and all three arms honouring it, the guarantee is real:
    /// **no value of this argument produces a session whose slot the caller did
    /// not choose**, and no session at all unless that slot's byte is held.
    ///
    /// [`0035`]: https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0035-the-creators-slot-is-taken-not-found.md
    #[must_use]
    pub fn already_attached_at(mut self, slot: u32) -> Open {
        self.already_attached = Some(slot);
        self
    }

    /// The resolved rendezvous.
    #[must_use]
    pub fn rendezvous(&self) -> &Rendezvous {
        &self.rendezvous
    }

    /// Run the §3.4 algorithm.
    ///
    /// # Errors
    ///
    /// [`IpcError::ArenaHeldButUnreachable`] on timeout — a live arena exists
    /// but nothing serves it; [`IpcError::ArenaAbsent`] under
    /// [`CreatePolicy::Never`]; [`IpcError::NoParticipantSlots`] when the
    /// participant table is full; and any lock or directory failure.
    pub fn open<P: ServerProbe>(&self, probe: &mut P) -> Result<Session<P::Attached>, IpcError> {
        self.rendezvous.ensure_dir()?;
        let lock = LockFile::open(self.rendezvous.lock_path())?;
        let identity = Identity::of_self_best_effort(self.mode);

        // **The declared slot is checked once, here, and honoured on every path
        // below.** `already_attached_at(n)` asserts *"I already hold the arena
        // at slot `n`"*, and every arm then returns `n` instead of assigning a
        // slot — so if the assertion is false the session names a participant
        // record whose lock byte is free, which §0.0 calls the class that reads
        // dead to every probe-carrying observer. A peer's reaper CASes that
        // record away while its owner publishes.
        //
        // One `F_OFD_GETLK`, and it range-checks too: `probe_participant`
        // refuses a slot past `MAX_PARTICIPANTS` before probing. Both holes were
        // executed on the first revision of this change — `TookOver slot=0` with
        // byte 0 unheld, and `Ok(4294967295)` — which is why the check is up
        // here rather than in the one arm that first needed it.
        if let Some(slot) = self.already_attached {
            if !lock.probe_participant(slot)?.held {
                return Err(IpcError::NotAttachedAt { slot });
            }
        }

        let start = Instant::now();
        let mut backoff = MIN_BACKOFF;
        loop {
            // 1. Someone is already serving. Join.
            match probe.probe(self.rendezvous.sock_path())? {
                // Terminal. Retrying cannot change a version or layout
                // disagreement, and burning the deadline on it would replace a
                // precise message with a timeout.
                Reach::Rejected(why) => return Err(why),
                Reach::Serving { attached, slot } => {
                    // The owner named the slot, so §3.3's specified order —
                    // write the identity, *then* take the lock — is restorable
                    // here. It was not in the fallback below, which used to
                    // find a free slot itself and would race two writers onto
                    // one record — that fallback was `register_any`, deleted
                    // with issue #201's takeover arm.
                    // `None` means the byte the owner named is held by somebody
                    // the owner has not noticed leaving yet. Drop this
                    // attachment and go round again: the owner re-probes its
                    // table and will name a different slot. Nothing was written
                    // to the arena, so nothing is left behind — which is the
                    // point of taking the byte before touching it.
                    // **A declared slot wins here too.** A process that
                    // already holds the arena at slot `n` is already a
                    // participant; registering it a second time at the owner's
                    // chosen byte leaves it holding two bytes with its arena
                    // record at neither — the §3.5 race where the *other*
                    // survivor binds first, and the same divergence issue #201
                    // is about. Executed on the first revision of this change:
                    // `already_attached_at(5)` against a serving owner returned
                    // `Joined slot=1`. The owner's assignment is dropped rather
                    // than taken, which costs it nothing: nothing was written to
                    // the arena, so its assigner re-probes and grants the byte
                    // to somebody else.
                    if let Some(declared) = self.already_attached {
                        return Ok(Session {
                            outcome: OpenOutcome::Joined,
                            lock,
                            slot: declared,
                            owner: false,
                            attached: Some(attached),
                        });
                    }
                    if let Some(slot) = self.register_at(&lock, &identity, slot)? {
                        return Ok(Session {
                            outcome: OpenOutcome::Joined,
                            lock,
                            slot,
                            owner: false,
                            attached: Some(attached),
                        });
                    }
                }
                Reach::Absent => {}
            }

            // 2. Nobody is serving. Try to become the owner.
            if lock.try_take_ownership()? == LockAttempt::Acquired {
                // 3. We hold ownership and already have the arena, so this is a
                //    takeover: reuse it, and skip step 4 entirely. A process
                //    that already holds the arena is the one thing that cannot
                //    create a second one.
                if let Some(slot) = self.already_attached {
                    // **No participant registration.** `0028` question 3: the
                    // heir keeps the slot, byte and arena record it already has,
                    // and takeover is the ownership byte plus a `bind`. It is
                    // already a participant; taking a second byte here is what
                    // made this arm produce issue #201's divergence, and
                    // The identity record it would write is already written,
                    // by whatever attached this process in the first place.
                    return Ok(Session {
                        outcome: OpenOutcome::TookOver,
                        lock,
                        slot,
                        owner: true,
                        attached: None,
                    });
                }

                // 4. SPLIT-BRAIN CHECK. A held participant byte means a live
                //    arena exists whose holder has not taken over yet, so yield
                //    to it. Deterministic: no grace period, no timing
                //    assumption, no window to tune.
                if self.create != CreatePolicy::Always && lock.any_participant_held()? {
                    lock.release_ownership()?;
                } else if self.create == CreatePolicy::Never {
                    // Nothing is serving and nothing is alive, so there is
                    // genuinely no arena to join. Fail fast rather than wait out
                    // a timeout that cannot change the answer.
                    lock.release_ownership()?;
                    return Err(IpcError::ArenaAbsent);
                } else if let Some(slot) = self.register_creator(&lock, &identity)? {
                    // 5. Serve. The caller owes: memfd create + seal (§3.6),
                    //    unlink stale sock, bind, listen.
                    return Ok(Session {
                        outcome: OpenOutcome::Created,
                        lock,
                        slot,
                        owner: true,
                        attached: None,
                    });
                } else {
                    // Somebody took the creator's byte between step 4's scan and
                    // step 5's acquire. That is step 4's own condition arriving
                    // late, so it takes step 4's branch — release ownership and
                    // go round again. Nothing was built, so there is nothing to
                    // unwind.
                    lock.release_ownership()?;
                }
            }

            if start.elapsed() >= self.timeout {
                return Err(self.held_but_unreachable(&lock)?);
            }
            std::thread::sleep(backoff);
            backoff = core::cmp::min(backoff * 2, MAX_BACKOFF);
        }
    }

    /// Take a participant slot and write its identity record.
    ///
    /// **Deviation from §3.3's "written with `pwrite` before taking the slot
    /// lock", and it is deliberate.** That ordering presumes the slot is already
    /// known, which it is in the finished protocol: §3.7 has the *owner* assign
    /// the slot in its accept loop and return it in the `HelloResponse`. Until
    /// the socket exists, a joiner has to find a free slot itself, and
    /// write-then-lock loses the race it is meant to win — two processes both
    /// see slot 3 free, both write, one takes the lock, and the record now names
    /// the process that *lost*. A lock byte held with somebody else's name
    /// against it is worse than one held with no name yet: it makes
    /// [`IpcError::ArenaHeldButUnreachable`] point an operator at the wrong pid.
    ///
    /// So: lock, then write. The window where a slot is held with a stale record
    /// is a few microseconds long, and the record is advisory (§5.1) — the lock
    /// is the liveness.
    ///
    /// **This path is now only for a creator.** A joiner uses
    /// [`Open::register_at`], where §3.3's order *is* restored because the slot
    /// is known before anything is written, and a taker-over registers nothing
    /// at all — `0028` question 3, and see [`Open::already_attached_at`]. Take
    /// **the creator's slot**, or report that somebody else holds it.
    ///
    /// # Why this is not a scan for the first free byte
    ///
    /// A creator is the *first* participant — §3.4 step 4 refuses to create
    /// while any participant byte is held — so its slot is `0`, and every
    /// consumer of a created arena relies on that: the arena's first `FREE`
    /// record is `0` too, and the facade's liveness predicates index the lock
    /// byte and the arena record with **one** integer.
    ///
    /// A scan for the first free byte — which is what `register_any` did until
    /// issue #201 deleted it — is a *separate* pass from step 4's. Between them the correspondence is only a hope:
    /// `any_participant_held` probes byte 0 first and then 63 more before
    /// returning, so the window in which byte 0 can be taken by somebody else is
    /// the rest of that scan — 63 `F_OFD_GETLK` calls wide, not one instruction.
    /// Measured with a second open file description toggling byte 0, 4000
    /// iterations of exactly the two calls step 4 and step 5 make: **2242 took a
    /// non-zero byte** while the arena would have registered record 0.
    ///
    /// That is reachable from outside this workspace whatever this workspace
    /// does, because `LockFile::try_take_participant` is public API on a
    /// published crate.
    ///
    /// So the check and the take are **one operation**: a single `F_OFD_SETLK`
    /// on byte 0, whose atomicity is the kernel's. There is no window to close
    /// because there is no gap. `Ok(None)` means somebody else holds it, which
    /// is step 4's condition, so the caller takes step 4's branch.
    ///
    /// This is cheaper than what it replaces — one `SETLK` instead of a scan —
    /// and the diverged state stops being *detected* and becomes
    /// unrepresentable. The facade's `ParticipantSlotDiverged` guard stays where
    /// it is, now as an assertion this path cannot trip.
    ///
    /// **`CreatePolicy::Always` gets the same treatment on purpose.** It skips
    /// step 4, so `Ok(None)` there means a *live* participant holds the
    /// creator's byte. Forcing a fresh arena past one is the split brain
    /// `--force-new` is supposed to resolve, not cause; the caller loops and, if
    /// the holder persists, times out into
    /// [`IpcError::ArenaHeldButUnreachable`], which names it.
    ///
    /// **And byte 0 is free in exactly the case the escape hatch is for**, which
    /// is not the reason this comment used to give. It said the wedged arena has
    /// *dead* participants whose bytes the kernel already released — false, and
    /// backwards: a wedge **requires** a live holder, because if every holder
    /// were dead no participant byte would be held, step 4 would not fire, and
    /// an ordinary [`CreatePolicy::IfAbsent`] create would already succeed with
    /// no force involved. The real reason is the slot assignment `0035` makes
    /// exact: byte 0 is the *owner's*, held for the owner's whole life, and
    /// joiners get `>= 1`. So a free byte 0 with a held byte `>= 1` means the
    /// owner is gone and a non-owner survived — §3.4's stranded participant, the
    /// case the hatch resolves — and a held byte 0 is somebody on the creator's
    /// slot, which no force can pass. Usually that is the owner; it need not be,
    /// because [`Session::release_ownership`] keeps byte 0 while giving up the
    /// role, and `defect_201_release_ownership_strands_a_live_non_owner_on_byte_0`
    /// pins that state. The refusal is the same either way, which is the point
    /// of predicating it on the byte rather than on who is meant to hold it.
    /// Measured by
    /// `a_live_byte_0_refuses_both_policies_and_says_no_force_can_pass`
    /// (`crates/tf_tree/tests/rendezvous.rs`): with byte 0 held both policies
    /// return the *same* error; move the held byte to 3 and `Always` creates.
    fn register_creator(
        &self,
        lock: &LockFile,
        identity: &Identity,
    ) -> Result<Option<u32>, IpcError> {
        if lock.try_take_participant(CREATOR_SLOT)? == LockAttempt::Contended {
            return Ok(None);
        }
        lock.write_identity(CREATOR_SLOT, identity)?;
        Ok(Some(CREATOR_SLOT))
    }

    /// Take the slot the owner named, in §3.3's specified order.
    ///
    /// Write the identity record first, then the lock byte. That ordering is
    /// safe here and was unsafe in the deleted `register_any`, and the
    /// difference is who chose the slot: nobody else is racing us for *this* byte, because the
    /// owner hands each client a different one. So the record can be in place
    /// before the byte is held, and a reader that sees the byte held never sees
    /// it nameless.
    ///
    /// Returns `None` if the byte is already held — the owner named a slot whose
    /// previous holder it has not seen leave yet. The caller retries the
    /// handshake rather than falling back to another slot: falling back would
    /// re-open the split between the byte and the arena record that
    /// `register_at` exists to close.
    fn register_at(
        &self,
        lock: &LockFile,
        identity: &Identity,
        slot: u32,
    ) -> Result<Option<u32>, IpcError> {
        lock.write_identity(slot, identity)?;
        match lock.try_take_participant(slot)? {
            LockAttempt::Acquired => Ok(Some(slot)),
            LockAttempt::Contended => Ok(None),
        }
    }

    /// Build the timeout error, naming the slots an operator has to deal with.
    ///
    /// **The ownership probe is what makes the message actionable.** Every
    /// caller reaching this line has released the ownership byte again — step 2
    /// either never acquired it, or one of step 4's / step 5's branches gave it
    /// back — so `F_OFD_GETLK` here reports somebody *else*, which is precisely
    /// the bit that decides whether [`CreatePolicy::Always`] could help: it has
    /// to take that byte before it reaches the participant bytes it is allowed
    /// to skip. Read at the deadline and advisory like the identity record, not
    /// a claim about the whole timeout.
    fn held_but_unreachable(&self, lock: &LockFile) -> Result<IpcError, IpcError> {
        let holder_slots = lock.held_participants()?;
        // `trailing_zeros()` is 64 on an empty mask, which is not a slot any
        // arena has. `Display` happened to special-case the empty mask, but the
        // field is public and a supervisor logging it would have recorded a
        // fictional slot — so make "no holder" unrepresentable instead.
        let first = (holder_slots != 0).then(|| holder_slots.trailing_zeros());
        let first_pid = match first {
            Some(slot) => lock.read_identity(slot)?.map_or(0, |id| id.pid),
            None => 0,
        };
        Ok(IpcError::ArenaHeldButUnreachable {
            holder_slots,
            first_slot: first,
            first_pid,
            ownership_held: lock.probe_ownership()?.held,
        })
    }
}

/// The result of a successful `open()`: the outcome, and the locks that make it
/// true.
///
/// Dropping a `Session` closes the lock file, which releases the ownership and
/// participant bytes — the same thing the kernel does when the process dies, by
/// the same mechanism. There is no separate teardown to forget.
#[derive(Debug)]
pub struct Session<A = ()> {
    outcome: OpenOutcome,
    lock: LockFile,
    slot: u32,
    owner: bool,
    attached: Option<A>,
}

impl<A> Session<A> {
    /// How `open()` resolved, and therefore what the caller owes: nothing for
    /// [`OpenOutcome::Joined`], create-and-bind for [`OpenOutcome::Created`],
    /// bind-only for [`OpenOutcome::TookOver`].
    #[must_use]
    pub fn outcome(&self) -> OpenOutcome {
        self.outcome
    }

    /// This process's participant slot.
    ///
    /// **Whether *this* `Session` holds the byte depends on how `open()`
    /// resolved.** For [`OpenOutcome::Created`] and [`OpenOutcome::Joined`]
    /// without a declaration, this session took the byte and releases it on
    /// drop. Where [`Open::already_attached_at`] was used — every
    /// [`OpenOutcome::TookOver`], and a `Joined` that raced a serving owner —
    /// the byte is held by whatever attached this process in the first place,
    /// and **this session neither took it nor releases it**. `open()` verifies
    /// it is held before returning, so the slot always names a held byte; what
    /// it does not do is take a second one.
    ///
    /// The distinction matters to exactly one caller: a heir must not drop the
    /// attachment that owns the byte on the strength of holding this session.
    /// It would leave a `LIVE` arena record over a free byte, which reads dead
    /// to every probe-carrying observer and is reaped mid-publish.
    #[must_use]
    pub fn slot(&self) -> u32 {
        self.slot
    }

    /// Whether this process holds byte 0 and is therefore the owner.
    #[must_use]
    pub fn is_owner(&self) -> bool {
        self.owner
    }

    /// The lock file, for identity lookups and (later) claim locks.
    #[must_use]
    pub fn lock_file(&self) -> &LockFile {
        &self.lock
    }

    /// Give up the owner role while staying attached (§3.5).
    ///
    /// Ownership is a role, not a property of the arena: releasing byte 0 lets
    /// another participant take over. **Lookups do not stop, slow down, or
    /// observe anything during a takeover** — the data plane touches only the
    /// mapping, and ownership lives entirely in the control plane.
    ///
    /// # Errors
    ///
    /// [`IpcError::LockFailed`].
    pub fn release_ownership(&mut self) -> Result<(), IpcError> {
        if self.owner {
            self.lock.release_ownership()?;
            self.owner = false;
        }
        Ok(())
    }

    /// Take what the §3.7 handshake yielded, if this session joined one.
    ///
    /// `None` for [`OpenOutcome::Created`] and [`OpenOutcome::TookOver`]: both
    /// already have the arena and never ran a handshake.
    ///
    /// Taking rather than borrowing, because the payload owns file descriptors —
    /// the segment and the connection whose closure tells the owner this
    /// participant is gone (D17). Leaving it inside the `Session` would make the
    /// lifetime of the liveness signal the lifetime of a struct the caller has
    /// no reason to keep.
    pub fn take_attached(&mut self) -> Option<A> {
        self.attached.take()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::error::EnvVar;
    use crate::rendezvous::ArenaName;
    use crate::runtime_dir::{current_uid, RuntimeDir};

    fn rendezvous(tag: &str) -> (Rendezvous, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("tf_tree_ipc_open-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let rd = RuntimeDir::resolve_with(&EnvOverride(dir.clone()), current_uid()).unwrap();
        let rv = Rendezvous::new(rd, 0, ArenaName::new("default", EnvVar::Name).unwrap());
        // Tests that pre-seed a lock (a surviving participant, a process
        // mid-bind) open the lock file before `Open::open` would have created
        // the directory.
        rv.ensure_dir().unwrap();
        (rv, dir)
    }

    struct EnvOverride(std::path::PathBuf);

    impl crate::runtime_dir::EnvLookup for EnvOverride {
        fn var(&self, key: &str) -> Option<std::ffi::OsString> {
            (key == "TF_TREE_RUNTIME_DIR").then(|| self.0.clone().into_os_string())
        }
    }

    /// A probe that reports "serving" only after `n` calls, so a test can make
    /// the owner appear mid-loop. Grants slot `1`, since the creator in these
    /// tests already holds slot `0`.
    /// Probe a participant byte from a *third* description, so the answer is
    /// about the file rather than about whoever this test is holding.
    fn lock_probe(path: &Path, slot: u32) -> bool {
        LockFile::open(path)
            .unwrap()
            .probe_participant(slot)
            .unwrap()
            .held
    }

    struct ServingAfter(u32);

    impl ServerProbe for ServingAfter {
        type Attached = u32;

        fn probe(&mut self, _sock: &Path) -> Result<Reach<u32>, IpcError> {
            if self.0 == 0 {
                return Ok(Reach::Serving {
                    attached: 1,
                    slot: 1,
                });
            }
            self.0 -= 1;
            Ok(Reach::Absent)
        }
    }

    #[test]
    fn nothing_present_means_create() {
        let (rv, dir) = rendezvous("create");
        let s = Open::new(rv).open(&mut NoServer).unwrap();
        assert_eq!(s.outcome(), OpenOutcome::Created);
        assert!(s.is_owner());
        assert_eq!(s.slot(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_reachable_server_means_join() {
        let (rv, dir) = rendezvous("join");
        let s = Open::new(rv).open(&mut ServingAfter(0)).unwrap();
        assert_eq!(s.outcome(), OpenOutcome::Joined);
        assert!(!s.is_owner(), "a joiner must not hold byte 0");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_second_opener_joins_rather_than_creating() {
        let (rv, dir) = rendezvous("second");
        let first = Open::new(rv.clone()).open(&mut NoServer).unwrap();
        assert_eq!(first.outcome(), OpenOutcome::Created);
        // The first process is now serving, so the second one's probe succeeds.
        let second = Open::new(rv).open(&mut ServingAfter(0)).unwrap();
        assert_eq!(second.outcome(), OpenOutcome::Joined);
        assert_ne!(first.slot(), second.slot(), "slots must be distinct");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_split_brain_check_refuses_to_create() {
        // §3.4 step 4, the whole design. A participant byte is held (a surviving
        // participant of an arena whose owner died) and nothing is serving. A
        // fresh `open()` must NOT create.
        let (rv, dir) = rendezvous("split-brain");
        let survivor = LockFile::open(rv.lock_path()).unwrap();
        assert_eq!(
            survivor.try_take_participant(2).unwrap(),
            LockAttempt::Acquired
        );

        let err = Open::new(rv.clone())
            .timeout(Duration::from_millis(50))
            .open(&mut NoServer)
            .unwrap_err();
        match err {
            IpcError::ArenaHeldButUnreachable {
                holder_slots,
                first_slot,
                ..
            } => {
                assert_eq!(holder_slots, 1 << 2);
                assert_eq!(first_slot, Some(2));
            }
            other => panic!("expected ArenaHeldButUnreachable, got {other}"),
        }

        // And the yielded ownership byte was released, so the survivor can take
        // over the instant it notices.
        let taker = LockFile::open(rv.lock_path()).unwrap();
        assert_eq!(taker.try_take_ownership().unwrap(), LockAttempt::Acquired);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// **A declared slot is verified once and honoured on every arm** — issue
    /// #201, and the three ways the first revision of its fix was wrong.
    ///
    /// `already_attached_at(n)` asserts *"I already hold the arena at slot n"*,
    /// and every path then returns `n` instead of assigning a slot. That is only
    /// safe if the assertion is true, and the first revision checked nothing:
    ///
    /// * **The byte need not have been held.** Executed: `already_attached_at(0)`
    ///   from a process holding nothing returned `TookOver slot=0` with byte 0
    ///   free — a session naming a participant record whose byte nobody holds,
    ///   which `docs/PHASE2.md` §0.0 calls the class that reads dead to every
    ///   probe-carrying observer. A peer's reaper CASes it away mid-publish.
    /// * **The slot need not have been in range.** Executed:
    ///   `already_attached_at(u32::MAX)` returned `Ok(4294967295)`, handed out
    ///   through public API to a caller that indexes a 64-record table with it.
    /// * **A serving owner overrode it.** Executed: against `ServingAfter(0)`,
    ///   `already_attached_at(5)` returned `Joined slot=1` — the §3.5 race where
    ///   the other survivor binds first, reproducing #201's divergence on the
    ///   arm that was supposed to have closed it.
    ///
    /// Mutants, run: delete the `probe_participant` guard — the first two cases
    /// return `Ok`; delete the `already_attached` arm in `Reach::Serving` — the
    /// third returns slot 1.
    #[test]
    fn a_declared_slot_is_verified_and_honoured_on_every_arm() {
        // (a) Declared but unheld: refused rather than minted.
        let (rv, dir) = rendezvous("declared_unheld");
        let err = Open::new(rv)
            .already_attached_at(0)
            .timeout(Duration::from_millis(50))
            .open(&mut NoServer)
            .unwrap_err();
        assert!(
            matches!(err, IpcError::NotAttachedAt { slot: 0 }),
            "a declaration nobody backs must be refused, not turned into a \
             session whose byte is free: {err:?}"
        );
        std::fs::remove_dir_all(dir).unwrap();

        // (b) Out of range: the same probe range-checks before it probes.
        let (rv, dir) = rendezvous("declared_range");
        let err = Open::new(rv)
            .already_attached_at(u32::MAX)
            .timeout(Duration::from_millis(50))
            .open(&mut NoServer)
            .unwrap_err();
        assert!(
            matches!(err, IpcError::NoParticipantSlots { .. }),
            "an out-of-range slot reached a Session: {err:?}"
        );
        std::fs::remove_dir_all(dir).unwrap();

        // (c) A serving owner does not override the declaration.
        let (rv, dir) = rendezvous("declared_serving");
        let other = LockFile::open(rv.lock_path()).unwrap();
        other.try_take_participant(5).unwrap();
        let s = Open::new(rv)
            .already_attached_at(5)
            .timeout(Duration::from_millis(50))
            .open(&mut ServingAfter(0))
            .unwrap();
        assert_eq!(s.outcome(), OpenOutcome::Joined);
        assert_eq!(
            s.slot(),
            5,
            "a serving owner's slot assignment overrode the caller's own: it \
             would hold two bytes with its arena record at neither"
        );
        drop(s);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_survivor_that_holds_the_arena_takes_over_instead() {
        // Same world as the previous test, but from the survivor's side: step 3
        // short-circuits, so it takes over rather than yielding forever.
        let (rv, dir) = rendezvous("takeover");
        let lock_path = rv.lock_path().to_path_buf();
        let other = LockFile::open(rv.lock_path()).unwrap();
        other.try_take_participant(5).unwrap();

        let s = Open::new(rv)
            .already_attached_at(5)
            .timeout(Duration::from_millis(50))
            .open(&mut NoServer)
            .unwrap();
        assert_eq!(s.outcome(), OpenOutcome::TookOver);
        assert!(s.is_owner());
        // **The heir keeps its own slot** (`0028` question 3), which is the half
        // of issue #201 `0035` left open. This arm used to call `register_any`
        // and hand back the first *free* byte — executed at the time as
        // `session slot=0` against a caller whose arena record was 5 — and every
        // liveness predicate indexes the byte and the record with one integer,
        // so that session reported a running process as dead.
        assert_eq!(
            s.slot(),
            5,
            "the takeover handed back a slot the caller did not declare: its \
             lock byte and its arena record now name different participants"
        );
        // **And it registered nothing**, which is the change rather than its
        // consequence: `0028` question 3 makes takeover the ownership byte plus
        // a `bind`, because the heir already is a participant. Asserting only
        // the slot leaves a mutant that takes a free byte and then overwrites
        // the returned value passing.
        for slot in 0..8 {
            assert_eq!(
                lock_probe(&lock_path, slot),
                slot == 5,
                "participant byte {slot} after a takeover: the arm took a byte \
                 it had no business taking"
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn create_never_fails_fast_when_nothing_exists() {
        let (rv, dir) = rendezvous("never");
        let err = Open::new(rv.clone())
            .create(CreatePolicy::Never)
            .timeout(Duration::from_secs(30))
            .open(&mut NoServer)
            .unwrap_err();
        assert_eq!(err, IpcError::ArenaAbsent);
        // It must not have left byte 0 held on the way out.
        let taker = LockFile::open(rv.lock_path()).unwrap();
        assert_eq!(taker.try_take_ownership().unwrap(), LockAttempt::Acquired);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// **The escape hatch is this policy, not a flag.** The name this test used
    /// to carry — `force_new_is_the_documented_escape_hatch` — named
    /// `RUNBOOK.md`'s `--force-new`, which no binary has ever had (#189), so the
    /// one test covering the capability was named after the one thing about it
    /// that was untrue. What it exercises is the branch: the loudness §3.4 asks
    /// for belongs to whoever sets the policy, and nothing here can assert it.
    ///
    /// The survivor keeps byte 1 throughout — the created session must take a
    /// different one, because the two arenas share the lock file and the byte is
    /// the kernel's, not the arena's.
    #[test]
    fn create_always_overrides_the_split_brain_check() {
        let (rv, dir) = rendezvous("force");
        let survivor = LockFile::open(rv.lock_path()).unwrap();
        survivor.try_take_participant(1).unwrap();
        // IfAbsent refuses...
        assert!(Open::new(rv.clone())
            .timeout(Duration::from_millis(30))
            .open(&mut NoServer)
            .is_err());
        // ...and Always is the explicit override.
        let s = Open::new(rv)
            .create(CreatePolicy::Always)
            .open(&mut NoServer)
            .unwrap();
        assert_eq!(s.outcome(), OpenOutcome::Created);
        assert_ne!(s.slot(), 1, "the escape hatch took the survivor's byte");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// **The escape hatch abandons an *unreachable* arena, never a served one.**
    ///
    /// Step 1 runs before the create decision, so a rendezvous with a server
    /// answering is joined whatever the policy says. That is what bounds the
    /// damage — the doc on [`CreatePolicy::Always`] claims it, and a caller
    /// reaching for the hatch is entitled to have it tested rather than argued.
    #[test]
    fn create_always_still_joins_a_reachable_server() {
        let (rv, dir) = rendezvous("force-reachable");
        let s = Open::new(rv)
            .create(CreatePolicy::Always)
            .open(&mut ServingAfter(0))
            .unwrap();
        assert_eq!(s.outcome(), OpenOutcome::Joined);
        assert!(!s.is_owner(), "a joiner must not hold byte 0");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_contended_ownership_byte_is_retried_not_failed() {
        // Another process is mid-bind: it holds byte 0 but is not yet serving.
        // The opener must back off and retry, and join once the socket appears.
        let (rv, dir) = rendezvous("mid-bind");
        let binder = LockFile::open(rv.lock_path()).unwrap();
        binder.try_take_ownership().unwrap();

        let s = Open::new(rv)
            .timeout(Duration::from_secs(2))
            .open(&mut ServingAfter(3))
            .unwrap();
        assert_eq!(s.outcome(), OpenOutcome::Joined);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ownership_can_be_released_without_detaching() {
        let (rv, dir) = rendezvous("migrate");
        let mut s = Open::new(rv.clone()).open(&mut NoServer).unwrap();
        assert!(s.is_owner());
        s.release_ownership().unwrap();
        assert!(!s.is_owner());

        let heir = LockFile::open(rv.lock_path()).unwrap();
        assert_eq!(heir.try_take_ownership().unwrap(), LockAttempt::Acquired);
        // The participant byte is still held: releasing the role is not
        // detaching.
        assert!(heir.probe_participant(s.slot()).unwrap().held);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
