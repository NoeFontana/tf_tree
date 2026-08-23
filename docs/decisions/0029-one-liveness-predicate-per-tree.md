# 0029: one liveness predicate per tree

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** none — this record exists so that the adapter does not land
as a one-liner.

## Context

Filed as issue #213. `Tree::reparent` is the only path that takes A2's in-arena
topology lock, and it decides whether the current holder is alive from the
`(pid, start_time, boot_id)` triple — **even on a rendezvous tree that is holding
an `F_OFD_GETLK` probe**. `docs/PHASE2.md` §5.1 is NORMATIVE and says liveness is
the lock byte; §0.0 records this as the third of three places where the triple is
still on a correctness path (#205, out of #194).

The mechanical change is small. `crates/tf_tree/src/tree.rs:1735` builds the
predicate from the free function `participant_is_alive` (`:3007`) and hands it to
`TopoLockView::acquire` (`:1736`); `self.liveness`, where the probe lives, is
consumed at exactly two places and this is not one of them.

**It is filed as a record rather than a PR because the change is not the
adapter.** A liveness verdict here decides when a topology lock becomes
*stealable*, which is a mutation protocol, so D15 applies and it owes a §11.3
walk. And two things this repository learned in the last three days bear on it
directly, neither of which is written down anywhere a person changing that line
would look.

## The facade has five answers to "is slot *s* running?", and they disagree

Enumerated from the code rather than from the spec, because §0.0's row names
three and there are five.

| # | site | the fact it consults | which trees |
|---|---|---|---|
| P1 | `liveness_for` (`tree.rs:2999`) | `record_is_alive` (`:2952`) — the `/proc` triple, entire. Plus a boot-id arm that returns **`false` for every slot** when the arena's boot id differs from the host's | every tree with no probe: heap, `TreeBuilder::build_shared` called directly, `Tree::attach_shared` |
| P2 | `use_ofd_liveness` (`tree.rs:2413`) | the OFD byte, with an explicit "never report ourselves dead" guard, falling back to `record_is_alive` when `F_OFD_GETLK` declines to answer | both arms of `Open::attempt`, and nothing else |
| P3 | `Tree::participant_alive` (`tree.rs:2564`) | the state word `Acquire`, **then** `self.liveness` — the order is right, and it is right by `&&`'s short-circuit rather than by statement | whichever of P1/P2 the tree carries |
| P4 | `Tree::reparent` (`tree.rs:1735`) | `participant_is_alive`, i.e. the triple, **never** `self.liveness` | every tree, including those that paid for a probe. **This is #213** |
| P5 | the owner server's socket-hangup callback (`crates/tf_tree/src/open.rs:1360–1462`, mutating at `:1446–1458`) | neither the byte nor the triple — only the socket (D17) | the owner's serving thread |

**P5 is the one that matters most and is missing from every existing
enumeration**, including §0.0's row and this record's first draft of this table.
It is the *only* facade path that **mutates** the participant table on a liveness
verdict — `rec.state.load(Acquire)` at `:1449`, then
`table.reclaim(slot, observed)` at `:1456`, driving the word to `FREE`.
Everything else merely reports. **This row cited `identity(slot)` at `:763` and
`release(slot, incarnation)` at `:764` until `0028`'s plan step 4 rebased the
callback onto the observed word**, and the numbers had rotted twice over besides;
the substance is unchanged, because the guard is the same comparison for a
`live_word` — `live_word` packs the incarnation into the word — so a wrong
verdict is still a spurious free rather than a second occupant. For the
`RESERVED` word the callback now also collects, that bound comes from the lock
byte instead (`0028` open question 6, and `ParticipantTable::reclaim`'s doc
comment). What does not change is the point of the row: any statement of the form
"the facade decides liveness from X" has to account for a path that decides it
from neither X nor Y. **A sixth predicate must not be added here without reading
that step**, because this path now shares `reclaim` with the slot assigner, which
*does* decide from the byte.

Downstream, two shipped consumers already override P1/P2 rather than trusting
them, which is worth knowing before adding a sixth:

- `tf_tree top` (`crates/tf_tree_cli/src/top.rs:466`) assigns `existing.alive =
  *held` from the lock-file row, commented *"The kernel's answer wins over the
  arena record's"*. So `top` already reports a byte-less live participant dead.
- `tf_tree doctor` (`crates/tf_tree_cli/src/doctor.rs:497`) records
  `alive: alive || before != after`, bracketing the probe with two `Acquire`
  reads of the state word, so any occupancy change forces *alive*. Strictly more
  fail-safe than P3.

## The ordering constraint, which is new and which the adapter can break

Under `loom`, a predicate that composes an arena word with a lock-byte probe
**must observe the word first**. Reverse the two reads and a published record is
erased in 0.00 s: the reclaimer probes byte *s* free before a registrant takes
it, then observes `live_word(1)`, and CASes a live byte-holder's record to `FREE`.

The mechanism, not just the observation: under word-then-byte the `Acquire` load
of a `live_word` **synchronises-with** the publishing `Release` store, so a byte
probe sequenced after it must see the byte held. Two independently built models
reached this separately; it is recorded in `0028` open question 6.

P3 has the safe order. **An adapter for P4 need not**, and the natural way to
write one does not: `TopoLockView::acquire` wants `Fn(u32) -> bool` while
`BoxedLiveness` is `(u32, &ParticipantRecord)`, so the adapter has to fetch the
record — and `ParticipantTable::get` (`participant.rs:192`) performs no atomic
load at all, so *when* the word is read becomes the adapter author's choice
rather than the type's. Worse at the sweep scale: `LockFile::held_participants()`
returns all 64 bytes in one call, so anyone writing this from the mask takes
every probe before reading any word — the failing order, by construction.

## `just loom` gives this change zero coverage, and its own models say why

The ordering constraint above is a `loom` result, so be exact about what `loom`
does *not* cover. `crates/tf_tree_core/src/loom_tests.rs` is the only file in the
workspace containing `loom::model`; A2's lock appears there as `TopoModel`, a
documented reimplementation whose `acquire` takes the predicate as a parameter,
"injected exactly as in the real code".

**Every predicate any model passes is correct by construction.** All four sites:

| site | predicate | what it models |
|---|---|---|
| `loom_tests.rs:525` | `\|_\| true` | nothing is ever stolen |
| `:566` | `\|_\| true` | nothing is ever stolen |
| `:645` | `\|_\| true` | the corpse taking the lock, before it dies |
| `:661` | `\|slot\| slot != DEAD_SLOT` | the thief, exactly right about a corpse |

So the model's liveness is an **oracle**, and A2's exclusion is a theorem of the
oracle rather than of the code.

**Measured, and the obvious experiment does not work — which is the sharper
result.** Flipping `:566`'s predicate from `|_| true` to `|_| false`, so the
model is maximally wrong about two live holders, leaves
`two_mutators_race_the_lock_and_a_reader_sees_no_mix` **green** (6.42 s). It is
not that loom explored and found nothing. That model never reaches the code under
test: `acquire` only considers stealing after `MODEL_SPIN_LIMIT` (= 3) CAS
attempts fail, and there the lock is always released inside that budget. A
`panic!` planted in the steal branch proves it directly:

```
test loom_tests::a_dead_lock_holder_is_stolen_from_and_leaves_no_trace ... FAILED
        panicked at loom_tests.rs:458: PROBE: steal branch reached
test loom_tests::two_mutators_race_the_lock_and_a_reader_sees_no_mix ... ok
```

**One model steals, and its victim is inert by construction.** In
`a_dead_lock_holder_is_stolen_from_and_leaves_no_trace` the holder is
`core::mem::forget`ed — "the crash: no release, no `Drop`" — so it executes no
further instruction, ever. There is therefore no live holder anywhere in the
suite for a wrong predicate to steal *from*, which is exactly the failure piece 1
would introduce.

That test says as much itself: its first draft ran the dying participant as a
loom thread and the authors removed it, because *"it is **not this one**: it is
the false-negative case, where liveness wrongly declares a live-but-stalled
participant dead and it later resumes"*. **That is the class piece 1 enters** —
excluded from the model on the strength of a fail-safety this change gives up.

**So `just loom` stays green through this change whatever it does**, and the
§11.3 walk plus a real multiprocess test are the entire gate. Closing the gap
means a model with a live holder and a fallible predicate, which is a new model
rather than a new assertion.

## Decision — proposed, not taken

**`Tree::reparent` uses `self.liveness`, and `participant_is_alive` is deleted
rather than left as a second spelling — but not until
[`0031`](./0031-the-participant-record-with-no-byte.md) has decided what a `LIVE`
record with no lock byte means.** Three pieces:

1. **An adapter that reads the word first, and that is *not* P3.** It takes the
   slot, loads `rec.state` with `Acquire`, returns `false` if the word is not
   `LIVE`, and only then consults a liveness source. That much is P3's body and
   is not negotiable — it is the one place the word-before-byte order is stated.

   **An earlier revision of this piece said the adapter "should *be* P3 rather
   than resemble it". Written and run, that is a regression in the corrupting
   direction** (#213, 2026-08-22). P3 is `Tree::participant_alive`; its liveness
   source on a rendezvous tree is `use_ofd_liveness`'s closure, which ends
   `probe.is_held(slot).unwrap_or_else(|| record_is_alive(rec))`. For `0031`'s
   byte-less class — a directly-called `TreeBuilder::build_shared` creator holds
   a `LIVE` record and no byte at all — `is_held` returns `Some(false)`, so P3
   answers **dead about a live, still-publishing process**. Not a prediction:
   `a_byteless_creators_record_reads_dead_and_is_reaped_while_it_publishes`
   asserts exactly that and passes at `HEAD`. Today `reparent` refuses such a
   holder with `Err(LockContended { owner_slot: 0 })` and is *right*, by accident
   of using the weaker predicate; an adapter that **is** P3 turns that refusal
   into `Ok(())` and **steals A2's topology lock from a live mutator** — the
   direction §6.2 forbids and the direction this record exists to prevent.

   **It is a design question, not an adapter detail, because the probe API
   collapses the two cases.** `LivenessProbe::is_held` is
   `self.lock.probe_participant(slot).ok().map(|p| p.held)`, so "byte released by
   a dead holder" and "holder never had a byte" are the same `Some(false)`. No
   adapter can separate them, and no in-arena discriminator may be added for it —
   `FORMAT_VERSION = 3` already happened.
2. **`participant_is_alive` is removed.** `grep` gives it three occurrences —
   the call at `tree.rs:1735`, a doc-link at `:2939`, and the definition at
   `:3007` — so no public item changes and no signature moves. What changes is
   `reparent`'s semantics, which belongs in `CHANGELOG.md` as a behaviour entry,
   not as a breaking-API entry.
3. **A §11.3 walk, because a topology lock is a mutation protocol.** §11.3 has
   **fourteen** rows — `docs/PHASE2.md:868-881`, the table data rows strictly
   between §11.3's heading and §11.4's. This record said "exactly eleven rows
   (`docs/PHASE2.md:865-876`)": eleven was the count before
   [`0028`](./0028-the-slot-a-killed-participant-keeps.md)'s plan steps 3 and 4
   added the `hangup.*`/`reclaim.*` rows, so the citation contradicted the
   document it cites, inside the one unmet gate item. Bound the window by the two
   headings when recounting — §11.2 above and §11.4 below both carry prose that
   reads like table rows, and a wider window gives sixteen.

   The row this obviously touches is `topo.holding_lock` — *"lock stuck → stealable after liveness check (A2)"*.
   Its holder classes have to be re-walked with P5 in them: a holder whose slot
   the owner already released on `HUP` reads dead under P4 today via
   `identity() -> None`, and would read dead under the adapter too, by a
   different route. That is the same verdict for a different reason, which is
   exactly the kind of coincidence that stops being true later.

**This narrows one exposure and widens another; it removes neither.** A tree that
carries no probe of its own — heap, a directly-called `build_shared`,
`attach_shared` — still has nothing but the triple, so P1 remains its whole
predicate and #205's row stays open. And a *holder* with no byte becomes newly
stealable from, by every observer that does carry a probe. Those are two
different axes — the observer's probe and the subject's byte — and the second is
`0031`'s. See *Consequences*.

### Blocked on `0031`, and on exactly one of its questions

[`0031`](./0031-the-participant-record-with-no-byte.md) is `draft` and
"authorises nothing"; its Decision is **"None yet"**, between two shapes: give
every registration a byte, so a `LIVE` record over a free byte really is dead and
P3 becomes safe for `reparent` unchanged; or restore a second fact for byte-less
records only, which changes P3 *itself*, and then piece 1's adapter must be P3
*as amended*. Piece 1 is unwritable under "either", because the two options give
opposite answers to the only question the adapter asks.

`0031`'s own gate makes its Decision wait on **its question 2** — "is a served
`build_shared` arena a shape this project supports at all?", which it calls a
scope decision rather than an engineering one. That single question blocks this
record. `0031` needs nothing from this record; the dependency runs one way.

## Rationale

**Why not leave it.** §5.1 is NORMATIVE and the current code contradicts it on
the one path where a false "dead" lets a second process mutate topology
concurrently with the first. The predicate has been biased against proving death
since #204, so this is not a live wrong answer on an ordinary host — but the
bias is what makes it survivable, not what makes it right, and `/proc` has two
documented ways to say "dead" about a running process (§0.0's row from #205:
`hidepid=2` without §3.10's same-user rule, and a PID-namespace mismatch, which
is not even `ENOENT`-shaped).

**Why not a one-line adapter in a PR.** Because of the ordering constraint above,
which is three days old, is recorded only inside a draft record's open question,
and is invisible at the call site. A reviewer looking at `is_alive = move |slot|
...` has nothing to check it against. Writing it down is most of the value of
this record.

**Why delete `participant_is_alive` rather than keep both.** `CLAUDE.md` forbids
a second spelling of an existing path. Two functions that answer the same
question from different facts is precisely that, and the drift is already
measurable: `tree.rs:2908-2936`'s seam documentation renders onto
`record_is_alive` while `participant_is_alive` at `:3007` carries no doc at all.

### Alternatives considered

- **Pass `self.liveness` directly, no adapter.** Fails on shape: the closure
  types differ, so something has to fetch the record, and that is where the
  ordering hazard lives. An adapter that exists but is unremarked is worse than
  one the record names.
- **Make `TopoLockView::acquire` take the record-shaped predicate.** Moves the
  problem into `tf_tree_core`, which has no `LivenessProbe` and must not gain
  one — `tf_tree_core` is `libm` + `bytemuck` + `blake3` (D14) and the probe is
  a syscall. Rejected on the dependency budget.
- **Do nothing and document it.** A real option in this project — `PHASE5.md` §8
  is a whole section about not building something. It is rejected here only
  because §0.0 already documents it and the documentation has not stopped the
  code from being wrong; the row has been true and unfixed since #205.

## Consequences

- One predicate per tree, stated in one place, with the ordering in it.
- **The change trades one exposure for another. It is two bullets, and an
  earlier revision wrote only the first.**
- *Narrowed.* `reparent` on a rendezvous tree stops stealing a topology lock from
  a live mutator that `/proc` misreports — `hidepid` against a **non-dumpable**
  participant, a PID-namespace mismatch, or an unreaped **zombie**. This bullet
  used to say bare "`hidepid`", dropping the qualifier the *Rationale* and
  `PHASE2.md` §0.0's #205 row both carry; and the discriminating shape is a
  resolvable entry describing the **wrong** process, not an unreadable one. See
  question 1.
- *Widened.* `reparent` **starts** stealing a topology lock from a live mutator
  that holds no lock byte — `0031`'s class: a directly-called
  `TreeBuilder::build_shared` creator, `LIVE` record, no lock file, still
  publishing. Measured rather than predicted (#213, 2026-08-22): at `HEAD` the
  call is `Err(LockContended { owner_slot: 0 })` and the creator keeps
  publishing; with the adapter it is `Ok(())`. The control that isolates the
  cause is a properly joined participant that *does* hold its byte, under the
  same adapter — refused, `topo_lock.owner` unchanged. **The exposure this
  change removes and the exposure it creates are neither the same size nor the
  same class**, and until `0031` answers, the second is unbounded.
- **The residual class was stated on the wrong axis, and that is the error worth
  naming.** "Trees with no probe" classifies by the *observer*. The hazard is set
  by whether the **holder** has a byte: the probe belongs to the observer and the
  subject needs none. `0031` says it outright, and this repository has already
  let the same conflation reach `PHASE2.md` §0.0 once. Restated correctly: a
  *holder* with no byte is newly stealable from on every tree obtained from
  `tf_tree::Open`.
- `reparent` on a **probe-less** tree is unchanged, so the class §0.0's #205 row
  describes shrinks from three paths to two rather than closing.
- One more caller of `self.liveness`, which is `Box<dyn Fn>` — an indirect call
  on a path that already takes a lock and does I/O-free arena work. `reparent` is
  not a hot path (D3 keeps it off the query path), so this is not a budget
  question.

## Implementation plan

1. **The adapter, as P3's body**, with the ordering stated in a comment naming
   the `loom` result. *Verified by:* a unit test that the adapter returns `false`
   for a non-`LIVE` word without consulting the liveness closure at all —
   i.e. that the word is read first — using a counting fake.
2. **Delete `participant_is_alive`**, repoint the doc-link at `:2939`.
   *Verified by:* `rg participant_is_alive crates/` returning nothing.
3. **The §11.3 walk for `topo.holding_lock`**, with P5's holder class added.
   *Verified by:* the row's text naming which holder classes are stealable and
   why, reviewed against the five predicates above.
4. **A multiprocess test that the two predicates differ on a real tree.** Today
   no test in the tree distinguishes them on this path, which is part of the
   finding. *Verified by:* a `SIGSTOP`ped topology-lock holder — byte held,
   process not scheduled — which the byte calls alive and which `/proc` also
   calls alive, so the *discriminating* case is a holder whose `/proc` entry is
   unreadable; if that cannot be staged without `hidepid`, say so rather than
   claiming coverage.

## Open questions

1. **RESOLVED 2026-08-22: yes — with `-U`. The command in this question is the
   right one missing one flag; the other mechanism it names is real but is
   narrower than this record says.** ~~Can step 4's discriminating test be built
   on an ordinary host? The two predicates agree except where `/proc` lies, and
   the two known ways to make it lie are `hidepid=2` and a PID-namespace
   mismatch. `unshare --fork --pid` was unavailable in the environment §0.0's
   #205 row was written in. If neither can be staged, this record ships a change
   with no test that distinguishes the old behaviour from the new, and that has
   to be stated rather than glossed.~~

   Measured on `dev-box-2026-01` (Ubuntu 24.04, kernel 6.8.0-136, util-linux
   2.39.3, `apparmor_restrict_unprivileged_userns=1`), unprivileged unless a line
   says otherwise. **Not measured on a CI runner**, and every capability below is
   distro policy rather than a kernel guarantee.

   **`unshare --fork --pid` is still refused, so the row's environment note stands
   as written** — `unshare: unshare failed: Operation not permitted`, exit 1. So
   is `unshare -Ur` (`write failed /proc/self/uid_map: Operation not permitted`,
   the AppArmor restriction). **`unshare -U --fork --pid` succeeds, exit 0**, as
   does the bare `unshare(2)` call, so no binary profile is doing the work.
   Creating the user namespace in the same call supplies the capability the
   pid-namespace check wants, and the task keeps its host kuid for permission
   checks: `open` a 0600 lock file `O_RDWR`, `fcntl(F_OFD_SETLK)`, `shm_open` an
   existing 0600 segment, `mmap(MAP_SHARED, RW)`, write through it, `connect()`
   to a 0600 unix socket — all OK.

   **It is a real participant, not a mimic.** `tf_tree_rendezvous_child` built at
   `7739805` with `--features shm,unstable`: an owner and a control joiner in the
   host namespace, a third joiner under
   `unshare -U --fork --kill-child --pid`. The namespaced one completed the whole
   rendezvous — `SCM_RIGHTS`, slot assignment, mapping — and printed
   `joined bfd2ea4f46b8358b:3fd057317fb64a89:…`, byte-identical to the control.
   Then, on that live arena:

   ```
   tf_tree --attach top       ->  slot 2   pid 1   rw   live   yes
   F_OFD_GETLK(byte 16 + 2)   ->  F_WRLCK                      # byte says ALIVE
   tf_tree peer-alive 2       ->  alive true
   read_start_time(1)         ->  Known(12)  vs stored 287088242
   alive_given(...)           ->  false                        # triple says DEAD
   ```

   `/proc/1` is `systemd`. That is #205's predicted **collision**,
   `Known(st) != stored` — not `ENOENT`-shaped, so the bias that makes everything
   else survivable does not fire. **§0.0's "derived from the code and from §3.1's
   own text, not reproduced" can be retired: it is reproduced, against the
   shipped binaries.**

   **`hidepid=2` is narrower than this record says, and not zero.** An earlier
   revision of this answer claimed it discriminates nothing because §3.10 makes
   participants same-user. That is refuted by measurement. Same-user is not
   sufficient: a same-user but **non-dumpable** target is hidden. In a throwaway
   container with its own procfs — a host remount is *not* private, see below —
   two uid-1000 `sleep`s, one under `prctl(PR_SET_DUMPABLE, 0)`:

   ```
   /proc mounted hidepid=invisible
   dumpable target:     pid=13 uid=1000 dumpable=1   -> VISIBLE
   NON-dumpable target: pid=15 uid=1000 dumpable=0   -> HIDDEN
   owner of /proc/13 = 1000 ; owner of /proc/15 = 1000
   remount hidepid=off:  NON-dumpable same-user      -> VISIBLE
   ```

   The mechanism is `has_pid_permissions` falling through to
   `ptrace_may_access`, which fails on `dumpable == 0`; directory ownership is a
   separate effect and is *not* what hides it. So a participant that drops
   privileges without a re-exec, or hardens itself with `PR_SET_DUMPABLE(0)`, is
   hidden from a same-user reader. **§0.0's #205 row and this record's
   *Rationale* are already right** — they say `hidepid=2` "**without** §3.10's
   same-user rule" and head the paragraph "§3.10 is *a* dependency … and it is
   not sufficient". What is wrong is the unqualified phrasing in **this question
   as written above and in the second *Consequences* bullet** ("a live mutator
   that `/proc` cannot see (`hidepid`, PID namespace)"), and
   `proc_answers_here`'s doc comment, which says a hidden entry cannot belong to
   a participant. Dumpability is a **third** dependency, alongside same-user and
   a shared PID namespace, and it is stated nowhere.

   `hidepid` cannot be staged unprivileged here in any case: with full `CapEff`
   inside the userns, every `mount`/`remount` returns `EACCES` from AppArmor, and
   so does mounting a *fresh* procfs at a new mountpoint.

   **A third staging this record never named, needing nothing: an unreaped
   zombie.** `/proc/<pid>` survives with `state=Z` and its start time intact, so
   the triple reads `Known(287080225) == stored` → **alive** while the kernel has
   released the byte → `F_UNLCK` → **dead**. No namespace, no sudo, no container,
   no mount. Two qualifications, both measured. Its **reaping parent must stay
   alive**: with the parent gone the subreaper collected instantly and the
   predicate returned `NoSuchProcess … => triple says DEAD`, and the
   discriminator vanished. And it is **not a case of `/proc` lying** — `state=Z`
   is accurate and `read_start_time`/`alive_given` never read field 3 — so unlike
   the namespace collision it is fixable *inside* the `/proc` predicate. It
   discriminates old code from new code, so it survives as a test; it is much
   weaker as an argument that the byte is authoritative.

   **Pid reuse stays out.** `pid_max` is 4194304 and this host forks ~6600
   processes/second, so a wrap is ~630 s of storm per run and racy besides;
   `/proc/sys/kernel/ns_last_pid` needs `CAP_SYS_ADMIN` over the namespace and is
   refused from the host and from inside the userns. Docker 29.1.3 is present and
   a container is a fourth staging, but it is strictly heavier than four words of
   `unshare` and would put a container dependency on `cargo test`.

   **Two preconditions, or the staging silently fails.** (a)
   `TF_TREE_RUNTIME_DIR` must be named explicitly: the default is
   `/tmp/tf_tree-<getuid()>` (`runtime_dir.rs:127`) and inside the empty-map
   userns `getuid()` is 65534 — measured, the namespaced joiner reported
   `error no arena is serving and CreatePolicy::Never forbids creating one` and
   created `/tmp/tf_tree-65534` beside the owner's `/tmp/tf_tree-1000`. The
   `Scratch` harness in `tests/rendezvous.rs` already sets it. (b) The staging
   passes `runtime_dir.rs:155`'s `meta.uid() != uid` gate only because an
   **empty** `uid_map` collapses both `getuid()` and `stat()`'s uid to the
   overflow uid 65534. With a *mapped* userns, `getuid()` would be 0 against
   `meta.uid()` 65534 and the open would be refused — so "improving" the staging
   into a mapped namespace breaks it.

   **This already produces a wrong shipped diagnosis, which is worth more than
   the test.** `slot_leak` classifies `(LockByte::Held, RecordedProcess::Gone)`
   as `SlotLeak::ForkInheritor` (`crates/tf_tree_cli/src/checks.rs:1329`), and
   `recorded_given`'s `Ok(_) => R::Gone` (`crates/tf_tree_cli/src/lib.rs:1924`)
   is exactly the start-time collision above. So `doctor` fires `TFT014` on the
   namespaced participant with the fork-inheritor text — *"a fork inheritor —
   byte still HELD, recorded pid gone … which on Linux means a forked child
   inherited it (PHASE2 §6.2) … Stop the child, or start workers with a start
   method that inherits no descriptors: multiprocessing's `spawn`"*
   (`checks.rs:1598-1612`). The cause is a PID-namespace mismatch; the
   remediation is about fork inheritance and is useless. Reported from a live
   run whose transcript was not archived; **the code path is confirmed by reading
   and the run has to be retaken and captured before it is cited as a
   measurement.**

   **So step 4 is buildable and this record does not ship uncovered.** What it
   must absorb is P5: the socket-hangup reclaim drives a dead joiner's word to
   `FREE`, and both predicates short-circuit on a non-`LIVE` word, so a zombie
   *joiner* stops discriminating before anything can observe it. The namespace
   holder is immune — it is alive and its socket is open. **Whether a killed and
   unreaped *owner*'s word stays `LIVE` long enough to observe the zombie
   disagreement in a live session was not tested by anyone, and it is the one
   thing that could make test (B) unwritable.**
2. **RESOLVED 2026-08-20: this record survives, and the ordering constraint is
   stated once — in `PHASE2.md` §5.1, not in either record.** ~~Does the adapter
   belong to this record or to `0028` piece 2?~~ They are not the same object and
   should not be merged: piece 2 decides whether to **reclaim** a slot, this
   decides whether to **steal** a topology lock, and they owe different §11.3
   rows. What must not be duplicated is the word-before-byte rule, so **whichever
   of the two lands first writes it into §5.1 and the other cites it.** `0028` is
   `ready` as of the same day and its step 1 lands first on the current
   sequencing, so the amendment travels with that step unless the order changes.
   Note the amendment is narrow and additive: §5.1 already makes the byte the
   answer for a tree that carries a probe; what it does not yet say is *when* the
   byte may be read relative to the word.
3. **Is P5 in scope?** The socket-hangup callback decides liveness from neither
   fact. It is correct under D17 and its incarnation guard bounds the damage, so
   nothing here proposes changing it — but a record titled "one liveness
   predicate per tree" that leaves a fifth in place should say why, and "D17
   makes the socket authoritative for its own class" may or may not be the whole
   answer.

## What would make this `ready`

- ~~Question 2 answered — it decides whether this record survives at all or is
  folded into `0028`'s plan as a step.~~ **MET 2026-08-20: it survives.**
- **`0031`'s question 2 answered and `0031`'s Decision taken.** Piece 1 is
  unwritable under "either option", so this is a hard prerequisite rather than a
  courtesy.
- **A `loom` model containing a live holder that a wrong predicate can steal
  from, or an explicit statement that there will not be one.** No existing model
  contains one, so `just loom` is green on this change by construction — see the
  section above.
- ~~Question 1 attempted, with the command, and its answer written down either
  way.~~ **MET, and it did better than "attempted": `unshare -U --fork --pid`
  stages the collision unprivileged on an ordinary host, so the change will not
  ship without a test that distinguishes it. Step 4 needs rewriting around
  `Known(st) != stored` rather than an unreadable entry — see question 1.**
- The §11.3 `topo.holding_lock` walk drafted and agreed, since D15 makes it the
  gate rather than a deliverable.

## Not in this record: #220

**#220 is not record material and should not become a second draft.** The
`Created | TookOver` arm called `builder.build_shared(...)`, so a taker-over
would have built a *new* arena rather than inherit the one it already has.
Checked at that call site, there was no fd, no socket path, no `Rendezvous` and
no way to take the session's arena in scope — **so the arm could not be fixed in
place under any answer to `0028` question 3.** Fixing it means restructuring
where the heir's arena comes from, which is exactly what question 3 is about.

**Landed as `0028` plan step 9, which is where this said its home was.** The arm
is split: `OpenOutcome::Created` keeps the `build_shared`
(`crates/tf_tree/src/open.rs:978`, the line this paragraph has now cited as
`:546` and `:613`; it is pinned to a moving file and will rot again)
and `OpenOutcome::TookOver` refuses with `OpenError::TakeoverUnsupported`
(`:1057`) — a refusal rather than an adoption for the reason above, that nothing
at the match names the arena this process already holds. Restructuring where the
heir's arena comes from is still unbuilt, and still question 3's.

One sentence here needs its correction stated rather than deleted, because it is
what a reader would check first: `OpenOutcome::TookOver` **remains
unconstructible through `tf_tree::Open`'s public surface** — the builder has no
`already_attached` setter, under no feature, and step 9 deliberately did not add
one. It is reachable from exactly one place, a `#[cfg(test)]` field of the same
name on that builder, which only `tf_tree` compiled as its own test target can
set; `open::tests::a_takeover_refuses_rather_than_building_a_second_arena`
(`:996`) is its only user.

Filing a second record would have put the same question-3 dependency in two
places.
