# tf_tree — Phase 2 Implementation Specification: Shared Memory

> **Companion documents:** `docs/PROJECT.md` (vision, roadmap, decision log) and `docs/PHASE1.md` (single-process core). Read §1 of this document before writing any Phase 2 code — it contains mandatory amendments to the Phase 1 design that were discovered by working the multi-process failure modes through.

**Deliverable:** the same arena, mapped into N processes, with the *identical unmodified* reader code from Phase 1 running against it. Plus the lifecycle, liveness, and fault-tolerance machinery that makes that safe when processes die at arbitrary points.

**Framing.** Phase 1 was the easy half. In one process, a crash takes down every reader with it, so a torn write is unobservable. Across processes it is not: a writer can be `SIGKILL`ed between two stores and leave a data structure permanently wedged while sixteen readers keep running. **Every mutation protocol in the arena must therefore be crash-consistent — there must be no state a dead process can leave behind that a live process cannot detect and repair.** That single requirement drives most of this document.

Sections marked **NORMATIVE** are requirements. Where a syscall behaviour is asserted, it has been verified on Linux 6.18; the probe is reproduced in Appendix B so you can re-run it on your target kernel.

---

## 0.0 Implementation status

**Implemented**, except for `tf_tree serve` (§9), which its own row records as
declined and not scheduled. **§10's recorder and §7.4's memory locking are
declined by record** —
[`0047`](./decisions/0047-the-recording-this-reader-would-refuse.md) and
[`0049`](./decisions/0049-the-flag-that-prefaults-the-arena.md) — so they are
work *not owed* rather than work pending. §11.4's long-running fault harness and
§11.3's crash points are **done**, and their rows say what each does and does not
reach. The
rendezvous, the attach protocol, claims-as-leases, reaping, fork poisoning and
per-region population all landed under
[`0005`](./decisions/0005-the-shared-memory-seam.md). **§3.5's ownership
migration landed on 2026-08-28**, in the shape
[`0037`](./decisions/0037-a-takeover-is-not-a-second-open.md) argued for; its row
below states that shape, and states the one thing about it that is *not*
automatic.

**This sentence keeps its corrections rather than overwriting them**, because a
preamble narrower than its own table is how a reader stops at the summary. It
first read "except for the daemon/tooling surface and the long-running fault
harness", and named ownership migration among what landed — wrong in both halves:
§3.5's takeover half was **deleted** on 2026-08-27 (#275, `0037`) and §7.4 was
never built at all. It then read "except for … **§3.5's takeover half**", which
was true for one day: the replacement landed on 2026-08-28, so the exclusion is
dropped rather than kept as a hedge. It then listed §10, §7.4 and §11.4 as
outstanding while their rows said, respectively, declined, declined and done.
The rows below are the statement of all of it, and where a row and this preamble
disagree the row wins.

