# 0055: the recovery capacity a fleet cannot add later

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** **every step has landed** — 2 in #353 (`0411adb`), 1/3/4 in
#355 (`ea98e56`), 6 in #356 (`d121589`), 7 after it. Step 5 is **struck**: open
question 1 answered *no mechanism*, so there was nothing to build.

## Context

An arena reaches a state from which no process outside it can ever attach again.
It takes three conditions at once:

1. the **ownership byte is free** — nobody is serving the rendezvous; and
2. **at least one participant byte is held** — some process still has the
   segment mapped; and
3. **no *eligible* participant is there to call `Tree::inherit_ownership`** —
   where eligible means attached **and** read-write **and** actually polling.
   A participant fails that by being read-only, by being built against a release
   that has no such call, or because its loop simply never polls. **Eligibility,
   not mode, is the axis**, and *The gap* below is about the two ways to miss it
   that no reader is currently in a position to derive.

(1) and (2) together refuse every new **rendezvous** attach, and they refuse it
by two different mechanisms. Nothing is serving, so §3.7's join cannot start —
the socket path may still be there, unlinked only by whoever next wins ownership
(§3.9), but nothing is behind it and there is no `SCM_RIGHTS` fd to receive. A
joiner therefore falls through to the create path, and that is refused by §3.4's
split-brain check, which yields to any held participant byte with no grace
period and no window to tune (`crates/tf_tree_ipc/src/open.rs:365-372`).

**"Rendezvous" is load-bearing there, and it is also the mechanical reason the
candidate set cannot grow by some other door.** Exactly one attachment path does
not go through the rendezvous — `Tree::attach_shared` / `attach_shared_at`, over
a descriptor somebody handed you — and it is *not* closed by (1) or (2). It
still cannot supply an heir, and that is a property of the API rather than of
the state:

- `refuse_a_byteless_writer` (`crates/tf_tree/src/tree.rs:2897` and `:2928`)
  turns `AttachMode::ReadWrite` away with `ShmError::ReadWriteNeedsRendezvous`
  (`:3715`) before the segment is even mapped — a bare descriptor has no lock
  file to take a byte in, and a participant record with a permanently free byte
  is indistinguishable, by the byte alone, from a slot leaked by a killed
  process ([`0028`](./0028-the-slot-a-killed-participant-keeps.md) plan step 0b).
- The read-write path that *does* carry a byte, `Tree::attach_joined_at`
  (`crates/tf_tree/src/tree.rs:2947`), is `pub(crate)` and called from exactly
  one site — `Open`'s `Joined` arm (`crates/tf_tree/src/open.rs:1183`), which is
  the arm "nothing is serving" makes unreachable.
- `Tree::inherit_ownership` answers `Inheritance::NotApplicable` unless
  `is_joined()` (`crates/tf_tree/src/open.rs:599-601`), so even a hypothetical
  writable byte-less mapping could not be the heir.

A **read-only** `attach_shared` over a passed fd therefore still succeeds while
(1) and (2) hold, and this record must not say otherwise. It takes no lock byte
and registers nothing, so it neither joins the candidate set nor supplies
condition (2) — but it does hold a *mapping*, which matters one paragraph down.
(It is the one attachment that is not a byte-holder, so "holder" below always
means *byte*-holder.)

**So the set of processes that can satisfy (3) is fixed at the instant (1)
becomes true, and from then on it can only shrink.** A member leaves it by dying
or detaching; no process can join it, because joining is exactly what (1) and
(2) forbid. The state is escaped only from the inside, by a member that calls.
Once the set is empty it stays empty for as long as any participant byte is
held — and when the last *byte*-holder exits, §3.4 step 4's participant scan
finds nothing and the next `open()` creates. Whether the **segment** also dies
is a separate question, deliberately not claimed here: §3.9 frees it when the
last *mapping* drops, and a byte-less read-only mapping can outlive every byte.
The wedge ends with the last byte; the memory need not be reclaimed at the same
instant. So the wedge lasts precisely as long as the longest-lived byte-holder,
which on a robot carrying a logger, a visualiser or a `tf_tree top --attach` is
the life of the robot.

**This is not a defect in the ownership path**, and that matters for what the
record is about. §3.5 works: `owner_lost` is a question about the owner
([`0043`](./0043-owner-lost-is-a-question-about-the-owner.md)),
`take_over_ownership` takes byte 0 on a description the survivor already holds
([`0037`](./0037-a-takeover-is-not-a-second-open.md)), and the recovery surface
reaches C, C++ and Python
([`0044`](./0044-recovery-the-languages-a-robot-is-written-in-cannot-reach.md)).
Every one of those mechanisms needs a *caller that is still there*, which is the
thing the state above removes.

### What is already documented, and must not be re-announced

Three passages already state pieces of this, in the terms this record uses. Two
of them are about the **read-only** shape; the second is about **not calling**,
which is why the gap below is careful to say *unreconciled* rather than *absent*:

- [`PHASE2.md`](../PHASE2.md) §3.4's paragraph on the timeout case — no new
  process can join while a participant is stuck, "and that is the right answer,
  because the alternative is divergence".
- [`PHASE2.md`](../PHASE2.md) §3.5's NORMATIVE closing paragraph — "a fleet
  whose survivors never call `owner_lost()` still ends up with an ownerless
  arena, and new joiners still time out on `ArenaHeldButUnreachable`".
- [`RUNBOOK.md`](../RUNBOOK.md), *The arena's owner died* and
  *`ArenaHeldButUnreachable`* — including the remedy in as many words: "a fleet
  of read-only consumers cannot rescue itself ... that is a reason to open one
  process read-write even if it never publishes, since read-write is what makes
  a survivor eligible rather than what makes it a writer."

**A record claiming this is undocumented would be wrong**, and so would one
claiming that only the read-only case is. The gap is narrower than either, and it
is about which of these a reader can act on.

### The gap: eligibility is an instant, not a census

The runbook's remedy is stated as a property of the **fleet's configuration** —
*open one process read-write*. The condition that actually decides recovery is a
property of **one instant**: whether a participant *eligible to inherit* was
attached at the moment the role fell vacant. Those come apart, and the ways they
come apart are not written anywhere a reader could act on them.

**The axis is eligibility, not mode**, and getting that wrong is how a reader
concludes the runbook's remedy is sufficient. Conditions (2) and (3) above,
restated as the two things the wedge needs and relabelled so this section can
refer to them:

- **(a) — condition (2) — a byte-holder that outlives the vacancy.** *Any*
  rendezvous attachment supplies this. `register_at` runs for every
  `Reach::Serving` regardless of `AccessMode`
  (`crates/tf_tree_ipc/src/open.rs:328-345`), so a read-only consumer holds a
  participant byte exactly as a publisher does.
- **(b) — condition (3) — no attached-and-polling read-write participant at the
  vacancy instant.** Membership of the candidate set needs all three at once —
  attached, read-write, and actually calling `owner_lost()` /
  `inherit_ownership()`. Miss any one and the process is ineligible, whatever its
  `mode` column says.

**Read-only is one way to be ineligible. There are three, and the existing text
covers the first one only.** Being careful about what "covers" means here matters,
because two of the three *facts* are written down and it is the **consequence for
a fleet** that is not:

- **Read-only.** D18's default shape. `Inheritance::ReadOnly` is returned before
  anything is attempted (`crates/tf_tree/src/open.rs:605-606`), which is D18
  working rather than failing: an owner writes the participant table on every
  grant and a `PROT_READ` mapping cannot. Such a consumer supplies (a) while
  being disqualified from (b). **Documented, above.**
- **Attached and read-write, and never polling.** `AttachMode::ReadWrite`
  describes a mapping's protection bits; it promises nothing about a control
  loop. A node that opens read-write and never calls `owner_lost()` holds a byte
  and is not a candidate. **The fact is documented twice and reconciled with the
  remedy nowhere, and that is the defect.** `PHASE2.md` §3.5's NORMATIVE closing
  paragraph and [`RUNBOOK.md`](../RUNBOOK.md)'s own lead-in both say it outright
  — *"a survivor that never calls `owner_lost()` never becomes owner, and the
  arena stays ownerless"* — and then, a few lines later in that same runbook
  section, the remedy bullet ends *"read-write is what makes a survivor eligible
  rather than what makes it a writer"*, full stop. Read alone, which is how an
  operator reads a remedy, that clause is **wrong**: read-write is what makes a
  survivor eligible *in principle*, and polling is what makes it a candidate *at
  the instant*. **This is the reader the record exists to warn** — a fleet with
  one long-lived read-write attachment that never polls *satisfies the remedy as
  written* and is squarely in the absorbing state — and it is a correction to
  operator guidance, which is why it is a record and not a doc patch. **Nothing
  in this record edits `RUNBOOK.md`**; step 1 is where the correction would go.
- **Read-write and polling, and not attached right then.** Every process in the
  fleet is read-write and every one of them polls, and the candidate set is
  still empty at the vacancy instant because none of them happened to be
  attached. A launch-managed robot whose publishers are restarted by a
  supervisor is the ordinary shape of this. **This one is absent rather than
  unreconciled**: nothing in `docs/` states the requirement as a property of an
  attachment's *lifetime* at all. That was checked rather than assumed —
  searching for the requirement's spelling (`at least one` near read-write or
  survivor) and for the mechanism (`transient`, `respawn`, `launch-managed`,
  publisher restart) returns the runbook's mode bullet and nothing about
  lifetime. (A fourth way to be ineligible exists and is a release question
  rather than a fleet one: a participant built against a release with no such
  call — every binding before
  [`0044`](./0044-recovery-the-languages-a-robot-is-written-in-cannot-reach.md)
  landed on 2026-08-29, which is to say every C, C++ and Python node.)

**And (a) and (b) have to hold at once, which is what makes the second and third
bite.** A fleet of *nothing but* short-lived read-write processes self-heals:
when the last one exits no byte is held, and the next `open()` creates. What
does not self-heal is the ordinary combination — publishers that churn or stop
polling, plus one attachment that stays: a logger, a `tf_tree top --attach`, a
read-only node. The holder need not be the ineligible one and usually is not;
what matters is that some byte outlives the vacancy while the candidate set is
empty. The `shm_torture` harness holds attachments of both kinds (one
`AttachMode::ReadOnly` site, three read-write), which is why it reaches the state
at all.

The sharpening this record exists to state: **the runbook's advice is necessary
and not sufficient**, on both axes. A launch file that always names a read-write
node satisfies the advice at every instant *except* the one that matters, and a
read-write node that never polls satisfies it at every instant and is never a
candidate. No number of read-write processes in a fleet's declaration adds
capacity to an arena that has already gone ownerless. Recovery capacity is
provisioned before it is needed or it does not exist.

### What forced it now

A three-night nightly failure of `shm_torture` was root-caused on 2026-09-10 to
this state, not to the ownership path. Measured in that investigation over two
runs on stock engine code with no crash points, the §3.5 trigger tally was
`inherited=1956 owner-alive=11 contended=10` with **zero** `err-*` — the
mechanism firing correctly nearly two thousand times, and the harness
nonetheless driving its own candidate set to empty across a kill. *(Those
numbers are quoted from that investigation. This record's author did not re-run
the harness — the CPU was in use elsewhere — so they are cited, not reproduced
here.)*

The minimal form was reproduced in a staged test: owner dies, one in-process
read-only consumer remains, and a fresh
`Open::new().mode(ReadWrite).create(Never).open()` is refused with
`ArenaHeldButUnreachable`.

#### It happened again after the harness was repaired, and this time the margin was measured

**2026-09-10, `nightly` run 34453737033.** Six of seven jobs green;
`shm-torture-asan` red. Judged per job, not by the run's colour. The repair
landed in #310 worked exactly as designed — the harness **classified** the
failure itself, and this is an excerpt of the one sentence it emits (the full
line also gives the recovering migration, the recorded owner pid and the elapsed
time):

```text
owner kill 10 left the arena in an UNRECOVERABLE state ... Classification:
POPULATION — no eligible heir remained to ask, so §3.5's trigger was never
answered. This is the state the pre-kill census exists to prevent; reaching it
means the census passed and the pool drained inside the vacancy ... Heirs
attached at this kill: 0 (first round after: 0).
```

§3.5 trigger tally `inherited=9`, **zero `err-*`**, over ten owner kills. The
engine refused no heir; there was none left to ask. That is this record's state,
reached by a fleet that had been *checked* for capacity moments earlier.

**The margin, measured on this host.** Four runs at the failing job's
`--children 4 --kill-hz 4` — its duration is 30 minutes
(`.github/workflows/nightly.yml`), and these are 120–150 s, so a fifteenth of it:

| Run | ASan | Cores | Owner kills | Result |
|---|---|---|---|---|
| 1–3 | no | 8 | 15 each | **PASS**, all recovered |
| 4 | yes | 8 | 15 | **PASS**, all recovered |
| 5 | yes | **2** (`taskset`) | 18 | **PASS**, all recovered |

Ninety-three consecutive owner kills recovered. What matters is not that they
passed but the **per-kill heir census the harness prints beside each one**, which
is the population instrument here. Run 5's eighteen kills read:

```text
heirs attached at the kill: 2 1 3 3 2 1 2 2 2 2 2 2 3 3 1 2 2 3
```

**Three of eighteen ran with exactly one heir left.** `heirs_at_kill == 1` means
that after the role holder died, precisely one attached read-write survivor
remained to answer §3.5 — and it did, every time. The nightly saw **0**. So the
distribution's left tail already touches one process, the failure is one step
further out, and a 30-minute run at this cadence draws roughly 225 owner kills
against these 15–18. Nothing about that requires ASan or a two-core runner to be
the cause, and nothing here excludes them either.

**Two arguments that were made for this and are withdrawn**, recorded rather than
quietly replaced, because both were built on numbers that do not say what they
were read as saying:

- *"The attached fraction is `writers=1.6–1.7/4`, identically in CI and here, so
  ~42% of children are attached and `heirs_before` is essentially always exactly
  2."* **`writers=N/4` is not a fraction of children.** It is `writers_live` over
  the **four `CHAIN` edges**, and the `4` in that format string is `CHAIN.len()`
  — the harness's own long form says *"{:.2} of the 4 chain edges had a live
  writer"*. It is bounded by 4 whatever `--children` is, and a child that is
  attached while holding no claim contributes nothing to it. `--children 4`
  coinciding with `CHAIN.len() == 4` is exactly what hid that. The population
  instruments are `slots=Nreg/Malive` and the per-migration `heirs_before` /
  `heirs_at_kill` / `heirs_first_round` above.
