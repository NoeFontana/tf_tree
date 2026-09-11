# 0055: the recovery capacity a fleet cannot add later

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** (none yet)

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

**This is a draft and it authorises nothing.** It decides one thing, recommends
a second, and deliberately leaves the mechanism open — the three questions below
are the record's substance, and answering them from this document rather than
from a review is the failure it is trying to avoid.

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

**2. Recommended, not decided.** Of the three candidate mechanisms in question 1
below, the NORMATIVE fleet requirement is the cheapest and the only one with no
tension against `0019`. It is also the weakest, and this record says so rather
than selling it.

**3. Separable, and recommended to land ahead of this record on its own.** The
`ArenaHeldButUnreachable` message recommends a recovery path that fails when
followed verbatim. That is a defect in operator-facing prose today, independent
of how anything above resolves, and coupling it to an undecided record delays a
fix an operator meets at the worst possible moment. Detail in question 3.

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

1. **State the property** in [`PHASE2.md`](../PHASE2.md) §3.5 and in
   [`RUNBOOK.md`](../RUNBOOK.md)'s *The arena's owner died* and
   *`ArenaHeldButUnreachable`*, on the **eligibility** axis — the three ways to
   be ineligible, and the instant-versus-census sharpening. The runbook edit is
   a correction, not an addition: its *"open one process read-write even if it
   never publishes"* is necessary and not sufficient as written, and the
   sentence has to gain the polling half. — verified by `just artifact-versions`
   (relative links, table rows) and by reading the three texts against each
   other, so the runbook's remedy and the spec's normative paragraph say the
   same thing in the same terms.
2. **The message defect**, on its own branch and not gated on this record: the
   two `ArenaHeldButUnreachable` arms in `crates/tf_tree_ipc/src/error.rs` that
   name `CreatePolicy::Always` also name the layout the hatch requires. —
   verified by **extending** `the_escape_hatch_creates_over_a_stranded_participant`
   (`crates/tf_tree/tests/rendezvous.rs:506`), not by a new test: that case
   already reaches the stranded state, already passes
   `.layout_if_creating(layout())` and already asserts `CreatePolicy::Always`
   creates. The only half that is new is the negative one — the *verbatim*
   reading of the message, `CreatePolicy::Always` with **no** builder, which must
   return `OpenError::NoLayoutToCreate`. That assertion is what stops the two
   operator-facing texts drifting again.
3. **Pin the eligibility half, and only that half.** The mechanical sequence —
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
   records the provisioning precondition. — **this step may not land while this
   record is `draft`**: a §0.0 row resting on a draft is precisely what
   `just artifact-versions`' decision-citation check exists to catch, and three
   records were cited that way at fourteen sites before it did.
5. **The mechanism** — whichever of question 1's candidates survives review.
   Deliberately not broken into steps here: planning it would be choosing it,
   and this record does not have the standing to choose.

## Open questions

All three are open. **Two of them can be answered wrongly in a way that costs a
protocol**, so neither should be answered from this document.

### 1. Does the library owe a supported way to hold recovery capacity?

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

`IpcError::ArenaHeldButUnreachable`'s `Display` has an arm for the state where
every held byte is a non-owner's and nothing holds ownership — §3.4's
stranded-participant case — and it ends:

> ... which is the case PHASE2 §3.4's escape hatch is for: CreatePolicy::Always
> will create a fresh arena and abandon this one

**Following that verbatim fails.** It was measured in the 2026-09-10
investigation: an `Open` with `CreatePolicy::Always` and no builder returns
`OpenError::NoLayoutToCreate` — *"no layout was supplied and the arena had to be
created"* (`crates/tf_tree/src/open.rs:721`) — because decision `0004` sizes an
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