| Area | Status |
|---|---|
| Amendments A1–A8 (§1) | **Applied** — `FORMAT_VERSION` 3 |
| `MappedArena` — `memfd`, sealed, `MAP_SHARED`, `MADV_DONTFORK`/`HUGEPAGE` (§4) | **Done** (`tf_tree_arena::mapped`, behind `--features shm`) |
| `TreeBuilder::build_shared` / `Tree::attach_shared`, read-only mode (§8) | **Done** |
| Zero-diff read path, proven by the relocation gate (§4) | **Done, and tested** (`just shm-test`) |
| Multi-process read scaling (part of §12.2) | **Done** (`just shm-scaling`; results in `docs/benchmarks/tf2.md`) |
| Amendment A2 — in-arena topology lock | **Applied, and it is no longer only in-arena** ([`0029`](./decisions/0029-the-topology-lock-is-a-kernel-lock.md), #213). `Tree::reparent` holds it; bounded spin, loom- and multi-process-tested. What changed is what licenses the steal: on a tree that carries a lock file the mutator takes **§3.3's byte 1 first** and the arena word second, releasing them in the other order, so A2's exclusion is a kernel fact and a live holder is refused before any `/proc` inference runs. The word is kept, not demoted — it is the only exclusion a lock-file-less tree has, and it is what a byte-holding and a byte-less mutator still contend on. The liveness predicate is kept too, as a **residual**: holding the byte means a non-zero word belongs to a holder that is either dead or has no lock file, and `/proc` decides only the second, in the safe direction only. §11.3's `topo.holding_lock` row is the crash walk D15 makes this owe |
| Amendment A8 — bounded intern spin | **Applied** — `claiming` array, bounded spin, takeover of a dead claimant |
| `instance_uuid` (§3.6 step 4, A7) | **Done** — header offset 136, in pre-existing alignment padding |
| Discovery, rendezvous, `open()` (§3.1–§3.4) | **Done** (`tf_tree_ipc`, `tf_tree::open`) |
| §3.4's `--force-new` escape hatch | **The capability shipped; the flag never existed.** It is `CreatePolicy::Always`, settable on `tf_tree::Open`, and it skips the split-brain check as §3.4 asks. §3.4's other adjective is not met: the policy is explicit, but nothing in `tf_tree_ipc` or `tf_tree` is *loud* about taking it — no holder is named on the way past — because loudness belongs to whatever offers the escape hatch to a human, and nothing does. No binary in the workspace carries a flag of that name, and `tf_tree_cli` cannot usefully grow one: it supplies no `layout_if_creating`, so every create path it can reach ends in `OpenError::NoLayoutToCreate` — measured, against a rendezvous wedged with a held participant byte — and it exits, so an arena it did create would not outlive the command. A flag arrives with the subcommand that owns a topology and stays running, which is [`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md) §1's `tf_tree serve`. An earlier revision of this row gave a second reason to hold — that this policy is one of the paths on which a process's lock byte and its arena participant record get different indices (#201) — and **that was wrong, so it is retracted here rather than deleted.** Measured: an owner plus two read-write survivors holds bytes `[0, 1, 2]`, `SIGKILL`ing the owner leaves `[1, 2]`, and the forced creator then takes byte 0 against record 0. They agree, because the owner holds record 0 and byte 0 for its whole life and the assigner skips slot 0 for every joiner, so no survivor can hold byte 0 when the owner dies. #201 is real, and the live non-owner holder of byte 0 it needs is **no longer hypothetical**: `tf_tree_ipc::Session::release_ownership` produces one from a documented §3.5 call on a published crate, and the divergence has since been reproduced through shipped public API — see the participant-registry row below. The retraction above stands for exactly what it retracted, the owner-death route; what was wrong was concluding from it that nothing outside a test produces the state. **`0035`'s correction of its own first draft was over-broad, and this row is where that is retracted too** (#257). `0035` is right about what it retracts: the justification it had written — that the wedged arena `--force-new` is for has *dead* participants whose bytes the kernel released — is false, and backwards, because a wedge requires a live holder and an arena all of whose holders are dead is created over by an ordinary `IfAbsent` open with no force involved. **What does not stand is the step it takes to get there.** `0035` writes that §3.4 "offers `--force-new` as the escape hatch for a participant that is `SIGSTOP`ped — alive, holding its byte, never taking over" and concludes "against a live holder of byte 0, it does not abandon anything". Both halves are true and they are about **different bytes**: a `SIGSTOP`ped *participant* is a joiner, and joiners are assigned slots `>= 1`, so the stranded participant §3.4 names is not a holder of byte 0. Read in its own scope, "None of the three delivers the documented escape hatch" is a claim about three revisions of one test whose staged state is a live byte-0 holder, and in that scope it is right; it is the slide from that state to §3.4's that does not hold. The hatch does deliver, in exactly the case §3.4 names: `the_escape_hatch_creates_over_a_stranded_participant` (`crates/tf_tree/tests/rendezvous.rs`) strands a joiner's byte, refuses an ordinary create, and then creates under `CreatePolicy::Always` — measured passing at this revision. `0035` reasons from one staged state, a live holder of **byte 0**, and byte 0 is the *owner's* slot for the owner's whole life while joiners are assigned `>= 1`, so that state is an owner that is still there and not the stranded participant §3.4 is about. The rule the code implements is that a forced create passes iff nothing is serving **and** the ownership byte is free **and** byte 0 is free — one variable each way, measured in `a_live_byte_0_refuses_both_policies_and_says_no_force_can_pass` and `a_held_ownership_byte_refuses_the_hatch_and_freeing_it_lets_one_through`. `0035` remains `implemented` and untouched; the retraction lives here, the way the retraction earlier in this row does for this row's own first version. `IpcError::ArenaHeldButUnreachable` gained `ownership_held` so the three states are distinguishable in the message an operator reads (#257) — a breaking change to a variant on a published crate, taken deliberately on the `0.0.x` line. `RUNBOOK.md` names the policy; §3.4's prose still names the flag, and this row is what it is read against (#189); §5.1's sentence was corrected against this row. |
| Attach protocol — `SOCK_SEQPACKET` + `SCM_RIGHTS` (§3.7) | **Done** — owner serves from a thread, not a daemon |
| Ownership migration (§3.5) | **Done as of 2026-08-28. The mechanism is complete; the *trigger* is the caller's, and that is a design constraint rather than an omission.** Ownership is byte 0 and the kernel releases it when the owner dies — and a survivor now takes it **without reopening anything**. `Session::take_over_ownership` (`crates/tf_tree_ipc/src/open.rs:696`) takes byte 0 on *the file description this session already holds*, so its slot, its participant byte and its arena record cannot move and cannot disagree: the invariant is structural rather than checked, which is what `0028` question 3 was reaching for. `Tree::inherit_ownership` (`crates/tf_tree/src/open.rs:560`) is the facade seam `0037` question 1 asked for — it has the arena, the segment fd and the `Rendezvous`, which `Attachment::Joined` now retains and previously did not — and it binds the rendezvous socket over the **existing** segment, returning `Inheritance::{Inherited, OwnerAlive, Contended, ReadOnly, NotApplicable}`. Racing survivors need no arbitration (`0037` question 2): one takes the byte, the other gets `Contended` **with its slot intact**, which is precisely what the deleted arm could not do. Every error path restores the attachment and releases byte 0, so a failed inheritance leaves a plain participant rather than an owner that is not serving — the state that makes an arena unjoinable. A read-only attachment answers `ReadOnly` and cannot be the heir: an owner writes the participant table on every grant and a `PROT_READ` mapping cannot, which is D18 working. **The trigger had never existed at all, and is the other half of this**: `Tree::owner_lost` (`crates/tf_tree/src/tree.rs:3023`) over `tf_tree_ipc::peer_hung_up` (`crates/tf_tree_ipc/src/client.rs:75`) is a zero-timeout `poll` for `POLLHUP`/`POLLERR` on the attach socket — the same hangup the owner reads from its own end (D17). `EINTR` reads as *no hangup*, and `owner_lost` maps a `poll` failure to `false` as well, because reporting one would send a survivor to take an arena whose owner is alive and serving, which is the split-brain §3.4 exists to prevent. Three tests in `crates/tf_tree/tests/rendezvous.rs`: `a_survivor_inherits_ownership_and_the_arena_becomes_joinable_again` (:4081) reproduces `ArenaHeldButUnreachable` with the owner dead **first**, then inherits, then joins a fresh process and compares the transform bit for bit against what the dead owner wrote; `two_survivors_race_and_exactly_one_inherits` (:4176) asserts both survivors report the **same slot before and after**, whichever won — `0037` question 2 made executable, and the property the deleted arm could not hold; and `a_read_only_survivor_reports_that_it_cannot_inherit` (:4267) pins the `ReadOnly` refusal and that a read-only consumer keeps reading through it. **What is still owed, and it is policy and coverage rather than mechanism.** Nothing calls the trigger for you: there is no background thread and no daemon, per [`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md) — every process a user is *required* to run is a place adoption dies, and a thread per attachment is the library-shaped version of that cost — so a survivor evaluates `owner_lost()` in its own loop, and **an arena whose survivors never call it stays ownerless and wedges new joiners exactly as it did before**. §11.3's `takeover.after_ownership_lock_before_bind` crash point **is** placed and executed (that row). **`shm_torture` kills the owner since 2026-09-04, and this sentence read “still does not” until then** — a status claim that outlived its cause by a week, in the row whose own subject is a mechanism that shipped and was exercised by nothing. §11.4's row carries what the harness now does: the owner is a child, every survivor evaluates `owner_lost` in its own loop, and each kill must be followed by a *fresh* process joining and by a survivor recording an inheritance or the run fails. Measured 2026-09-04 over six seeds at `--duration 20s --children 5 --kill-hz 5` and four at `--duration 30s --children 6 --kill-hz 6`: every migration recovered and every one was answered by a recorded inheritance. **The `SIGKILL`-to-fresh-join interval this row published is deleted rather than re-narrowed.** It read `0.4–0.7 ms`, both ends were falsified within a day — a review run observed below the floor and the same session's own report quoted a ceiling three times the top — and the replacement was a wider interval from one afternoon's sweep. It is a scheduling quantity taken on a host that is also running the fleet, so a fixed figure here expires without saying so; the run prints its own mean and worst per migration, and §11.4's row says what the harness actually asserts. **A survivor that does *not* inherit used to spin forever, and stopped on 2026-08-29 ([`0043`](./decisions/0043-owner-lost-is-a-question-about-the-owner.md)).** Its attach socket still names the dead owner and stays hung up for the life of the process, so while `Tree::owner_lost` polled only that socket it answered `true` permanently and the documented poll-and-inherit loop re-attempted an `F_OFD_SETLK` on byte 0 **every control cycle** — on a fleet of *N* read-write survivors, *N−1* processes doing a syscall per cycle to be told each time that somebody else owned it. `owner_lost` now follows a hangup with `F_OFD_GETLK` on byte 0 of the lock file the session already holds (`Session::ownership_held`), so the three states separate: owner alive (`poll` alone, no probe), role vacant (`true`, inherit), and **role taken or mid-bind** (`false`) — the third being the one *N−1* survivors are in and the one that did not previously exist. The healthy path is unchanged: the `poll` is `false` and the probe never runs. **And a loser stays eligible**: if the new owner dies too the kernel frees byte 0 and the next call answers `true` again, so inheritance chains with nothing latched — which is why latching the loser was never the fix, and is what §3.5's *"retry connect with backoff"* was reaching for. `a_survivor_that_did_not_inherit_stops_being_told_the_owner_is_gone` is the three-poke test, deliberately serialised so the middle answer is a fact about the code and not about the scheduler. **One observable change in correct behaviour, recorded because a passing test caught it and this row did not predict it**: a survivor that polls after the winner has taken the byte now reports `OwnerAlive` where it used to report `Contended`, so `two_survivors_race_and_exactly_one_inherits` accepts either — `Contended` remains reachable for a genuine tie. **What is still not implemented, deliberately**: §3.5's literal reconnect, which would need a new wire message (requirement 2 forbids a survivor registering a second time), and whose only remaining benefit is that the new owner learns of *this* participant's death promptly. Its lock byte still answers — the authoritative fact since [`0029`](./decisions/0029-the-topology-lock-is-a-kernel-lock.md) — so the slot assigner and `Tree::reap_participants` still reclaim its slot, a grant or a sweep later. That is a reclamation-latency residue, not a correctness gap, and `0043` records it as the cost of not reconnecting. **The history below is kept, because it is why the shape above is what it is.** The first takeover half was **deleted** on 2026-08-27 (#275, [`0037`](./decisions/0037-a-takeover-is-not-a-second-open.md)): `tf_tree_ipc::Open` had a builder that took a takeover path — step 3, skipping the split-brain check because a process that already holds the arena cannot create a second one — and it handed back the first *free* participant byte while the caller's arena record was elsewhere (#201). It could not be repaired in place, because the declaration it rested on is unverifiable from a new file description: `F_OFD_GETLK` answers *does anyone else hold this byte*, so a caller holding byte *n* on a fresh description and a live **peer** holding byte *n* are indistinguishable. Two rounds of repair produced **five executed unsound states, four of them introduced while fixing the one before**. That builder, that arm and the unit test that reached it stay deleted, and §3.4's NORMATIVE pseudo-code keeps its amendment. **`OpenOutcome::TookOver` and `OpenError::TakeoverUnsupported` are deleted, and this row said otherwise for a day.** `0037` question 3 answered `no` — the variant does not survive, because a takeover is not an outcome of `open()` — and the removal has since been made; `rg TookOver crates/` finds only comments about its deletion. What stood here was *"as of 2026-08-28 the variant is still a public `tf_tree_ipc` enum member"*, which was true when written and stopped being true without this row noticing. Corrected rather than deleted, because a §0.0 row that has been wrong once is worth a reader knowing about. The consequence this row used to record, measured with `shm_torture` (§11.4) — kill the owner and the arena keeps serving lookups exactly as §3.5 promises, but no new process can join it for as long as any survivor lives — is now the failure a survivor's `inherit_ownership()` ends. |
| Participant registry — owner-side slot assignment (§5) | **Done; #201 is closed (2026-08-27), by deleting the path that could break the invariant rather than by asserting it there.** The verdict below is kept as the history it is — read the closing paragraph of this cell first. "The arena slot and the lock byte are the same integer" is *established* rather than lucky on the two ordinary paths: §3.4 step 4 refuses to create while any participant byte is held (`crates/tf_tree_ipc/src/open.rs:369`), so a normal creator runs against an empty lock file and takes byte 0 while `build_shared` hands it a fresh arena whose first `FREE` record is 0 (`crates/tf_tree/src/tree.rs:516`); and a joiner is handed one number for both, which `register_participant_at`'s doc comment (`crates/tf_tree/src/tree.rs:2893–2898`) gives the reason for in as many words — “two independently-chosen numbers would make every liveness answer be about somebody else”. **It is checked now, and the check refuses** ([`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md) plan step 0c). `Open::attempt` compares `Session::slot` with `Tree::participant_slot` at the single `hold_ownership` call site and returns `OpenError::ParticipantSlotDiverged` instead of a tree whose every liveness answer would be about somebody else. The comparison sits two statements *before* `Tree::hold_ownership` (`crates/tf_tree/src/tree.rs:2397`), which is where both numbers first meet and still compares neither, because `spawn_owner_server` binds the rendezvous socket between them: refusing after the bind would tear an arena out from under a joiner that had already attached, so the refusal has to land while the arena is still private. **Not** in `register_any` — `tf_tree_ipc` has no arena dependency and cannot see a record index at all. **And it is reachable through shipped public API — reproduced 2026-08-19, unstaged.** `tf_tree_ipc::Session::release_ownership` (`crates/tf_tree_ipc/src/open.rs:525`) gives up the ownership byte while keeping participant byte 0, which is exactly what §3.5 asks of it (“give up the owner role while staying attached”), so a live **non-owner** is left holding byte 0. A second process opening with `CreatePolicy::Always` then skips step 4's guard, `register_any` (deleted 2026-08-27) hands it the first *free* byte — 1 — and `build_shared` still registers it at arena record **0**. Measured: `arena record = 0, lock byte = Some(1)`, and after the first process exits a joined peer reports `participant_alive(0) = false` about a process that is live, holds record 0, and has just pushed a sample, while that process's own tree reports `true`. That is the corrupting direction §6.2 forbids. A second producer of the same state needs no library call at all: `tf_tree_ipc_child hold-participant <lock> 0`, a `[[bin]]` target of the published crate. **What the retraction in the `--force-new` row above still gets right** is the *operator scenario* #201 was filed on — kill the owner, then force-create — which does not diverge, because an owner death frees byte 0 along with the ownership byte. What was wrong was concluding from that that nothing produces the state; the route above is a different producer of it. #201's second path, the takeover arm, is closed as of 2026-08-27 — see the end of this cell. **The reproduction above is history as of step 0c**, kept because it is the evidence the check exists on: both of the tests that carried it (`defect_201_release_ownership_strands_a_live_non_owner_on_byte_0` and `defect_201_a_forced_creators_record_reads_dead_while_it_is_publishing`, `crates/tf_tree/tests/rendezvous.rs`) now assert the refusal, and the divergence can no longer be reached through `tf_tree::Open` — `use_ofd_liveness` is installed only by the two arms of `Open::attempt`, one of which now refuses. **The false-dead verdict itself can still be retaken, and was, on 2026-08-22.** This row used to end *"so no `Tree` whose byte and record disagree can be constructed"*, and that sentence conflated the tree being **judged** with the tree doing the **judging**: the probe belongs to the observer, which joins normally, and the subject needs no probe and no byte at all. Measured at `7739805` with no `tf_tree_ipc::Open` call, no session and no divergence — a `TreeBuilder::build_shared` creator published through `OwnerServer::bind_at`/`serve`, both shipped public API — a facade joiner reported `slot 0 state live word 0x6 pid 2544764 alive false` about it, and `Tree::reap_participants` from an ordinary read-write peer CASed its record `0x6 -> 0x0` while it was still publishing. The general statement is that **a `LIVE` record at index *i* whose byte *i* is unheld reads dead to every probe-carrying observer**: `reclamation_verdict` (`crates/tf_tree/src/open.rs:299`) consults the state word and the kernel byte and never the record's own pid. #201's divergence is one producer of that state and, isolated, produces no wrong verdict at all — with the byte still held the same sweep reclaims nothing — while a directly-called `build_shared`, which `register_participant` (`crates/tf_tree/src/tree.rs:3155`) registers with no byte to pair it against, is a second producer needing neither `tf_tree_ipc` nor a divergence. Which of the two facts a reclaimer may key on for a byte-less record is [`0031`](./decisions/0031-the-participant-record-with-no-byte.md), and until it is answered `Tree::reap_participants` must not be run in a process tree where anything created its arena that way. **#201 IS CLOSED (2026-08-27, #275), and the second half needed a decision after all.** `0035` shut the creator path by *taking* byte 0 rather than scanning for it, and deferred the **takeover** arm believing its correct slot was an open question. It was not — `0028` question 3 resolved it on 2026-08-20, and `0035` cited it as "`0029` question 3", a typo `0029` corrected on 2026-08-25 while `0035` was frozen. **But the resolved answer was that §3.5 cannot be an `Open::open` call at all**, which was read as *nothing calls it yet* rather than *nothing can*. Two rounds of repair produced five executed unsound states — the original first-free-byte divergence, a session over a free byte, an out-of-range slot through public API, a serving owner overriding the declaration, and a stranded owner grant — four of them introduced while fixing the one before. The root cause is that `F_OFD_GETLK` answers *does anyone else hold this byte*, so no new file description can verify *I hold slot n*. **So the arm and its builder are deleted**, `register_any` with them, and `OpenOutcome::TookOver` has no producer. [`0037`](./decisions/0037-a-takeover-is-not-a-second-open.md) is the draft record for what a real §3.5 would be: a method on the `Session` the heir already holds, where the invariant is structural rather than checked. `ParticipantSlotDiverged` stays as an assertion over hand-rolled `tf_tree_ipc::Open` plus `build_shared` construction, which is now the only route that can still produce the divergence. |
| §5.1 liveness from `F_OFD_GETLK` | **Done for a tree from `tf_tree::open`** — both arms of `Open::attempt` install the probe, and nothing else does. Every other tree keeps `/proc`, and the row below is what "survives as a diagnostic" is read against. |
| §5.1's "no longer on any correctness-critical path" | **False in two places, all `crates/tf_tree/src/tree.rs`** (#205, reported out of #194). **This row said three until [`0029`](./decisions/0029-the-topology-lock-is-a-kernel-lock.md) landed, and the third is where the shrink comes from — read (3) below for what it does and does not close.** **(1)** `use_ofd_liveness`'s fallback, `probe.is_held(slot).unwrap_or_else(\|\| record_is_alive(rec))` — whenever `F_OFD_GETLK` declines to answer, the triple decides A8's claim liveness. **(2)** `liveness_for` — for a tree with no probe (a heap tree, one from `TreeBuilder::build_shared` called directly, or one from `Tree::attach_shared`), `record_is_alive` is the *entire* predicate handed to `ArenaView::with_liveness`, so it decides A8's intern takeover and `Tree::participant_alive`. **(3) — CLOSED for a tree with a lock file, by [`0029`](./decisions/0029-the-topology-lock-is-a-kernel-lock.md) (#213), and open for one without.** `Tree::reparent` used to call `participant_is_alive` and nothing else, so the triple was the whole predicate for A2's topology-lock steal on **every** tree, including those holding a probe. It now takes §3.3's byte 1 before it touches the arena word, which excludes a live holder by a kernel fact rather than an inference: the two ways `/proc` says "dead" about a running process — a PID-namespace collision (`Known(st) != stored`) and a same-user but non-dumpable target under `hidepid` — no longer authorise anything, and both are reproduced (`0029`'s appendix, [`0033`](./decisions/0033-the-identity-record-cannot-name-a-namespace.md)). **What is not closed** is a tree with no lock file — a heap tree, a directly-called `TreeBuilder::build_shared`, an `attach_shared` over an inherited fd — where there is no byte for anyone to take and the triple is still the whole predicate in both directions, and a **holder** with no lock file seen from a tree that has one, which the residual predicate still decides. That residual is `0029`'s T3 and closing it is a capability question ("may a writer with no lock file mutate topology on a shared arena?") narrower than [`0031`](./decisions/0031-the-participant-record-with-no-byte.md)'s scope question, and not taken there. `0029` is `implemented`; #194 named the first two paths and this one was found verifying them. **The regression is pinned**: `a_live_holder_that_proc_calls_dead_keeps_the_topology_lock` (`crates/tf_tree/tests/rendezvous.rs`), with the control that moves one variable, and the mutant run — deleting the byte acquire gives *a live holder was stolen from: Ok(())*. Each false "dead" is the corrupting direction §6.2 forbids: a topology lock stolen from a live mutator, or an intern entry taken from a running interner. The predicate is biased against it since #204 — `alive_given` returns alive wherever it cannot *prove* death — so this row records what §5.1 licenses, not a miscount in the code. **§3.10 is *a* dependency that keeps this survivable, and it is not sufficient.** It is load-bearing for correctness rather than convenience: `hidepid=2` answers `ENOENT` for another user's `/proc/<pid>`, indistinguishable at the call site from a pid that does not exist, and what makes that harmless is that a hidden entry cannot belong to a participant — they are same-user by construction. That much is stated on `proc_answers_here` and, until this row, nowhere in the spec, which is the shape #194 fixed one layer down. **The second dependency is stated nowhere at all: participants must share a PID namespace.** `read_start_time` resolves a *namespace-local* pid in the reader's namespace, and §3.1 recommends sharing the runtime directory across containers by volume mount — which does not share a PID namespace, and §3.1's own warning is about the *network* namespace only. Two outcomes, both dead-about-a-live-process: `ENOENT` while `/proc/self/stat` reads fine, which the bias at least classifies as unprovable; and a **collision** with an unrelated local pid, `Known(st) != stored`, which is not `ENOENT`-shaped, so no "cannot prove death" bias catches it. `ParticipantRecord` carries no namespace discriminator and `boot_id` is identical across namespaces on one host, so neither guard fires. **Both outcomes are now reproduced, and the "not reproduced" this row used to carry is retired rather than deleted** — it read *"`unshare --fork --pid` is unavailable in the environment this was written in"*, which was true of that flag and not of `unshare -U --fork --pid`, refused without `-U` and permitted with it. [`0033`](./decisions/0033-the-identity-record-cannot-name-a-namespace.md) stages the collision (a namespaced participant's recorded pid 1 resolving to the host's `systemd`, `Known(st) != stored`) and the `ENOENT` outcome (a host participant seen from a container) and a *third* the row did not name: a `doctor` whose own `/proc` is not its namespace's reports **its own participant slot** as a fork inheritor. **What is fixed is `doctor` and only `doctor`** — `tf_tree_ipc::Identity` gained `pid_ns_inode` and `TFT014` gained two guards, so the diagnostic no longer inverts the operator's remediation. **The three corrupting paths this row names are untouched**: they read `ParticipantRecord`, which is an arena structure, and a namespace discriminator there is a `FORMAT_VERSION` bump and its own decision record. §5.1's `start_time` paragraph is read against this row — **all three of its clauses**, not only the last. It reads "It is still parsed — carefully — because `doctor` reports it and the takeover path prints it, but it is no longer on any correctness-critical path", and `doctor` does **not** report it (`crates/tf_tree_cli/src/doctor.rs` records both extra fields as captured, never read, and deliberately removed) and **no takeover path prints it** — the only live producer is `client_start_time` on the wire, which no reader consumes. **No amendment is proposed here:** §5.1's wording is `0028` step 0's ground, and the owner's answer to that record's question 1 (#195) retired the amendment step 0 planned without touching these three paths, so moving them off the triple is a decision record, not a docs edit. |
| Claims as OFD leases (§6.1) | **Done** — the arena CAS is the decision, the lease makes death observable |
| Reaping (§6.3) | **Done for both objects, by any read-write participant — and the participant half is on demand, not automatic.** Claims: `Tree::reap_dead` / `reap_participant`. Participant *records*: `Tree::reap_participants` ([`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md) plan step 5), which sweeps the table through the one reclamation predicate — the state word observed **before** the OFD byte is probed — and `ParticipantTable::reclaim`s each slot the kernel reports free, `RESERVED` records included. It is refused on a read-only tree and on a tree with no lock file, and it never judges its own slot. **§6.3's "reaping must not be owner-only" is now a property of the code**: the case that proves it is the owner's own slot, which no `HUP` can reach because no socket of the owner's closes, and `a_survivor_reaps_the_killed_owners_slot_which_no_hangup_can` (`crates/tf_tree/tests/rendezvous.rs`) kills the owner and has a surviving joiner return slot 0 to `FREE`. §3.9's "the owner reaps its arena-side records" is met by the hangup callback (#191) for a joiner and by this for everything else. **Three collectors, one predicate, one `reclaim`:** the owner's slot assigner reclaims before it grants ([`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md) plan step 3), the owner's socket-hangup callback is the O(1) fast path for a joiner (#191, rebased onto `reclaim` by step 4), and this sweep is the only one that reaches what neither can — the owner's own slot, and any slot on an arena whose owner is dead. **What is not done:** the sweep runs when a participant *calls* it, so the slots only it can reach are reclaimed on demand and not automatically. And the fork case is **deliberately** not reclaimed (§6.2): a forked child keeps the parent's open file description, so the kernel says the byte is held and this refuses to act, which is `0030`'s ground. |
| Fork poisoning (§7.3) | **Done** — `pthread_atfork` counter; five destructors guarded |
| Per-edge page population (§7.1) | **Done** — measured 66.3 MiB → 3.8 MiB on an over-provisioned arena |
| Memory locking (§7.4) | **Declined by [`0049`](./decisions/0049-the-flag-that-prefaults-the-arena.md), not merely absent.** `LockPolicy::{None, Populate, Locked}` is specified at §7.4 and exists nowhere: grepping `LockPolicy`, `mlock` and `mlock2` over `crates/**/*.rs` finds no such type and no such call — only comments, `TFT016`'s diagnostic strings and `tf_tree_cli`'s `MemLock` limit parser. `MappedArena` applies exactly two advices at attach, `MADV_DONTFORK` and `MADV_HUGEPAGE` (`crates/tf_tree_arena/src/mapped.rs:436–437`), plus §7.1's per-edge `MADV_POPULATE_*`; nothing pins a page. **The half of §7.4 that did ship is its diagnostic**, which is why the absence is visible to an operator rather than silent: `TFT016` compares `RLIMIT_MEMLOCK` against the arena size and says what is not pinnable if it is short (`fn tft016`'s `match host.memlock` arm in `crates/tf_tree_cli/src/checks.rs`, parsed by `parse_memlock` in `hostfacts.rs`). **This row exists because the table did not have one**: §7.4 sat between rows for §7.1 and §7.3 with no row of its own, and the preamble above named no gap for it until it was corrected in the same change, in the table `CLAUDE.md` calls the source of truth over its own prose. §7.4 carries `0049`'s amendment banner. **This row read "Not implemented, and named in no decision record." until 2026-09-05**, which is a *status* and not a decline — the same distinction the `tf_tree_record` row below records, and it survived into the change that added the decline three sentences later in the same cell. *This row read* **"Nothing schedules it, and this row does not decide it — no decision record covers `LockPolicy`. What one would have to weigh is that `MLOCK_ONFAULT` … does not prefault, so it adds nothing over §7.1's shipped per-edge `MADV_POPULATE_*`, and that pinning a whole mapping is `mlockall(MCL_CURRENT\|MCL_FUTURE)` in the embedding application"** — and the weighing was wrong in two of its three clauses, which is what `0049` was written for. `MLOCK_ONFAULT` does not prefault (**true**); it does **not** add nothing over §7.1, because §7.1 establishes PTEs once and the flag is what keeps them (measured with `VM_LOCKED` as the only variable); and the recommended incantation is `mlockall(MCL_CURRENT\|MCL_FUTURE\|MCL_ONFAULT)`, because without `MCL_ONFAULT` the call prefaults an untouched 64 MiB mapping outright, which is per-arena population at address-space scope and the thing [`0024`](./decisions/0024-population-is-per-edge-at-take-up.md) removed at 5.2×. Whether a swapless host reclaims the arena at all is **undetermined** and no document may state it either way: `crates/tf_tree_bench/examples/mlock_probe.rs` is the executor and `0049` §*Open questions* is the residual. **The reason for declining is unchanged and is `docs/API.md` §8.3's second bullet** — a library cannot see the `RLIMIT_MEMLOCK` budget it would spend. The `TFT016` citation is by **name** rather than by line: `fn tft016`'s `match host.memlock` arm in `crates/tf_tree_cli/src/checks.rs`, parsed by `hostfacts.rs`'s `parse_memlock`. **The line number that stood here (`checks.rs:1987`) was exact when written and had drifted 466 lines**, which is why it is gone; nothing in `scripts/` guards a doc citation, so naming a function is a mitigation and not a fix. |
| CLI adoption — `--attach`, `tf_tree participants` | **Done** |
| `tf_tree serve` (was `tf_treed`, §9) | **Not implemented, and not scheduled.** §9 is superseded by [`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md), whose steps 6–7 are *"not built, and are not scheduled"*; §15's box for it is marked `~` rather than left unticked, because an unticked box reads as work owed and this is work declined. |
| `tf_tree_record` (§10) | **Declined by [`0047`](./decisions/0047-the-recording-this-reader-would-refuse.md), not merely absent.** §10(c) — the NORMATIVE heap-against-mapped bit-identity test — **is met**: `crates/tf_tree_cli/tests/replay_bit_identity.rs`, run by `just shm-check` and by the CI step that invokes it, **not** by `just test`. §10(a) *Record* and §10(b) *Replay* are declined: §10's own channels carry no `tf2_msgs` schema, and `crates/tf_tree_ingest/src/source.rs` accepts a channel only by schema (§3.3), so the recorder's artifact would be refused by the only MCAP reader here and closing that means a second reading path. **This row read "Not implemented" until 2026-09-05**, which is a *status* and not a decline — it recorded that nothing was built and left open whether something was owed. |
| `/tf` ingest bridge | **Done**, as PHASE4 §5 rather than here — `docs/PHASE4.md` §0.0 reads *"Done, except §5.9's affinity knobs and §6.3's replay rows"*, and `ros/tf_tree_ros/` is the `ament_cmake` half. **This was carried as "Not implemented" in this table after it had shipped under another phase's number.** |
| Diagnostics (§9's `doctor` / `top` / `participants`) | **Done.** The CLI-adoption row above covers `--attach` and `tf_tree participants`; `tf_tree top` is `docs/PHASE5.md` §0.0's *"§7 `tf_tree top` — Done, both halves"*. |
| Fault injection (§11.3) | **Implemented. This row read "Not implemented. There is no `crash-points` feature and no `TF_TREE_CRASH_AT`" until 2026-08-29, and it was false in both halves** — the feature and the variable shipped with the first six sites, and six more landed the day this was corrected. A §0.0 row is what a reader consults instead of the code, so it is the one place a stale "not implemented" costs most. **Thirteen of the fourteen rows carry a site**: **seven** in `tf_tree_core` (`push.*` ×3, `topo.after_copy_before_publish`, `claim.after_cas`, `intern.after_hash_cas_before_id_store`, plus `attach.after_slot_assigned_before_publish`), and six in the facade (`takeover.*`, `topo.holding_lock`, `open.*` ×2, `reclaim.after_probe_before_cas`, `hangup.after_probe_before_cas`). **All thirteen are now driven by a test that fires them** — the seven of `tf_tree_core::crash::SITES`, which `crash_tests::the_published_site_list_is_the_one_the_tests_arm` makes structural, and all six of the facade's, the last two having gained tests on 2026-08-29. **This sentence carried three arithmetic errors, and every one understated the work already done**: it said "six" while enumerating seven; "Eight" when eleven were covered; and then "Eleven" in a cell that went on to say both remaining sites now had a test — thirteen. A §0.0 row is consulted instead of the code, so an undercount is as costly as an overstatement — it invites somebody to rebuild what is already there, and an undercount shipped *in the commit that fixes it* is the same failure twice; **`reclaim.after_probe_before_cas` and `hangup.after_probe_before_cas` had a site and no test until 2026-08-29; both now have one**, and the reason they were the two that lacked one is structural rather than incidental. `reclaim.*` needed a sweeper that is **not the test binary** — every `reap_participants` call in `tests/rendezvous.rs` runs in-process, where arming the site aborts the runner — so `rendezvous_child`'s new `join-sweep` arm is the missing half, and the fixture has to kill the **owner**, because a joiner's death fires the hangup callback that collects its record before any sweep can find it. `hangup.*` needed the **owner** armed, where every other crash test arms a joiner. **And the facade's list had no completeness gate at all** — `crash_tests::the_published_site_list_is_the_one_the_tests_arm` covers `tf_tree_core` only, which is exactly why the two untested sites were both in `tf_tree::CRASH_SITES`. `the_facade_site_list_is_pinned_by_index_and_every_site_has_a_test` closes it, and it pins **index to name**: the facade arms by index (`CRASH_SITES[0]`…`[5]`), so a set-equality gate would still pass after a reorder that made `reap_participants` arm the *hangup* name. **The fourteenth is not a crash point**, and that is a finding rather than a shortfall: `reclaim.probe_then_reoccupied` names an interleaving between two *live* processes, which a mechanism that kills one of them cannot produce — its row argues it, and `loom` and §11.4 are what can reach it. `crash_tests::the_published_site_list_is_the_one_the_tests_arm` refuses a site added to `tf_tree_core`'s list without a test that arms it, which is why the two untested sites are in the facade's. **`shm_torture --crash-points` arms these sites; it refuses only in a build that compiled them out.** This sentence said it *refuses*, full stop, which was the pre-2026-08-29 behaviour and was already stale in the row that corrects the same staleness one clause earlier. What survives is the part that was never about implementation status, and it is the reason the flag exists at all: a run **without** it must not be quoted as §11.3 coverage, because `SIGKILL` lands wherever the scheduler puts it and that is a different and much shallower set of states — a distinction that is demonstrable rather than argued, since the named sites exist to compare against. §11.4's row carries which of the thirteen that workload can actually reach, measured with `--crash-site`: twelve fire, `topo.holding_lock` does not, and two of the twelve fire only where the state their row names cannot exist. §11.3's `intern.after_hash_cas_before_id_store` row is the exception — its fix is amendment A8, which *is* applied. |
| `shm_torture` (§11.4) | **Done, and since 2026-09-04 it kills the rendezvous owner. Two of §11.4's four continuous invariants are checked continuously; a third is checked at teardown and the fourth is not implemented — read the last paragraph of this cell before quoting “invariants checked continuously”.** `crates/tf_tree_bench/src/bin/shm_torture.rs`: N processes on one arena doing random attach/detach/claim/reap/push/lookup through the real rendezvous, with the driver `SIGKILL`ing one of them several times a second and replacing it. Every reader validates every transform (§11.4's non-unit-quaternion and NaN rules, unconditionally — which is what `TF_TREE_PARANOID=1` was for, so there is no such switch), and after the run a participant that was never killed reclaims and checks that no claim and no participant slot leaked. `just shm-torture` is §13's 30-minute nightly and `just shm-torture-asan` is its “under ASan” half; **`just shm-torture-self-test` is the part that runs on a branch** — it asserts an injected corrupt transform is caught *by a process that did not write it*, that a run which validated too little **fails** rather than printing the same `0 violations` a healthy one does, that a run in which nothing inherits the owner role fails naming that, and — since 2026-09-04 — that a run which never migrates passes with every worker record on the leak check's **strict** path, which is the one configuration in which the owner's hangup collector is the only collector that can answer for them. That test is in `just shm-check`. **This cell read “Done, minus §11.3's crash points and minus killing the owner”, and both halves have now expired — the second one twice over.** The reason the killed processes were joiners was that §3.5's takeover was specified and not wired, so an owner's death wedged every joiner at `ArenaHeldButUnreachable`; §3.5 shipped on 2026-08-28 and §11.3's crash points on 2026-08-29, and this cell then said what was owed was *this harness using them rather than their absence*. It now uses both. **The owner is a child.** It creates the arena, serves the rendezvous and does nothing else, so killing it is an owner's death and not a writer's; the driver joins with `CreatePolicy::Never`, which keeps the two roles that mattered — it holds the segment alive for the recovery check and it is a reader that is never killed — and drops the one that made the owner unkillable. Every child evaluates `Tree::owner_lost` in its own loop and calls `Tree::inherit_ownership`, which is §3.5's caller-driven trigger used as specified, so the property does not depend on whether the surviving population happened to include a read-write participant. **Each owner kill must produce two things or the run fails**: a *fresh* process joining the arena again — probed from the outside with a real `Open::new().create(Never)`, because an ownerless arena is exactly what refuses a new joiner and an internal flag would not have detected the 2026-08-27 regression — and a survivor recording an inheritance, which is what says the join succeeded *because* §3.5's trigger ran. **`--no-inherit` is the negative control** and is expected to fail. Measured 2026-09-04, six seeds at `--duration 20s --children 5 --kill-hz 5`: every owner kill recovered and was answered by a recorded inheritance, and the observer kept validating transforms while the role was vacant — which is §3.5's “lookups do not pause”, observed rather than restated. **The interval that stood here is deleted rather than corrected a third time.** It read `0.4–0.7 ms`, was refuted at both ends within a day, was republished as a wider interval, and it is a scheduling quantity taken on a host that is simultaneously running the fleet and the reader — so a fixed figure in this cell is a measurement with a date on it and no way to notice when it expires. The run prints its own `mean … worst …` per migration; read it there. What the harness *asserts* is not a duration but the pair: a fresh `Open::new().create(Never)` succeeding inside the 10 s recovery deadline, and a survivor recording an inheritance. Whatever the run prints there is **not** §12.2's latency row: `just owner-migration` measures that on an idle host with readers that make no control-plane call, and this one is taken by a driver that is also reading. **One consequence of migrating is paid for in `check_recovery`, and the first version of that payment was a weakening.** `docs/decisions/0043` records that a survivor of a migration never registers with the new owner, so its record is invisible to that owner's hangup collector. That cell said the leak check therefore *sweeps `Tree::reap_participants` once first* on any run that killed the owner — which, with the owner-kill arm on by default, is every run, and it turned a check into a note: with the owner's hangup CAS deleted from `crates/tf_tree/src/open.rs`, the harness still printed `PASS`. What it does now is **partition first**. Records held by a process that has held the rendezvous, or by a child whose last attachment named an owner that has since died — the driver knows both, the second because each child records the owner it is about to attach under — are swept and then *required* to have been collected; every other record is judged before any sweep runs and fails the run. **The first version of that partition shipped the strict verdict on the migrating arm too, and failed healthy runs — which in a torture harness is worse than the weakening it replaced, because it teaches the reader to re-run.** Two things were wrong and repairing the first did not fix it. (1) The exempt set was **sampled**: the driver learned who had held the rendezvous by re-reading the `owner.pid` marker once a driver round, and the role turns over far faster than that — polling the marker at 100 Hz through a healthy 20 s run at 5 children / 5 Hz shows it naming dozens of distinct processes, while the run's own §3.5 summary counts only the handful that answered an owner *kill*, which is a different question. Every turnover the poll missed became a record judged strictly that no hangup callback could have collected. That is now read whole from the inheritance ledger the heirs already append to, plus the one process that created the arena without inheriting from anybody, so the sampling is removed rather than compensated for and there is no second mechanism to keep in step. (2) **The premise itself does not hold on that arm.** The heir is an ordinary worker; its `work` loop ends on a detach arm or on its operation cap, and once it detaches with every other child already dead nothing remains to inherit, so the arena spends most of the teardown with no owner at all — measured by sampling `Tree::owner_lost` from the driver every 10 ms across the settle window, where `--no-kill-owner` never reports the role vacant and the migrating default reports it vacant for most of the window in the large majority of runs. A verdict that blames the hangup callback there blames a process that was not running. So `check_recovery` reaches the strict verdict **only where that collector is pinned** — decided from the ledger, not from the flag, because an armed §11.3 site can end the creating owner's life on a run that was never going to kill it — and on a migrating run it reports the same partition as a note while still requiring the sweep to have collected. **That means the default arm asserts less than the strict version claimed and more than the permissive one did**, and `a_run_that_never_migrates_holds_every_worker_record_to_the_strict_path` is the self-test that drives the arm which asserts the most. The teardown also kills whoever currently holds the role **last**, which after a migration is an ordinary child in the fleet and was previously signalled with the rest of it, and the margin it then waits is a *bounded poll of the records the verdict is about* rather than a flat sleep, because `check_recovery`'s own retries cannot make up for a short margin — by the time they run, every process that could run a hangup callback has been reaped. That change removes an assumption rather than a measured failure: the flat 200 ms passed every strict-arm run it was given under six busy loops on an eight-core box. **A pair of seed tallies stood in this paragraph and is deleted**: both were single draws of a scheduling outcome, and re-measurement moved one of them. With the hangup CAS deleted, `--no-kill-owner` fails on every seed it has been tried at, idle and loaded; run the mutant rather than reading a number. **§11.4's four continuous invariants, one at a time, because the clause is quoted as though it were one thing.** *No reader ever observes a non-unit quaternion or a NaN* — checked on every read in every process. *No two writers ever hold one edge* — checked on every push, **since 2026-09-04**, by the writer that holds the edge: it reads the claim word, and a push that succeeds while that word names another participant slot means the claim epoch never moved, which is a second grant (A3/A4, D7). Before that it was inferred at teardown from “every edge can be claimed again”, which is a different question; seeding a real second writer, the teardown check prints `PASS` and the push-time check fires within the first second. It is a check by the holder, so it cannot see a double grant on an edge no live writer holds, and the property itself remains held by construction. *Participant and claim slots never leak* — **teardown only**, in `check_recovery`. *The arena hash is stable across quiescent points* — **not implemented**, for two reasons, of which the first one this cell gave was wrong. It read *"not implementable from this crate: `tf_tree_bench` is `#![forbid(unsafe_code)]`"*, and **that attribute is on `crates/tf_tree_bench/src/lib.rs`, which does not govern `src/bin/shm_torture.rs`** — a bin is a separate crate root, this one's only crate-level attribute is a `clippy` allow, and sibling bins in the same package carry `unsafe` — `scripts/unsafe-budget.txt` is the list ([`0048`](./decisions/0048-a-kind-is-not-a-crate-name.md)). The two reasons that survive are the ones this cell already gave beside it: there is **no safe accessor for the arena's bytes**, so it needs a new public API and therefore a decision record; and **“quiescent point” is undefined** for a harness that kills processes several times a second. The conclusion is unchanged; the mechanism named for it was not the mechanism. **§12.3 gate 3 has two clauses — “Zero corrupt reads across the full `shm_torture` run” and “every §11.3 crash point recovers” — and one of the two is met.** This cell read “met on three of its four clauses”, which is a denominator §12.3 does not have: it counted “the owner dies mid-run”, which is §11.4's description of this harness and appears nowhere in the gate, and it never named a fourth clause at all. Zero corrupt reads is measured — over ~250 composed `map -> tool` reads per observation round, with a floor that fails the run below 16. The owner does now die mid-run, which is what §11.4 asks and what this branch built; it is recorded here as work done and not as a gate-3 clause. **“Every §11.3 crash point recovers” is partly measured and cannot become fully measured here**, which is a structural statement and not a matter of duration: `--crash-points` arms a random site in ~10% of children, read from `tf_tree_core::crash::SITES` and `tf_tree::CRASH_SITES` rather than re-spelled, and `just shm-torture-crash-points` is the recipe. **That recipe ran in no workflow until 2026-09-04**; it has a nightly job of its own now, separate from the plain soak because the build carries `--features shm,crash-points` and the parameters are gentler. A job reads an exit status rather than the `§11.3:` line, so the binary REFUSES under `--crash-points` when nothing was armed and, separately, when nothing fired — two bounds of "at least one", not tuned thresholds, without which the job would go green on a build that cannot arm a site at all. `aborted` is a floor rather than a count: the driver's victim path discards the child's exit status, so an armed child killed after it aborted is not counted. Measured 2026-08-29 at 40 s / 10 children / 2 Hz: **11 children armed, 4 aborted at four distinct sites** — `attach.after_slot_assigned_before_publish`, `push.after_seq_odd`, `push.after_data_before_seq_even`, `claim.after_cas` — with 0 violations and a clean recovery check. `attach.*` firing there is the one worth noting: §11.3's row records that its ~12 ns window could not be reached without fault injection, so a torture run now *produces* the state §11.2's collector tests stage. **The run reports children armed and children that aborted separately**, and now also the site *names* on both sides, because “armed at a site this workload cannot reach” and “armed and the `SIGKILL` got there first” are different runs and counts alone cannot separate them. **Twelve of the thirteen sites fire in this workload and one does not — and “fires” is not the same as “exercises the row's repair claim”, which is the split worth carrying.** Measured 2026-09-04 by forcing each site in every child with the new `--crash-site NAME[:nth]` probe (12 s / 4 children / 1 Hz / seed 777). **The probe form is not uniform and neither is where the answer is read, and this sentence claimed both were**: at `NAME:1` only **eight** of the thirteen produce the run's own `§11.3:` armed/fired line — `push.*` ×3, `claim.after_cas`, `takeover.*`, `reclaim.*`, `hangup.*` and the never-firing `topo.holding_lock`. **That eight is not the live-arena eight below**: this one is about where the answer is read and includes `topo.holding_lock` (whose answer is `0 aborted`) while excluding `attach.*`; the one below is about what the site exercises. `attach.after_slot_assigned_before_publish` needs the **bare** `NAME` form, where the nth is drawn above 1 so the creating owner child survives its first hit; at `:1` every one of `spawn_owner`'s six attempts aborts and the run bails before printing anything. The remaining four — both `open.*`, `topo.after_copy_before_publish` and `intern.*` — bail the same way, so their verdict is read from the child's `tf_tree_core: crash point <site> hit N, aborting` on stderr, which is exactly the *trusted rather than checkable* state the claim said had ended. `shm_torture.rs`'s header carries the same split per site. **Eight fire in a live arena and the state they leave is met by live peers**: `push.*` ×3, `claim.after_cas`, `attach.after_slot_assigned_before_publish`, `takeover.after_ownership_lock_before_bind`, `reclaim.after_probe_before_cas` and `hangup.after_probe_before_cas`. Killing the owner is what converted the last three — survivors call `inherit_ownership`; the owner is a child now, so it is armed like any other; and a `reap_participants` operation entered the workload with the migration, which `check_recovery`'s own comment had recorded as the reason `reclaim.*` was unreachable. **Two more fire only in the *creating* owner child and their rows are still exercised** — `open.after_ownership_lock_before_bind` and `open.after_create_before_bind` are about what the *next* `open()` finds, and `spawn_owner`'s retry is that next `open()`. **Two fire only in the creator where the state their row names cannot exist**: `topo.after_copy_before_publish` (`TreeBuilder::build_with` calls `set_parent`, so the abort destroys the arena being built, and A1's “inactive block dirty, no observable effect” is a claim about a *live* arena) and `intern.after_hash_cas_before_id_store` (the owner interns all five chain names at build time, so it is the first interner, and there is no *next* interner of a name whose arena never existed). **One never fires**: `topo.holding_lock`, inside `Tree::reparent`, because nothing here reparents and a participant that reparented the fixed four-edge chain would destroy the property every other check reads — probed with every child armed and none aborting. **The per-site armed/aborted pairs this cell and `shm_torture.rs`'s header published are gone**: re-running the same command at the same seed moves them, because what a `SIGKILL`-driven fleet reaches in twelve seconds is a scheduling outcome and the seed fixes only the driver's draws. The *split* — which sites print the run's own `§11.3:` line and which are read off a child's abort on stderr — is what reproduced, and is the claim. So **“covers all thirteen” is not a bar this harness can clear**, and that is a statement about this workload rather than about the sites: every one of the thirteen has a targeted test, which is where §11.3's per-site coverage lives and is what that row records. **Two of this paragraph's own claims were wrong before the probe existed and are corrected rather than quietly replaced.** The reading that produced it said eight of thirteen were unreachable; three of those eight — both `open.*` and `hangup.*` — became reachable the moment the owner became a child, and two more fire in the creator, which no call-path reading had considered because before 2026-09-04 nothing in this harness created an arena in a process that could be armed. And the first version of the probe drew its hit count at random, so it reported four sites unreachable that fire at `:1` and never at `:2` — a probe's own sampling reading as a property of the workload. |
| §3.8's generous default layout | **Superseded by decision `0004`**, which sizes the arena from declared edges. Reconciling the two is its own decision; `0005` records the conflict rather than resolving it silently. |

**What the remaining gap means.** There is no daemon and no recorder, so "an
owner always exists" is the operator's job rather than something a service
guarantees — which is exactly what D16 says it should be. And there is no
long-running fault harness, so the crash matrix in §11.3 is covered by targeted
tests at each crash point rather than by randomised injection over hours. Both
are additions, not corrections: nothing in the shipped protocol is waiting on
them.

## 0. Scope

### In scope

| | |
|---|---|
| `MappedArena` | memfd-backed, sealed, `MAP_SHARED` |
| Discovery & rendezvous | zero-config `open()`; runtime dir as the sharing boundary; kernel file locks as the election |
| Attach protocol | `SOCK_SEQPACKET` + `SCM_RIGHTS`, with version and layout negotiation |
| Ownership migration | owner death is inherited by a surviving participant; lookups never pause |
| Participant registry | in-arena advisory records; OFD locks authoritative for liveness |
| Liveness and reaping | claims as kernel locks — no heartbeats, no `/proc`, no zombie window |
| Crash-consistency | every arena mutation protocol audited and repaired (§1) |
| Read-only attach | `PROT_READ` mapping as a real safety boundary |
| Mapping policy | per-edge population, `MADV_HUGEPAGE`, `MADV_DONTFORK`, optional `mlock` |
| ~~`tf_treed`~~ | **Not a crate.** [`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md) makes it `tf_tree serve`, a subcommand of `tf_tree_cli`, so the workspace gains no member |
| ~~`tf_tree_record`~~ | **Not a crate, and not owed.** [`0047`](./decisions/0047-the-recording-this-reader-would-refuse.md) declines §10's Record and Replay halves; §10(c), the NORMATIVE bit-identity test, is met by `crates/tf_tree_cli/tests/replay_bit_identity.rs` |
| `/tf` ingest bridge | read-only ROS 2 → arena, for real-data benchmarking |
| Diagnostics | `doctor`, `top`, `participants` |

### Out of scope — NORMATIVE

Everything excluded in §0 of `PHASE1.md` remains excluded. Additionally:

| Excluded | Why |
|---|---|
| Network, discovery beyond one host | Phase 6 |
| Python bindings | Phase 3 (but see §14 for the handoff constraints you must not break) |
| `tf2_ros::Buffer` API shim | Phase 4. The *ingest bridge* here is one-way and does not implement the tf2 API. |
| macOS / Windows shared memory | §2. Those platforms get in-process only until Phase 6. |
| Multi-arena federation on one host | One arena per `(runtime_dir, domain, name)`. Distinct triples are fully independent (§3.1). |
| Any security boundary against a malicious RW peer | §3.10. Say this out loud in the docs. |
| Dynamic arena resize | D4. Still forbidden. Capacity is planned, not grown. |

### Trust model

See §3.10. Summary: mutually trusting same-user processes; read-only attach is the only real boundary; crash and hang are both non-corrupting.

---

## 1. Phase 1 amendments — NORMATIVE, apply before Phase 1 is frozen

`PROJECT.md` states that if Phase 2 requires changes outside `tf_tree_arena`, the Phase 1 design was wrong. Working through the crash matrix found eight places where it was. Seven are cheap; one (A6) changes the arena layout. A8's motivating crash point is described in §11.3; the amendment itself is specified below with the rest.

**If Phase 1 has not yet been frozen, apply all of these to it.** If it has shipped, they constitute `FORMAT_VERSION = 2` and no version-1 arena may be attached.

---

### A1 — Pack the topology generation and active index into one atomic word

**Problem.** Phase 1 §5.2 uses a seqlock: bump `topo_generation` to odd, copy the block, flip `topo_active`, bump to even. A writer `SIGKILL`ed after the first bump leaves the generation permanently odd. Every reader then spins forever in plan compilation. **This wedges the entire arena and there is no recovery.**

**Fix.** There is no need for an odd state at all. The writer mutates an *inactive* block, which no reader is looking at; the active block is never mutated in place. So publication is a single store, and there is nothing to make atomic across a window.

```rust
/// bits 63..8 = generation (monotone), bits 7..0 = active block index
#[repr(C)]
pub struct TopoWord(pub AtomicU64);

#[inline] pub const fn pack(gen: u64, active: u8) -> u64 { (gen << 8) | active as u64 }
#[inline] pub const fn unpack(w: u64) -> (u64, u8) { (w >> 8, (w & 0xff) as u8) }
```

Writer:

```
hold the topology lock (A2)
w    = topo.load(Relaxed); (gen, active) = unpack(w)
next = (active + 1) % TOPO_BLOCKS
copy block[active] -> block[next]; apply mutation; recompute depths
fence(Release)
topo.store(pack(gen + 1, next), Release)     // single publishing store
release the lock
```

Reader (plan compilation only; `Plan::at` never touches this):

```
for _ in 0..TOPO_RETRY_LIMIT {
    w1 = topo.load(Acquire); (gen, active) = unpack(w1)
    ...walk block[active], bounds-checked, step budget max_frames...
    fence(Acquire)
    if topo.load(Relaxed) == w1 { return plan.with_generation(gen) }
}
return Err(TopologyChurn)
```

A dead writer now leaves the arena in a state that is *indistinguishable from no write having happened*. Readers are wait-free and never spin on a writer.

**`TOPO_BLOCKS = 4`, not 2.** With two blocks a reader is only hit if the writer flips twice mid-read. Four blocks require four flips. At `max_frames = 256` a block is 1.5 KB, so four cost 6 KB — free. Mutations happen a few hundred times per process lifetime; this makes `TopologyChurn` effectively unreachable outside a torture test.

**Topology block arrays become `[AtomicU32]` and `[AtomicU16]`.** A reader racing a writer on the same block reads garbage and discards it — but reading a non-atomic `u32` while another process writes it is a data race and therefore UB, even when the value is thrown away. Relaxed atomic loads compile to the same instruction. **Every index read from a topology block must be bounds-checked before use and the parent walk must be capped at `max_frames` steps**, because garbage from a losing race must not panic or index out of bounds before the validity check catches it.

---

### A2 — The topology mutation lock lives in the arena and is reapable

Phase 1 serializes topology mutations with a Rust `Mutex`, which is per-process and therefore does nothing across processes.

```rust
#[repr(C, align(64))]
pub struct TopoLock {
    /// 0 = free, else participant_slot + 1
    pub owner: AtomicU64,
    pub acquired_at_nanos: AtomicI64,
    _pad: [u8; 48],
}
```

Acquire is `compare_exchange(0, slot + 1, AcqRel, Acquire)` with bounded spin. On failure, resolve the owning participant (§5) and check liveness (§6.2); if dead, `compare_exchange(stale, slot + 1)` to steal. Because A1 makes an abandoned mutation leave *no trace*, stealing the lock is safe with no rollback: the new holder simply re-copies from the current active block. This is the payoff for A1 — recovery is a no-op.

---

### A3 — Claim ownership is a participant slot, not a PID

Phase 1's `ClaimRecord` stores `owner_pid` and `owner_boot_id` as separate fields written *after* the state CAS. A writer killed between the CAS and the PID store leaves `state = HELD, owner_pid = 0` — held by nobody, reapable by nobody. Permanently leaked edge.

**Fix.** One atomic word carries both the state and the full identity, because the identity is an *indirection* into a participant record that was fully written at attach time, long before any claim.

```rust
#[repr(C, align(64))]
pub struct ClaimRecord {
    /// 0 = free, else participant_slot + 1. Claim and identity publish atomically.
    pub owner: AtomicU64,
    /// Incremented on every reap and every successful claim. Fences zombies (A4).
    pub epoch: AtomicU64,
    /// Advisory only. NEVER a reaping trigger on its own (§6.4).
    pub heartbeat: AtomicU64,
    pub clock_offset_nanos: AtomicI64,
    _pad: [u8; 32],
}
```

Claim: `owner.compare_exchange(0, slot + 1, AcqRel, Acquire)`, then `epoch.fetch_add(1, AcqRel)`, and the `Publisher` records the resulting epoch.

**§6.1 supersedes the reaping role of this record.** Claims are held as kernel file locks and the lock is authoritative; `ClaimRecord` is retained for diagnostics and for readers asking who publishes an edge. The slot indirection is still required, because a record written after a partial CAS must never be mistaken for a valid identity.

---

### A4 — `push` must verify the claim epoch

**The zombie writer.** A process `SIGSTOP`ped, or stalled in a GC pause or on a page fault against a slow device, can be judged dead, have its claim reaped, and then *resume* and continue pushing to an edge another process now owns. Two writers, silent corruption — precisely the failure the claim model exists to prevent.

**Fix.** One relaxed load per push, on a cacheline the writer already touches:

```rust
if self.claim.epoch.load(Ordering::Relaxed) != self.epoch {
    return Err(PushError::ClaimRevoked { edge: self.id });
}
```

Cost is ~1 ns. **§6.1 changes why this exists:** with claims held as kernel locks, a stalled writer keeps its lock and cannot be reaped, so the zombie is impossible by construction. Keep the check anyway as defence in depth against a bug in the `ClaimRecord` path — but do not describe it in comments as the sole barrier, or someone will delete it after reading §6.1.

---

### A5 — The slot sequence writer forces parity instead of incrementing

A writer killed between `seq.store(s+1)` (odd) and `seq.store(s+2)` (even) leaves the slot permanently odd. Every reader that reaches it burns `SEQ_RETRY_LIMIT` and returns `SlotContended`. The sample was never published — `head` was not bumped — so no *correct* reader looks at it, but when the ring wraps, the next writer reads an odd `s` and its `s+1` lands even, inverting the protocol for that slot.

**Fix.** Force the parity rather than incrementing:

```rust
let s    = slot.seq.load(Ordering::Relaxed);
let odd  = s | 1;                                    // self-heals a stale odd
slot.seq.store(odd, Ordering::Relaxed);
core::sync::atomic::fence(Ordering::Release);
// ...write stamp and pose data...
slot.seq.store(odd.wrapping_add(1), Ordering::Release);
```

The seqlock still works: any reader that observed the stale odd value retried without reading, so no reader can be mid-read holding it. Additionally, **claim acquisition normalizes the slot at `head & mask`** — one store, once, at claim time.

---

### A6 — The arena gains a participant table (layout change)

New region, sized `max_participants * 128`, placed between the claim table and the edge table. `ArenaHeader` gains `participant_table_off: u32`, `max_participants: u32`, and `participant_count: AtomicU32`, taken from `_reserved`. Default `max_participants = 64`.

**This is the only amendment that changes the layout, and therefore the only one with a `FORMAT_VERSION` consequence.**

---

### A7 — Header identity fields

`ArenaHeader` gains `owner_start_time: u64` alongside the existing `creator_pid`, and `boot_id: [u8; 16]` replaces the `u64` (a boot ID is a 128-bit UUID; truncating it to 64 bits loses the property that makes it useful). Both fit in `_reserved`.

`instance_uuid: [u8; 16]` joins them (§3.6 step 4). It is what makes a split-brain detectable after the fact: two processes that believe they share an arena but print different `instance_uuid`s are on different arenas, and no other field distinguishes them.

---

### A8 — Interning must not spin forever on a dead claimant

Phase 1 §5.1's interning waits for `ids[i] != U32_MAX` with an **unbounded**
spin. A process that wins the hash-slot CAS and dies before publishing the id
leaves that slot claimed and unpublished forever, and **every future interner of
that name spins forever**. In one process this is unobservable — the dead
process took its readers with it. Across processes it wedges every live
participant that touches the name.

This is the same class of defect as A1, and it is the crash point
`intern.after_hash_cas_before_id_store` in §11.3.

**Fix.** Record the claimant alongside the hash, bound the spin, and take the
entry over if the claimant is dead:

```rust
/// Parallel to `hashes`/`ids`: the participant slot that won the CAS, + 1.
/// Written BEFORE the hash is published, so a reader that sees the hash can
/// always resolve who claimed it.
claiming: [AtomicU32],

// waiter, after INTERN_SPIN_LIMIT iterations:
let owner = claiming[i].load(Acquire);
if owner != 0 && !is_alive(&participants[(owner - 1) as usize], boot) {
    // Take over: republish `claiming`, write the record, publish the id.
    if claiming[i].compare_exchange(owner, my_slot + 1, AcqRel, Acquire).is_ok() {
        write_record(id); ids[i].store(id, Release);
    }
}
```

The takeover is idempotent and CAS-guarded, so concurrent rescuers cannot both
publish. `is_alive` is §6.2's predicate, which fails **safe** — an unreadable
`/proc` means "alive", so a slow interner is never stolen from.

Note this interacts with Phase 1's `ID_FAILED` sentinel, which already handles
the *capacity* failure by publishing a terminal marker. A8 handles the *crash*
failure, which has no such marker because the claimant never got to write one.

## 2. Platform, dependencies, feature gating

**NORMATIVE.** Shared memory is **Linux-only** in Phase 2, requiring **kernel ≥ 3.17** for `memfd_create` and `F_ADD_SEALS`, and **≥ 3.15** for OFD locks (§3.3). Target and test on 5.15 (Ubuntu 22.04 / JetPack 6) and current stable. `MADV_POPULATE_WRITE` (§7.1) needs ≥ 5.14 and has a documented fallback.

Do not build a POSIX abstraction layer. macOS and Windows keep `HeapArena` and in-process operation; a file-backed unsealed `MappedArena` for macOS developer ergonomics is acceptable *later*, explicitly labelled dev-only, and must not shape the Linux design.

| Crate | New dependencies |
|---|---|
| `tf_tree_arena` | `rustix` (feature `shm`, `mm`, `fs`, `net`) — no libc crate, no C build step |
| `tf_tree_ipc` (new) | `rustix`, **and `libc` for `fcntl(F_OFD_*)` only**. Attach protocol, participant registry, reaping. |
| ~~`tf_tree_record` (new)~~ | **Retired by [`0047`](./decisions/0047-the-recording-this-reader-would-refuse.md).** The isolation argument was honoured by a different crate and without the second dependency: MCAP arrived in `tf_tree_ingest`, and no crate in this workspace declares `serde` at all — `grep -rn '^serde' crates/*/Cargo.toml Cargo.toml` finds only `serde_json` |
| `tf_tree_core` | **none.** Unchanged. |

`tf_tree_core` gaining a dependency in this phase is a design failure, not a tradeoff. If you find yourself needing one, stop and report it.

**The `libc` exception, recorded rather than hidden.** This section originally said "no libc crate". `rustix` 1.1 turned out to have **no OFD locking at all** — its `fcntl_lock` is the classic, whole-file `F_SETLK` that §3.3 rejects by name, and `flock` is whole-file too. The alternatives were to hand-roll the `fcntl` syscall or to take `libc`. Hand-rolling was implemented first and then rejected on review: it pinned syscall numbers and `struct flock`'s layout by hand and refused to compile on any architecture except x86-64 and aarch64 — including riscv64 and ppc64le. A hand-maintained kernel ABI underneath the primitive the whole rendezvous depends on is a worse risk than one more dependency, and `libc` introduces **no C build step**, which is what this rule was protecting against. Scope it to `tf_tree_ipc` and to that one call.

---

## 3. Discovery, rendezvous, and ownership

**This is the seam that makes everything else usable.** A process calls `tf_tree::open()` and either joins the arena that already exists on this machine or creates it. No configuration file, no daemon, no start-order requirement, and no possibility of two processes silently ending up on different arenas.

The design principle: **do not implement leader election — borrow the kernel's.** A rendezvous needs exactly three properties: mutual exclusion, automatic release when the holder dies, and a way to ask whether anyone holds it. Linux file locks provide all three, maintained by the kernel, with no timeouts, no heartbeats, and no stale state that can survive a `SIGKILL`. Every distributed-consensus flavoured problem in this section dissolves into one `fcntl` call.

### 3.1 The sharing boundary is the runtime directory — NORMATIVE

Two processes share an arena **if and only if they resolve to the same runtime directory, domain, and name.** That is the whole mental model, and it should be the first sentence of the user-facing documentation.

```
<runtime_dir>/<domain>/<name>.lock     # rendezvous + kernel-managed liveness
<runtime_dir>/<domain>/<name>.sock     # SOCK_SEQPACKET, owner-bound, FD passing
```

Resolution order for `runtime_dir`, first hit wins:

1. `$TF_TREE_RUNTIME_DIR`
2. `$XDG_RUNTIME_DIR/tf_tree` (normally `/run/user/<uid>/tf_tree`, tmpfs, per-user, cleaned on logout)
3. `/run/tf_tree` if writable (system services)
4. `/tmp/tf_tree-<uid>`, created mode `0700`

**Containers:** sharing the runtime directory is a volume mount (`-v /run/tf_tree:/run/tf_tree`), and not sharing it is complete isolation. This is deliberately the same idiom people already use for X11 and D-Bus sockets, and it means the isolation boundary is inspectable with `ls` rather than being an implicit property of a namespace. Do **not** use abstract Unix sockets, which would tie the boundary to the network namespace — an invisible, surprising, and usually wrong place to put it.

**NORMATIVE check:** `statfs` the runtime directory at open and reject NFS (`0x6969`) and CIFS. File locks over network filesystems have subtly different semantics and the entire rendezvous depends on them being exact.

### 3.2 Identity and defaults

```
domain: $TF_TREE_DOMAIN, else $ROS_DOMAIN_ID, else 0
name:   $TF_TREE_NAME,   else "default"
```

Falling back to `ROS_DOMAIN_ID` is deliberate: a ROS 2 system already has its isolation configured, and inheriting it means `tf_tree` partitions exactly the way the rest of the stack does with no additional setup. Two robots on one bench, or a simulator alongside hardware, stay separated because they were already separated.

The zero-argument case must work:

```rust
// A consumer. The defaults are the consumer: read-only, and `CreatePolicy::Never`.
let tree = tf_tree::open()?;                 // join, or ArenaAbsent
let tree = tf_tree::open_named("robot")?;

// A consumer that may start before the publisher's process does.
let tree = tf_tree::Open::new()
    .domain(7)
    .name("robot")
    .await_open(Duration::from_secs(5))?;    // 0019 §2b

// A creator. Read-write is required: a read-only attach *cannot* create.
let tree = tf_tree::Open::new()
    .domain(7)
    .name("robot")
    .mode(AttachMode::ReadWrite)
    .create(CreatePolicy::IfAbsent)          // IfAbsent | Never | Always
    .layout_if_creating(layout)              // sizes the arena if we turn out to be the creator
    .open()?;
```

> **Amended by [`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md) §2a.**
> `Open::new()`'s `create` default was `IfAbsent`; it is **`Never`**, so the builder's own defaults are
> the consumer. And `AttachMode::ReadOnly` with any creating policy is now
> `OpenError::ReadOnlyCannotCreate` rather than a request that could only ever produce an arena its
> creator cannot write. The sample above used to show exactly that combination as the ordinary consumer
> spelling.

`CreatePolicy::Never` is what a consumer gets without asking. It is the default because a consumer that creates an arena the estimator has not populated looks healthy and finds nothing — worth stating explicitly in a supervised deployment even so.

### 3.3 The lock file — NORMATIVE

A small regular file used as a lock substrate with **open file description locks** (`F_OFD_SETLK`, Linux ≥ 3.15). OFD locks, not classic POSIX `F_SETLK`, because classic locks are dropped when *any* file descriptor to the file is closed anywhere in the process — an unfixable footgun for a library that shares an address space with code it does not control.

| Offset | Meaning |
|---|---|
| byte 0 | **Ownership.** Exclusive. The holder serves the socket. |
| byte 1 | **Topology mutation** (A2). Exclusive, held for one `Tree::reparent`. |
| bytes 2–15 | reserved |
| bytes 16 + *i* | **Participant liveness** for slot *i*. Exclusive, held for the lifetime of the attachment. |
| 4096 + 64·*i* | **Identity record** for slot *i*: pid (`0..4`), start_time (`4..12`), boot_id (`12..28`), mode (`28`), name (`32..48`), pid_ns_inode (`48..56`). `29..32` and `56..64` are padding and read zero. Written with `pwrite` before taking the slot lock. Advisory; diagnostics only. |

**`pid_ns_inode` is the `nsfs` inode of the PID namespace its writer's pid is drawn from, and `0` means *unknown namespace*** ([`0033`](./decisions/0033-the-identity-record-cannot-name-a-namespace.md)). It is here because a recorded `pid` is namespace-local: resolved against another namespace's `/proc` it names a different process or none, and the rest of the identity triple cannot tell — `boot_id` is identical across every namespace on one host and the kernel has no per-namespace boot id. An observer compares it against its **own**, never against one read through the recorded pid, which is a probe that fails open. Zero must keep the pre-`0033` behaviour rather than mean "namespace 0": a record written before the field existed reads zero, and so does one whose writer could not read `/proc`.

**The field cost no stride and no `FORMAT_VERSION`.** `name` narrowed from `[u8; 32]` to `[u8; 16]`, which is free because the kernel caps `comm` at 15 bytes plus its NUL (`TASK_COMM_LEN`) — measured, on a participant whose binary basename is 52 characters — so `48..56` was padding in every record ever written. The 64-byte stride is unchanged, the second page stays exactly one page, and **this is the lock file, not the arena**: no arena field moved and no layout hash changed.

Verified behaviour on Linux 6.18:

| Operation | Result |
|---|---|
| Second process takes a held byte | `EAGAIN` |
| Holder dies without unlocking | lock released by the kernel, immediately |
| `F_OFD_GETLK` on a free byte | `l_type = F_UNLCK` |
| `F_OFD_GETLK` on a held byte | held, but **`l_pid = -1`** |

**That last row matters.** An OFD lock belongs to an open file description, not a process, so `GETLK` cannot report a PID. The lock file therefore answers *"is anyone alive?"* — which is all the rendezvous needs — while *"who?"* comes from the identity records, which is why they exist as plain `pwrite` data rather than living only in the arena. A process that cannot reach the arena can still run `tf_tree doctor` and get names and PIDs.

Because liveness is now a kernel fact rather than an inference, **`/proc` parsing and PID-reuse defence are no longer on the rendezvous path at all.** They remain only for the arena's advisory participant table (§5).

### 3.4 `open()` — NORMATIVE algorithm

```
deadline = now + open_timeout (default 5 s)
loop {
    // 1. Someone is already serving. Join.
    if connect(sock) succeeds {
        Hello handshake -> recv arena fd -> validate -> mmap
        pwrite identity record; F_OFD_SETLK participant byte
        return Joined
    }

    // 2. Nobody is serving. Try to become the owner.
    if F_OFD_SETLK(byte 0, exclusive) fails {
        // another process is mid-bind; it will be serving shortly
        backoff; continue
    }

    // 3. DELETED 2026-08-27 (#275, decision 0037). It short-circuited past
    //    step 4 for a process declaring it already held the arena, and no new
    //    file description can verify that declaration: F_OFD_GETLK reports
    //    conflicts and cannot name a holder, so "I hold byte n" and "a live
    //    peer holds byte n" are the same answer. Do not re-add it.

    // 4. SPLIT-BRAIN CHECK. Is any participant byte locked?
    if any participant byte is held {
        // an arena exists and is alive, but its holder has not taken over yet.
        release byte 0; backoff; continue    // yield to the real participant
    }

    // 5. Serve.
    if creating {
        // The creator's slot is 0, and the ACQUIRE is the check: step 4 is a
        // separate pass, so participant byte 0 can change hands between them.
        if F_OFD_SETLK(participant byte 0, exclusive) fails {
            release the ownership byte; backoff; continue   // step 4, arriving late
        }
        pwrite identity for slot 0
        memfd_create; ftruncate; mmap; init header; seal (§3.6)
    }
    unlink stale sock; bind sock.tmp; chmod; rename -> sock; listen
    // No takeover branch: a heir is already a participant and registers
    // nothing (decision 0028 question 3). Nothing produces TookOver.
    return Created
}
on timeout -> Err(ArenaHeldButUnreachable { holder_slots, first_slot, first_pid, ownership_held })
```

> **Amendment (2026-08-27, [`0037`](./decisions/0037-a-takeover-is-not-a-second-open.md)): step 3 and the takeover branch above are deleted, and §3.5 has no implementation path through this algorithm.** (**Still true on 2026-08-28, when §3.5 was implemented — because it is implemented somewhere else: a method on the session a survivor already holds, never a re-entry into this algorithm.**)
>
> The block above is NORMATIVE and mandated a heir taking a participant byte — the one act `0028` question 3 forbids, since the slot is baked into every claim the heir already holds. Issue #201 found the arm handing back the first *free* byte; two rounds of repair produced five executed unsound states, four introduced while fixing the one before. The root cause is that `F_OFD_GETLK` answers *"does anyone **else** hold this byte"*, so the declaration the branch rested on is unverifiable from a fresh file description.
>
> **The consequence, and its counterpart, which arrived on 2026-08-28.** For one day `Session::release_ownership` (§3.5's "give up the owner role while staying attached") had no route by which any survivor became owner: a fresh `open()` takes ownership at step 2, meets step 4 against the survivors' held bytes, releases, and times out — for as long as any survivor lives. That was already true of every caller not using the unsound arm; deleting the arm made it total. **`0037` open question 5 is now answered, and the answer is that `release_ownership` is the correct half of a pair**: its other half is `Session::take_over_ownership`, a method on the session that already holds the byte, reached through `tf_tree::Tree::inherit_ownership` (§3.5). Neither is reachable from this algorithm and neither should be — a survivor does not re-enter `open()`, which is the whole of `0037`.

**Step 4 is the whole design.** Without it, this sequence is possible: the owner dies; a fresh process starts, finds no socket, wins the ownership lock before any surviving participant notices the `HUP`, and creates a *second* arena. The surviving participants keep using the first. Two arenas, both live, silently diverging — worse than any failure to start, because nothing reports an error and the robot's transform tree is quietly inconsistent between nodes.

The check is **deterministic, not a grace period.** If any participant byte is locked, a live arena exists; a fresh process must not create one, full stop. No timing assumption, no window to tune.

**A creator takes participant slot 0, and the acquire is the check.** Not "any
free byte": the arena's first `FREE` record is 0, and the facade indexes the lock
byte and the arena record with **one** integer, so a creator on any other byte
hands out a tree whose liveness predicates disagree with themselves (#201).

Step 4 does not establish that on its own, and the gap is not one instruction:
`any_participant_held` probes byte 0 **first** and then 63 more before it
returns, so byte 0 can be taken for the rest of that scan. Measured, with a
second open file description toggling byte 0 across 4000 iterations of exactly
the two calls steps 4 and 5 make, **2242 took a non-zero byte**. It is reachable
from outside this workspace whatever this workspace does, because
`LockFile::try_take_participant` is public API on a published crate.

So the two become one operation — a single `F_OFD_SETLK` on participant byte 0,
whose atomicity is the kernel's. There is no window because there is no gap, and
it is cheaper than the scan it replaces. Contention is not a new failure: it is
step 4's condition arriving late, so it takes step 4's branch.

**`--force-new` is not exempt, and does not need to be.** It skips step 4 by
design, so a contended byte 0 there means a *live* participant holds it —
forcing a fresh arena past one is the split brain the flag exists to resolve, not
to cause. When a live holder does persist, the loop times out into the error
below, which names it.

**Which live holder, though, is the whole of it, and an earlier revision of this
paragraph got the reason wrong in a way that contradicted the paragraph below
it** (#257). It said the wedged arena the hatch is for has *dead* participants
whose bytes the kernel released when they died — while the next paragraph
describes the arena as wedged by a `SIGSTOP`ped participant, which is alive and
holding its byte. The next paragraph is the true one. A wedge **requires** a live
holder: if every holder were dead, no participant byte would be held, step 4
would not fire, and an ordinary create would already succeed with no force at
all. So "the participants are dead" is not a case the escape hatch is *for* — it
is the case where nobody needs it.

The reason byte 0 is nonetheless free in exactly the case the hatch is for is the
slot assignment, not death: **byte 0 is the owner's**, taken by the creator and
held for its whole life, and the owner assigns every joiner a slot `>= 1`. So the
escape hatch creates iff nothing is serving **and** the ownership byte is free
**and** participant byte 0 is free. Its *ordinary* cause is that the owner is
gone and non-owner participants survive — the stranded-participant case this
section names — but "the owner is gone" is not the rule and must not be
substituted for it: `Session::release_ownership` (§3.5, published API) frees the
ownership byte and **keeps** byte 0, leaving a live non-owner on the creator's
slot with no owner at all, and there the hatch still refuses. §0.0's
participant-registry row documents that producer and
`defect_201_release_ownership_strands_a_live_non_owner_on_byte_0` pins it. The
three bytes are the rule; ownership is only the usual reason they are free. It
cannot pass a live holder of byte 0 or of the ownership byte, and it does not
need to: the remedy there is to stop that process, after which the
kernel frees the byte and the create that was refused goes through — and once the
last holder is gone, an ordinary `IfAbsent` create does it with no force at all
(`a_live_participant_prevents_a_second_arena`'s positive control,
`crates/tf_tree_ipc/tests/multiprocess.rs`, kills the holder and creates).
Pinned by `the_escape_hatch_creates_over_a_stranded_participant`
(the hatch working), `a_live_byte_0_refuses_both_policies_and_says_no_force_can_pass`
and `a_held_ownership_byte_refuses_the_hatch_and_freeing_it_lets_one_through`
(the two states it cannot pass, each with the freeing control), all in
`crates/tf_tree/tests/rendezvous.rs`. `IpcError::ArenaHeldButUnreachable` carries
`ownership_held` so its message can tell an operator which of the three they are
looking at.

The timeout case is also correct behaviour rather than a limitation. If a participant is `SIGSTOP`ped and never takes over, no new process can join — and that is the right answer, because the alternative is divergence. The error names the stuck slots and their identities, so an operator can see exactly what to kill. Provide `--force-new` as an explicit, loud escape hatch that abandons the existing arena **when the creator's byte 0 and the ownership byte are both free** — which is the stranded-participant case above, and is the whole of what it can do; against a live holder of either, the paragraph above applies and the remedy is to stop that process. Never take the path automatically.

### 3.5 Ownership migrates; the data plane never pauses — NORMATIVE

Ownership is a **role**, not a property of the arena. The arena is the memfd, which lives as long as any mapping does; the owner is merely whichever participant currently holds byte 0 and the listening socket.

A surviving **read-write** participant that observes the owner's hangup inherits the role:

```
keep serving lookups from the existing mapping -- untouched, uninterrupted

poll(OUR OWN attach socket, timeout 0) for POLLHUP/POLLERR   -- the caller's loop; nothing polls for it
  no hangup -> the owner is alive; nothing to do
  hangup    -> F_OFD_GETLK(byte 0)                             -- 0043: is the ROLE vacant?
               held -> somebody already took over, or is mid-bind. Nothing to do.
               free -> if this attachment is read-only: we cannot be the heir (D18).
                         Keep reading; wait for a read-write survivor to take it.
                       otherwise: F_OFD_SETLK(byte 0) ON THE DESCRIPTION THIS
                                  SESSION ALREADY HOLDS
                         acquired  -> bind a pid-suffixed socket, listen, rename it
                                      over the rendezvous path, serve OUR EXISTING
                                      segment fd
                                      on any failure: release byte 0, restore the
                                                      attachment, stay a plain
                                                      participant
                         contended -> another survivor is taking over; KEEP OUR SLOT.
                                      The next poll's GETLK sees the byte held and
                                      says "nothing to do" by itself -- which is
                                      what the earlier "retry connect with backoff"
                                      was for, reached without a reconnect (0043).
```

Five requirements. The first is the one the deleted attempt could not satisfy at all; the rest are what an heir must not break on the way in:

1. **The ownership lock is taken on the file description the session already holds. A takeover may not be expressed as a second `open()`.** `Open::open` builds its own `LockFile`, and from a fresh description `F_OFD_GETLK` answers *does anyone **else** hold this byte* — so a caller holding byte *n* and a live peer holding byte *n* are indistinguishable, and the declaration a takeover rests on ("I already hold the arena at slot *n*") cannot be verified at all. Taking the lock on the existing description makes the invariant structural: nothing is verified because nothing can have moved.
2. **The heir keeps its existing participant slot, byte and arena record, and does not register a second time.** The slot is baked into every claim and every topology guard it already holds — A3 encodes claim ownership as `participant_slot + 1` — so an heir that registered again would arrange for its own live claims to be reaped.
3. **The heir serves the segment it already has.** It does not `memfd_create` a new one. An heir on a creating path would own a fresh, empty arena under the rendezvous name every survivor is still mapped through: forking the tree rather than inheriting it.
4. **Publication is a rename, not an unlink-then-bind.** The heir binds a pid-suffixed path, listens, and `rename`s it over the rendezvous path, so a client sees the old socket or a listening one and never a half-built one (§3.7).
5. **Serving must stop before byte 0 is released.** The reverse order leaves a window in which a successor takes ownership and binds while the old owner is still answering handshakes on the old socket — two servers on one path, clients split between them.

> **Amendment (2026-08-28, [`0037`](./decisions/0037-a-takeover-is-not-a-second-open.md) question 4), and it replaces this section's protocol rather than annotating it.** What stood here was `try F_OFD_SETLK(byte 0)` / `acquired -> unlink stale sock; bind; listen; serve OUR existing fd`, under the opening "When a participant's client socket reports `HUP`". **Both of its premises were false the day `0037` was written**: nothing watched the client socket for `HUP`, so the trigger did not exist, and the arm the acquisition led to had been deleted by #275 — while §3.5 alone, still headed NORMATIVE, said neither. The pseudo-code above is the shipped protocol. Requirement 1 is the part that is genuinely new; requirement 4 **corrects** the old text, which said `unlink stale sock` where §3.7's publication is an atomic `rename` (`OwnerServer::bind_at`); 2, 3 and 5 were the intent throughout. §0.0's *Ownership migration (§3.5)* row carries the implementation, the three tests and the history — including the five executed unsound states two rounds of repair produced against the old shape. `0037` question 4 is answered there in the same terms this amendment uses: the algorithm was right, the plumbing and the trigger were what it lacked.

The shipped spelling: `tf_tree_ipc::peer_hung_up` and `tf_tree::Tree::owner_lost` are the poll, and `tf_tree_ipc::Session::ownership_held` is the `GETLK` that follows a hangup ([`0043`](./decisions/0043-owner-lost-is-a-question-about-the-owner.md)); `tf_tree_ipc::Session::take_over_ownership` is the lock, on the session's own description; `tf_tree::Tree::inherit_ownership` is the seam that binds and serves, and reports `Inheritance::{Inherited, OwnerAlive, Contended, ReadOnly, NotApplicable}`. Requirement 5 is expressed as **field declaration order** on `Attachment::Owner` — Rust drops an enum variant's fields in declaration order (RFC 1857), and the serving thread's handle is declared before the session — rather than as an `impl Drop`, which is why that impl is gone.

**The trigger is the caller's, and this is NORMATIVE too: no background thread, no daemon, no watcher a user must run.** A survivor evaluates `owner_lost()` when convenient — between control cycles, in its own loop — and *nothing evaluates it on the survivor's behalf*. [`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md) is the reason: every process a user is *required* to run is a place adoption dies, and a thread per attachment is the library-shaped version of the same cost. The honest consequence is that a fleet whose survivors never call `owner_lost()` still ends up with an ownerless arena, and new joiners still time out on `ArenaHeldButUnreachable` — the mechanism is here, the policy belongs to the integrator, and `docs/RUNBOOK.md` says so where an operator will meet it.

**Lookups do not stop, slow down, or observe anything during a takeover.** Not during the poll, not during the lock, not during the bind. `Plan::at` lives in `tf_tree_core` and touches the mapping and the `Guard` and nothing else; the lock file, the socket and the attachment are unreachable from it, and ownership lives entirely in the control plane. **Re-verified against the shipped code rather than restated**: the claim survives the amendment above unchanged, and it is the answer to the first question any integrator asks about owner death — *nothing observable* — which is a strong one. That used to carry one caller-side qualification — *"`inherit_ownership` takes `&mut self`, so the inheriting handle's own `Guard<'_>` cannot be outstanding across the call"* — and **it is gone as of 2026-08-29** ([`0044`](./decisions/0044-recovery-the-languages-a-robot-is-written-in-cannot-reach.md)). The attachment lives behind a `Mutex`, the method takes `&self`, and a control loop holding one guard per cycle can recover wherever it is convenient rather than only between cycles. `a_guard_may_be_held_across_inheriting_ownership` is the test, and it is a compile-time property: with `&mut self` the file does not build. Every other handle, thread and process reads throughout, as before.

This supersedes the previous draft's "ownership is configured, not negotiated". Ownership is neither configured nor negotiated: it is *inherited*, and the kernel picks the heir.

### 3.6 Creation sequence — verified on Linux 6.18

```
1. memfd_create("tf_tree.<domain>.<name>", MFD_CLOEXEC | MFD_ALLOW_SEALING)
2. ftruncate(fd, arena_size)
3. mmap(NULL, arena_size, PROT_READ|PROT_WRITE, MAP_SHARED, fd, 0)     // NOT MAP_POPULATE (§7.1)
4. initialize header: magic, format_version, layout_hash, arena_size, instance_uuid, boot_id
5. fcntl(fd, F_ADD_SEALS, F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_SEAL)
6. madvise(base, len, MADV_DONTFORK)      // §7.3 -- must precede any fork
7. madvise(base, len, MADV_HUGEPAGE)      // best-effort
```

Step 5 is load-bearing:

| Operation | Result |
|---|---|
| `F_ADD_SEALS SHRINK\|GROW` while a writable mapping is held | **succeeds** — the owner keeps write access |
| `F_ADD_SEALS WRITE` while a writable mapping is held | `EBUSY` — cannot over-seal by accident |
| `ftruncate` shrink after sealing | `EPERM` |
| `F_ADD_SEALS` anything after `F_SEAL_SEAL` | `EPERM` |
| `F_GET_SEALS` | `0x7` |

**Sealing against shrink is what makes `SIGBUS` structurally impossible.** Without it any holder of the fd could truncate the segment and every reader touching a truncated page would fault inside a lookup — unrecoverable, in the middle of a control loop, with nothing a library can do about it. Do not skip step 5, and do not substitute `shm_open`, which cannot be sealed and additionally leaves stale segments in `/dev/shm` after a crash.

### 3.7 Attach

```
1. connect SOCK_SEQPACKET
2. send HelloRequest
3. recvmsg -> HelloResponse + SCM_RIGHTS fd   (or a rejection carrying no fd)
4. fstat(fd).st_size == response.arena_size
   F_GET_SEALS & (F_SEAL_SHRINK|F_SEAL_GROW) == both     // refuse an unsealed segment
5. mmap(PROT_READ [| PROT_WRITE], MAP_SHARED)
6. verify header magic, format_version, layout_hash, arena_size, boot_id
7. madvise DONTFORK, HUGEPAGE
8. pwrite identity record; F_OFD_SETLK participant byte
9. KEEP THE SOCKET OPEN for the lifetime of the attachment
```

**Step 9:** the socket is not a handshake channel to be closed after use — it is how a participant learns the *owner* has died, in microseconds, with no polling. Participant death is detected by the lock file; owner death is detected by the socket. Both are kernel-maintained; neither involves a timeout.

Message structs are fixed-size `#[repr(C)]`, little-endian, over `SOCK_SEQPACKET` so framing comes from the kernel:

```rust
#[repr(C)]
pub struct HelloRequest {
    pub magic: [u8; 8],            // b"TF_TREE\0"
    pub format_version: u32,
    pub layout_hash: u32,
    pub mode: u8,                  // 0 = ReadOnly, 1 = ReadWrite
    _pad: [u8; 7],
    pub client_pid: u32,
    _pad2: u32,
    pub client_start_time: u64,
    pub client_boot_id: [u8; 16],
    pub client_name: [u8; 32],
}

#[repr(C)]
pub struct HelloResponse {
    pub magic: [u8; 8],
    pub status: u32,               // 0 = Ok
    pub format_version: u32,
    pub layout_hash: u32,
    pub participant_slot: u32,     // matches the lock-file byte the client must take
    pub arena_size: u64,
    pub instance_uuid: [u8; 16],
    pub owner_pid: u32,
    _pad: u32,
}
```

Rejections: `VersionMismatch`, `LayoutMismatch`, `BootIdMismatch`, `NoParticipantSlots`, `ModeNotPermitted`, `Malformed`. Each must name both sides' values.

`LayoutMismatch` is the one operators will hit — a binary built against a different struct layout. **The message must say exactly that** and print both hashes, because the raw symptom is attach failing on a machine where everything looks fine, which is otherwise a multi-hour debugging session. This is why `layout_hash` was computed in Phase 1 despite nothing reading it then.

### 3.8 Capacity without planning — NORMATIVE

Fixed capacity (D4) is in tension with zero-configuration startup: whoever creates the arena fixes the layout, and a process that joins later cannot grow it. Resolving that tension by making users plan capacity would destroy the seamlessness this section exists to provide.

**The resolution is that virtual capacity is nearly free.** Measured on Linux 6.18 with a 1 GiB memfd:

| State | Pages charged |
|---|---|
| After `ftruncate` to 1 GiB | **0 KiB** |
| After `mmap` without `MAP_POPULATE` | **0 KiB** |
| After touching 16 MiB | exactly 16 MiB |

So the default layout is **generous** — 1024 frames, 1024 edges, 8192 samples per edge, roughly 600 MiB of address space — and resident cost is only what is actually declared and used. A robot with 24 frames and 4 dynamic edges pays for 24 frames and 4 dynamic edges.

**Per-edge population, not whole-arena population.** `MADV_WILLNEED` does *not* pre-fault a memfd region (measured: no change in charged pages). Use `madvise(MADV_POPULATE_WRITE)` (Linux ≥ 5.14, measured working) on an edge's stamp and pose ranges at `declare_dynamic` time, falling back to an explicit zeroing write on older kernels. This moves faulting to declaration — startup, where it belongs — instead of into the first lookup, without paying for capacity nobody declared.

`doctor` warns at 80% occupancy of frames, edges, or participants, and the error on exhaustion (`ArenaFull`) must state the current limit and that raising it requires recreating the arena.

### 3.9 Teardown

- **A participant dies** → its lock byte releases, its mapping drops, and the owner reaps its arena-side records (§6). Nothing else notices. **Both halves of "records" are done from the hangup callback as of 2026-08-29**, and only one of them was before: the callback freed the participant *record* and left every **claim** that participant held, so a supervised node that was killed and restarted was granted its predecessor's slot and then refused its own edges with `EdgeAlreadyClaimed` — permanently, because `reap_claims` skips `own_slot` and the restarted process is therefore the one process that cannot repair it. `Tree::reap_participant` had been written for this call site (its doc names `EPOLLHUP` as how the owner learns *which slot* went away) and had no caller in the workspace outside a benchmark and a test helper. **Two producers of a stale claim remain, and neither is a hangup**: a dead **owner**, whose hangup nobody observes, and a `TreeBuilder::build_shared` participant, which has no socket at all. For those, `Tree::reap()` from a surviving read-write participant is still the only collector — and it is reachable from Rust only, which `docs/RUNBOOK.md` records.
- **The owner dies** → surviving participants take over (§3.5). Lookups never pause.
- **The last mapping drops** → the kernel frees the segment. **No stale segments, ever** — the second reason to prefer memfd over `shm_open`.
- A stale **socket path** may persist; it is unlinked by whoever wins ownership. A stale **lock file** is harmless: it holds no state, only locks, and locks cannot be stale.

### 3.10 Trust model — NORMATIVE, and state it in the public docs

Participants are **mutually trusting, same-user, cooperating processes**. A read-write participant can corrupt any part of the arena, and no checksum changes that. What the design does guarantee:

- A **read-only** participant cannot corrupt anything, enforced by the MMU (§8). This is the only real boundary, and it is the default.
- A participant that **crashes**, at any instruction, cannot corrupt anything or wedge any other participant. Hard requirement, tested by fault injection (§11.3).
- A participant that **hangs** cannot corrupt anything and cannot be mistaken for a crashed one (§6).

"Shared memory IPC is not a sandbox" belongs in the README.

---

## 4. `MappedArena`

**NORMATIVE:** the diff against Phase 1 outside `tf_tree_arena` and the new `tf_tree_ipc` crate must be **zero lines in the read path**. `PoseSlot`, `EdgeBuffer`, `Plan::at`, bracket search, and interning are byte-identical code operating on a different base pointer.

**The premise is tested, not merely asserted.** "A different base pointer" only works if nothing in the arena is an absolute address, which Phase 1 documented but never checked. `crates/tf_tree_bench/tests/relocation.rs` is that check: it byte-copies a populated arena to a different address, wraps the copy in a minimal `Arena` impl, and requires **bit-identical** results across every frame pair in the fixture, plus frame-name resolution and header validation. Keep it green: a regression that caches one resolved address would otherwise surface for the first time in another process, as a wild read rather than an error.

**The premise is now tested, not merely asserted.** "A different base pointer" only works if nothing in the arena is an absolute address, which Phase 1 documented but never checked. `crates/tf_tree_bench/tests/relocation.rs` is that check: it byte-copies a populated arena to a different address, wraps the copy in a minimal `Arena` impl standing in for `MappedArena`, and requires **bit-identical** results — not approximate — across every frame pair in the fixture, plus frame-name resolution and header validation. It guards against a vacuous pass twice (the copy must land at a different address; more than 1000 queries must actually be compared).

It passes today, which is the evidence that the "zero lines in the read path" claim above is achievable rather than aspirational. Keep it green: a regression that caches one resolved address would otherwise surface for the first time in another process, as a wild read rather than an error.

```rust
pub struct MappedArena {
    base: NonNull<u8>,
    len: usize,
    fd: OwnedFd,
    mode: AttachMode,
    _socket: Option<OwnedFd>,      // liveness — dropping this signals detach
    participant_slot: u32,
}

unsafe impl Arena for MappedArena {
    fn base(&self) -> *mut u8 { self.base.as_ptr() }
    fn len(&self) -> usize { self.len }
}
```

`Drop` order is fixed and matters: publish detach in the participant record, then `munmap`, then close the socket, then close the fd. Publishing detach before unmapping means the owner's reap path never races a half-torn-down participant.

Write a test asserting `Tree` is generic over `A: Arena` with no `MappedArena`-specific branches, and a compile-fail test asserting `Publisher` cannot be constructed from a `ReadOnly` arena.

---

## 5. Participant registry

```rust
#[repr(C, align(64))]
pub struct ParticipantRecord {
    /// 0 = free, 1 = attaching, 2 = live, 3 = detaching. Published last.
    pub state: AtomicU32,
    pub mode: u8,                  // 0 RO, 1 RW
    _pad0: [u8; 3],
    pub pid: u32,
    _pad1: u32,
    pub start_time: u64,           // /proc/<pid>/stat field 22 — defeats PID reuse
    pub attach_nanos: i64,
    pub heartbeat: AtomicU64,
    pub name: [u8; 32],
    _pad2: [u8; 24],
}
```

**Slot assignment and record population are done by different processes, and this section used to say otherwise.** The *owner* assigns the slot: its accept loop scans for an index whose lock byte the kernel reports free and whose arena record is either absent or **collectable**, and returns it as `HelloResponse.participant_slot` (§3.7). "Collectable" is §5.1's predicate and not a reading of `state`: a record left behind by a participant that never ran its `Drop` is reclaimed — `reclamation_verdict` then `ParticipantTable::reclaim`, on the word that verdict was formed against — *before* the slot is granted. Deciding without reclaiming would be useless rather than merely incomplete, because `fill_slot` CASes from `FREE`: a slot correctly judged collectable and left `LIVE` is refused to the very joiner the grant is for ([`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md) plan step 3, which is what replaced this loop's `identity(slot).is_some()` skip — the defect that record was opened about). The *joiner* writes the record — its own, **with a CAS**, and **after** it has taken the lock byte for that slot. A **creator** has no owner to ask, so it finds its own free record the same way and through the same CAS, again after its byte — on the *creator's* byte, which `Open::register_creator` takes rather than scans for ([`0035`](./decisions/0035-the-creators-slot-is-taken-not-found.md)). **There is no third registrant**, and this sentence named one — "a process taking ownership" — until 2026-08-27. A taker-over is already a participant and registers nothing: it keeps the slot, byte and arena record it has ([`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md) question 3, resolved 2026-08-20), because a heir that acquired a *second* slot would arrange for its own live claims to be reaped. `Open` no longer has a path that could try — see [`0037`](./decisions/0037-a-takeover-is-not-a-second-open.md). **“After its byte” holds on every path that *has* a byte, and one public path still does not**: a directly-called `TreeBuilder::build_shared` (`crates/tf_tree/src/tree.rs:504`, registering at `:516`) opens no lock file, so there the CAS is the only ordering there is — which is what §11.3's `attach.after_slot_assigned_before_publish` row is distinguishing. That shape is supported and stays: it is how an arena gets created. **This sentence read “three public ones” until [`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md)'s plan step 0b, and the other two have since stopped registering at all.** `Tree::attach_shared` / `attach_shared_at` (`:2227`, `:2254`) still open no lock file, but their `AttachMode::ReadWrite` arm now returns `ShmError::ReadWriteNeedsRendezvous` before the segment is mapped, and their `ReadOnly` arm writes no participant record — the `is_writable` branch at `:2302` hands a non-writable backing the `u32::MAX` sentinel (`:2313`) instead of registering — so neither has a record whose ordering could be at issue. `TreeBuilder::build` (`:460`) is a heap tree and never had one.

So the CAS is load-bearing rather than avoidable. `fill_slot` (`crates/tf_tree_core/src/participant.rs:154`) opens with `compare_exchange(FREE, RESERVED)`, which wins the slot exclusively; writes the identity fields under `RESERVED`, where no reader may trust them; and release-stores the live word last. **That publication order is what makes A3's indirection sound**, not owner-side population: a claim can only name a slot some process drove to `LIVE`, and the `Release`/`Acquire` pair means whoever sees `LIVE` sees every field written above it. A process killed in between leaves `RESERVED` — distinguishable garbage rather than a plausible-looking record, which is the state §11.3's `attach.after_slot_assigned_before_publish` row is about.

**The deviation is recorded rather than corrected silently**, in the same shape as `Open::register_creator`'s doc comment, which records the analogous deviation from §3.3's "written with `pwrite` before taking the slot lock" for the *lock file's* identity record. The two deviations point opposite ways and both are deliberate. For the lock-file record on a self-chosen slot it is lock-then-write, because write-then-lock loses the race it exists to win and leaves a byte held under the losing process's name. For the arena record it is byte, then CAS, then fields, then publish. The code gives the reason as retry-cleanliness — “nothing was written to the arena, so nothing is left behind, which is the point of taking the byte before touching it” (`crates/tf_tree_ipc/src/open.rs:334`); that it also keeps a record from reading live before its byte is held, which is what §5.1 needs, is an inference this section draws and no comment states. Neither ordering leaves a record a reader can mistake for a live participant.

### 5.1 Identity is advisory; the lock file is authoritative — NORMATIVE

**Liveness comes from the participant's OFD lock byte (§3.3), never from these records.** A `ParticipantRecord` describes *who* a slot belongs to; whether it is live is a kernel fact. Any code deciding liveness from `state` or `heartbeat` is a bug.

**Where that rule is implemented for participant records, once.** `reclamation_verdict` (`crates/tf_tree/src/open.rs:298`, [`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md) plan step 2) is the single predicate every reclamation decision goes through, and it answers from the lock byte and nothing else — no `/proc`, no `heartbeat`. It does read `state`, and the line this section draws is the one that makes that legitimate: the word answers *is there a record here*, never *is its process alive*. **A `FREE` word is very often a live process** — a read-only joiner takes its lock byte in the handshake and then registers no arena record, because the table is in the arena and a `PROT_READ` mapping cannot be written — so the predicate reports such a slot *unknown*, and a revision that reported it reclaimable would be issuing a death verdict about a running consumer, on the shape D18 makes the default. Three properties of it are not stylistic and a reader changing it needs all three. It **skips this process's own slot unconditionally**, because `F_OFD_GETLK` reports only conflicting locks and a description does not see its own. It **observes the `state` word before it probes the byte**: under that order the `Acquire` load of a live word synchronises-with `fill_slot`'s publishing `Release` store, so a byte probe sequenced after it must see the byte held — reversed, or taken from one up-front `held_participants()` mask, a model erases a published record (`0028` open question 6). And it is **sound only because plan steps 0b *and* 0c both landed**: 0b buys *every participant that joined through the rendezvous holds a byte*, 0c buys *the byte at index `slot` is the byte of the record at index `slot`*, and question 6's resolution is explicitly conditional on both. **What 0b does not buy is *every participant*, and this paragraph used to say it did.** It went on: *"It is scoped to a tree carrying a liveness probe, which only `tf_tree::Open` installs; a directly-called `TreeBuilder::build_shared` has no lock file, therefore no probe, and never reaches it."* That is a statement about the **observer** offered as though it were one about the **subject**. The predicate is handed a `ParticipantRecord` out of a shared arena; whether *that* participant holds a byte has nothing to do with whether the caller holds a probe. Measured at `7739805`: a `build_shared` creator whose arena is served through `tf_tree_ipc::OwnerServer` is reported `alive false` by an ordinary facade joiner, and `Tree::reap_participants` frees its record while it is publishing (`a_byteless_creators_record_reads_dead_and_is_reaped_while_it_publishes`, `crates/tf_tree/tests/rendezvous.rs`). **So the predicate is total over the rendezvous population and not over the table**, and which fact a reclaimer may key on for a byte-less record is [`0031`](./decisions/0031-the-participant-record-with-no-byte.md). **A second copy of this predicate is the defect `0028` was opened about, re-created.**

**The ordering rule above binds a *probe*, and A2's topology lock takes an *acquire* — NORMATIVE.** `reclamation_verdict` reads somebody else's byte advisorily, which races every subsequent take, and that race is what makes the word-before-byte order load-bearing. `Tree::reparent` instead takes an exclusive `F_OFD_SETLK` on **byte 1** (§3.3) and holds it for the whole mutation, which *excludes* every subsequent take: what it reads afterwards cannot be invalidated by a taker, because there cannot be one. So its order is byte-then-word and that is not a contradiction of the rule — it is the other side of it. The invariant the two orders buy, stated once here because [`0029`](./decisions/0029-the-topology-lock-is-a-kernel-lock.md) is where it is argued: **the topology word is CASed non-zero only while its process holds byte 1, and byte 1 is released only after the word is; therefore a process holding byte 1 that observes a non-zero word is looking at a holder that is either dead or has no lock file.** The `/proc` triple decides only that second disjunct, and only in the direction that withholds a steal — a rule §0.0's row for this section's "no longer on any correctness-critical path" is read against. Anything that reverses either order, or that reaches for `held_participants()`'s up-front mask on this path, gives that invariant up.

`(pid, start_time, boot_id)` remains the identity triple for diagnostics and for the forced-create path — which is `CreatePolicy::Always` (`crates/tf_tree_ipc/src/open.rs:114`), **not** a `--force-new` flag: no binary in the workspace carries one, and `tf_tree_cli` cannot usefully grow one. §0.0's row for §3.4's `--force-new` escape hatch is the authoritative statement of what shipped, of why the flag did not, and of what a flag would have to arrive with; this sentence is read against it (#189). A bare PID is not an identity: PIDs are recycled, and on an embedded system with a low `pid_max` they recycle fast.

`start_time` is field 22 of `/proc/<pid>/stat`, in clock ticks since boot, and it is still parsed — carefully. **This sentence used to give three reasons for that and two of them are false**, so they are corrected here rather than overwritten. It read: *"It is still parsed — carefully — because `doctor` reports it and the takeover path prints it, but it is no longer on any correctness-critical path."*

**`doctor` does not report it.** `ParticipantInfo` (`crates/tf_tree_cli/src/doctor.rs:361`) carries four fields because four is what the checks read; `start_time` and `incarnation` were both captured there at first, neither was ever read, and both were **deliberately removed** — its doc comment gives the reason, that a captured field nothing reads is a claim nobody maintains. No operator-facing surface in `tf_tree_cli` shows either: `tf_tree top` discards both from `identity`'s tuple and `tf_tree participants` never opens the arena at all. Composing `(pid, start_time)` against `/proc` *there* is refused on purpose — it would be a second liveness spelling in a layer that must have exactly one ([`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md) §6.2).

**No takeover path prints it, and the reason has changed under this sentence — twice, so both are stated.** It first read that the takeover path prints `start_time`; #275 then deleted the arm and the builder that reached it ([`0037`](./decisions/0037-a-takeover-is-not-a-second-open.md)), and this sentence read *"because there is no takeover path"*. **There is one again as of 2026-08-28** — `tf_tree::Tree::inherit_ownership` over `Session::take_over_ownership` (§3.5) — **and it still prints nothing**, which is now a property of the path rather than of its absence: inheritance takes byte 0 on the description the session already holds, so it forms no verdict about anybody else's process and never reaches the `/proc` triple. What #275 left standing is unchanged: `OpenOutcome::TookOver` survives as a public variant of `tf_tree_ipc` with **no producer**, and `tf_tree::open` answers that outcome with `OpenError::TakeoverUnsupported` (`crates/tf_tree/src/open.rs:1276`) after dropping the session, because inheritance is a method on an attached session and not an `open()` outcome. §0.0's ownership-migration row is the authoritative statement of §3.5's status.

**What parses it now** is the `/proc` triple and one wire field, and neither is what the retracted clauses claimed. `read_start_time` (`crates/tf_tree/src/tree.rs`) feeds `alive_given` at both of its call sites — which is the liveness predicate §0.0's row for this section records, not a diagnostic — and `self_start_time` fills `client_start_time` in the attach `Hello` (`crates/tf_tree/src/open.rs:960`, `crates/tf_tree_ipc/src/wire.rs:166`), which the codec encodes and decodes and no reader consumes.

**The third clause is left standing exactly as it was**, and is not amended here. §0.0's row for §5.1's *"no longer on any correctness-critical path"* records where it is false — two places, both `crates/tf_tree/src/tree.rs` (#205) — and says in as many words that no amendment is proposed, because moving those predicates off the triple is a decision record and not a docs edit. This correction is about the two clauses of fact in front of it and touches none of that argument.

**The parsing trap — NORMATIVE.** Field 2 is `comm`, the executable name in parentheses, and it may contain spaces *and parentheses*. Splitting the line on whitespace and taking index 21 is wrong and will silently return a different field for any process whose name contains `) (`. Always locate the **last** `)` in the line and parse fields from there:

```rust
let rp = raw.rfind(')').ok_or(ProcParseError)?;
let field22 = raw[rp + 2..].split_ascii_whitespace().nth(19).ok_or(ProcParseError)?;
```

A demonstration of the naive parse returning the wrong value is in Appendix B. Include that exact case as a unit test against a fixture string — you cannot easily create a process with such a name, but you can test the parser.

---

## 6. Liveness and reaping

### 6.1 Claims are kernel locks — NORMATIVE

A5's parity fix, A3's slot indirection, and A4's epoch check were all built to answer one question: *how do you tell a dead writer from a slow one, without ever getting it wrong?* The rendezvous design answers it outright, so claims move to the same primitive.

**`claim(edge)` takes an exclusive OFD lock on `CLAIM_BASE + edge_id` in the lock file**, held for the life of the `Publisher`. Consequences:

| Previously | Now |
|---|---|
| heartbeat freshness heuristics | none — the lock is the liveness |
| `/proc` liveness checks, PID-reuse defence | none on this path |
| reaping algorithm with epoch ordering | `F_OFD_GETLK` says free ⇒ definitively dead |
| the zombie writer (§A4) | **impossible by construction** |

The zombie case is worth stating plainly, because it was the nastiest hazard in the previous draft. A `SIGSTOP`ped or GC-stalled writer **still holds its kernel lock**, so it cannot be reaped while alive, and another process attempting to claim the edge gets `EdgeAlreadyClaimed` rather than silently becoming a second writer. There is no window, no timeout to tune, and no heuristic that can be wrong. A heuristic that is wrong once in a thousand hours is exactly the kind of bug that ships.

One syscall per claim, on a path that runs at startup. Free.

**Two sources of truth, one authoritative.** The arena's `ClaimRecord` remains for diagnostics and for readers asking who publishes an edge, but **the lock file is authoritative**. Claim = take the lock, then write the record. Reap = if the lock is free and the record says held, clear the record. The record may lag; the lock never does. Any code that makes a decision from `ClaimRecord` alone is a bug.

**A4 is retained but downgraded.** The epoch check in `push` is now defence in depth against a bug in the record path rather than the sole barrier against a zombie. Keep it — one relaxed load — and update its comment so nobody removes it believing it was only there for the zombie case.

### 6.2 Fork is still the exception — NORMATIVE

OFD locks are held by the open file description, which **survives `fork` and is shared with the child**. Parent and child therefore both "hold" every claim, and both would pass A4's epoch check.

`MADV_DONTFORK` (§7.3) is what closes this: the child has no mapping, so it faults immediately and loudly rather than corrupting quietly. `MADV_DONTFORK` and OFD claims are a matched pair — neither is safe without the other, and a comment at each site must say so.

### 6.3 What remains of reaping

Arena-side cleanup after a death, performed by any read-write participant, all steps idempotent:

```
for each edge whose ClaimRecord says held:
    if F_OFD_GETLK(CLAIM_BASE + edge) reports free {      // holder is definitively dead
        claim.epoch.fetch_add(1, AcqRel);                  // fence a buggy Publisher
        normalize_slot_parity(edge, head & mask);          // A5 repair
        claim.owner.compare_exchange(stale, 0, ...);       // racing reapers are harmless
    }
for each participant slot whose record is populated:
    if its lock byte is free { clear the record }
```

The owner runs this on socket `HUP`; others run it lazily when a claim appears held. **Reaping must not be owner-only** — an owner-only design leaks every claim held at the moment the owner died.

### 6.4 Heartbeats are diagnostics only — NORMATIVE

`heartbeat` and `clock_offset_nanos` remain in `ClaimRecord` and are **never** a reaping trigger.

**Neither of them detects the hang case, and this sentence used to say they did.** A live process that has stopped publishing is found from its *stamps*: `TFT009` asks how long ago the newest sample was published, and `TFT008` asks whether the retained window still covers anything. `heartbeat` is a monotone counter, so one reading of it says nothing about elapsed time; `clock_offset_nanos` is a difference and answers nothing about *when*. The claim survived here because nothing read either field, which is the same reason `TFT004` was blind.

**They are written on different schedules, and the difference is a measurement rather than a preference** ([`0036`](./decisions/0036-the-receipt-time-the-format-already-reserved.md)). `heartbeat` is bumped on **every** push, inside `SampleRing::push`, because it is a counter that path already holds in a register ([`0014`](./decisions/0014-the-push-heartbeat-is-a-store.md)). `clock_offset_nanos` needs a **wall-clock** reading — 38.4 ns against a ~5 ns push — so it is **sampled**: `tf_tree`'s `EdgeWriter` records it on a claim's **first** push and then once every `max(nominal_rate_mhz / 1000, 1)` pushes, which is one offset per second of published data at any rate of 1 Hz or more and one per push below that; an edge that declares no rate gets a fixed **1024**, which at 10 Hz is 102 seconds and not one second — a reader of this field must not assume the per-second figure. The `max` is not decoration: a 0.2 Hz `map->odom` corrector declares 200 mHz, and `200 / 1000` is zero. **The first push is part of the rule and not an optimisation** — starting a full interval away would leave a 10 Hz edge reading `0`, indistinguishable from *never sampled*, for its first 102 seconds, and a 0.2 Hz one for 85 minutes. **A claim also clears the field it inherits**, because a claim inherits the edge and not the writer, and a departed publisher's offset is not this one's. **And `0` is written by nothing**: an offset that computes to exactly zero is stored as `1`, because a host whose clock is coarser than a push makes exact zero the *normal* reading for a self-stamping publisher, which would then read as never-sampled forever. The clock is read **in the facade, after the ring write returns**, and never inside `SampleRing::push`: inside, it would widen the seqlock window and convert one writer's diagnostic into every reader's `SlotContended` retries.

**Only a `SystemDomain` (tag 0) edge records anything at all.** `wall clock - stamp` is an offset only where both sides share an epoch, and a `SimDomain` edge stamping nanoseconds since a simulation began would record ~1.79e18. The domain is read at claim time, beside the rate.

**The sampling costs +1.0–1.1 ns per push (~21–23%) at the 1024 default**, measured paired, in one process, by `just push-sampler-cost`. Only 3% of that is the clock **at that interval**; the rest is the per-push counter. The split moves with the rate — at a declared 10 Hz the clock is 78% of a ~4.9 ns overhead — but the cost *per second of publishing* is `38.4 + rate x 1.06` ns and stays under a microsecond at 1 kHz. `docs/PHASE1.md` §11.2 tabulates both ends; `docs/benchmarks/EVIDENCE.md` carries the provenance.

**What it stores is the *offset*, `wall clock - stamp`, and not the wall clock.** A receipt time on its own cannot be paired with a stamp by any reader: sampling means the ring's newest stamp belongs to a later push than the receipt does, so `receipt - newest_stamp` reads anywhere from +3 µs to -900 ms on a 10 Hz publisher whose clock is *exact* — measured — and the sampling interval is ~1 s for every publisher by construction, so it does not cancel in a fleet comparison either. The writer is the only party holding both sides at one instant. `0` means **no sample yet**, and a claim clears the value it inherits.

**This paragraph read *"bumped on every push"* of both until 2026-08-26, and that was half wrong from the day it was written.** `heartbeat` was; the other was written by nothing at all — four `rg` hits, all of them definitions and zero-initialisers — which is the whole reason `TFT004` (clock skew) could detect nothing. It was called `last_push_nanos` then.

**A field that is now actually written is exactly what a later reader will reach for as a staleness trigger, and it is still not one** — the less so since it holds a difference and not a time.

Reaping on staleness would be actively unsafe: an edge legitimately published at 0.2 Hz, such as a map-to-odom correction from a slow global localizer, is indistinguishable from a hung writer under any timeout short enough to be useful. With claims as kernel locks there is no reason to offer such a policy at all, so **do not add one**, not even opt-in.

---

## 7. Mapping policy

### 7.1 Page population is per-edge, not per-arena — NORMATIVE

A minor page fault costs single-digit microseconds. The Phase 1 gate is a **150 ns p50, with a p99.9 that matters more** — one fault in the lookup path blows that budget by two orders of magnitude.

But §3.8 makes the default layout deliberately generous so that zero-configuration startup works, and `MAP_POPULATE` over a 600 MiB address space would fault in — and charge — hundreds of megabytes nobody declared. The two requirements are reconciled by populating at **declaration** granularity:

- `mmap` **without** `MAP_POPULATE`. Untouched regions of a memfd cost nothing (measured, §3.8).
- At the moment an edge is **taken up**, `madvise(MADV_POPULATE_WRITE|READ)` (Linux ≥ 5.14, measured working) over that edge's stamp and pose ranges. On older kernels, fall back to touching one byte per page. **Amended by [`0024`](./decisions/0024-population-is-per-edge-at-take-up.md);** this bullet used to say "at `declare_dynamic`", and [`0004`](./decisions/0004-builder-time-edge-declaration.md) deleted that function when it moved declaration to build time. The two moments that replace it are `Tree::claim` for a writer and plan compilation for a reader, both off the query path by D3. Populating every declared ring at attach instead — which is what the code did while this bullet named a dead function — is *per-arena* population, which the title of this section forbids, and it charged every reader for every edge on the vehicle: measured at **5.2×** on a process using 4 of 64 declared edges.
- On attach, populate the header, frame table, topology blocks, claim table, participant table, edge table and both counter regions — small, always touched, always hot. **Not the two ring arenas:** they are 99.8% of a large arena and are the previous bullet's business.

**`MADV_WILLNEED` does not work here** (measured: zero change in charged pages on a memfd). Do not substitute it.

§12 requires benchmark rows for first-access-after-attach with per-edge population on and off, because it is the clearest demonstration of why this exists and it stops someone removing it to speed up startup.

### 7.2 Huge pages

`madvise(MADV_HUGEPAGE)`, best-effort. A 260 MB arena on 4 KB pages needs ~63 000 TLB entries; on 2 MB pages, 130. With a dozen processes walking it, TLB pressure is real. THP must be `madvise` or `always` in `/sys/kernel/mm/transparent_hugepage/enabled` — `doctor` reports the current setting, and the benchmark reports both configurations rather than assuming.

### 7.3 `MADV_DONTFORK` — NORMATIVE, and easy to forget

A `MAP_SHARED` mapping survives `fork()`. A forked child inherits the mapping *and* the parent's `Publisher` structs, including their claim epochs — so both processes pass the A4 check and both write the same edge. Silent corruption, from an entirely ordinary `fork`.

`madvise(base, len, MADV_DONTFORK)` removes the mapping from the child, so a child touching it faults immediately and loudly instead of corrupting quietly. It must be applied at attach, before any fork can occur.

The child must re-attach to use the tree, which is correct: it is a different process and needs its own participant slot and claims. **Document this prominently**, because Python's `multiprocessing` defaults to `fork` on Linux and Phase 3 users will hit it (§14).

### 7.4 Memory locking

> **AMENDED by [`0049`](./decisions/0049-the-flag-that-prefaults-the-arena.md).
> There is no `LockPolicy` and no `mlock` call of any kind in this library, and
> none is owed. This section is history.**
>
> The reason is `docs/API.md` §8.3's second bullet, which is about *who decides*:
> a library that locks memory on its caller's behalf spends an `RLIMIT_MEMLOCK`
> budget it cannot see, and §3.8's arena is deliberately over-provisioned, so the
> process that knows how much of it a node will touch is the embedding
> application. `TFT016` reports the limit against the arena size, which is one
> term of two — `mlockall` charges the whole address space — and its own message
> now says its silence is not a clearance.
>
> **The reason §8.3 published was partly wrong, and `0049` corrects it rather
> than the outcome.** `MLOCK_ONFAULT` does not prefault (true), but it does not
> *"add nothing over §7.1"*: §7.1 establishes PTEs once and the flag is what
> keeps them, measured with `VM_LOCKED` as the only variable. And whether that
> matters on a swapless host is **undetermined** — directed reclaim tears the
> mapping down, organic memory-cgroup pressure here did not and OOM-killed
> instead, and global `kswapd` pressure is untested.
> `crates/tf_tree_bench/examples/mlock_probe.rs` is the executor, registered in
> `docs/benchmarks/EVIDENCE.md`; this section asserted syscall behaviour and
> reproduced no probe, which is why it survived.
>
> The original text is kept below, because `0049` argues against it and an
> argument needs its opponent on the page.

`LockPolicy::{ None, Populate (default), Locked }`. `Locked` calls `mlock2(MLOCK_ONFAULT)` and requires `RLIMIT_MEMLOCK`; failure is a warning, not an error, and `doctor` reports the current limit against the arena size. Worth it for hard-real-time consumers on a system with any swap or memory pressure.

---

## 8. Read-only attachment

**NORMATIVE:** `AttachMode::ReadOnly` maps `PROT_READ` only and is **the default for any participant that does not declare an intent to publish.**

This is the strongest safety property in the system and it costs nothing: a buggy or crashing perception node *cannot* corrupt the transform tree, enforced by hardware. Lead with it in the documentation — for an industrial integrator it is a more compelling argument than any latency number, because it converts a class of whole-system failures into a single-process fault.

Consequences, which must be enforced by types where possible and by errors where not:

| Operation | ReadOnly |
|---|---|
| `plan`, `at`, `at_many`, `at_adaptive` | permitted, identical code path |
| resolve an existing frame name | permitted |
| **intern a new frame** | `Err(FrameNotDeclared)` — interning writes |
| `claim` / `push` | not expressible: `Publisher` construction requires `ReadWrite` (compile-fail test) |
| reaping | not permitted — reaping writes |
| heartbeat | not written; the socket carries liveness |

`FrameNotDeclared` must explain itself: a read-only participant asking for a frame nobody has declared is usually a startup-ordering problem, and the message should say "no publisher has declared this frame yet" rather than "unknown frame".

---

## 9. `tf_treed`

> **SUPERSEDED by [`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md).
> There is no `tf_treed` binary. The capability is `tf_tree serve`, a subcommand
> of the binary that already ships — and it is an escalation, not a prerequisite.**
>
> Two things dissolved this section. First, **most of its responsibilities were
> already discharged elsewhere**: D16 said ownership is *configured, not
> negotiated*, and this daemon existed to make configuring it trivial —
> [`0005`](./decisions/0005-the-shared-memory-seam.md) §8 then retired the "no
> takeover" half, so ownership is a role the kernel reassigns on an uncontended
> `F_OFD_SETLK` and `OpenOutcome::TookOver` ships. Liveness, reaping and owner
> death need no daemon.
>
> > **Corrected three times, and every correction is kept** (#275, the
> > 2026-08-28 §3.5 commit, and #278 —
> > [`0037`](./decisions/0037-a-takeover-is-not-a-second-open.md)). The first
> > clause has always stood: ownership *is* lock-file byte 0 and the kernel
> > releases it when the owner dies. The second — *"`OpenOutcome::TookOver`
> > ships"* — was true of the shape `0005` §8 retired the "no takeover" half
> > for, and it has been false since; **what moved is why.** The first
> > correction read that the arm and its builder were deleted, so `TookOver` was
> > "a public variant of `tf_tree_ipc` with no producer" that `tf_tree::open`
> > refused with `OpenError::TakeoverUnsupported`. **That is now false too: the
> > variant does not exist.** `OpenOutcome` is `Joined | Created`
> > (`crates/tf_tree_ipc/src/open.rs:139`), `OpenError::TakeoverUnsupported` is
> > deleted with it, and `grep -rn 'TookOver\|TakeoverUnsupported' crates/`
> > returns only comments about the removal. The `crates/tf_tree/src/open.rs:1276`
> > this paragraph used to cite as the refusal site is a
> > `ParticipantSlotDiverged` check, so the old citation now lands on an
> > unrelated refusal — which is the reason a line number is quoted here with the
> > grep that regenerates it.
> > Inheritance shipped somewhere else: it is a method on the `Session` an heir
> > already holds, where the invariant is structural rather than checked, which
> > is exactly what a second `open()` could not verify. **§0.0's
> > ownership-migration row is the authoritative statement.**
> >
> > **What this costs §9's argument, stated rather than assumed — and the cost
> > was paid back.** Owner death never stopped the arena: its existing
> > participants keep reading, which is what §3.5 promises and what `shm_torture`
> > measures. Between 2026-08-27 and 2026-08-28 *"owner death needs no daemon"*
> > did not follow in the full sense this paragraph uses it, because no survivor
> > could inherit the role and **no new process could join that arena for as long
> > as any survivor lived** (§0.0: it wins the ownership byte, meets §3.4's
> > split-brain check against the survivors' held bytes, backs off, and times out
> > with `ArenaHeldButUnreachable`). §3.5 now closes that, and closes it **still
> > without a daemon**: a surviving read-write participant calls `owner_lost()`
> > and `inherit_ownership()` in its own loop, which is
> > [`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md)'s
> > position rather than a departure from it. The residual cost is honest and
> > small: the call is the integrator's to make, so a fleet that never makes it
> > is back in the paragraph above. That is an argument for adoption, not for
> > reinstating this section — the second paragraph below, pre-declaration
> > answered by `0019` §2, is what actually dissolved it, and D16's *configured,
> > not negotiated* is unaffected.
>
> Second, **the part that did remain is not a lifecycle problem.** What this
> section was really for is the last paragraph below: pre-declaration, so a
> consumer can attach and plan before any publisher runs. `0019` §2 fixes that
> without a daemon and without a config file — a read-only attach implies
> `CreatePolicy::Never` (`Open::new()` *defaulted* to read-only **and**
> create-if-absent, a configuration no correct program wants — it failed
> `NoLayoutToCreate` rather than creating an empty arena, so what §2a removed
> was a latent class; the default is now `Never`), a consumer waits for the
> arena with
> `Open::await_open` and for topology with `Tree::await_frames`, and
> `frame_headroom`/`edge_headroom` cover frames that arrive later.
>
> What survives, as `tf_tree serve --config <topology.toml>`: create and seal
> from the config, pre-declare, hold the arena open, export metrics, drain on
> `SIGTERM` leaving the segment alive. What is retired: `--lock` and
> `--socket-mode`, both of which the rendezvous owns and neither of which was ever
> daemon-specific. [`0009`](./decisions/0009-descoping-phase-6.md)'s amendment
> below travels with it — URDF is still owed by no phase.
>
> The rest of this section is kept as written, because `0019`'s §*Rationale*
> argues against it and an argument needs its opponent on the page.

The reference owner. Target ~400 lines. Deliberately boring: it holds no application logic, so it is the process least likely to crash.

```
tf_treed --domain <n> --name <n> --config <file.toml> [--participants 64]
         [--lock] [--socket-mode 0600] [--metrics-port <p>]
```

> **Amended by [`0009`](./decisions/0009-descoping-phase-6.md):** `--config` took
> `<file.toml|urdf>` here. URDF parsing is no longer owed by any phase — it
> leaves the engine and becomes an optional converter that emits the topology
> config Phase 4 already ships, built on the existing `urdf-rs` crate. So this
> daemon takes the config format and nothing else; a user with a URDF converts
> it first. Costs no code, since §9 is unimplemented, but it does retract a
> promise rather than leaving it to be discovered.

Responsibilities: create and seal the segment; declare frames and static edges from the config so the tree exists before any node starts; serve the attach socket; `epoll` participant sockets and reap on `HUP`; serve `doctor`/`top` queries; export Prometheus metrics; on `SIGTERM`, drain and exit, leaving the segment alive for existing participants.

It must **not** publish, claim any edge, or interpret transforms. It is a lifecycle daemon.

The config-driven pre-declaration is what makes startup ordering deterministic: with the tree's static structure declared up front, read-only consumers can attach and plan before any publisher exists.

---

## 10. Recording and replay — the correctness harness

> **§10(a) *Record* and §10(b) *Replay* are DECLINED by
> [`0047`](./decisions/0047-the-recording-this-reader-would-refuse.md). There is
> no `tf_tree_record` crate, no `tf_tree record` subcommand and no
> `tf_tree replay` subcommand, and none is owed. §10(c), the NORMATIVE test, is
> met — `crates/tf_tree_cli/tests/replay_bit_identity.rs`, §15's box for it.**
>
> Two arguments, and `0047` carries both in full. The recorder's own artifact is
> one nothing here can read: §10 names the channels `tf_tree/topology` and
> `tf_tree/samples`, and `crates/tf_tree_ingest/src/source.rs` accepts a channel
> only when its schema is `tf2_msgs/msg/TFMessage` or `tf2_msgs/TFMessage` —
> §3.3's own rule — so the output would refuse to ingest and closing that means a
> second MCAP reading path beside the one that ships. And this phase's own
> Definition of Done (§15) asks for the NORMATIVE test and for no tooling
> around it.
>
> **The read half of (b) shipped one phase later**, under
> [`0006`](./decisions/0006-the-eight-phase-roadmap.md)'s re-cut: the new Phase 5
> is *"bag ingestion"* and `tf_tree_ingest` is it. §10 and `0006`'s Phase 5 both
> claimed record/replay; `0047` is where that was settled.
>
> **What §10 promised and nothing delivered is named rather than quietly
> dropped**: the regression corpus for subsequent phases, and *"§12 real robot
> data instead of synthetic input"*. Read `docs/PHASE5.md` §0.0's §3 row before
> assuming a substitute exists — *"Nothing in this repository is a rosbag2
> bag"*.
>
> The rest of this section is kept as written, because `0047` argues against it
> and an argument needs its opponent on the page.

`tf_tree_record` lands **early in this phase, not at the end**, because it is how the shared-memory layer gets validated.

- **Record.** A read-only participant tapping every edge, writing MCAP with two channels: `tf_tree/topology` (declaration and mutation events) and `tf_tree/samples` (`edge_id`, `stamp`, `pose`). Recording is itself read-only, so it can be attached to a production system without risk.
- **Replay.** Reconstructs an arena from a recording and re-publishes deterministically.
- **The test that matters — NORMATIVE.** Replay one recording into a `HeapArena` and a `MappedArena`, run an identical query set against both, and assert **bit-identical `f64` results**, not approximate equality. Lookups are pure functions of `(plan, stamp, buffer contents)`, so any difference at all means the shared-memory path is not the same code, which is the central claim of this phase.

This also gives every subsequent phase a regression corpus, and gives §12 real robot data instead of synthetic input.

---

## 11. Test plan

### 11.1 What Miri and loom can and cannot do

**Neither tool crosses a process boundary.** `MappedArena` cannot be tested by either. The mitigation is architectural: the protocols are identical for both arena types, so run every existing Phase 1 loom test unchanged against a `HeapArena` with the A1–A5 amendments applied, and cover the multi-process dimension by fault injection instead. Add loom cases for:

- Two threads racing `try_reap` on the same claim: at most one `Reaped::Yes`, and the epoch is bumped at least once.
- Reap concurrent with `push` from the reaped `Publisher`: the push returns `ClaimRevoked` or completes before the epoch bump; it never lands after a new claimer's first push.
- Topology mutation concurrent with plan compilation across four blocks: readers see one consistent block or return `TopologyChurn`.
- Claim, reap, re-claim, zombie push: the zombie always fails.

### 11.2 Multi-process integration harness

`tf_tree_test_harness` spawns real child processes (not threads), coordinates via pipes, and asserts on arena state. Required scenarios:

1. 1 owner, 1 writer, 14 read-only readers. Sustained 1 kHz for 60 s. Zero errors, zero divergence between readers.
2. Attach/detach churn: 32 processes attaching and detaching randomly for 60 s while a writer publishes. Participant slots must not leak.
   - **2b. Slot recycling under abnormal exit.** Attach and `SIGKILL` a read-write participant 128 times against a 64-slot arena, one at a time. Every attach must succeed. This is the direct falsifier for #184 — 63 of 64 slots holding `LIVE` records for dead pids, every subsequent attach refused `NoParticipantSlots` — and it is `slot_recycling_under_abnormal_exit` in `crates/tf_tree/tests/rendezvous.rs`. **Two independent mechanisms satisfy it and the scenario does not distinguish them**, which is worth stating because [`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md)'s plan expected it to fail at `HEAD` and it does not: the owner's hangup callback frees the record on `EPOLLHUP` (#191) and, since that record's plan step 3, the assigner reclaims from the lock byte before granting. Disabling either alone leaves this scenario green; disabling the callback alone fails it at the **64th** attach, not the 65th, because the owner holds slot 0 and only 63 are ever available to joiners. What pins the assigner on its own is `the_assigner_reclaims_a_stale_record_no_hangup_will_ever_clear`, which stages the state no hangup can reach — a record that is not `FREE`, a free byte, and a slot this owner never granted. Neither of those pins the **`RESERVED`** half of the same change, and that is the half with no prior mechanism at all: `the_assigner_collects_a_record_left_reserved_by_a_killed_registrant` and `the_hangup_collects_a_record_left_reserved_by_a_killed_registrant` are one test per collector, both staged (§11.3's row says why the crash point itself is unreachable), and both fail if either caller is narrowed back to `LIVE`.
3. Owner dies mid-run: existing participants continue for 60 s; new attach fails cleanly; reaping still functions.
4. `FORMAT_VERSION` / `layout_hash` mismatch: attach is rejected with the correct status and a message naming both values.
5. Read-only participant attempts every write operation: all fail, arena bytes unchanged (verify by hashing the arena before and after).
6. 64 participants, then a 65th: `NoParticipantSlots`, and the message says how to raise the limit.
7. **Thundering herd:** 32 processes call `open()` simultaneously with no arena present. Exactly one creates; 31 join; all 32 see the same `instance_uuid`.
8. **Ownership migration:** kill the owner mid-run. A surviving participant takes over; a new process joins the *same* arena (identical `instance_uuid`); and a reader thread running throughout observes zero failed lookups and no latency excursion beyond its steady-state p99.9.
9. **Split-brain attempt:** kill the owner, and immediately — before any participant can notice the `HUP` — start a fresh process. It must block on §3.4 step 4 and then join the existing arena. **Two distinct `instance_uuid`s on one `(runtime_dir, domain, name)` is a hard test failure.** Run this one a thousand times in a loop; it is the single most important race in the phase.
10. **Stuck participant:** `SIGSTOP` the only participant, then `open()` from a fresh process. Must fail with `ArenaHeldButUnreachable` naming the stuck slot — never create a second arena. `SIGCONT`, then confirm a subsequent `open()` succeeds.
11. **Domain isolation:** two arenas under different domains, and two under different runtime dirs, never observe each other.

### 11.3 Fault injection — the core of this phase

**NORMATIVE.** A build-time `crash-points` feature places named, deterministic abort sites in every mutation protocol:

```rust
#[cfg(feature = "crash-points")]
macro_rules! crash_point { ($name:literal) => { $crate::crash::maybe_abort($name) }; }
```

Armed by `TF_TREE_CRASH_AT=<name>:<nth_hit>`, which `abort()`s (not `panic!` — a panic unwinds and runs `Drop`, which would clean up and defeat the test). Required sites, one test per site:

| Crash point | The state it leaves behind must be repairable |
|---|---|
| `push.after_seq_odd` | slot odd, `head` unbumped → A5 self-heals on next claim |
| `push.after_data_before_seq_even` | as above; sample invisible because `head` never moved |
| `push.after_seq_even_before_head` | sample fully written but unpublished → invisible, then overwritten |
| `topo.after_copy_before_publish` | inactive block dirty, word unchanged → **no observable effect** (A1) |
| `topo.holding_lock` | **PLACED and executed (2026-08-29)** — the site is in `Tree::reparent`, after A2's word is CASed and *before* `set_parent`, and `a_killed_topology_holder_leaves_a_word_the_next_acquirer_steals` runs it across a real process boundary. Placed before the mutation deliberately: after it, the surviving state is indistinguishable from a completed reparent whose guard had not dropped, and the test would pass on a build where the byte was never taken. **byte released by the kernel; word left stale and overwritten by the next acquirer** ([`0029`](./decisions/0029-the-topology-lock-is-a-kernel-lock.md)). A2 is two locks: `Tree::reparent` takes the lock file's topology byte (§3.3) and only then CASes the in-arena word, and releases them in the other order. A holder killed anywhere inside that has its byte released by the kernel with no cooperation, so the next acquirer takes it, finds the word still naming the corpse, spins out its budget and steals — and stealing needs no rollback, which is A1's payoff and unchanged. **The liveness check survives and its scope is now stated rather than assumed.** Holding the byte means a non-zero word belongs to a holder that is either dead or **has no lock file**, so the `/proc` predicate decides only the second, and it may only ever *withhold* a steal — a false "alive" refuses a mutation, a false "dead" would put two live processes in the A2 critical section, which is what `topo.after_copy_before_publish`'s "no observable effect" bound does **not** cover (that bound is argued for a holder that stopped executing). Three holder classes, and which one is stealable is now decided before any inference: a **live** holder with a lock file is refused by the byte, whatever `/proc` says about it — the #213 case, where a PID-namespace collision or a non-dumpable process under `hidepid` used to authorise the steal; a **dead** holder with a lock file is stealable, because the kernel released the byte and the triple agrees; a holder with **no lock file** — a directly-called `TreeBuilder::build_shared`, and the class [`0031`](./decisions/0031-the-participant-record-with-no-byte.md) is about — is decided by the triple alone in both directions, exactly as before, because there is no byte for anyone to have taken. A fourth class is the fork inheritor (§6.2, [`0030`](./decisions/0030-the-atfork-handler-and-inherited-descriptors.md)): a child holding the inherited description holds the byte, so a dead parent's topology lock is **not** stealable while that child lives. That is an availability failure and not corruption, and `TFT014` reports the inheritance. **It is a much narrower exposure than the claim byte's and must not be quoted as the same one**: a claim byte is held for a `Publisher`'s whole life, so any `fork` in a publishing process inherits a held one, while this byte is held for two `fcntl`s and one block copy — inheriting a *held* topology byte needs a `fork` from another thread inside that window **and** a death before it closes. **P5's class needs no separate row and used to look as though it did**: a holder whose slot the owner already released on `HUP` reads dead through `identity() -> None`, which is the same verdict the byte reaches by a different route — a coincidence, and it stops mattering here because the byte is consulted first and the participant table is not consulted at all on a lock-file tree |
| `claim.after_cas` | claim held by a dead participant → reapable via slot indirection (A3) |
| `intern.after_hash_cas_before_id_store` | hash slot claimed, id unpublished → next interner spins then... **see below** |
| `attach.after_slot_assigned_before_publish` | **PLACED and executed (2026-08-29)** — in `participant::fill_slot`, between the `FREE -> RESERVED` CAS and the `live_word` store, driven by `attach_after_slot_assigned_before_publish_aborts_at_the_named_point`. **That closes the gap this cell states below**: the window is ~12 ns, so nothing outside fault injection could kill a process inside it, and §11.2's two tests therefore *staged* the word instead. They still do, and they are still the *repair*; what changes is that the state they collect is now produced by a real death rather than arranged. participant slot in `ATTACHING` (`RESERVED` in the code), with **two possible byte histories** — on the rendezvous path the byte was **taken first and then released by the kernel** at death, because the joiner CASes its record only after holding it (§5); on the one byte-less path left — a directly-called `TreeBuilder::build_shared`, `Tree::attach_shared` / `attach_shared_at` having left this list at `0028`'s plan step 0b, where their `ReadWrite` arm began refusing and their `ReadOnly` arm registers no record, so neither reaches this crash point at all — no lock file is opened and the byte was **never taken** → record cleared by any reaper. **On the rendezvous path that is now performed** rather than promised: `ParticipantTable::reclaim` accepts any observed word, `RESERVED` included ([`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md) plan step 1, conditional on steps 0b and 0c), and both of its callers act on one — the owner's hangup callback (step 4) and its slot assigner (step 3). **The crash point is still unreachable and the state it leaves is not**: with no fault injection nothing can kill a process inside that window (~12 ns, measured in [`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md) open question 4), so §11.2's two `..._collects_a_record_left_reserved_by_a_killed_registrant` tests *stage* the word — `register_at`, which is `fill_slot`, then the publishing store rewound — and assert one collector apiece against it. That is coverage of the recovery, not of the crash. On the byte-less `build_shared` path it is still unmet by any code: that tree opens no lock file, so it carries no liveness probe and no reclaimer ever runs against it |
| `hangup.after_probe_before_cas` | **PLACED (2026-08-29)** — in the owner's hangup callback, between the `state` load and `reclaim`. **Armed by `a_killed_owner_in_its_hangup_callback_leaves_the_role_inheritable` since 2026-08-29** — reaching it needs a *joiner* to hang up while **this owner** is armed, and every other crash test arms a joiner, which is why this was one of the two sites that shipped without one. `Kid::spawn_with_env` on the `own` arm is all it needed. record not `FREE`, byte free, and the owner killed between the callback's `state` load and `reclaim`'s CAS → one CAS, so there is no torn intermediate: the reclamation either happened or it did not, and what is lost is the reclamation and not consistency. It is *repairable* only because two other collectors form the same verdict later — the assigner at the next grant, and any surviving participant's `Tree::reap_participants` sweep (plan step 5) — and under the hangup callback alone the honest entry here would be "not repairable" |
| `reclaim.after_probe_before_cas` | **PLACED (2026-08-29)** — in `Tree::reap_participants`, between `reclamation_verdict` and the CAS. The *general* sweeper's version; the hangup callback has its own row because what makes that one repairable is different — there, two other collectors form the same verdict later, and this sweep is one of them. **Armed by `a_killed_sweeper_leaves_the_record_for_the_next_one` since 2026-08-29**, which needed two things that did not exist: a sweeper that is **not the test binary** (every `reap_participants` call in `tests/rendezvous.rs` runs in-process, where arming the site aborts the runner) — hence `rendezvous_child`'s `join-sweep` arm — and a fixture that kills the **owner**, because a joiner's death fires the hangup callback that collects its record before any sweep can reach it. nothing published → idempotent. Racing reclaimers are harmless: at most one `compare_exchange` succeeds |
| `reclaim.probe_then_reoccupied` | **This row is not an abort site, and saying so is the finding rather than a shortfall (2026-08-29).** Every other row in this table names an *instruction a process can die at* and a state its death leaves; §11.3's mechanism is `abort()`, armed by `TF_TREE_CRASH_AT`, and its whole value is that a test can name the instruction instead of hoping a `SIGKILL` lands there. This row names something else: **an interleaving between two processes that are both alive** — a reclaimer holding a verdict formed before the slot was freed, re-granted and re-occupied. Killing either participant does not produce it; it needs both to keep running, in a particular order. So `crash_point!` is the wrong tool and placing one here would produce a test that passes without ever reaching the state. What the row's analysis is *about* — the `RESERVED` word carrying no incarnation, so the CAS can succeed against a new occupancy, bounded by the byte rather than the word — stands unchanged and is argued in the cell below; what is retracted is only its membership in a table of crash points. The mechanisms that can reach a two-live-process interleaving are `loom` (which models it) and `shm_torture` §11.4 (which may stumble on it), and both are named in `ParticipantTable::reclaim`'s doc comment alongside the precondition. **The original analysis, unchanged:** a reclaimer holding a verdict formed before the slot was freed, re-granted and re-occupied → for `live_word(inc)` the new occupancy carries a different incarnation and the CAS fails. For `RESERVED`, which is the bare constant and carries no incarnation, the CAS **can** succeed against the new occupancy, and what bounds that is the byte rather than the word: with plan steps 0b and 0c the record it erases belongs to a joiner that is holding the matching lock byte and is about to publish `live_word` over it, so the outcome is a **spurious free and never a second occupant**. `ParticipantTable::reclaim`'s doc comment carries the precondition and what to narrow it back to if either half stops holding. Through plan step 4 the interleaving was additionally unreachable, for a reason step 5 then spent: reaching a second `RESERVED` needs the word to pass through `FREE` first, `reclaim` is the only operation that can drive a `RESERVED` word there — `release` guards on `live_word(inc)` — and its callers were the two closures of one `epoll` loop on one thread. `Tree::reap_participants` is the third caller and runs in another process, so that argument is gone and the byte is the whole of the bound |
| `open.after_ownership_lock_before_bind` | **PLACED and executed (2026-08-29)**, with `open.after_create_before_bind`, by `a_creator_killed_before_or_after_the_arena_exists_leaves_nothing_behind`. ownership lock released by the kernel → the next `open()` proceeds; **no arena created twice** |
| `open.after_create_before_bind` | **PLACED and executed (2026-08-29)** — and its last clause is why the placement sits before `use_ofd_liveness`: the tree this abort abandons holds only the segment, which is what "no participant byte held" asserts. arena exists, nothing serving, no participant byte held → next `open()` finds nothing alive and creates fresh; the orphan memfd is freed with its last mapping |
| `takeover.after_ownership_lock_before_bind` | ownership released; another participant takes over; joiners retry |

**`intern.after_hash_cas_before_id_store` needs a fix that Phase 1 does not have.** Phase 1's interning spins waiting for `ids[i] != U32_MAX`; if the process that won the hash CAS dies before publishing the id, every future interner of that name spins forever. Add a bounded spin plus recovery: after `INTERN_SPIN_LIMIT`, verify the participant that claimed it — record `claiming_slot` alongside the hash — and if dead, take over the entry. This is **amendment A8** in §1; cover it with a loom test.

### 11.4 `shm_torture`

Nightly CI, 30 minutes: N processes, random attach/detach/claim/reap/push/lookup, random `SIGKILL` at 1–10 Hz, a random crash point armed in 10% of children. Invariants checked continuously: no reader ever observes a non-unit quaternion or a NaN; no two writers ever hold one edge; participant and claim slots never leak; the arena hash is stable across quiescent points.

Run it under ASan (works across processes) and with `TF_TREE_PARANOID=1`, a debug mode that validates quaternion normalization and stamp monotonicity on every read.

> **Amendment — the crash-point clause runs nightly since 2026-09-04, and until
> then it ran in no automated job at all.**
>
> The `SIGKILL` half has been `nightly.yml`'s `torture` job since that file
> existed. The crash-point half — *"a random crash point armed in 10% of
> children"* — is a different build (`--features shm,crash-points`, because a
> site compiled out of the driver is compiled out of every child) and different
> parameters (5 minutes, 10 children, 2 Hz: longer and gentler, since a high
> kill rate wins the race against the site more often than not). `just
> shm-torture-crash-points` had existed for a while and `rg -n 'crash-points'
> .github/` returned **nothing**, so this sentence of §11.4 was held by a
> hand-measured number in a justfile comment — a measurement with a date on it,
> not a gate. `nightly.yml`'s `crash-points` job is now that gate, as its own
> job rather than a row on `torture`, for the reason the `gate4` job gives about
> itself: a five-minute run must not go unrun on a night the thirty-minute one
> fails early.
>
> **Wiring it required giving the run a verdict, and that is the part worth
> recording.** The binary prints `§11.3: N child(ren) armed …, M aborted at
> one`, and those two numbers differ on purpose — an armed child the driver's
> `SIGKILL` reached first never got to its site. The recipe's own comment said
> *"read the `§11.3:` line, not the exit status"*, which is correct advice to a
> human and impossible for a job whose only step is the recipe. So the binary
> now **refuses** a `--crash-points` run with `armed 0` (nothing was armed:
> check the build) and, separately, one with `armed N, aborted 0` (nothing
> fired: raise `--duration` or lower `--kill-hz`). Without those two refusals
> the new job would have been the vacuous green that this harness's own
> `a_run_that_validates_nothing_fails_instead_of_passing` exists to prevent one
> level down — a run that armed nothing prints `0 violation(s)` exactly as a
> healthy one does. Both bounds are "at least one" rather than a tuned
> threshold, and both were red-tested by seeding the defect they name.

---

## 12. Benchmarks and the gate

### 12.1 Fixture

The Phase 1 24-frame robot tree, plus: 1 writer process (4 dynamic edges as in Phase 1), 1–16 read-only consumer processes each running 4 reader threads, cores pinned, `isolcpus` if available. Compare against ROS 2 `tf2` with an equivalent tree over the default DDS, same rates.

### 12.2 Required measurements

| Benchmark | Report | Measured |
|---|---|---|
| depth-3 cross-process lookup, warm | p50, p99, p99.9 vs the Phase 1 in-process baseline | — |
| first access after attach, per-edge population on vs off | p99.9, both | **Half done — `just attach-bench`.** With population **on** (the shipped path): the first lookup after attach is **130–170 ns p50** over 25 runs (sixteen in the sitting that rewrote this row, nine in the review before it), indistinguishable from a steady-state lookup — which is the guarantee §7.1 exists to buy, now demonstrated rather than asserted. **Its tail is one sample, and is quoted as one:** nearest-rank p99.9 over 201 cycles *is* the maximum (`pct`, in the binary), so what moves between runs is one draw from the scheduler's tail and not a distribution — 1.2–4.7 us across the sixteen runs of this sitting, ~1.3 us in the sitting that first filled this row, and the cold cycle to the nanosecond in fifteen of those sixteen but not in the sixteenth. The **off** arm is absent and deliberately so: `populate_hot()` is unconditional inside `attach_shared_inner`, and manufacturing an "off" arm out of a different code path would measure something else. It arrives with `docs/decisions/0022`'s B2-prime, the change that gives the attach path a policy. |
| THP `madvise` vs `never` | p50, p99.9, both | — |
| aggregate read throughput, 1→16 consumer processes | scaling curve | **Done — `just shm-scaling`; the curve is in [`docs/benchmarks/tf2.md`](./benchmarks/tf2.md).** §0.0 has recorded it **Done** since it was measured, which is why this cell is not a dash. 1/2/4/8 reader processes on one arena: **4.66 → 9.04 → 15.43 → 18.17 M lookups/s** aggregate (1.00x → 1.94x → 3.31x → 3.90x) at 213 → 219 → 257 → 431 ns a lookup, with unique resident 3.5 → 18.7 MiB (Pss, one arena) against the N × 1.4 MiB a private tf2 buffer per process would cost. **The bend is cores, not contention**: this host has 4 physical cores, 4 × 213 ns is an 18.8 M/s roofline, the 8-process row measures 18.2, and its per-lookup cost doubles exactly as 2:1 oversubscription predicts (426 ns predicted, 431 measured). The curve stops at 8 for the same reason — 16 processes on 4 cores measures the scheduler, and the roofline argument is already made. |
| **CPU per consumer at 1 kHz × 20 edges, vs ROS 2 `/tf`** | %CPU per consumer, both | — |
| **total RSS across 16 consumers, vs ROS 2 `/tf`** | MB, both | — |
| publish → visible-to-consumer latency, vs ROS 2 `/tf` | p50, p99.9, both | — |
| `SIGKILL` writer → claim reapable → re-claimed | p50, p99 | — |
| attach time, cold and warm | p50 | **Done — `just attach-bench`, which does not run §3.7's rendezvous.** The binary calls `Tree::attach_shared(dup, AttachMode::ReadOnly)` on a duplicated memfd: no `connect`, no version handshake, no `SCM_RIGHTS`, no assign closure. What it times is map, validate, take a participant slot, `populate_hot` — and on a read-only mapping that advice is `MADV_POPULATE_READ`, not `POPULATE_WRITE`: `MappedArena::populate` follows the mapping's protection, because `POPULATE_WRITE` on `PROT_READ` is `EINVAL`. On the §11.1 fixture, 201 cycles a run: attach **12.3–14.2 us p50**, and the first plan compile — which is where the ring population went — **66.3–92.3 us p50**. Those are **observed extremes over 28 runs on this host, rounded outward** (sixteen in the sitting that rewrote this row, nine in the review that falsified its first draft, three in [`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md)), at load averages of 4 to 7 wherever the run recorded one — a record of what was seen, **not a bound**. The width is the host: `0028` watched three unpaired repeats of its join arm drift 132 → 138 → 180 us "as the other agents' load rose", and the first draft of this row published ranges over five and eight runs, of which nine fresh runs fell outside eight of ten. **Only the p50s are given as ranges, and that is the point:** `cold` is cycle 0 and nearest-rank p99.9 over 201 cycles *is* the maximum, so each of those is a single sample whose run-to-run movement is one draw from the scheduler's tail — attach's cold cycle has been seen at 16–25 us and its maximum at 19–127 us, the first compile's at 74–113 us and 107–171 us, and a rerun landing outside those intervals says nothing about the code. **This row read `97.5 us p50` until 2026-08-19, and that is a split rather than a regression**: it was measured on `1e18234` (2026-08-14), and [`0024`](./decisions/0024-population-is-per-edge-at-take-up.md) moved ring population off attach and onto edge take-up on `0f17fb8` (2026-08-16). The two halves still sum to **79.3–106.4 us p50** — per run, paired — on this fixture, whose plan walks essentially every edge, and the pre-`0024` pair summed to 100.3 us (99 791 + 550 ns, `0024`'s own before column), so the total did not move. That before column measuring attach at 99.8 us where this row said 97.5 is the same run-to-run width, two days apart. The arena is 1 401 472 B — 343 pages at 4 KiB, printed by the recipe so the division is reproducible — and that sum over 343 is ~230–310 ns/page, which is arithmetic over the whole arena and not a per-page measurement: attach populates the tables, the first plan compile populates the rings its plan reaches. **Attach alone is not that number and neither is a real join**: `0028` measured the whole §3.7 path — `Open::open()` against a live owner, assign closure included — at ~133 us p50, out of tree, so nothing in this repository reproduces it. "Cold" is the first cycle — fresh VMA and page tables — and **not** a cold page cache, which needs root to arrange. |
| `open()` when the arena exists vs when creating | p50, both | — |
| owner kill → new owner serving | p50, p99 | **Done — `just owner-migration`.** Measured from the outside, because that is where the question is meaningful: the driver stamps its `SIGKILL` and then retries `Open::new().create(Never)` until one succeeds, so what is timed is *a fresh process being able to join again* — the thing an ownerless arena refuses. **0.6–1.2 ms p50, 1.1–2.0 ms p99** over five runs of five migrations on this host, each round killing whichever process was serving. Ranges, not a figure: this is a scheduler-sensitive measurement on a shared host, and unlike the row below it is a direct timing rather than a quotient, so it is the more quotable of the two |
| lookup latency across an ownership migration | p99.9 during vs steady-state | **Measured — `just owner-migration` — and the quotient is only weakly evaluable. Read this before quoting it.** At the default 5 migrations it reads **0.976, 1.000, 1.025, 1.025, 1.093** over five runs: one of the five is past gate 4b's 1.05. At `--repeat 15` it reads **1.000 three times running**. Those two facts are the same fact — **the quotient's sensitivity falls as its sample count rises**, because any window wide enough to contain the migration is dominated by ordinary steady-state samples, so adding migrations drives it to 1.000 while small counts leave it swinging on tail noise of the same order as the 5% budget. It cannot be made both stable and sensitive by tuning the window or the repeat count, and picking the repeat count that passes would be choosing the vacuous end on purpose. **Re-cutting the criterion is a decision record, not a benchmark edit** — `0023` did exactly that for `PHASE4` §7 gate 1, which is the same shape of finding. **What is load-bearing on this host, and did not move:** *zero failed lookups* in every run, and the per-phase stall count (lookups past 10× the steady p99.9) at **510–542 per million in the steady phase against 517–531 during** — like for like. The readers are read-only and make **no control-plane call at all**, which is what makes any of this an answer to 4b rather than a measurement of the poll |

**The third column is where a result goes, and a dash is not a status.** Three
of these rows carry a measurement. Two of them carried it as a third cell in a
two-column table until 2026-08-19 — which GFM discards silently, so on
github.com the figures in them rendered as nothing at all, and one of the two
was stale for five days behind that, because nobody proofreads what they cannot
see (#208). The third had never been in this table at all: §0.0 records
multi-process read scaling **Done**, and a dash here against a **Done** there
would be this table contradicting the §0.0 that outranks it. A dash means *this
table* holds no figure for that row, not that none exists: more are measured in
[`docs/benchmarks/tf2.md`](./benchmarks/tf2.md) (`just mp-bench`,
`just tf2-bench`), and that register rather than this list is where the `tf2`
comparison lives. `just artifact-versions` now fails on a row whose cell count
disagrees with its header, so the next one is caught before it is invisible.

### 12.3 The gate — NORMATIVE

Proceed to Phase 3 if:

1. **Cross-process depth-3 p50 within 10% of the in-process baseline, p99.9 within 25%.** This is the central claim of the phase: the same code, the same speed, in another process. If it fails, the mapping policy is wrong (§7), not the design.
2. **Aggregate read throughput scales ≥ 12× from 1 to 16 consumer processes.**
3. **Zero corrupt reads across the full `shm_torture` run**, and every §11.3 crash point recovers. Not negotiable; a single failure here means the arena is not crash-consistent and the phase is not done.
4. **Kill → re-claimable p99 under 10 ms.**
4b. **Ownership migration is invisible to the data plane:** lookup p99.9 during a migration within 5% of steady state, and zero failed lookups. **Given an artifact on 2026-08-29 — `just owner-migration` — and the honest verdict is *partly met, on a criterion that is only weakly evaluable*.** *Zero failed lookups* holds in every run. The p99.9 quotient reads 0.976–1.093 at the default 5 migrations — **one run of five past the 1.05 bound** — and exactly 1.000 at `--repeat 15`, because its sensitivity falls as its sample count rises (§12.2's row carries the argument). Choosing the repeat count that passes would be choosing the vacuous end deliberately, so the default stays at the sensitive-but-noisy end and the recipe is **wired into no CI workflow**. Re-cutting the criterion — as `0023` re-cut `PHASE4` §7 gate 1 for the same reason — is a decision record, not a benchmark edit. It had no artifact at all until that date, which is the more important half of this note: §3.5's mechanism shipped on 2026-08-28 with correctness tests, and *nothing under `crates/tf_tree_bench/` referenced `owner_lost` or `inherit_ownership`*, so this criterion could not be evaluated while the phase was recorded **Implemented**. Read §12.2's row for what the quotient is and is not sensitive to before quoting it.

    **One finding came out of building it, and it is not about ownership — nor, as this paragraph first claimed, about composition.** A lookup against an actively-written ring transiently refuses with an *inverted* window: `oldest` a few milliseconds **past** `newest`, at roughly one lookup in 4 × 10⁷ and at the same rate in the steady phase as in the migration window. **The first revision of this note said "only the composed path can produce it … it intersects the four edges' windows". That was wrong, and the error carries the refutation in its own shape**: `LookupError::Extrapolation` names a *single* `edge` (`crates/tf_tree_core/src/error.rs:156`), so its `oldest`/`newest` are one ring's bounds and never an intersection.

    The mechanism is a sampling race in how those bounds are read. `SampleCursor::sample` loads `head`, then reads the two stamps with **two independent `Relaxed` loads** — `t_old = stamp_at(lo_logical)` and `t_new = stamp_at(newest)` (`crates/tf_tree_core/src/sample.rs:139-141`), where `stamp_at` is a bare `Relaxed` load with no seqlock (`:323-325`), deliberately, because these are bounds probes rather than sample reads. A writer that laps the ring between the two loads leaves the slot at `lo_logical` holding a stamp *newer* than the one already read for `newest`, and the reported pair inverts by exactly the number of slots the ring advanced — two, at the 4 ms observed against a 2 ms publish period.

    **The refusal itself is correct and conservative**; what is inconsistent is only the pair it reports. It is not quite free, because the pair is consumed. A torn pair has `requested < newest`, so `counter_of` files it as `extrap_before` and the counter path computes `gap = oldest - requested` and `fetch_max`es it into `worst_extrap_gap_ns` (`crates/tf_tree_core/src/plan.rs:2645-2667`) — the high-water mark **`TFT011` reads against the ring's actual retained span**, gated on `extrap_before > 0` (`crates/tf_tree_cli/src/checks.rs:1473-1476`), so the gate is satisfied and the bogus gap does reach the check. **The honest bound on that, measured rather than assumed: it did not come close to mattering here** — the observed gap is ~50 ms against a retained span of ~8 s, three orders short of the comparison `TFT011` makes. So the defect is that the mark can describe a window that never existed, not that any check has been seen to misfire from it. **Making the two bounds mutually consistent is a change to a hot-path read under a concurrent writer, which `CLAUDE.md` routes to a decision record rather than a PR** — this is recorded here, not fixed here. `owner_migration` counts these separately and excludes them from 4b's tally with the arithmetic printed, because they are a property of the ring's bounds reporting and not of ownership.
4c. **Scenario 9 of §11.2 passes 1000 consecutive runs with a single `instance_uuid`.**
5. Total RSS across 16 consumers under 1.2 × arena size.

### 12.4 What the numbers are actually for

The latency figures are the engineering gate. **The CPU-per-consumer and RSS figures are the industrial argument**, and they are the ones to put in the README.

Under `/tf`, every consumer independently deserializes every transform on the topic and maintains a full private replica: cost scales as O(consumers × edges × rate), and a robot with sixteen perception nodes pays for the same data sixteen times in both CPU and memory. Under `tf_tree`, consumers read shared pages: CPU is O(1) in the number of consumers, and RSS is one arena regardless.

Expect the latency ratio to be dramatic and the resource ratio to be the thing that actually persuades someone to migrate a working system. Lead with the latter.

---

## 13. Failure modes and runbook

Ship this table as `docs/RUNBOOK.md`. Every row must correspond to a `doctor` check and a distinct error type.

| Symptom | Cause | Response |
|---|---|---|
| `LayoutMismatch` on attach | binaries built from different commits | rebuild all participants; layout changes require a full restart |
| `BootIdMismatch` | arena predates a reboot (only possible with a file-backed dev arena) | recreate the arena |
| `ConnectionRefused` | owner not running, or a stale socket path | start the owner; the stale path is unlinked automatically |
| `NoParticipantSlots` | more than `max_participants` attached | find the leak. **Capacity is fixed at construction by design** ([`PROJECT.md`](./PROJECT.md) §5 D4) and there is no flag: `ArenaLayout` sets `max_participants` from `tf_tree_arena::layout::DEFAULT_MAX_PARTICIPANTS` in every constructor, and `check.rs`'s header validation requires an attaching segment to agree with it, so a differently-sized arena is refused on attach rather than served. Raising it means changing that constant and rebuilding every participant together. *This row read* raise `--participants` *until 2026-09-05; no binary in this repository has ever had that flag* — `grep -rn -- '--participants' crates/` finds nothing — *and [`RUNBOOK.md`](./RUNBOOK.md)'s row for the same error had it right all along.* |
| `FrameNotDeclared` on a read-only participant | startup ordering: no publisher has declared it yet | wait for it — `Tree::await_frames` ([`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md) §2). Check the consumer is not creating the arena itself: a read-only attach implies `CreatePolicy::Never`, and the builder's default is now `Never` too |
| `ClaimRevoked` during `push` | this writer was judged dead and reaped | the process was stalled; investigate scheduling, GC, or page-fault stalls |
| `EdgeAlreadyClaimed` | two nodes configured to publish one edge | a genuine configuration error — `doctor` names both PIDs |
| `SlotContended` / `SlotRecycled` | reader starved, or ring too shallow for the publish rate | increase edge capacity; `doctor` warns at 80% occupancy |
| `TopologyChurn` | topology mutated ≥ 4 times during one plan compilation | almost certainly a bug: topology should be near-static after startup |
| `SIGBUS` in a lookup | **structurally impossible with sealing (§3.6)** | if it ever happens, the segment was not sealed — file a bug |

---

## 14. Phase 3 handoff — constraints you must not break

> **Superseded in part by [`PHASE3.md`](./PHASE3.md) §1.** That document's §1.1
> corrects item 5 below: `abi3` alone is not a sufficient distribution target,
> because it does not work on free-threaded builds — and §1.2 adds the constraint
> that turned out to matter most, which this section missed entirely. Read
> `PHASE3.md` §1 before acting on items 4 or 5.

Phase 3 binds Python directly to the Rust core. Five Phase 2 properties must be preserved or Python users will hit them hard:

1. **`fork` safety.** `multiprocessing` defaults to `fork` on Linux. `MADV_DONTFORK` means the child's mapping is gone and any inherited handle is a fault waiting to happen. Phase 3 must register an `os.register_at_fork(after_in_child=...)` hook that poisons every inherited `Tree` handle so the child gets a clear Python exception rather than a segfault.
2. **GIL and liveness.** The socket carries liveness, not the heartbeat, so a long GIL-held pause does not risk reaping. Preserve that: do not add heartbeat-based reaping to make Python "safer" — it would make it strictly less safe (§6.4).
3. **Read-only by default.** The Python `attach()` default must be `ReadOnly`. Most Python consumers are analysis and visualization tools; they should be incapable of corrupting a robot's transform tree, and the default is what determines whether that is true in practice. Pair it with `CreatePolicy::Never` so a notebook started before the robot fails loudly instead of creating an empty arena that a later publisher then refuses to join.
4. **`tf_tree.open()` with no arguments must work in a notebook.** Zero-config discovery (§3) is most of the perceived quality of the Python binding; if a user has to pass paths, the seam has leaked.
5. **Distribution: `abi3` wheels via maturin.** ~~One wheel per platform.~~ **Corrected by `PHASE3.md` §1.1** — `abi3` does not cover free-threaded builds, so the matrix needs a version-specific `cp314t` wheel alongside it (built by a *second* maturin invocation, not a flag), and an `abi3.abi3t` job (PEP 803) for 3.15 onward. `cp313t` is not buildable on PyO3 0.29 and is deliberately not in the matrix.

Write these into `docs/PHASE3.md` as you finish, alongside the measured numbers from §12.

---

## 15. Definition of done

- [x] Amendments A1–A8 applied to Phase 1; all Phase 1 tests still pass unchanged — §0.0's first row records all eight as **Applied**, and `just test` is green on the whole workspace
- [x] `FORMAT_VERSION` bumped if Phase 1 had already been frozen, with a documented compatibility table — **the table is executable rather than written down, and that is the stronger form.** `tf_tree doctor --explain-version` prints the build's `format_version` and `layout_hash`, what each one means, why they are checked separately, what a mismatch requires (rebuild every participant from one commit and restart together — there is no compatibility layer), and that a version-2 arena cannot be attached. It reads the constants, so unlike a static table it cannot drift from them
- [x] Diff in `tf_tree_core`'s read path against Phase 1: **zero lines** — structural rather than diffed: the read path is written against `ArenaView` and never names a backend, so a heap arena and a mapped one traverse one body of code. §0.0's row records it as tested, and `another_process_reads_the_same_arena_bit_identically` (`crates/tf_tree_bench/tests/multiprocess.rs`) is the executable half — a second *process* answering bit-identically from the shared segment
- [x] `tf_tree_core` dependency list unchanged (D14) — re-measured from `cargo metadata`, not from the manifest's prose: the normal-kind third-party dependencies are exactly `blake3`, `bytemuck` and `libm`, plus the two workspace siblings `tf_tree_arena` and `tf_tree_math`. `proptest` and `loom` are dev-kind and reach no shipped artifact
- [x] All §11.2 integration scenarios pass in CI on x86-64 **and aarch64** — eleven scenarios, all named, all in `crates/tf_tree/tests/rendezvous.rs` except 4 and 5. They run under `just shm-rendezvous`, in the `shared memory (${{ matrix.os }})` job whose matrix is `[ubuntu-latest, ubuntu-24.04-arm]`, so both architectures are real rather than intended. Scenario 2b was already `slot_recycling_under_abnormal_exit`; 4 is `a_layout_mismatch_names_the_owners_hash_and_sends_no_fd` plus `a_version_mismatch_outranks_a_layout_mismatch` in `tf_tree_ipc`; 5 is `read_only_refuses_mutation_instead_of_faulting`; 8 is `a_survivor_inherits_ownership_and_the_arena_becomes_joinable_again`. **Scenarios 1, 2, 3, 6, 7, 9, 10 and 11 were added on 2026-08-30.** Two of §11.2's own durations are not reproduced and the reason is stated rather than skipped: scenario 1's *sustained 1 kHz for 60 s* and scenario 2's *60 s of churn* are soak shapes, and the properties they name — N readers of one arena agreeing exactly, and a participant table surviving churn — are falsified in the first round if they are falsifiable at all. `just shm-torture` is the sustained arm and checks the same invariants for thirty minutes nightly; scenario 2's test churns **three tablefuls, eight at a time**, so the collectors race each other rather than each getting a quiet moment, which is what distinguishes it from 2b's strictly-serial kills
- [x] Scenario 9 (split-brain) passes 1000 consecutive runs — **1000/1000 clean**, `just split-brain-soak`. One run is `scenario_9_a_split_brain_attempt_never_produces_a_second_arena` in the ordinary suite; the thousand is a soak, because its value is the tail. Mutation-verified: switching the racer to `CreatePolicy::Always` fails it with *"a racer that opens must join, not create"*, which is the split brain itself. **§11.2's own prediction for this scenario is wrong and the test says so**: it says the newcomer *"must block on §3.4 step 4 and then join"*, and it is **refused** with `ArenaHeldButUnreachable` — because ownership migration is caller-driven ([`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md)), so until a survivor calls `owner_lost()` nobody serves and there is nothing to join. The test asserts what protects the arena — refused is fine, joined is fine, a second `instance_uuid` never is — and then pokes the survivor into inheriting to show the join half
- [x] `tf_tree::open()` with no arguments joins-or-creates correctly from any start order — the order no sequential test covers is *simultaneous*, which is §11.2 scenario 7: `scenario_7_a_thundering_herd_produces_exactly_one_arena` starts sixteen openers at once against an absent arena and asserts every one of them sees **one** `instance_uuid`. The uuid comparison is the assertion rather than a count of who created, because a design that let two processes each create would report "one created" from each of their own points of view and only the uuids would disagree. Sixteen rather than §11.2's thirty-two, stated rather than silently reduced: the race is among the first few to reach step 4, and the rest of thirty-two is spent in a four-core scheduler
- [x] `doctor` prints `instance_uuid` and the resolved runtime dir, and works without the arena — **the two halves are one requirement, and the runtime dir was the missing one.** The run in which an operator most needs to know which directory was searched is the run in which nothing was found in it, so the directory is resolved independently of the source and a resolution failure degrades to omitting the line rather than failing the command. Both renderers carry it, with the rule that produced it (`Env`, `XdgRuntimeDir`, …), because an unexpected path is usually an unexpected rule. `crates/tf_tree_cli/tests/doctor_runtime_dir.rs` pins all three properties against the fixture — no arena, nothing mapped — and is mutation-verified: reporting a constant instead of the resolved directory fails `the_reported_dir_follows_the_environment`
- [x] Every §11.3 crash point has a test proving recovery — **thirteen of §11.3's fourteen rows carry a site and all thirteen are driven by a test that fires them**; the fourteenth, `reclaim.probe_then_reoccupied`, names an interleaving between two *live* processes, which a mechanism that kills one of them cannot produce (§0.0's fault-injection row argues it, and `loom` plus §11.4 are what can reach it). **Two completeness gates, not one, and the second exists because the first could not see the facade**: `crash_tests::the_published_site_list_is_the_one_the_tests_arm` refuses a site added to `tf_tree_core::crash::SITES` without a test, and covers `tf_tree_core` only — which is exactly why the last two untested sites were both in `tf_tree::CRASH_SITES`. `the_facade_site_list_is_pinned_by_index_and_every_site_has_a_test` closes that, and pins **index to name**, because the facade arms by index (`CRASH_SITES[0]`…`[5]`) and a set-equality gate would still pass after a reorder that made `reap_participants` arm the *hangup* name
- [x] `shm_torture` runs 30 minutes nightly, clean, under ASan — **three arms now, and the third ran in no workflow at all until 2026-09-04.** §11.4 asks for a random crash point armed in 10% of children as well as the `SIGKILL` soak, and `just shm-torture-crash-points` is that arm: `nightly.yml`'s `crash-points` job runs it, its own job because the build carries `--features shm,crash-points` and the parameters are gentler on purpose. Its exit status now reports what the `§11.3:` line says — see §11.4's amendment. The two ASan/soak arms, unchanged: **two arms, and the second one was two minutes.** `just shm-torture` has defaulted to `--duration 30m` and runs in `nightly.yml`'s `torture` job; `just shm-torture-asan` is in the same workflow's `sanitizers` matrix and defaulted to **120 s**, which is right for a local check and is not what this box asks of a nightly. The matrix now passes `--duration 30m --children 4 --kill-hz 4` explicitly, leaving the recipe's own default short so `just shm-torture-asan` stays two minutes for somebody checking a change. Four children rather than six because ASan's shadow memory and interceptors make six under it a different amount of work than six without, and the 90-minute job budget also has to cover a `-Zbuild-std` rebuild
- [x] `HeapArena` / `MappedArena` replay produces **bit-identical** results (§10) — `crates/tf_tree_cli/tests/replay_bit_identity.rs`. **Two bit-identity tests already existed and neither was this pair**: `a_frozen_lookup_is_bit_identical_to_the_live_one` compares heap against *frozen*, and `another_process_reads_the_same_arena_bit_identically` compares one mapped segment against *itself* from another process. Heap against mapped — the pair §10 names, and the one that would catch a shared-memory read path that had diverged from the in-process one — was covered by neither. **One variable**: both arenas are filled by the same `replay` function from the same `Vec<FixtureMessage>`, so the only difference between them is the backing store. **This sentence read *"from the same recording, written to MCAP and read back"* until 2026-09-05, and no read-back happens**: the test writes `run.mcap` and then asserts only that the file exists — `replay(&heap, &msgs)` and `replay(&mapped, &msgs)` both take the in-memory fixture, and the test imports no reader (`crates/tf_tree_cli/tests/replay_bit_identity.rs`; its own module doc and an inline comment carry the same overstatement and are code, so they are fixed separately). **The claim §10 asks for is unaffected and is the one the test proves** — one recording, two backends, bit-identical `f64` — because the property is about the two read paths and not about serialisation. What is *not* proven, and was never proven, is a round trip: CDR encode/decode of the pose bits, MCAP chunking, and the reader's own path are outside the assertion. Reading the file back would also change the query set, because the fixture's `/tf_static` edges carry `stamp_ns == 0` and `read_tf` consumers routinely skip zero stamps and statics — so widening the test is a change with a decision in it, not a one-line addition. 5 edge pairs x 300 stamps = 1500 answers, 865 of them successful lookups, compared as raw bits. Mutation-verified: a **one-ULP** perturbation of a single pushed pose fails it. **Where this box's evidence runs**, which it did not say while its neighbours did: the test is `#![cfg(all(feature = "shm", target_os = "linux"))]`, so **`just test` does not run it** — `cargo nextest list -p tf_tree_cli` does not list it and the same command with `--features shm` does. `just shm-check` runs it, and `ci.yml`'s `shm` job runs `just shm-check`. At least one other box in this list has the same omission — the one citing `another_process_reads_the_same_arena_bit_identically` — so this clause fixes one row and is not a sweep. **§10's *tooling* is not absent-and-owed but DECLINED** — [`0047`](./decisions/0047-the-recording-this-reader-would-refuse.md); there is no `tf_tree_record` binary tapping a live arena and no replayer command, and none is owed. §0.0's own rows record that, and this box is the NORMATIVE test they name
- [x] §12.3 gate met, or a written explanation of which criterion failed and by how much — **the explanation, criterion by criterion, on a 4-physical-core / 8-thread development host (2026-08-30).** Three states, and the box's wording admits only two, so the third is named rather than folded into "failed": **met**, **failed by a stated margin**, and **not evaluable on this hardware** — which is a fact about the host, not about the code, and must not be recorded as the code failing (`docs/PROJECT.md` §6).

  | criterion | verdict | evidence |
  |---|---|---|
  | 1. cross-process depth-3 p50 within 10%, p99.9 within 25% | **not evaluable here** | `just mp-bench` **refuses to run**: the host was 13% busy against its own 10% threshold, and it named the load. Latency there is largely a measurement of the scheduler, so a number taken now would describe the other workload. The refusal is the harness working |
  | 2. aggregate read throughput scales ≥ 12× from 1 to 16 processes | **not evaluable here, and the ceiling is arithmetic** | `just shm-scaling`: 1.90× at 2, 2.98× at 4, **3.72× at 8** processes on **4 physical cores**. The harness stops at 8 because the row above that oversubscribes. 12× at 16 needs ≥ 16 cores to be *possible*; this host cannot express the question |
  | 3. zero corrupt reads, and every §11.3 crash point recovers | **met** | `just shm-torture` PASSes with `0 violation(s)`; the crash-point half is §15's box 9 — 13 of 14 rows carry a site, all 13 driven by a test, under two completeness gates |
  | 4. kill → re-claimable p99 under 10 ms | **met, 0.182 ms** | `just reclaim-latency`, 200 trials: p50 0.122 ms, **p99 0.182 ms**, max 0.400 ms — inside the bound by ~55×. **The gate had no artifact until 2026-08-30**, and the first revision of the one that closed it was vacuous: with `wait()` inside the timed region the edge was takeable on the first attempt in 50 of 50 trials, so it reported the cost of `kill` as a 0.247 ms PASS. Moving `wait` out gives 0/200 first-attempt successes, and that counter is now a **refusal** rather than a note — a run where it is high prints `INVALID, not FAIL` and no verdict |
  | 4b. migration invisible to the data plane | **partly met, weakly evaluable** | unchanged, and argued at length in §12.3's own note: zero failed lookups always; the p99.9 quotient is 0.976–1.093 at the sensitive default and exactly 1.000 at `--repeat 15`. Re-cutting it is a decision record, not a benchmark edit |
  | 4c. split-brain, 1000 consecutive runs, one `instance_uuid` | **met** | `just split-brain-soak`: **1000/1000 clean**. §15 box 6 |
  | 5. total RSS across 16 consumers under 1.2 × arena size | **not evaluable at 16, and the criterion's arithmetic does not survive contact** | `just shm-scaling` at 8 processes: unique resident **19.9 MiB** against an arena of **1368 KiB**. The criterion's *intent* is met and is visible in the same table — the arena is `MAP_SHARED` and resident once, so `unique res` grows ~2.3 MiB per process while `sum RSS` double-counts it — but its literal form compares whole-process RSS against the arena alone, and per-process overhead (binary, stack, allocator) dominates at any arena this size. It is not reachable by any implementation, which is a defect in the criterion |

  **So: three met, one partly met, three not evaluable on this host — and none failed.** The two that need ≥16 cores and the one that needs a quiet machine are hardware, and both are the same kind of gap `docs/PHASE5.md` §9.3 already handles by printing `UNAVAILABLE` with a reason. Criterion 5 is the one that should be *re-cut* rather than measured: as written it compares two quantities that are not comparable.
- [~] `tf_tree serve` ships with a systemd unit and a container example — **out of scope for this phase by its own text and by `0019`.** §9 is superseded by [`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md), whose steps 6–7 are *"not built, and are not scheduled"*, and this box already said *"not first-release scope"*. Marked `~` rather than left `[ ]`: an unticked box reads as work owed, and this is work declined. It is also the home the `--force-new` flag was deferred to (§0.0's §3.4 row), so that row is gated on this decision rather than on effort
- [x] `docs/RUNBOOK.md` complete; every row maps to a `doctor` check (rows for unimplemented Phase 2 errors are marked as such)
- [~] `docs/PHASE3.md` written and carrying §14 forward; the measured numbers land with §12

---

## Appendix A — implementation order

Steps 1–3 are the phase. Everything after them is comparatively mechanical.

1. **Amendments A1–A8 against `HeapArena`**, with the loom tests. Do this before touching a single syscall. Every one of these is a correctness fix that is testable single-process, and finding a bug here after the IPC layer exists costs ten times as much to diagnose.
2. **The lock file and `open()`.** Runtime-dir resolution, OFD ownership and participant bytes, the §3.4 algorithm including the split-brain check, ownership migration. Build §11.2 scenarios 7–11 alongside it; scenario 9 in particular should exist before the code it tests.
3. **`MappedArena` + attach protocol.** Owner and attacher, sealing, `SCM_RIGHTS`, header validation. Assert the zero-line-diff property in the read path.
4. **Claims as OFD locks; arena-side reaping; crash-point harness** — the harness built alongside, not after.
5. ~~`tf_tree_record`~~ and the bit-identical replay test. **The replay test is done** (`crates/tf_tree_cli/tests/replay_bit_identity.rs`); the recorder is declined by [`0047`](./decisions/0047-the-recording-this-reader-would-refuse.md).
6. `tf_tree serve` — last, and possibly never: [`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md) §2 is shaped so the daemon may never become urgent.
7. `doctor` / `top` / `participants` extensions.
8. `/tf` ingest bridge. **Note the impedance mismatch:** ROS permits any number of publishers per edge; `tf_tree` permits one. The bridge must claim each edge on first sight and adopt a documented, configurable policy on conflict — `FirstWriterWins` (default, with a loud diagnostic naming both ROS publishers) or `LastWriterWins`. This mismatch is a feature, not a bug: it surfaces multi-publisher conflicts that `tf2` silently averages into garbage, and the bridge is where users will first discover their robot has one.
9. Benchmarks and the gate.

Do not proceed past step 4 until every §11.3 crash point recovers and §11.2 scenario 9 passes a thousand consecutive runs. Everything after assumes crash-consistency and a single arena identity; a violation in either will present as an impossible numerical result somewhere around step 8.

## Appendix B — kernel behaviour probe

Verified on Linux 6.18. Re-run on the target kernel; the sealing results in §3.6 are load-bearing.

```c
#define _GNU_SOURCE
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>
#include <fcntl.h>
#include <stdio.h>

int main(void) {
    int fd = syscall(SYS_memfd_create, "tf_tree.probe",
                     MFD_CLOEXEC | MFD_ALLOW_SEALING);
    ftruncate(fd, 1 << 20);
    void *p = mmap(NULL, 1 << 20, PROT_READ | PROT_WRITE,
                   MAP_SHARED | MAP_POPULATE, fd, 0);

    printf("seal SHRINK|GROW w/ writable map: %d (expect 0)\n",
           fcntl(fd, F_ADD_SEALS, F_SEAL_SHRINK | F_SEAL_GROW));
    printf("seal WRITE w/ writable map:       %d (expect -1 EBUSY)\n",
           fcntl(fd, F_ADD_SEALS, F_SEAL_WRITE));
    printf("ftruncate shrink after seal:      %d (expect -1 EPERM)\n",
           ftruncate(fd, 1 << 10));
    printf("seal SEAL:                        %d (expect 0)\n",
           fcntl(fd, F_ADD_SEALS, F_SEAL_SEAL));
    printf("F_GET_SEALS:                      0x%x (expect 0x7)\n",
           fcntl(fd, F_GET_SEALS));
    printf("MADV_DONTFORK:                    %d (expect 0)\n",
           madvise(p, 1 << 20, MADV_DONTFORK));
    printf("MADV_HUGEPAGE:                    %d (expect 0)\n",
           madvise(p, 1 << 20, MADV_HUGEPAGE));
    return 0;
}
```

**OFD locks and lazy allocation.** Measured on Linux 6.18:

```
OFD exclusive lock on a byte                      -> 0
same byte from another process                    -> -1 EAGAIN
holder killed without unlocking, then GETLK       -> F_UNLCK   (kernel released it)
GETLK on a byte held by a live process            -> held, l_pid = -1   (see §3.3)

memfd ftruncate to 1 GiB                          -> 0 KiB charged
mmap without MAP_POPULATE                         -> 0 KiB charged
touch 16 MiB                                      -> 16384 KiB charged
madvise(MADV_WILLNEED, 16 MiB)                    -> 0 KiB charged   (does NOT populate)
madvise(MADV_POPULATE_WRITE, 16 MiB)              -> 16384 KiB charged
```

**The `/proc` parsing trap (§5.1), as a test fixture.** For a process whose `comm` is `evil) proc`, the naive whitespace split returns field 12's value where field 22 was intended:

```
raw    = "1234 (evil) proc) S 1 1234 1234 0 -1 4194304 1 2 3 ... 39"
naive  : raw.split()[21]                       -> 12    WRONG
robust : raw[raw.rindex(')')+2:].split()[19]   -> 13    correct
```