- *"A 30-minute run is fifteen times the exposure, and one kill loses the race."*
  True of the number of draws and misleading as stated: a POPULATION wedge ends
  the run, so the nightly's failure on its **tenth** owner kill puts it at
  t ≈ 80 s, inside the window every local run above covered. Ninety-three clean
  local kills against a failure on the nightly's tenth is *consistent* with a low
  per-kill rate — it does not establish that CI differs, and does not exclude it.
  The two-core ASan run above was taken to test one concrete mechanism for a
  difference and did not reproduce it.

**What is established, and what is not.** Established: the state recurs after the
census-and-defer remedy; the engine is clear by tally; and the harness's own
classification — the census passed and the pool drained inside the vacancy — is
entailed by `heirs_at_kill == 0`, not inferred. Not established: why *that* kill
and not the ninety-three. The reap-plus-census interval is the obvious candidate,
since `heirs_at_kill` is taken after the victim's `kill()` and `wait()` and that
is precisely the window in which the surviving heir must not leave — and it is
untested at CI's core count and load.

**Why this belongs in *this* record and not in a harness patch.** The census
`kill_the_owner` performs is a correct reading of eligibility at the instant it is
taken, and the vacancy has *duration* — which is [*The gap: eligibility is an
instant, not a census*](#the-gap-eligibility-is-an-instant-not-a-census) stated
about a real fleet rather than a staged test. `heirs_at_kill == 1` on one kill in
six is that gap with a number on it. The harness is now a **first consumer** of
whatever open question 1 answers, because what it needs is a supported way to
hold recovery capacity that cannot evaporate between two syscalls; everything
available without that either lowers the rate and proves nothing (more children,
longer attachments) or redesigns what the harness pins, which is a decision.

**Until this record is `ready`, `shm-torture-asan` is expected to fail on some
nightlies, and this is the discriminator.** It is safe to attribute a failure to
this record **only** when the run's final error carries
`Classification: POPULATION`, `Heirs attached at this kill: 0`, and a §3.5 trigger
tally with **no `err-` entry**. Anything else in that job — a `violation`, an ASan
report, `Classification:` naming anything but POPULATION, or any `err-` in the
tally — is a different failure and must not be read as this one. Nothing notifies
on a red nightly, so a job left red is a job whose next real failure is invisible;
that is a cost this record is accepting knowingly and it is the strongest argument
for answering open question 1 rather than deferring it.

### What holds this today: one paragraph of prose, and a gate that stops half way

- **No type.** `AttachMode::ReadWrite` is a fact about the mapping's protection
  bits, not a promise to poll. `Inheritance::ReadOnly` reports ineligibility
  only when asked, after the owner is already dead.
- **Gated in two halves that do not meet, and an earlier revision of this record
  said "no gate", which was false.** The **mechanical** absorbing property is
  pinned, and pinned hard. `a_live_participant_prevents_a_second_arena`
  (`crates/tf_tree_ipc/tests/multiprocess.rs:230`, §11.2 scenario 9) refuses 128
  consecutive `open()`s against a held participant byte, asserts after **each**
  refusal that the ownership byte was handed back — so the refusal is keyed to
  the held byte and cannot latch — and ends with an explicitly labelled positive
  control: kill the holder, and the next `open()` returns
  `OpenOutcome::Created`. [`PHASE2.md`](../PHASE2.md) §3.4 cites that control in
  prose for exactly this purpose. The **eligibility** half is pinned separately
  and only at an instant: `a_read_only_survivor_reports_that_it_cannot_inherit`
  (`crates/tf_tree/tests/rendezvous.rs:4577`) asserts one
  `Inheritance::ReadOnly` after one owner death, and
  `scenario_3_an_owner_dying_leaves_readers_working_and_joins_refused` (`:5472`)
  asserts one `ArenaHeldButUnreachable` against one live survivor — both under
  `just shm-check`.
  **What no test joins is the two.** The byte tests reach the state with a *bare
  byte-holder* — a lock byte and no attachment, deliberately, because a held byte
  is the entirety of what §3.4 step 4 consults — so they know nothing about who
  could inherit. The eligibility tests know nothing about how long the refusal
  lasts. The uncovered property is the conjunction, and it is condition (b): an
  ownerless arena whose surviving byte-holder is *attached and ineligible*
  — read-only, or read-write and never polling — admits no new process for as
  long as that holder lives. That is the property, and it is the one this record
  is about.
- **No tooling, and the tooling that exists points the wrong way.** No path in
  `crates/tf_tree_cli/src` calls `owner_lost` or `inherit_ownership` at all, so
  no invocation of the shipped binary can ever be the heir. `tf_tree --rw top`
  is **refused by design** — `cmd_top` argues that a live view is "the tool most
  likely to be left running unattended on a robot" and that D18 exists to keep a
  read-write mapping away from a diagnostic. That argument is right, and its
  consequence is that the longest-lived attachment on a robot is guaranteed to
  be a byte-holder that can never inherit.
- **One of the three eligibility conditions is observable; the other two are
  not.** `tf_tree participants` prints a `mode` column (`ro`/`rw`) read from the
  identity records (`crates/tf_tree_cli/src/lib.rs:1660-1661`), so "does my fleet
  have a read-write attachment right now" is answerable today with no new
  surface. *Attached at the instant the role fell vacant* is answerable only
  while it is happening, and nobody is asking then. *Does it call?* is not
  answerable at all, and [`RUNBOOK.md`](../RUNBOOK.md) already says so —
  "`tf_tree participants` shows you who is attached; it cannot show you who is
  looking." That is D17 and
  [`0019`](./0019-one-binary-and-topology-you-can-wait-for.md) being consistent,
  not an omission: intent is not a kernel fact. It means capacity can be
  *configured* but not *verified* from outside the process holding it.

## Decision

**Decided 2026-09-18 by the owner. What changed is the *standing* of the parts
below, not their content — except part 4, where the evidence moved.** This record
was drafted to state the property out loud and to refuse to choose a mechanism
from its own pages; the questions it left are answered here, with their reasons,
and the answer to both of the expensive ones is **no new surface**. So what this
record authorises is documentation and a test, not a protocol.

The three questions are answered below as parts 2, 3 and 4. Question 1's table of
candidates, question 2's costed routes and question 3's measurement stay where
they are: they are the working, and a reader who wants to reopen one needs them
rather than this summary.

**1. Decided (documentation of an existing consequence; no code, no new
surface).** The property is stated where a reader meets it —
[`PHASE2.md`](../PHASE2.md) §3.5 and [`RUNBOOK.md`](../RUNBOOK.md)'s two
sections — in the sharpened form:

> Recovery capacity is whatever was **attached and eligible at the instant the
> ownership role fell vacant**. It cannot be added afterwards: an ownerless
> arena with a held participant byte admits no new **rendezvous** attachment,
> and the rendezvous is the only door a would-be heir can come through — the
> descriptor-passing path refuses `AttachMode::ReadWrite` outright — so the set
> of processes that could inherit can only shrink from that instant on. A fleet
> that intends to survive owner death holds that capacity **before** the owner
> dies. **Eligibility, not mode, is what is held**: a read-write attachment that
> never polls `owner_lost()` is not recovery capacity, and neither is a
> read-write node that was not attached at that instant. It is a property of an
> attachment's *lifetime and its loop*, not of a fleet's declaration.

...together with the three ways to be ineligible, because the two beyond
read-only are the ones an integrator will not derive from the existing text —
and with the statement, in [`RUNBOOK.md`](../RUNBOOK.md), that its own remedy is
necessary and not sufficient.

**2. Decided (question 1): the NORMATIVE fleet requirement, and no mechanism.**
Question 1's other two candidates are one candidate wearing two hats. A
standby-heir helper polls, so it is a thread or a process; an `Open` policy either
spawns that thread or records an intent nothing acts on. [`PHASE2.md`](../PHASE2.md)
§3.5 is NORMATIVE that there is "no background thread, no daemon, no watcher a
user must run", and [`0019`](./0019-one-binary-and-topology-you-can-wait-for.md)
holds that every process a user is *required* to run is a place adoption dies. An
*optional* helper escapes the letter of that and not its point: the fleets that
wedge are exactly the fleets that did not opt in, so an optional helper **names**
the gap at the price of a permanent surface and closes it for nobody who did not
already know. The presumption that such a helper would belong inside `tf_tree
serve` makes the cost larger rather than smaller, because `serve` as specified is
the arena's *creator* — a standby attaching to somebody else's arena is a scope
change to an unbuilt capability, and it would be argued on its own or not at all.

What the requirement is worth is stated rather than sold: it is advice wearing a
normative label, and its second half — does anyone *call*? — is unobservable from
outside the calling process. That is the honest ceiling of what a library with no
daemon can promise here, and it is the same ceiling §3.5 already lives with.

**What would reopen this:** a measured field incident in which a fleet that had
read the requirement wedged anyway. Not an argument that a helper would be
convenient, and not a second reading of the same trade.

**3. Decided (question 2): no, and the refusal is recorded with its reasons so
the next proposal starts from them.** Question 2's three routes each *supersede* a
standing decision rather than extending one — a named segment runs at
[`PROJECT.md`](../PROJECT.md) §6's design-smell list and §3.9's "no stale
segments, ever", and *that* is the route whose proposal would have to supersede
the entry rather than amend §3.6's creation sequence; an fd depot is a daemon, so
`0019` applies in full, and §3.6 has nothing to do with it; an ownership handoff
through the lock file runs at §3.4's **deleted** step 3 and the five unsound
states [`0037`](./0037-a-takeover-is-not-a-second-open.md) enumerates — the list
is `0037`'s, and [`0035`](./0035-the-creators-slot-is-taken-not-found.md) is
state 1 of it rather than a second enumeration. A proposal may still be made. It
starts by superseding the decision its own route runs at.

**4. Decided (question 3), and the answer is not the one this record's own
framing suggested.** Question 3 asked whether the message should recommend the
abandoning path at all, when [`RUNBOOK.md`](../RUNBOOK.md) says to reach for
inheritance first, and left it open as "a judgement about operator guidance".

The first answer considered was that the two texts have different *readers* and
both orderings are therefore right: the process printing
`ArenaHeldButUnreachable` is a would-be joiner being refused an attachment, and
inheritance is reachable only by a survivor that is already attached, so telling
that reader to inherit first would name the one remedy it structurally cannot
reach. That argument is sound as far as it goes, and **step 2's implementation
(#353) falsified the conclusion it was used to support** — that the message needs
no structural change.

What #353 measured is that this arm cannot carry a remedy at all. `Display` sees
which lock bytes are held and **cannot tell whether one process holds two of
them** — nor name the ownership byte's holder at all; `first_pid` names the first
held *participant* slot and nothing else. The remedy wants to say what to *stop*,
which is a statement about processes. Three successive statements out of that one arm were wrong in
a reachable state — the missing layout clause, the discarded `ownership_held`, and
then a repair that asserted two holders and was false in the steady state of every
healthy single-owner arena. Each was a hedge added to an inference the type cannot
support.

**So: the message's job is facts plus one pointer, and the remedy belongs in the
runbook alone.** The bytes held, the first slot, its pid, and where to read what
to do — `RUNBOOK.md`, whose reader has every process in hand and can therefore be
given an ordering. This does not reopen [`API.md`](../API.md) R5: the prose stays
in the message layer and nothing enters the error type. It is a reduction of what
that prose claims, not a move of it.

**Not done here, deliberately.** As landed, the arm is correct and pinned in two
crates with mutants, and a third rewrite of the same text inside the branch that
found the second one is how a repair breeds a defect. It is step 6 below, and it
is this record's to own because it is the question this record left open.

**5. Separable, and landed ahead of this record on its own (#353).** The
`ArenaHeldButUnreachable` message recommended a recovery path that failed when
followed verbatim. That was a defect in operator-facing prose independent of how
anything above resolves, and coupling it to an undecided record would have delayed
a fix an operator meets at the worst possible moment. Detail in question 3.

## Rationale

**Why state a property rather than build a mechanism first.** Every mechanism
below is a new surface, and the project's rule is that a question the six API
rules do not answer is a decision record rather than an API choice
([`API.md`](../API.md) §1). But the *property* is not new — it is a consequence
of §3.4's split-brain check and §3.7's join, both true since the rendezvous
shipped, plus a refusal in `attach_shared` older than either. It is currently
derivable only by an operator who reads two spec sections and a runbook section
together and notices what none of the three says about polling. Writing down a
true consequence costs nothing and is not blocked on the choice.

**Why "provisioned" and not "requested".** The alternative framing — that a
process asks for recovery when it needs it — is the one the error message already
implies and the one `CreatePolicy::Always` half-delivers, and it cannot work: the
segment is an unnamed `memfd` ([`PHASE2.md`](../PHASE2.md) §3.6 step 1) handed
over only by a serving owner (§3.7 step 3). There is no name to open, no path to
`stat`, and no third party holding the descriptor. "Ask later" has nowhere to
ask.

**Why the NORMATIVE requirement is recommended despite being the weakest.** It
adds no process, no thread, no flag and no second spelling; it is implementable
by an integrator with one line in a launch file plus a call in one node's
existing loop; and it states a fact rather than promising behaviour the library
cannot deliver. Its cost is real and named in *Consequences*: a NORMATIVE line
that nothing checks is the exact shape of claim this repository has repeatedly
found going stale.

**Why not simply make read-only survivors eligible.** Because it is D18, which
is a decision in [`PROJECT.md`](../PROJECT.md) **§5** — *"Read-only attach is the
default for consumers"* — and which §6's design-smell list then enforces from the
other side (*"Defaulting a consumer to read-write attach (D18)"*). An owner
writes the participant table on every grant; a `PROT_READ` mapping cannot; and
**D18's own §5 entry** is where the MMU boundary is described as the only real
security boundary the design has. The wedge is a cost of that boundary, not an
argument against it.

## Consequences

- **A NORMATIVE line nothing gates.** If question 1 resolves to the requirement,
  the project acquires a normative statement with no check behind it. That is
  worth stating in advance, because it is a failure mode this repository has
  recorded: a status row or a normative claim outliving what it describes. The
  honest mitigation is smaller than "half": of the three things a candidate must
  be, `tf_tree participants`' `mode` column answers only **read-write**.
  *Attached at the vacancy instant* is observable in principle and by nobody
  after the fact, and *polling* is not observable at all (D17 —
  [`RUNBOOK.md`](../RUNBOOK.md): "it cannot show you who is looking"). One of
  three, not one of two, and the record should say so where it makes the claim.
- **[`RUNBOOK.md`](../RUNBOOK.md)'s existing remedy has to be *corrected*, not
  just extended.** *"Open one process read-write even if it never publishes"* is
  necessary and not sufficient: a read-write process that never polls
  `owner_lost()` satisfies the sentence and is not a candidate. An operator who
  followed that advice literally and still wedged has been told the wrong thing,
  which is why this is a record and not a doc patch — the change is to normative
  operator guidance, and **nothing in this record edits `RUNBOOK.md`**.
- **`shm_torture` is a harness change, not an engine change.** If the harness is
  to keep testing §3.5 rather than tripping over its precondition, it needs a
  candidate whose attachment brackets every owner kill. Its `--no-inherit`
  negative control already shows the harness can tell "nothing inherited" from
  "inheritance is broken"; what it lacks is a member of the candidate set whose
  lifetime spans the vacancy. **This record does not authorise that change** and
  is not a prerequisite for it.
- **Operator-facing text disagrees with itself in two independent places.** The
  `ArenaHeldButUnreachable` message contradicts [`RUNBOOK.md`](../RUNBOOK.md)'s
  snippet about the layout (question 3, fixable now, whichever way the rest
  resolves), and the runbook's owner-death remedy contradicts its own lead-in
  about polling (step 1, which is a correction and therefore this record's to
  propose, not to make).
- **Nothing here reopens anything `0009` cut.** Covariance, copy-on-write
  branches, multi-parent edges and URDF in the engine are untouched by every
  option considered; question 2 says which options do reopen a settled decision,
  and none of them is on that list.

## Implementation plan

**What would be done if this became `ready`.** Steps 1 and 3 are the decided
half; step 2 is recommended to land on its own, ahead of this record; step 5 is
deliberately unplanned.

1. **State the property. DONE, 2026-09-18.** In [`PHASE2.md`](../PHASE2.md) §3.5 and in
   [`RUNBOOK.md`](../RUNBOOK.md)'s *The arena's owner died* and
   *`ArenaHeldButUnreachable`*, on the **eligibility** axis — the three ways to
   be ineligible, and the instant-versus-census sharpening. The runbook edit is
   a correction, not an addition: its *"open one process read-write even if it
   never publishes"* is necessary and not sufficient as written, and the
   sentence has to gain the polling half. — verified by `just artifact-versions`
   (relative links, table rows) and by reading the three texts against each
   other, so the runbook's remedy and the spec's normative paragraph say the
   same thing in the same terms.
2. **The message defect. DONE, 2026-09-18 — #353.** On its own branch and not
   gated on this record: the two `ArenaHeldButUnreachable` arms in
   `crates/tf_tree_ipc/src/error.rs` that name `CreatePolicy::Always` also name
   the layout the hatch requires.

   **It went past this step in two places, and the second is why part 4 exists.**
   The arm also discarded `ownership_held`, so it promised that stopping slot 0
   was sufficient when the ownership byte was held elsewhere; and the first repair
   of *that* asserted two holders, which is false in the steady state of every
   healthy single-owner arena, where one process holds both bytes. Three wrong
   statements out of one arm, each a hedge on an inference `Display` cannot
   support — which is the measurement *Decision* part 4 rests on.

   **Of the two arms this step names, one already carried the clause when the
   step was written.** [#310](https://github.com/NoeFontana/tf_tree/pull/310)
   landed it in the `(Some(slot), false)` arm — §3.4's stranded-participant
   case, the arm question 3 quotes — on 2026-09-10, and this record was drafted
   the same day against a tree that already had it. What remained was the second
   arm question 3 names, the `next` hint on `(Some(0), _)`, which forwards an
   operator to `CreatePolicy::Always` as the escape hatch "provided nothing still
   holds the ownership byte" and stopped there; a reader who reached the hatch
   through *that* sentence still met a recovery path that fails when followed
   verbatim. **Both arms now carry the clause, in the same terms**, which is the
   property this step was for. Question 3's genuinely open half — whether the
   message should lead with the abandoning path at all, when `RUNBOOK.md` says to
   reach for inheritance first — is untouched and stays open. — verified by
   **extending** `the_escape_hatch_creates_over_a_stranded_participant`
   (`crates/tf_tree/tests/rendezvous.rs`, by name — the line number this step
   carried matched neither the doc comment nor the `fn` when it was written, and
   an edit to the test moves it again), not by a new test: that case
   already reaches the stranded state, already passes
   `.layout_if_creating(layout())` and already asserts `CreatePolicy::Always`
   creates. The only half that is new is the negative one — the *verbatim*
   reading of the message, `CreatePolicy::Always` with **no** builder, which must
   return `OpenError::NoLayoutToCreate`. That assertion is what stops the two
   operator-facing texts drifting again.
3. **Pin the eligibility half, and only that half. DONE, 2026-09-18**, and it
   **overran that charter by one test** — the two-holder conjunction below pins a
   `Display` arm, not eligibility. It landed here because the promotion round
   found that state unreached while step 3 was the step being written; step 6
   names it and owes its rewrite, and the
   open choice below was resolved to the **read-write survivor that never polls**
   — the case this record says nothing documents. `scenario_3`'s existing
   `join-sweep` child already *is* that survivor (read-write, `CreatePolicy::Never`,
   never calls `owner_lost()`), so the conjunction needed no new harness: the same
   `open-uuid` invocation is refused while it holds its byte and succeeds once it
   exits, which is the positive control. **The ordering turned out to be the
   assertion** — a first revision ran the refusal after the sweeper had exited and
   got a fresh arena back. The two-holder conjunction the promotion round found
   unpinned is `byte_0_and_ownership_held_by_two_different_holders_is_refused_without_naming_a_topology`,
   which stages both holders through `tf_tree_ipc::LockFile` on two descriptions;
   it asserts the *hedge*, because `Display` cannot tell that state from a single
   owner holding both bytes and has to be true of each. The mechanical sequence —
   refused, refused again, and creating once the last holder exits — is
   `a_live_participant_prevents_a_second_arena`
   (`crates/tf_tree_ipc/tests/multiprocess.rs:230`) already, 128 iterations and
   a positive control, so writing it again in `crates/tf_tree/tests/rendezvous.rs`
   would be a **second spelling of an existing path**
   ([`PROJECT.md`](../PROJECT.md) §6) and this step must not. What is unpinned is
   the conjunction: extend
   `scenario_3_an_owner_dying_leaves_readers_working_and_joins_refused` so the
   survivor is one condition (b) disqualifies — attached, holding a byte, and not
   an eligible heir — and assert that the refusal outlives the instant while that
   survivor lives and clears when it exits, citing the byte-level test rather
   than duplicating it. **Whether the ineligible survivor should be the read-only
   one (which `a_read_only_survivor_reports_that_it_cannot_inherit` already
   builds) or a read-write one that never polls (which nothing builds, and which
   is the case the record says is undocumented) is an open choice, not a settled
   one** — the second is the more valuable and the harder to write without
   asserting a negative about a loop. — verified under `just shm-check`
   (`cargo nextest run -p tf_tree --features shm,unstable,crash-points --test
   rendezvous`).
4. **[`PHASE2.md`](../PHASE2.md) §0.0's *Ownership migration (§3.5)* row**
   records the provisioning precondition. **DONE, 2026-09-18**, and it cites
   *"settled by `0055`"* — the checked spelling, which is what makes it gated
   rather than merely written: `just artifact-versions`' settled-citation count
   went 41 → 42 on that row alone. — this step **may not land while this
   record is `draft`**: a §0.0 row resting on a draft is precisely what
   `just artifact-versions`' decision-citation check exists to catch, and three
   records were cited that way at fourteen sites before it did. **That bar is
   cleared as of 2026-09-18**, and the status half is what the check reads: it
   parses each record's own `**Status:**` line and fails only on `draft`.
   **It does not read every citation, so the row has to use a spelling it
   reads.** `DECISION_SETTLED_VERB` matches a settled *verb* before the link —
   *declined by*, *superseded by*, *governed by* — and the script's own *What
   this does NOT prove* section lists the forms it cannot see, bare adjacency
   (`0055`'s own `(0048; the register is …)` shape) among them. A §0.0 row that
   merely mentions this record beside the precondition is invisible to it and
   held by review alone, which is the same standing the three miscited records
   had. So step 4's row cites in the checked form or it is not gated.
5. ~~**The mechanism** — whichever of question 1's candidates survives review.
   Deliberately not broken into steps here: planning it would be choosing it,
   and this record does not have the standing to choose.~~ **Struck: question 1
   is answered "no mechanism" (*Decision* part 2), so there is nothing to plan.**
   The record now has the standing, and used it to decline. What would bring this
   step back is named with the answer.
6. **Reduce what the `ArenaHeldButUnreachable` remedy claims. DONE,
   2026-09-18** (*Decision* part 4, and this record's own question 3).
   **793 bytes non-ASCII → 143 bytes ASCII**, 164 at the widest ids — *this
   read 152 until step 7, and 152 was what `samples()` measured rather than what
   the arm can render: it swept the mask and the slot to their maxima and left
   `first_pid` at 4242, six digits short, and never combined a wide mask with
   slot 0, whose `, the creator's` costs more than a wide slot's nine digits.
   The sweep and the figure are both corrected, and the correction is the
   step-6 lesson arriving one level up: a number is only as measured as the
   sample that produced it* — and the
   remedy is `docs/RUNBOOK.md`'s eight-row table, indexed by the facts the
   message still prints — the mask, the lowest slot and its pid, and the
   ownership byte — and placed where the search key lands a reader rather than
   165 lines into a bullet about #201.

   The negative rule is what makes the old defect unexpressible rather than
   merely fixed: no arm may state a count of holders or a procedure, asserted
   over **every state the four arms can render** by
   `every_unreachable_state_reports_the_facts_and_prescribes_nothing`. **That
   rule took two attempts** — a first version forbade the word "process" and
   failed on the empty-mask arm's "a process that never served", which is a fact
   (the byte was held throughout and nothing answered), not a count. **And a
   first version of the state list was not every state**: it omitted
   `(0b1000, Some(3), true)` and both `first_slot: None` states with a non-empty
   mask, and a procedure added to that arm passed both gates.

   The messages end with `(ArenaHeldButUnreachable)` — convention (g)'s
   parenthesised form, as `tf_tree_arena`'s `check.rs` and `frozen.rs` already
   spell it; a first version ended `: ArenaHeldButUnreachable`, a second
   spelling of a convention living in two other crates. **Convention (e), at
   most 120 bytes, is not met and is no longer claimed**: these arms are **116 to
   164** bytes, the upper end being the widest the fields can render, because (e) was derived for an arena error nested in two wrappers
   carrying an errno, and this one carries a 64-bit mask and two 32-bit ids.

   **Five tests needed rewriting, not the three this step predicted**: the two
   it named, plus
   `a_held_ownership_byte_refuses_the_hatch_and_freeing_it_lets_one_through`,
   `a_live_byte_0_refuses_both_policies` — renamed, because it no longer says
   what no force can pass, since the message does not — and the unit test
   itself, renamed from `…remedy_names_what_the_operator_must_supply`.
   - **Not a deletion of the prose, a reduction of what it claims.**
     [`API.md`](../API.md) R5 keeps the prose in the message layer, and nothing
     enters the error type: the variant stays `Copy`, its four fields unchanged.
   - **And the measurement this step was missing, which makes it a defect fix
     rather than a tidying.** `tft_tree_open_named`'s failure arm formats
     `could not open the arena: {e}` into `tft_error::message`, which is
     `TFT_MESSAGE_LEN = 256` and truncated by `set_message` at 255 bytes (pinned), with a
     `?` substituted per non-ASCII **byte**. The `(0b1, true)` remedy as landed
     is **793 bytes** at a four-digit pid, 819 with that prefix: a C operator
     reads to `"… can pass this. Stop"`, the whole remedy is gone, and the
     em-dashes render `???`. (Earlier revisions of this bullet said 790/816 and
     `"Stop th"` — the same state at a one-digit pid — and 788, a different
     state. The figure moves with the pid and the mask, which is part of why
     these numbers now live in one place each.) So the reduction is what makes
     this arm legible to C at all, and
     [`0059`](./0059-the-arena-errors-that-cannot-describe-themselves.md)'s
     convention (g) — ending with the variant name
     as the runbook's search key — are the shape to reduce it to, for the reason
     that record already gives. **A test asserting the rendered message fits
     `TFT_MESSAGE_LEN` belongs to this step**; none exists for `IpcError`
     today, which is why the overrun shipped. This is also the measured
     companion to [`0020`](./0020-the-consumer-side-of-the-arena-refusal.md):
     that record is about C not being told which cause it hit, and this is C
     being told and unable to read the answer.
   - **Verified by** the mutants #353 established, re-run against the reduced
     text, plus `just lint` (whose `artifact-versions` arm holds the runbook's
     table rows and links) and the runbook and spec read against each other as
     step 1 requires.
   - **Stop point:** if the reduced message leaves an operator with less than
     [`RUNBOOK.md`](../RUNBOOK.md)'s own `ArenaHeldButUnreachable` section gives
     them, this step has traded one incomplete text for two and should not land.
     (That section, and not a "§3.4" of the runbook — the runbook has no numbered
     sections, and every `§3.4` in it points at [`PHASE2.md`](../PHASE2.md).)
     **Not triggered:** that section gained the remedy the message gave up, in
     more detail than the message could carry, and the message's search key is
     how a reader gets to it.
7. **The same reduction for `HandshakeRejected`. DONE, 2026-09-18.** Step 6's
   gate measured what it was not chartered to fix: seven per-status remedies
   rendering **112 to 378 bytes**, of which **four truncated** in
   `tft_error::message` at the 26-byte wrapper, and six of seven were over this
   crate's own 220-byte budget. They are now **108 to 124 bytes** (133 at the
   widest owner numbers), ASCII, and every one of them fits: the message states
   the status, the owner's two numbers — which §3.7 requires a rejection to name
   — and `(HandshakeRejected)`, and `rejection_advice` is deleted. Its seven
   remedies are `docs/RUNBOOK.md`'s `HandshakeRejected` section, placed beside
   the header-validation checks that share two of their names, because that
   confusion is the one an operator actually makes.

   **The negative rule, as in step 6:** no rendering may name a status it did
   not get. That is the first defect this arm ever had — one `LayoutMismatch`
   sentence appended to every rejection, printed thousands of times over a
   `NoParticipantSlots` refusal — and the per-status repair that followed is
   what put 378 bytes in a 256-byte buffer. Both are refused by
   `every_rejection_names_only_the_status_it_carries`, which also holds the arm
   to a 140-byte budget of its own: prose returning here fails long before it
   reaches the 220 the C path allows.

   **A row a reader cannot reach is the failure this shape invites**, so the
   runbook is gated too — `crates/tf_tree_cli/tests/runbook.rs`, which reads
   `docs/RUNBOOK.md` with `include_str!` and requires a table **row** per
   refusal status (a `contains` over the section is satisfied for two of the six
   by the prose that tells them apart from the header checks sharing their
   names), requires those rows to carry the remedy words the message is
   forbidden to carry — the two halves keep each other honest — and requires the
   section's worked example to be `Display`'s own output rather than a
   transcription of it.

   **It lives in `tf_tree_cli` and not beside the type, and that is not a
   preference.** `tf_tree_ipc` is published, and `cargo package` does not put a
   file from outside the package directory into the tarball
   (`cargo package --list -p tf_tree_ipc` carries no `docs/`), so an
   `include_str!("../../../docs/RUNBOOK.md")` there ships a crate whose tests
   cannot build. `crates/tf_tree_cli/src/lib.rs` writes that rule down for the
   README and `checks.rs`'s `docs/API.md` gate is the precedent. The first cut
   of this step put it in `tf_tree_ipc`.

   **And what tells the next author a row is owed is a compile error, not a
   tripwire.** `rejection_advice` was the total `match` a new `HelloStatus` used
   to break; deleting it removed that prompt, and `HelloStatus::from_u32`'s
   catch-all arm absorbs a new variant without complaint — so a first cut of
   this step asserted that wire value 7 still decodes to `Malformed`, which
   fires only if the codec is updated too. `status_is_a_refusal` replaces it: a
   total `match` kept for no other purpose, whose author is standing in front of
   the list, the table and the test. Measured: adding a variant fails
   `cargo check -p tf_tree_ipc --all-targets`. **`--all-targets` is not
   decoration in that sentence** — the prompt is in `#[cfg(test)]`, so a plain
   `cargo check` still passes, and the two reader-facing copies of this claim
   said "fails to compile" without the qualifier until review round 2 measured
   it. `just build` and `just lint` both pass the flag, so the gate holds.

   **Round 2 found three more gates that were narrower than their own labels**,
   and they are recorded because each is the same shape as the defect this step
   is about — a check whose name promises more than it asserts. The runbook's
   remedy words were required *of the section* and not of the row, so blanking a
   remedy cell passed with the row still present; a row's *what to do* cell now
   has a floor, and blanking or stubbing one fails. The section's end bound
   stopped only at `###`, so a `HandshakeRejected` section that ever became the
   last one under its chapter would have swallowed the rest of the runbook and
   every `contains` in that test would have held vacuously. And
   `every_unreachable_state_reports_the_facts_and_prescribes_nothing` still
   carried the very defect the erratum above repudiates: a state labelled
   *widest ids* whose `first_pid` was 4242, under an assertion that two
   `contains` satisfied with one wide field. Each id is now checked on its own,
   and the widest state — slot 0, whose `, the creator's` outweighs a wide
   slot's digits — is swept beside it.

   - **Verified by** the step-6 gate with the exception and
     `HANDSHAKE_REJECTED_LENGTHS` both removed, so the variant is measured with
     every other one, plus **nine mutants, nine caught**: advice re-appended,
     the search key dropped, the owner's numbers dropped, 100 bytes of prose
     carrying none of the forbidden words, a runbook row deleted, the runbook's
     remedies emptied, the runbook section renamed away, a status added at wire
     value 7, and a named status folded onto another.
   - **Found and not fixed, because it is outside this step:** there are **two
     `boot_id` parsers** — `tf_tree_ipc::procstat::boot_id`, which rejects a
     UUID with trailing junk, and `tf_tree::tree::boot_id`, which ignores it —
     and they disagree on a malformed `/proc/sys/kernel/random/boot_id`, which
     is one of the ways `BootIdMismatch` becomes reachable. The runbook row says
     what an operator should check; a second spelling of a parser is a separate
     record (`PROJECT.md` §6).
   - **Corrected while writing the row, rather than copied:** the deleted
     `BootIdMismatch` advice said the arena "outlived a reboot ... nothing in it
     is alive and it should be removed". **A serving owner is proof it did not**,
     and the segment would not have survived one;
     [`wire.rs`](../../crates/tf_tree_ipc/src/wire.rs)'s own doc on
     `HelloResponse` says the two peers' ids "agree by construction" and that
     the reboot check belongs to the lock-file path. The advice was true of a
     different check with the same name. That is the same category error as
     step 6's, one layer over: prose that describes the *state the author was
     picturing* rather than the one the branch selects.

     **And a second, of the same kind:** `ModeNotPermitted`'s advice told a
     caller to attach read-only because the owner would not let it write.
     **Nothing in this workspace sends that status** — `OwnerServer::check`
     compares version, layout and boot id and nothing else, and the value exists
     because §3.7 lists it. So the advice described a policy the implementation
     does not have. Both errors survived every review this arm has had, because
     a per-status list *looks* like it was derived from the producers and was
     derived from the status names. The row now says what is true: a peer that
     is not this implementation refused the attach, and that is worth reporting.

## Open questions

**All three are answered, in *Decision* parts 2, 3 and 4 (2026-09-18).** They are
kept here in full rather than collapsed into their answers, because the working is
what a reopening needs: question 1's candidate table with its costs, question 2's
three routes with the decision each would have to supersede, and question 3's
measurement. The headings below say where each answer is.

The warning this preamble carried — **two of them can be answered wrongly in a way
that costs a protocol**, so neither should be answered from this document — is why
both expensive answers are *no new surface*, and why question 3's answer changed
after contact with the code (part 4).

### 1. Does the library owe a supported way to hold recovery capacity?

**ANSWERED: no — the NORMATIVE fleet requirement, no mechanism. *Decision* part 2.**

Three candidates, with costs rather than a ranking:

| Candidate | What it buys | What it costs |
|---|---|---|
| **NORMATIVE fleet requirement** — "a fleet that intends to survive owner death keeps at least one read-write attachment alive continuously *and* polls `owner_lost()` on some cadence" | no new surface, no process, no thread; says a true thing where an integrator meets it | nothing enforces it; it is advice wearing a normative label, and half of it — does anyone *call*? — is unobservable from outside the calling process |
| **A standby-heir helper** | an integrator gets capacity by running one thing | **it polls, so it is a thread or a process.** [`PHASE2.md`](../PHASE2.md) §3.5 is NORMATIVE that there is "no background thread, no daemon, no watcher a user must run", and `0019` holds that every process a user is *required* to run is a place adoption dies (`0019:330`), and [`PHASE2.md`](../PHASE2.md) §3.5 is where that is carried over to a thread — "a thread per attachment is the library-shaped version of the same cost" (`docs/PHASE2.md:590`; the same sentence is in `crates/tf_tree/src/open.rs:565` and, as *"of that cost"*, in [`0037`](./0037-a-takeover-is-not-a-second-open.md)). The quoted clause is **not** in `0019`, and an earlier revision of this row attributed it there |
| **An opt-in `Open` policy** (a `standby`-shaped knob) | it sits beside `mode` and `create`, where an integrator is already deciding | it is a promise about *behaviour*, and `Open` returns a `Tree` whose polling the library does not drive. Either it spawns a thread — which is the row above wearing a builder's clothes — or it records an intent nothing acts on |

**The tension with `0019` and D16 must be stated plainly, and it is narrower
than it first looks.** An *optional* helper does not violate the letter of "no
watcher a user must run" — nobody is required to run it. But then the fleets
that wedge are exactly the fleets that did not opt in, so an optional helper
**names** the gap rather than closing it. That is the real question behind this
one: is a mechanism that only helps the operators who already knew about the
problem worth a new surface?

**And a helper must not be a second spelling.** There is already a specified,
unbuilt process whose job is to hold an arena open and not publish:
`tf_tree serve` (`0019` §1, into which [`PHASE2.md`](../PHASE2.md) §9's daemon
was consolidated). A separate standby-heir binary or subcommand beside it would
be two answers to one question. If a helper is built, the presumption is that it
is part of `serve`. Stated honestly: `serve` as specified is the arena's
**creator**, not a standby attaching to somebody else's arena, so extending it
is a scope change to an unbuilt capability and needs its own argument — it is
not a free consequence of this record.

### 2. Should anything ever let a fresh process adopt an ownerless arena?

**ANSWERED: no, refused with each route's superseding cost named. *Decision* part 3.**

Today the answer is structurally no, and every way to change it is expensive.
Recorded here so the next person costs them before proposing one.

- **A named segment** (`shm_open`, or linking the `memfd` into the filesystem).
  This is **on [`PROJECT.md`](../PROJECT.md) §6's design-smell list** —
  "Using `shm_open` instead of a sealed `memfd`, or skipping `MADV_DONTFORK`" —
  and §3.9's "no stale segments, ever" is the second stated reason for the
  choice. Proposing it means superseding that entry, not amending §3.6.
- **An fd depot** — a process holding the descriptor and handing it out. That is
  a daemon, so `0019` applies in full; and a depot that outlives the owner is
  the standby heir of question 1 with extra parts and a second copy of the
  handover protocol.
- **Ownership handoff through the lock file.** The lock file "holds no state,
  only locks" (§3.9) and cannot carry a descriptor; `SCM_RIGHTS` needs a socket
  and a socket needs a server, which is what "nothing is serving" means. It also
  runs straight at a deleted decision: §3.4's **step 3 was removed** because it
  let a process declare it already held the arena, "a declaration no new file
  description can verify, since `F_OFD_GETLK` reports conflicts and cannot name
  a holder" — five unsound states are listed in
  [`0035`](./0035-the-creators-slot-is-taken-not-found.md) and
  [`0037`](./0037-a-takeover-is-not-a-second-open.md), and the comment at
  `crates/tf_tree_ipc/src/open.rs` says "**do not re-add the short-circuit**".
  Reading the identity records instead runs at
  [`0033`](./0033-the-identity-record-cannot-name-a-namespace.md).
- **Where [`0018`](./0018-blocking-waits-belong-in-the-shim.md) does and does
  not bite**, stated precisely rather than borrowed: it forbids a blocking wait,
  a futex or any notification primitive **in the arena**. A depot or a handoff
  socket is not arena state, so `0018` does not by itself forbid one — what does
  is `0019` and §3.10's trust model. A proposal that put a waiter *in the arena*
  so a joiner could block until an heir appears is squarely inside `0018` and is
  refused on that ground.
- **Nothing in this question reintroduces anything `0009` cut.** Its list is
  covariance, copy-on-write branches, multi-parent edges and URDF in the engine;
  none of the options above touches any of them. Recorded so the question is not
  refused for the wrong reason.

### 3. Is the operator guidance itself wrong? — yes, in one measurable place

**ANSWERED, and not as this section's closing paragraph expected: the message carries facts and one pointer, and the remedy lives in the runbook alone. *Decision* part 4, step 6.** The clause defect this section measured is fixed (#353); what changed is the larger judgement it left open.

`IpcError::ArenaHeldButUnreachable`'s `Display` has an arm for the state where
every held byte is a non-owner's and nothing holds ownership — §3.4's
stranded-participant case — and it ends:

> ... which is the case PHASE2 §3.4's escape hatch is for: CreatePolicy::Always
> will create a fresh arena and abandon this one

**Following that verbatim fails.** It was measured in the 2026-09-10
investigation: an `Open` with `CreatePolicy::Always` and no builder returns
`OpenError::NoLayoutToCreate` — *"no layout was supplied and the arena had to be
created"* (`crates/tf_tree/src/open.rs`, cited by variant name because the line
this carried had already drifted — `:721` is inside `OpenError::Map`/`Build`, and
step 2 stopped citing lines for the same reason) — because decision `0004` sizes an
arena from its declared edges, so a creator must bring a `TreeBuilder`. The
operator who follows the printed advice at 3 a.m. meets a second error.

Three facts make this small and separable rather than a design question:

- **The runbook is already right.** Its `ArenaHeldButUnreachable` snippet
  carries `.layout_if_creating(builder)` with the comment "required: decision
  0004 sizes an arena from its edges". The two operator-facing texts disagree,
  and the wrong one is the one an operator meets first — in a message, without
  the runbook open.
- **The trap is known one layer up, in a doc comment nobody reads at runtime.**
  `crates/tf_tree_cli/src/attach.rs` records that `--create --rw` "still cannot
  create anything and reports `NoLayoutToCreate`".
- **The fix is prose in the message layer**, which is where
  [`API.md`](../API.md) R5 puts it: errors stay `Copy` identifiers, no field is
  added, no `String` enters the type. Two arms carry the omission — the
  stranded-participant arm above, and the `next` hint on the slot-0 arm, which
  also names the hatch without naming the layout.

**Recommendation: its own fix, ahead of this record**, with the verbatim-path
test from step 2. It is wrong today no matter how questions 1 and 2 resolve, and
a defect in the sentence an operator reads while a robot is wedged should not
wait on an architectural review.

**What is not settled here**: whether the message should recommend the hatch at
all. [`RUNBOOK.md`](../RUNBOOK.md) warns that `CreatePolicy::Always` "abandons
the arena rather than recovering it" and leaves survivors publishing into a
segment nobody else can reach — the "two processes see different data" state —
and tells the reader to reach for inheritance first. An error message that leads
with the abandoning path and never mentions the recovering one may be mis-ordered
as well as incomplete. That is a judgement about operator guidance, it is larger
than a missing clause, and it is left open.
