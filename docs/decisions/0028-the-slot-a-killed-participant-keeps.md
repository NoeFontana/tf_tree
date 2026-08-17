# 0028: the slot a killed participant keeps

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** none — this record exists so that none lands first.

## Context

> **Line numbers.** Everything in *The defect* and *The ordering* is cited
> against `f058f4f`, the commit this record was opened on. `528eddd` has
> since landed the hangup patch and shifted `crates/tf_tree/src/open.rs` by
> six lines from `spawn_owner_server` onward; both are given where the
> distinction matters. Nothing outside that one function moved.

### The defect

Filed as issue #184. `docs/PHASE2.md` §5.1 is NORMATIVE and says it in one
sentence:

> Liveness comes from the participant's OFD lock byte (§3.3), never from these
> records. A `ParticipantRecord` describes *who* a slot belongs to; whether it is
> live is a kernel fact. **Any code deciding liveness from `state` or `heartbeat`
> is a bug.**

The owner's slot assigner decides from `state`. At `f058f4f`, the commit this
record was opened against, `crates/tf_tree/src/open.rs:703` — and **unchanged at
`HEAD`**, where `528eddd` has moved it to line 709 without touching it, which is
the point of this record:

```rust
if table.identity(slot).is_some() {
    continue; // a registered participant lives here
}
```

`ParticipantTable::identity` (`crates/tf_tree_core/src/participant.rs:311`)
returns `Some` iff `state_of(state) == LIVE`. It consults nothing else — no lock
byte, no `/proc`, no boot id. The comment says "lives here"; the code says
"has a `LIVE` word here", and those differ for exactly one reason.

The only `LIVE -> FREE` transition in the workspace is `ParticipantTable::release`
(`participant.rs:285`), and it has exactly one caller: `Tree`'s `Drop`
(`crates/tf_tree/src/tree.rs:2636`). A `SIGKILL`ed process does not run `Drop`.
So a killed read-write participant's slot is `LIVE` for the life of the segment,
the assigner skips it for ever, and `DEFAULT_MAX_PARTICIPANTS` is 64
(`crates/tf_tree_arena/src/layout.rs:96`). **Sixty-four abnormal read-write exits
wedge the arena permanently**, and that budget is spent over the segment's whole
life, not concurrently — a crash-looping node spends it in minutes.

Two normative statements are unmet by any code at HEAD. §3.9: "A participant dies
-> ... the owner reaps its arena-side records (§6)." §11.3's crash matrix, row
`attach.after_slot_assigned_before_publish`: "record cleared by any reaper". No
reaper of participant records exists. `Tree::reap_dead` /
`Tree::reap_participant` (`tree.rs:2428`, `2441`, `reap_inner` at `2445`) sweep
the **claim** table only; they never touch a `ParticipantRecord`.

### Why it hid for the project's life, and why that is a property of the arena

**A wedged arena scores perfectly.** A ring outlives the process that filled it,
so every composed read succeeds against a frozen arena at full rate. Nothing a
reader can compute from the arena distinguishes healthy from frozen; only the
*age* of the freshest sample does, and no lookup returns it. This is not a
harness defect to be corrected — it is why `shm_torture` grew a `freshest=` and a
`slots=Nreg/Nalive` column, and it is the reason the §12.3 gate 3 wording ("zero
corrupt reads") cannot detect this class on its own.

### Corrections to two documents, made here because both were cited at me

- **The crash matrix is §11.3, not §8.** `docs/PHASE2.md` §8 is *Read-only
  attachment*. The eleven-row crash-point table with
  `attach.after_slot_assigned_before_publish` in it is §11.3 (line 866). Every
  reference below is to §11.3.
- **§5 describes an attach protocol the code does not implement, and §11.3's row
  inherits the error.** §5 says slot assignment "is done by the owner under its
  accept loop, so it needs no CAS protocol", and that "the record is written by
  the *owner* … before the response is sent". The code does not do that: the
  *joiner* writes its own record, with a CAS protocol, in `fill_slot`, after
  taking its byte. `Open::register_at`'s doc comment
  (`tf_tree_ipc/src/open.rs:394–407`) records the deviation for the lock file's
  identity record and argues it; nothing records it for the arena record. The
  consequence is that §11.3's row text — "participant slot in `ATTACHING`,
  **lock byte never taken**" — is true only of the byte-less
  `Tree::attach_shared` path. On the rendezvous path a `RESERVED` record always
  had a byte, held until the kernel released it. Two different producers of one
  state, with different byte histories, and the row names only one of them.
  Amending §5 and §11.3's row is part of step 0, with the other §5-family
  corrections.
- **`--force-new` is a flag `tf_tree_cli` does not have.** `docs/RUNBOOK.md:414`
  tells an operator to use it; `rg 'force.new' crates/tf_tree_cli` returns
  nothing. The capability exists as `CreatePolicy::Always`
  (`tf_tree_ipc/src/open.rs:78`) and as `Open::create`, not as a CLI flag. This
  is the same failure `0019` fixed for `tf_treed` — a runbook row naming a
  program that does not exist.

### The measurements, and which of them are mine

**Not mine.** #184's numbers — `slots=63reg/0alive`, `freshest=12365ms`,
`writers=0.0/4`, 167 `NoParticipantSlots` refusals at
`--duration 45s --children 6 --kill-hz 6 --seed 4242`, and the 30-minute run
whose `tf_tree participants --attach` showed slot 0 live and slots 1..63 stale.
I did not reproduce them and **could not have**, for a reason worth recording:

```
$ stat -c '%y %n' crates/tf_tree/src/open.rs target/release/shm_torture
2026-08-17 12:50:46.535085017 +0000 crates/tf_tree/src/open.rs
2026-08-17 12:51:50.447862582 +0000 target/release/shm_torture
```

The on-disk binary post-dates the candidate patch below, which was in the working
tree at that moment. Anyone re-running #184's exact command on this host today
measures the *candidate*, not HEAD, and gets a green result. That is the
project's third recorded failure mode — "verified locally" against a stale
artifact — pointing the other way for once, and it is why the pre-patch numbers
must be re-taken on a provably pre-patch binary before this record goes `ready`.

**Mine**, against a binary built from a tree containing candidate B:

```
$ ./target/release/shm_torture --duration 45s --children 6 --kill-hz 6 --seed 4242
shm_torture: t=45s rounds=269 composed=68861/68864 overlap=100% window=212ms
             freshest=1ms writers=1.6/4 slots=6reg/4alive live=92%
shm_torture: 268 kills, 0 violation(s)
  recovery: 1 edge(s) still carried a killed writer's claim word; reap_dead reclaimed 1
shm_torture: PASS

$ ./target/release/shm_torture --duration 60s --children 8 --kill-hz 10 --seed 991
shm_torture: t=60s rounds=595 composed=152022/152320 overlap=100% window=196ms
             freshest=1ms writers=1.8/4 slots=8reg/6alive live=94%
shm_torture: 594 kills, 0 violation(s)
  recovery: 2 edge(s) still carried a killed writer's claim word; reap_dead reclaimed 2
shm_torture: PASS
```

862 kills, `reg` never exceeding the child count, `freshest` pinned at 1 ms. So
candidate B **does** hold against the workload the harness models. What follows
is about the workloads it does not model, and §0.0 already names the largest one:
*"The killed processes are joiners; the driver owns the rendezvous and is never
killed."*

**Not mine, and the one number that matters most.** The orchestrator has since
run the same harness against an engine reverted to the pre-patch behaviour, and
it fails rather than printing a green run:

```
RECOVERY FAILURE: 63 of 64 participant slot(s) hold a LIVE record for a process
                  the kernel says is dead
Error: the arena was being written on only 75/269 observation rounds ... a ring
outlives the process that filled it, so those reads say nothing about a live
arena.                                                                  exit=1
```

That closes the loop this record's *"a wedged arena scores perfectly"* section
opens: the harness now detects the class **and says which of the two facts
caught it**, so the failure cannot be re-hidden by a change that fixes the
symptom and not the wedge. It is not yet the paired before/after on one binary
lineage that *What would make this `ready`* asks for — that wants both commands
recorded side by side against one build tree — but it is what makes the
detection, rather than the fix, the durable part.

### The ordering, derived rather than assumed

My brief warned of a window in which the record is already `LIVE` or `RESERVED`
while the byte is not yet held, so that a byte-driven reclaimer would evict a
participant that is merely starting. **On the joiner path that window does not
exist, and it is the reverse that does.** The order is:

1. Owner's `assign` closure picks a slot and sets its `granted` bit
   (`tf_tree/src/open.rs:703–726` at `f058f4f`, `709–732` at `HEAD`), then
   `HelloResponse` is sent
   (`tf_tree_ipc/src/server.rs:409–443`).
2. Client takes the **lock byte**: `Open::register_at`
   (`tf_tree_ipc/src/open.rs:408–419`) does `write_identity` then
   `try_take_participant(slot)`, i.e. `F_OFD_SETLK`, and returns before any arena
   write. Called from the join arm at `tf_tree_ipc/src/open.rs:304`.
3. Only then does the client write the **arena record**:
   `Tree::attach_shared_at` (`tf_tree/src/open.rs:536`) →
   `attach_shared_inner` (`tree.rs:2217–2229`) → `register_at` →
   `fill_slot` (`participant.rs:154`), which CASes `FREE -> RESERVED`, writes the
   identity fields, and release-stores `LIVE`.

So a live joiner **always holds its byte before its record exists**. The three
states a slot can be in, and what each means:

| byte | record | meaning |
|---|---|---|
| free | `FREE` | genuinely free (or granted-and-not-yet-taken — that is what `granted` covers) |
| **held** | `FREE` or `RESERVED` | a joiner mid-attach. **This is the start-up window**, and the byte already covers it |
| held | `LIVE` | attached and running |
| **free** | `RESERVED` or `LIVE` | **the leak.** Nothing else produces it on the rendezvous path |

The creator/taker-over path is the same shape: `register_any`
(`tf_tree_ipc/src/open.rs:388`) takes the byte, then `build_shared` writes the
record. So "byte before record" is invariant across every path that has a byte.

There is exactly one path that has **no** byte: `Tree::attach_shared`
(`tree.rs:2171`), the fd-inheritance entry point, self-assigns via `register`
and never touches the lock file. In `ReadWrite` mode that produces a `LIVE`
record with a permanently free byte — indistinguishable, by the byte alone, from
the leak. Today only `crates/tf_tree_bench/tests/multiprocess.rs:398` does this,
but `attach_shared` is public API and §0.0 names "fd-inherited trees" as a
supported class. **A reclaimer keyed on "byte free ⇒ dead" evicts it while it is
running.** That, and not the start-up window, is the eviction hazard this record
has to defend against.

### The candidate that already exists

Another lane wrote a fix while this record was being written, and it has since
**landed**, as `528eddd` *"fix(open): the owner reaps a dead participant's
record, as §3.9 says it does"*. It is reproduced here in full rather than
referenced by line, because it moved between the working tree and `HEAD` twice
during the writing and the line numbers in *The defect* above are `f058f4f`'s.
Two hunks in `spawn_owner_server`: the owner's second mapping goes
`ReadOnly -> ReadWrite`, and the hangup callback — which already existed,
carrying only `granted_hangup.set(...)` — gains

```rust
let view = tf_tree_core::arena_view::ArenaView::new(&table_arena);
let table = view.participants();
if let Some((_pid, _start, incarnation)) = table.identity(slot) {
    table.release(slot, incarnation);
}
```

It is a good patch and this record does not throw it away — it is candidate B
below, and the Decision keeps it as the O(1) fast path, so it is a *subset* of
this record rather than a competing shape.

**But it has landed on its own, and that is the thing this record was opened to
prevent.** The five classes it does not reach are now the shipped behaviour, and
four of them are invisible behind a green `shm_torture` run by construction:
§0.0 records that the harness never kills the owner, and its children never
fork. So the argument that has to be maintained from here is not "does the patch
work" — the numbers below say it does, for what it covers — but *"which leaks
does a green run still permit"*, and only this record answers that. Step 4 is
now a rebase of a landed commit rather than an adoption of an unlanded one; the
rest of the plan is unchanged.

---

## Decision

**Reclaim at the point of decision, from two agreeing negative facts; keep the
hangup callback as the O(1) fast path; and move the assigner's authority off
`state`.** Concretely, four pieces, none of which adds an arena field and none of
which bumps `FORMAT_VERSION`:

**1. `ParticipantTable::reclaim(slot, observed: u32) -> bool`** in
`tf_tree_core` — a single `compare_exchange(observed, FREE, AcqRel, Acquire)`
against the *word the caller observed*, so any state change between observation
and CAS aborts the reclamation. It differs from `release` in one way that
matters: it does not need the incarnation, because the observed word carries it.
`release` stays exactly as it is for the clean-detach path.

> **Scoped to `live_word(inc)` only.** An earlier revision of this record had
> `reclaim` accept `RESERVED` as well, and claimed that closed §11.3's
> `attach.after_slot_assigned_before_publish` row. **That was wrong and is
> refuted below** (*"`RESERVED` is not a word this predicate may act on"*):
> `RESERVED` is the bare constant `1`, it carries no incarnation, and it is
> written *before* the identity fields — so neither the CAS guard nor either
> conjunct of piece 2 can tell a killed registrant from a running one. §11.3's
> row therefore stays **unmet** under this decision as written, and closing it is
> **open question 6**, which gates `ready`.

**2. One reclamation predicate, named once.** A slot whose `state` reads
`live_word(inc)` is reclaimable iff **both** of these are negative, and
**neither is allowed to be "don't know"**:

- its lock byte reads free via `F_OFD_GETLK` (`LivenessProbe::is_held`,
  `tf_tree/src/open.rs:87`, which already returns `Option<bool>` for exactly this
  reason), **and**
- the record's `(pid, start_time)` is absent from `/proc`, or names a different
  process than the one recorded.

Either answering "unknown" means *not reclaimable* (§6.2). The second conjunct is
not belt-and-braces: it is the only thing that protects the byte-less
`attach_shared(ReadWrite)` participant.

**That last sentence is the whole justification for the conjunct, and it rests on
an unanswered question — open question 1.** It is listed as an open question and
consumed here as a premise, which is the wrong order; if the byte-less class is
not a supported shape the conjunct has no reason to exist. Read question 1's
recommendation before treating this piece as settled.

**And "either answering unknown means not reclaimable" is not a property this
code currently has.** `/proc` has three answers, not two, and the partition is by
`io::ErrorKind`: `ENOENT` maps to `NoSuchProcess`, the *proof of death* branch,
and `ENOENT` is what an unmounted or `hidepid`-hidden `/proc` returns. A stored
`start_time` of `0` — what `process_start_time()` writes when it cannot read
`/proc/self/stat` — compares unequal to every real start time and reads as death
too. Both are worked through under *"the fail-safe claim is false on this code"*.
Step 2 therefore owes a **fourth** answer, *this host cannot answer at all*,
distinguished from *this process is gone*; without it the conjunct is not
conservative, it is inverted, on exactly the hardened deployments where it
matters.

Three constraints on how that is built, each of which an earlier revision got
wrong:

- **It cannot be `record_is_alive` (`tree.rs:2782`).** That function opens with
  `state_of(...) != LIVE { return false }`, so composing it *would* decide
  liveness from `state` — the thing §5.1 calls a bug — and it is precisely that
  branch which makes it answer "dead" about a live registrant sitting in
  `RESERVED`. The predicate needs the `/proc` half of it and not its `state`
  test: a new `pid_matches(rec) -> Known(bool) | Unknown` over
  `read_start_time`, with `record_is_alive` refactored onto it so there is one
  `/proc` comparison in the crate and not two.
- **The byte probe must skip this process's own slot.** `reap_inner` already
  carries this rule for claims — *"Never judge ourselves. `F_OFD_GETLK` reports
  only conflicting locks, so a description does not see its own"* — and
  `use_ofd_liveness` (`tree.rs:2378`) states outright that the sweep must not
  rely on the second-open-file-description detail that currently makes our own
  byte visible. A sweep that omits the guard is one refactor away from
  reclaiming its own live slot.
- **The predicate presumes the lock byte and the arena record are the same
  integer, and that holds on one path only.** `register_at`'s own doc comment
  (`participant.rs:236–245`) says so: the two are allocated independently *"as
  they are today, by `register` here and by `LockFile::take_any_participant`
  there"*, and if they diverge "every liveness answer would be about somebody
  else". Only the joiner path (`Open::register_at` + `attach_shared_at`) pins
  them together. The creator agrees by coincidence — a fresh arena's first free
  record and a fresh lock file's first free byte are both 0 — and a taker-over
  need not (open question 3). **The correspondence is a precondition of this
  predicate, not an invariant it may assume**, and step 2 owes an assertion of
  it rather than a comment about it.

**3. The assigner decides from the byte.** Replace the `identity(slot).is_some()`
skip with the byte probe that is already in scope eight lines below
(`open.rs:717` at `f058f4f`, `723` at `HEAD`), and on
`byte free + record not FREE` run the predicate and, if
it fires, `reclaim` before granting. The near miss #184 identified is the whole
point: **the authoritative signal is already at the point of decision, being
consulted for the read-only case and ignored for this one.**

**4. Reaping is not owner-only.** A public `Tree::reap_participants() -> usize`
on a read-write tree, sweeping all slots with the same predicate. `PROJECT.md`
§6 lists "Reaping from the owner only" as a named design smell, and §6.3 says it
outright: *"Reaping must not be owner-only — an owner-only design leaks every
claim held at the moment the owner died."*

**This is not a second spelling** of `reap_dead` (the rule in `CLAUDE.md`).
`reap_dead` reclaims *claims*; this reclaims *participant records*. They are
different objects, they use different lock-file regions (`CLAIM_BASE + edge`
versus `16 + slot`), and their predicates differ — a claim needs only the lease,
a record needs the two-fact test because of the byte-less path. Folding them into
one call was considered and loses on the return value: `usize` would become two
counts added together, and a caller that wants only the cheap per-edge sweep
loses it.

Candidate B's hangup callback is **kept**, with one change: it calls `reclaim`
with the observed word rather than `release` with a separately-loaded
incarnation, so the guard is one word instead of a load and a CAS. It does
**not** thereby collect `RESERVED` — see *"`RESERVED` is not a word this
predicate may act on"*.

### Cost

Nothing on the hot path. `Plan::at` is untouched; no allocation, no lock, no
conversion is added to the hot tier, so `API.md` R2 is unaffected and D3 keeps
all of it off the query path. The sweep is at most 64 `F_OFD_GETLK` plus at most
64 `/proc` reads, and only over slots the assigner would otherwise have skipped —
one grant is already a `connect`, a handshake, an `SCM_RIGHTS` `recvmsg`, an
`mmap` and `populate_hot` at a measured 97.5 µs p50 (§12.2). The hangup fast path
keeps the common case at one CAS.

### `FORMAT_VERSION`

**No bump.** `reclaim` is a new operation over the existing `state` word; no
region moves, no stride changes, `layout_hash` is unaffected.

**And it survives open question 6, whichever way that goes** — which is worth
stating, because a *state-word encoding* change is the kind of thing that slips
past `layout_hash` (it hashes region strides, not the meaning of bytes, which is
`0027`'s hazard). Re-encoding `RESERVED` as `(inc << 2) | RESERVED` is still
`state_of(...) == RESERVED` to a build that has never heard of it, because
`state_of` masks the low two bits and every existing reader goes through it
(`participant.rs:53`). An older build attaching to a newer build's arena reads
such a slot as `RESERVED`, which is what it is. Turning the publishing `store`
into a `compare_exchange` is invisible on the wire entirely. Neither needs a
format bump; both need a §11.3 walk, which is why they are a question and not a
footnote.

---

## Rationale

### Candidate B — hangup-driven owner reap (the existing patch)

*What it preserves.* D17 exactly: liveness is the socket, and the socket is the
signal it reclaims on. No second liveness source is introduced. §3.9's sentence
gets its implementation. The incarnation guard in `release` is genuinely correct
— a clean detach already freed the slot and the CAS no-ops, a re-granted slot has
a different incarnation and the CAS fails, so it cannot free a live participant's
slot. No new field, no format bump. And the start-up window is a non-issue for
it, because the socket exists before the grant.

*What it weakens.* It makes reaping **owner-only**, which `PROJECT.md` §6 lists
as a design smell by name and §6.3 forbids in a sentence written about this exact
situation.

*Five places it does not reach*, four of which are code I can point at:

1. **The fork-inherited connection.** Real, and worse than "no hangup" — see the
   section below. The harness cannot see it: its children do not fork.
2. **`RESERVED`.** A process killed inside `fill_slot`, between the
   `FREE -> RESERVED` CAS (`participant.rs:156`) and the release-store of `LIVE`
   (`participant.rs:168`), leaves `RESERVED`. `identity` returns `None`, the
   `if let Some` skips, and `fill_slot` only ever CASes from `FREE` — so that
   slot is lost to everybody, for ever. This is §11.3's
   `attach.after_slot_assigned_before_publish` row, whose stated repair is
   "record cleared by any reaper", still unmet. **The decision does not fix this
   either** — an earlier revision claimed it did, and that claim is refuted
   below. It is open question 6, and it is the one place where the decision is no
   better than B.
3. **The owner's own slot, and the takeover gap.** See below.
4. **The `epoll::add` failure path.** `server.rs:309–331` deliberately declines
   to call `on_hangup` when `epoll::add` fails after a successful handshake, and
   argues for it correctly. But that client is now unwatched: when it later dies,
   no hangup ever fires, and B never reclaims its record. The existing comment
   bounds the damage at "a slot nobody can use" — with B that becomes a slot
   nobody can use *and* a `LIVE` record nobody can clear.
5. **`attach_shared`.** No socket, no grant, no hangup, ever.

*Crash matrix.* B is a single CAS, so it has no torn intermediate of its own: an
owner killed inside the callback either performed the reclamation or did not, and
what is lost is the reclamation, not consistency. But since nothing else
reclaims, "lost" is permanent — which is (3) again.

*Verdict:* keep it, as the fast path. Reject it as the guarantee, on (1)–(5) and
on §6.3.

### Candidate A — the assigner consults the OFD lock, nothing else

*Why it is not sufficient on its own, in one line:* the assigner would correctly
**decide** the slot is free, and then `register_at` would refuse it, because
`fill_slot` CASes from `FREE` and the record is still `LIVE`. The joiner gets
`ShmError::ParticipantTableFull` (`tree.rs:2182–2191`, whose doc comment already
predicts this exact outcome and says "there is nothing useful to retry"). A
decides correctly and cannot act. It is a necessary half — it is piece 3 of the
decision — and not a candidate by itself.

### Candidate C — two-phase publication, or a grace period

*Rejected as already done.* C exists to close a window in which the record is
published before the byte is taken. The code takes the byte first on every path
that has one (the ordering table above), so the window C targets is closed by
construction today. Building C would be building a second mechanism for an
invariant the ordering already provides — and it cannot help the one path where
the record genuinely has no byte (`attach_shared`), because there is no byte to
reorder. A grace period is additionally forbidden in spirit by §3.4's "the check
is deterministic, not a grace period" and by §6.4's refusal of staleness-based
reaping.

### Candidate E — a lease/liveness flag in `ParticipantRecord`

*Rejected on cost.* It would let a reclaimer distinguish "byte-less because
fd-inherited" from "byte-less because dead" with no `/proc` read. It needs an
arena field, and `CLAUDE.md` is explicit: `FORMAT_VERSION = 3` already happened
and fields are not to be added opportunistically. The apparently free version —
spend one of `_pad: [u8; 88]` (`participant.rs:76`) — is the most dangerous
variant, and `0027` already wrote down why: `layout_hash` hashes region strides,
not field offsets, so two builds disagreeing about that byte would attach to each
other and misread. The `/proc` read costs a `syscall` on a startup path and needs
no format negotiation at all.

### Candidate D — the decision above

*What it preserves.* §5.1's rule that liveness is not decided from `state` —
**with one caveat that has to be stated rather than glossed.** `state` still
selects *which* slots are candidates: `FREE` means nothing to collect, and only
`live_word(inc)` is acted on. That is a read of `state`, and the defensible line
is that it answers "is there a record here" and not "is its process alive", which
is exactly the split §5.1 draws between the record and the kernel. The line stops
being defensible the moment the predicate is built out of `record_is_alive`,
whose first three lines *are* a liveness decision from `state` — see piece 2's
first constraint. Also preserved: §6.3's "not owner-only"; §6.2's fail-safe
direction on the byte conjunct — **not on the `/proc` conjunct, which does not
have it today**, see below; `0004`'s fixed capacity and D4's no-growth; D17,
which forbids a *heartbeat* timeout and is untouched — nothing here reads
`ParticipantRecord::heartbeat`, and the socket remains the fast-path signal;
`0018`, since no blocking wait, futex or notification primitive is added to the
arena — a sweep is a caller-scheduled syscall loop, not something anyone parks
on. The clean-detach path is unchanged.

*What it weakens.* It reintroduces `/proc` onto a correctness-critical path, and
**the two defences an earlier revision offered for that are both wrong.** They
are set out here rather than deleted, because each is the reading a reviewer
will arrive at independently.

*First defence, withdrawn: "§3.3 anticipated this placement."* §3.3 says
"`/proc` parsing and PID-reuse defence are no longer on the **rendezvous path**
at all. They remain only for the arena's advisory participant table (§5)". That
sentence splits on the *path*, and the reply split on the *table*. Piece 3 puts
the read on the rendezvous path by construction: `spawn_owner_server`'s `assign`
closure runs inside the owner's `serve` loop, answering a `HelloRequest` — §3.7
step 3 — which is why this record's own *Cost* section prices it against a
97.5 µs attach. So piece 3 contradicts §3.3 as written, independently of §5.1.

*Second defence, withdrawn: "a conjunct can only ever be conservative, so a
broken `/proc` postpones recovery and can never steal a slot."* **False on this
code.** `read_start_time` (`tree.rs:2866–2879`) splits on `io::ErrorKind`:

```rust
Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ProcStartTime::NoSuchProcess,
Err(_) => return ProcStartTime::Unreadable,
```

`NoSuchProcess` is the *proof of death* branch, and `ENOENT` is what an
unavailable `/proc` produces, not just an absent process. Three consequences the
predicate has to carry explicitly:

- **A container with no `/proc` mounted** fails every
  `open("/proc/<pid>/stat")` with `ENOENT` → `NoSuchProcess` → **dead**, for
  every participant. The conjunct does not go silent there, it inverts: the byte
  becomes the sole authority and the byte-less `attach_shared(ReadWrite)`
  participant — the one class this conjunct exists to protect — is reclaimed on
  sight. So the honest statement of open question 1 is worse than "the wedge
  returns quietly"; on that host the eviction hazard returns instead.
- **`hidepid=2`** hides other users' `/proc/<pid>` with `ENOENT` as well. §3.10's
  same-user trust model is what makes that survivable, and it should be named as
  the reason rather than left to luck.
- **`start_time` is compared as a value and `0` is a legal one.**
  `process_start_time()` (`tree.rs:2928`) returns `0` when it cannot read
  `/proc/self/stat`, and the registrant stores that `0`. A later reclaimer that
  *can* read `/proc` gets `Known(st)` with `st != 0`, so `st == start_time` is
  false and the verdict is **dead** about a running process. An unpopulated
  `start_time` does not degrade the conjunct to a bare-pid test, which would
  merely be weaker — it makes the slot unconditionally reclaimable.

So step 2 owes a third answer, distinct from both existing ones: *this host
cannot answer*, detected by `/proc/self/stat` being unreadable at the same
moment, and mapped to **not reclaimable** along with a stored `start_time` of
`0`. `record_is_alive`'s existing three-way split is the right shape and the
wrong partition for this use.

*And §5.1 has to be amended, not read around.* Its last paragraph is an
enumeration and a positive claim: the triple "remains the identity triple **for
diagnostics and for the `--force-new` path**", and `start_time` "is still parsed
— carefully — because `doctor` reports it and the takeover path prints it, but
**it is no longer on any correctness-critical path**". Reclamation is none of
those four things, and evicting a live participant leaves its edges looking
unclaimed, so the claim is falsified rather than stretched. **That makes this
plan step 0, not part of step 7.** `PHASE2.md` §1 is the precedent for the shape:
A1–A8 are numbered amendments to a normative section, written *before* the code
that depends on them, precisely so that a protocol change cannot arrive as an
interpretation. A decision that quietly widens what a NORMATIVE section permits
is the same defect as `identity()` in the assigner, one layer up.

One detail that makes the amendment easier rather than harder: §5.1's
enumeration licenses the triple for "the `--force-new` path", and this record
establishes that `--force-new` **does not exist**. One of the two uses §5.1
names is fictional, so the paragraph is stale on its own terms and is being
rewritten anyway.

What none of this mitigates is §5.1's parsing trap, which becomes a correctness
dependency rather than a diagnostic one.

*What it costs.* Bounded, on the attach path, quantified above. Unmeasured — see
open question 4.

*Crash matrix.* Walked below.

*Why it beats B:* it closes (3), (4) and (5) and repairs the §5.1 violation
itself rather than making its consequence rare. B makes the stale record
unlikely; D makes a stale record harmless. **It does not close (2)** — the
`RESERVED` row — and does not close (1), the fork case. Two of B's five, not
four.

---

## The fork hole is real, and it defeats candidate D as well as B

My brief asked whether a fork after attach breaks B's signal. It does, and the
mechanism is worse than "no hangup":

- The client's connection is created with `SocketFlags::CLOEXEC`
  (`tf_tree_ipc/src/client.rs:70`), and parked in `Attachment::Joined`
  (`tf_tree/src/open.rs:104–107`). `CLOEXEC` closes it across `exec`, and
  **`fork` alone does not exec**. So a forked child inherits an fd to the same
  open file description.
- `MADV_DONTFORK` (§7.3) removes the *mapping*, not the descriptors. The atfork
  handler (`tf_tree_ipc/src/fork.rs:91`) does exactly one thing — a `fetch_add`
  on a counter — and closes nothing.
- A connection dies when the last descriptor referring to its description is
  closed. So `SIGKILL`ing the parent while a forked child lives leaves the
  connection open, and the owner's `epoll` never reports `HUP`.

**And the same argument applies to the participant lock byte.** The lock file
description is inherited too, and an OFD lock is released when the last fd
referring to the description closes. So after the parent dies, the child still
holds the parent's byte — which is not a bug in the reclaimer, it is §6.2
NORMATIVE: *"OFD locks are held by the open file description, which survives
`fork` and is shared with the child. Parent and child therefore both 'hold' every
claim."* Candidate D's first conjunct therefore reads "alive", and D refuses to
reclaim, correctly by its own rules.

Two consequences worth being precise about:

- The leak is **deferred, not necessarily permanent**: when the last inheritor
  exits, the description closes, the byte releases and the hangup fires. For a
  short-lived child this self-heals. For a long-lived `multiprocessing` worker
  pool — the case §7.3 and §14 say Python users will hit — the deferral is the
  process's lifetime.
- The child cannot use the slot it is holding: no mapping, and its `Tree` is
  poisoned (`tree.rs:812`). So a live description is holding a slot on behalf of
  a process that provably cannot participate.

**The answer is not a third reclamation path.** It is to fix the signal at the
source: have the atfork child handler close the inherited lock-file and socket
descriptors, so the byte's and the connection's lifetimes track the process that
can actually use them. Closing an fd in the child does not unlock anything while
the parent lives (the description stays open), which is exactly the desired
semantics; and once the parent dies, the child's close *is* the release. `close`
is async-signal-safe, which is the standing constraint on that handler
(`fork.rs:46–56`), but a handler that closes a registered set of fds needs a
lock-free registry and must not allocate — it is a change to the fork protocol,
which is normative in §6.2/§7.3, so it is **step 7 below and it carries an open
question**, not something to slip in.

Until it lands, this is a **documented limitation with a detection**, not silent:
`TFT014` (step 6) reports a byte held whose recorded pid is gone as exactly what
it is — a fork inheritor holding a slot — which is actionable (kill the child, or
use the `spawn` start method, which is already the §14 advice for a different
reason). Reclaiming it automatically would mean overruling the kernel's answer
with a `/proc` inference, which is the inversion §5.1 exists to forbid.

---

## The three questions the candidate does not answer

### A crash between the hangup and the CAS

The callback is one `compare_exchange` on one word. There is no torn state to
repair: the reclamation either happened or it did not, and a slot that did not
get reclaimed is `LIVE` with a free byte, which is the state this whole record is
about. So the crash-matrix answer for `hangup.after_probe_before_cas` is *"no
observable intermediate; the reclamation is simply not performed"* — and it is
only a *repairable* state because of decision piece 3, which reclaims it at the
next assign. Under candidate B alone the honest matrix entry would be "not
repairable", and a matrix row that says that is a row that should not ship.

### Takeover (§3.5) inherits the leak, and adds to it

Read as coded:

- **Takeover is not wired at all.** `crates/tf_tree/src/open.rs:22–26` says so in
  its own module docs, and §0.0 says it louder: the lock-file protocol exists and
  `tf_tree_ipc`'s `migrate` test covers it, but *"nothing watches the client
  socket for `HUP`, so no participant ever calls that path"*. So today, owner
  death does not produce a new owner; it produces an arena no new process can
  join (`ArenaHeldButUnreachable`), which is a wedge of its own.
- **The owner's own slot leaks unconditionally.** The owner registers through
  `register_any` + `register` (self-assigned), and nothing hangs up on the owner.
  `shm_torture` cannot see this: §0.0 records that the driver owns the rendezvous
  and is never killed.
- **When §3.5 *is* wired, the heir inherits the leak.** A heir starts
  `spawn_owner_server` fresh: `granted` is zero and its `epoll` set contains no
  client sockets, because every existing participant's connection was to the dead
  owner. No hangup can ever fire for a participant that attached before the
  takeover, so under candidate B their records are unreclaimable for the life of
  the segment.
- **And it adds one.** `Open::already_attached` takes the takeover arm at
  `tf_tree_ipc/src/open.rs:323–332`, which calls `register_any` — a *new* byte
  and a *new* arena slot — while the heir's original session still holds its
  first pair. Each takeover would therefore consume a second slot unless the old
  session is dropped first. That is open question 3; it blocks wiring §3.5 more
  than it blocks this record, but it changes step 5's sweep. Note also that
  `register_any` and the arena's `register` allocate from two different
  free-lists, so a heir can end up holding lock byte *i* against arena record *j*
  — the exact divergence `register_at`'s doc comment warns about, and the reason
  the predicate's slot correspondence is listed as a *precondition* in piece 2.
- **A fourth thing, which is not this record's to fix but must not be discovered
  by whoever wires §3.5.** `tf_tree::Open::attempt` handles `Created` and
  `TookOver` on **one arm**, and that arm calls `builder.build_shared(...)`
  (`tf_tree/src/open.rs:546–556`) — which `memfd_create`s a **new segment**. A
  taker-over would therefore not take over the arena at all; it would create a
  second one and serve that, which is precisely the divergence §3.4 step 4
  exists to prevent, arrived at from the other side. It is unreachable today —
  `already_attached(true)` appears in exactly one place in the workspace, a
  `tf_tree_ipc` test at `open.rs:643`, and `tf_tree::Open` has no setter for it —
  so `OpenOutcome::TookOver` is dead code on this path. It is recorded here
  because open question 3's answer has to account for it.

Under the decision, the reclamation half of this is not fatal: the heir's first
`assign` sweeps and reclaims, and any surviving read-write participant can call
`reap_participants()` without being the owner. That is the concrete payoff of
"not owner-only".

### The read-only slot: right, but for a reason that is also a bug

A read-only participant takes a lock byte and writes **no** arena record —
`attach_shared_inner` gives a non-writable backing the `u32::MAX` sentinel
(`tree.rs:2217–2229`), because registering would write to a `PROT_READ` mapping.
D18 makes this the consumer default and it is the Python default too.

So on hangup, `identity(slot)` returns `None`, the `if let Some` does nothing,
and that is **correct**: there is no record to collect, and the byte is already
released by the kernel.

But it is correct for the same reason it is wrong for `RESERVED`. `identity`
collapses two different states into `None`:

| `state` word | `identity` | what it means | what should happen |
|---|---|---|---|
| `FREE` | `None` | read-only slot, or already released | nothing |
| `RESERVED` | `None` | killed mid-`fill_slot`, **or a healthy registrant mid-`fill_slot`** | see below — **not** simply "reclaim" |
| `live_word(inc)` | `Some` | a record to collect | run the predicate |

A predicate that cannot tell rows 1 and 2 apart reads healthy on a state that is
not, so `reclaim` takes the **observed `state` word** rather than an `Option`.
But row 2 collapses two states of its own, and that is the next section.

### `RESERVED` is not a word this predicate may act on — refuted, with the interleaving

An earlier revision of this record decided that `reclaim(slot, RESERVED)` closes
§11.3's `attach.after_slot_assigned_before_publish` row. **It does not. It
converts a leaked slot into two live participants sharing one slot index**, which
is the failure `release`'s own doc comment was written around: *"Two live
processes would then share a slot index, and the `slot + 1` owner encoding that
both claims (A3) and the topology lock (A2) rest on stops being unique."*

Three facts about `RESERVED`, all in `fill_slot` (`participant.rs:154–170`):

1. **It carries no incarnation.** `live_word(inc)` is `(inc << 2) | LIVE`;
   `RESERVED` is the bare constant `1`. So "CAS against the observed word" —
   this decision's entire safety bound — degenerates to a plain ABA on this one
   value. Two different occupancies of a slot are byte-identical words.
2. **The identity fields are written *after* the CAS.** At `RESERVED` the
   record's `pid` and `start_time` are still the *previous* occupant's (`release`
   leaves them deliberately) or zero. So the `/proc` conjunct is not merely
   unhelpful here, it is **reading somebody else's identity**.
3. **The publishing step is an unconditional `store`, not a CAS**
   (`participant.rs:168`). A registrant whose slot is taken from it underneath
   cannot find out, and overwrites whatever is there.

The interleaving needs no ABA and no second reclaimer:

- Process `X` calls `Tree::attach_shared(fd, ReadWrite)` — the fd-inheritance
  path, so **no lock byte, ever**. `register` → `fill_slot` CASes slot *s*
  `FREE -> RESERVED`. `X` is then preempted (a page fault on the fresh mapping is
  the ordinary way; cgroup throttling and a stop-the-world pause are others).
- Reclaimer `R` sweeps *s*: `state` is `RESERVED`; `F_OFD_GETLK` on byte *s* says
  free, because `X` never took one; the `/proc` conjunct reads the record's
  `pid`, which `X` has not written yet. Both conjuncts negative. **Reclaimable.**
- `R` CASes `RESERVED -> FREE`. It succeeds, because `X` is still in `RESERVED`.
- `X` resumes, writes its identity fields and `store`s `live_word(n)`.
- Registrant `Y` — or the owner's assigner under piece 3, which now sees *s*
  `FREE` — CASes *s* `FREE -> RESERVED` and publishes `live_word(n + 1)`.
- `X` and `Y` both believe they own slot *s*. Their claims (A3) and any topology
  lock they take (A2) name the same owner word. Nothing detects it.

On the **rendezvous** path the byte covers the same window (a joiner holds byte
*s* through `RESERVED`), so this needs the ABA variant: `R` probes while a dead
predecessor's `RESERVED` is there, the assigner reclaims and grants *s*, the new
joiner takes the byte and CASes `FREE -> RESERVED`, and `R`'s stale
CAS(`RESERVED -> FREE`) then succeeds against the *new* occupancy. Piece 3 and
piece 4 together guarantee two reclaimers exist, so this is not a contrived
race.

**Note what the existing code leans on instead**, because it is the trap:
`record_is_alive`'s doc says a non-`LIVE` slot is reported dead *"since `RESERVED`
is held for a handful of instructions by a healthy process"*. That is a **timing
argument** — the exact shape §3.4 refuses ("deterministic, not a grace period")
and §6.4 forbids for reaping. It is harmless for A8's rescue path, which only
declines to *wait* on such a slot. This decision would make it load-bearing for a
destructive CAS, and it is not strong enough to bear that.

So: **`reclaim` is scoped to `live_word(inc)`, §11.3's row stays unmet, and this
record does not pretend otherwise.** Closing it is open question 6. The shapes on
the table, none of which this record is willing to pick without an answer,
because CLAUDE.md is explicit that an ordering question the specs do not answer
is asked rather than guessed:

- **Give `RESERVED` an incarnation** — `reserved_word(inc) = (inc << 2) | RESERVED`
  — so the CAS guard works on it too. `state_of` still reads `RESERVED`, so an
  older build attaching to the same segment is not misled and `layout_hash` is
  untouched; but `fill_slot` bumps `incarnation` *after* its CAS today, so the
  ordering inside `fill_slot` has to change and that is a crash-consistency
  protocol change in its own right.
- **Make the publish a CAS** — `compare_exchange(reserved, live_word(inc))`, with
  a registrant that loses learning it was reclaimed and returning
  `ParticipantError::SlotTaken` instead of publishing over a stranger. This is
  the minimum needed for *any* reclamation of `RESERVED` to be safe, whatever
  else is chosen, and it costs one CAS on an attach path already measured at
  97.5 µs p50.
- **Leave `RESERVED` alone**, record §11.3's row as unmet-by-design, and accept
  one permanently lost slot per kill that lands in that window. It is a real
  cost — rare per kill, certain over a fleet's life — but it is a *bounded* leak
  against an unbounded correctness hazard.

---

## Crash matrix — §11.3, walked

Rows §11.3 is missing for this path. **Caveat stated up front:** §0.0 records
that the `crash-points` feature and `TF_TREE_CRASH_AT` **do not exist**, and that
`shm_torture --crash-points` refuses rather than passing a `SIGKILL` run off as
§11.3 coverage. So these rows are specifications, and until §11.3 is built they
can be reached only by the shallower `SIGKILL` distribution. Do not mark them
covered.

| Crash point | State left behind | Repair, under the decision |
|---|---|---|
| `attach.after_grant_before_byte` | slot granted, byte free, record `FREE` | none needed: the owner's `granted` bit holds it, and `server.rs:440` already releases the grant if the response never landed |
| `attach.after_byte_taken_before_record` | byte held, record `FREE` | none needed: the kernel frees the byte at death; nothing was written to the arena |
| `attach.after_slot_assigned_before_publish` *(existing row, promise unmet at HEAD)* | record `RESERVED`; byte **held-then-kernel-released** on the joiner path, **never taken** on the fd-inherited one — §11.3's row text describes only the second, see *Corrections* | **still unmet.** `reclaim` is scoped to `live_word(inc)`; acting on `RESERVED` produces two occupants of one slot (*"`RESERVED` is not a word this predicate may act on"*). Open question 6 |
| `detach.after_record_released_before_byte` | record `FREE`, byte held | none needed: assigner skips on the byte until the kernel frees it. This is `Tree::drop`'s order (record at `tree.rs:2636`, byte when `Attachment` drops) and it is the safe one |
| `hangup.after_probe_before_cas` | record still `LIVE`, byte free | one CAS, so no torn state; the next assign's sweep reclaims |
| `reclaim.after_probe_before_cas` | nothing published | idempotent; racing reclaimers are harmless, at most one CAS succeeds |
| `reclaim.probe_then_reoccupied` | reclaimer holds a stale verdict; slot has been freed, re-granted and re-occupied since | safe **only because the observed word is `live_word(inc)`**: the new occupancy's `incarnation` differs, so the CAS fails. An earlier revision called this row *unreachable* — it is not, piece 3 and piece 4 guarantee two reclaimers — and it is **not** safe for `RESERVED`, which carries no incarnation |
| `takeover.before_reaping_predecessor_clients` | every pre-takeover participant's record unwatched by the heir | the heir's first assign sweeps; any read-write survivor may call `reap_participants()` |
| `fork.parent_killed_while_child_holds_description` | byte **held** by the child's inherited description, record `LIVE`, owner sees no `HUP` | **not reclaimable, deliberately** (§6.2). Reported by `TFT014`; closed at the source by step 7 |

§11.2 also needs a scenario, between its existing 2 and 6:

> **2b. Slot recycling under abnormal exit.** Attach and `SIGKILL` a read-write
> participant 128 times against a 64-slot arena, one at a time. Every attach must
> succeed. This is the direct falsifier for #184 and it fails at HEAD on the 65th.

---

## Consequences

- `tf_tree_core` gains one function on the participant table. Dependency budget
  untouched (D14): no new dependency, and the `/proc` half lives in `tf_tree`,
  which already parses it.
- **A new invariant to maintain:** every reclamation decision uses the two-fact
  predicate, and there is exactly one implementation of it. A second copy is the
  bug this record is about, re-created. It should be greppable the way the
  `allow(unsafe_code)` exception is.
- The assigner stops being able to answer "is this slot in use" from the arena
  alone, so the owner's serving thread now needs the lock file as well as the
  table. It already opens one (`lock_probe`, `open.rs:635`).
- The owner's second mapping becomes `ReadWrite`. Candidate B's justification for
  that is sound and survives into the decision: an owner either created the
  segment or took over by building one, so it always has a writable segment and
  this cannot demote a read-only attachment.
- `Tree::reap_participants()` is new public surface, so it passes `API.md` §7's
  checklist before it is written. R6 (read-only by default) is preserved: it is
  refused on a read-only tree, the same way `reap_inner` already refuses at
  `tree.rs:2451`.
- We accept a documented, detected fork limitation until step 7 lands. That is a
  worse outcome than closing it, and a better one than pretending the byte means
  something it does not.
- **We also accept a documented `RESERVED` leak** until open question 6 is
  answered: one slot lost per kill landing between `participant.rs:156` and
  `:168`, unreclaimable by anybody. Bounded and rare; recorded rather than
  claimed closed.
- **`ParticipantRecord::state` acquires a second writer**, and every future edit
  to that word inherits the obligation. Today `fill_slot` and `release` are the
  only writers and both are the occupant. After this, a peer writes it too, and
  the *only* thing keeping that sound is that every observable word carries an
  incarnation. `RESERVED` does not, which is the whole of open question 6, and
  any future sentinel added to that word has to answer the same question before
  it ships.

---

## What a consumer should do today, on 0.0.2

**Plainly: there is no workaround inside the process, and there is one outside
it that is worse than it sounds.**

What is *not* broken, stated first because it changes how bad the advice has to
be: a participant that exits **cleanly** releases its slot — `Tree::drop`
(`tree.rs:2636`) is the one caller of `release`, and it runs on a normal return
from `main` and on a Python interpreter shutdown. A **read-only** participant
never writes a record at all, so it cannot leak one; its byte is released by the
kernel and the owner's `granted` bit clears on hangup. **The budget being spent
is 64 abnormal exits of read-write participants**, over the lifetime of the
segment.

> **`SIGTERM` is not a clean exit, and an earlier revision of this record said it
> was.** `grep -rn SIGTERM crates/` returns nothing: no crate in this workspace
> installs a handler, and neither Rust nor CPython installs one by default that
> unwinds. The default disposition of `SIGTERM` terminates the process **without
> running any destructor**, so a `SIGTERM`ed publisher leaks its slot exactly as
> a `SIGKILL`ed one does. Advice to "send `SIGTERM` instead" is therefore worth
> nothing on its own — it is worth something only to an application that has
> installed a handler which returns from `main` (or, in Python, lets the
> interpreter finalize). That is the application's job, not this library's, and
> saying so is the honest version. `tf_tree serve`'s "drain on `SIGTERM`" in
> §9 is unbuilt (§0.0), so it is not a counter-example.

- **`tf_tree participants` is the gauge, and it works.** It reads the lock file
  directly (`tf_tree_cli/src/lib.rs:1751`) and prints `stale` for any slot whose
  byte the kernel has released while an identity record remains
  (`lib.rs:1806`) — which is what #184's 30-minute confirmation showed. Note it
  reads the *lock file's* identity records, not the arena's participant table, so
  it shows the symptom and not the record that is actually wedging the assigner.
  Two tables, one of them displayed.
- **`tf_tree doctor` does not fire, and it is blind for the same reason as the
  assigner.** `TFT014` is titled "participant or claim slot leak" and implements
  only the claim half (`tf_tree_cli/src/checks.rs:1180`), predicated on
  `e.claimed && !e.claiming && e.owner_pid == 0`, where `owner_pid` comes from
  `view.participants().identity(slot)` (`doctor.rs:348`). A stale-`LIVE` record
  returns `Some`, so `owner_pid != 0`, so the check passes — in exactly the state
  it is named for.
- **There is no in-process recovery.** `Tree::reap_dead` reclaims claims, never
  participant records. Nothing public clears one.
- **Restarting the owner does not help.** The records live in the segment, and
  the segment outlives the owner: it is freed only when the last mapping drops
  (§3.9). The only recovery is to stop **every** participant — including
  read-only consumers and any `tf_tree top --attach` — so the memfd is freed, and
  start again.
- **`--force-new` is not an answer twice over.** There is no such flag
  (see *Corrections* above), and the capability it names (`CreatePolicy::Always`)
  abandons the arena rather than reclaiming a slot: survivors keep reading the
  old segment while the new process publishes into a new one. That is the
  divergence §3.4 step 4 exists to prevent, chosen deliberately.

**So the operational advice is:** install a `SIGTERM` handler in every read-write
publisher that returns from `main` rather than aborting — and verify it, because
a supervisor sending `SIGTERM` to a process with no handler achieves exactly
nothing here; use the `spawn` start method in Python (already the §14 advice, and
it closes the fork case above as a side effect); watch the `stale` count in
`tf_tree participants` and treat a nonzero one as a countdown from 64; and plan a
full stop-everything restart before it reaches it. **None of that is a
workaround** — it is a way of spending the 64 more slowly. There is no way to
get a slot back.

---

## Implementation plan

Steps 0–7 are **blocked on this record reaching `ready`** — all of them. This is
a crash-consistency protocol change, which `CLAUDE.md` routes through a record
rather than a PR, and the existing patch is the reason that sentence is in
`CLAUDE.md`. Step 8 is not blocked; it is an independent documentation defect.

Test obligations are recorded in `docs/PHASE1.md` §10 (loom cases in §10.2) and
`docs/PHASE2.md` §11 (§11.2 integration scenarios, §11.3 crash points). `just
loom` is the model checker; `just shm-check` is what compiles anything
`#[cfg]`-ed on `shm`, and every new `shm`-only target belongs on its list in the
commit that adds it.

0. **Amend `PHASE2.md` §5.1 and §3.3, in §1's numbered-amendment shape, before
   any code depends on them.** §5.1's last paragraph enumerates the permitted
   uses of `(pid, start_time)` and asserts it "is no longer on any
   correctness-critical path"; §3.3 asserts `/proc` is "no longer on the
   rendezvous path at all". Piece 2 falsifies the first and piece 3 falsifies the
   second. An amendment that arrives *after* the code it licenses is an
   interpretation rather than an amendment, which is the failure this record was
   opened about. The amendment says: exactly one predicate may consult the
   identity triple; it may only ever **refuse** a reclamation; no other
   correctness path may read it; and §5.1's enumeration drops `--force-new`,
   which does not exist. **First, because a normative section cannot be widened
   by the change that needs it widened** — and it is the only step that can be
   written and reviewed with no code at all.
   It carries two further corrections of the same kind, because they are what
   made the ordering hard to derive in the first place: **§5** says slot
   assignment needs "no CAS protocol" and that "the record is written by the
   *owner* … before the response is sent", and the code does neither — the
   joiner writes it, with a CAS, after taking its byte; and **§11.3's
   `attach.after_slot_assigned_before_publish` row** inherits that error, saying
   "lock byte never taken", which is true only of the byte-less
   `Tree::attach_shared` path.
   *Verified by:* `just artifact-versions`, and by the amendment being cited by
   number at the definition of the predicate in step 2.
1. **`ParticipantTable::reclaim`** in `tf_tree_core`, accepting `live_word(inc)`
   **only**, plus its doc comment carrying the three-row `state`/`identity` table
   above **and the reason `RESERVED` is excluded** — a doc comment that does not
   say why will be "fixed" by the next reader who notices the omission.
   *Verified by:* a loom case `reclaim_races_register` in
   `crates/tf_tree_core/src/loom_tests.rs` — a reclaimer observing `W` and a
   registrant CASing `FREE -> RESERVED` on the same slot never both succeed, and
   the slot never ends with two owners; unit tests that `reclaim` fires from
   `live_word(inc)` and **fails when the observed word has changed**; and a unit
   test that it **refuses `RESERVED`**, which is the regression test for the
   defect this record's first revision shipped. Add the loom case to
   `PHASE1.md` §10.2's list.
2. **The predicate, once.** A single private function in `tf_tree`, taking the
   `LivenessProbe` and the record, returning `Reclaimable | Live | Unknown`. It
   must not be built on `record_is_alive` (piece 2's first constraint), must skip
   this process's own slot (second), and must assert the byte/record slot
   correspondence rather than assume it (third).
   *Verified by:* four multiprocess tests in `crates/tf_tree/tests/` — target
   `SIGSTOP`ped (byte held ⇒ `Live`), target `SIGKILL`ed (both negative ⇒
   `Reclaimable`), **a live `attach_shared(ReadWrite)` participant with no byte
   at all (⇒ `Live`, via `/proc`)**, and **the sweeper's own slot (⇒ `Live`,
   unconditionally)**. The third is the eviction test and is the reason the
   predicate has two facts; it must exist before step 3.
3. **The assigner decides from the byte** and reclaims before granting
   (`open.rs:709` at `HEAD`; `703` at `f058f4f`).
   *Verified by:* §11.2 scenario 2b above — 128 sequential attach-then-`SIGKILL`
   cycles against a 64-slot arena, every attach succeeding. Fails at HEAD on the
   65th, which is what makes it a falsifier rather than a regression test.
4. **The hangup fast path**, i.e. the existing patch, rebased onto `reclaim` and
   onto the observed word rather than a separately-loaded incarnation. It does
   **not** gain `RESERVED` collection; that is open question 6.
   *Verified by:* a multiprocess test asserting the record reads `FREE` after the
   owner's `epoll` wakeup and **before** any new attach — which is what
   distinguishes the fast path from step 3 doing the work.
5. **`Tree::reap_participants()`**, refused on a read-only tree.
   *Verified by:* kill the **owner**, then have a surviving read-write
   participant call it and assert the owner's slot returns to `FREE`. No hangup
   can ever cover this case, so it is the test that proves "not owner-only" is
   real. Extend `shm_torture` to kill the owner — §0.0 currently records that it
   does not, and that gap is why this went unseen. *(That file is owned by
   another lane right now; coordinate rather than racing it.)* **An owner-kill
   mode must not assert on new joiners.** §3.5 is unwired, so after the owner
   dies every fresh `open()` correctly fails `ArenaHeldButUnreachable` (§0.0
   spells out the whole sequence). The mode tests survivor reclamation, not
   rejoin, and a harness that conflates the two reports a §3.5 gap as a
   reclamation failure.
6. **`TFT014` gets its participant half**, and its claim half stops reading
   `state`: use `Tree::participant_alive` (`tree.rs:2524`), which already
   composes `state == LIVE` with the injected liveness function, in place of
   `identity`. Report (a) a non-`FREE` record whose byte is free and whose pid is
   gone, and (b) the fork case — byte held, recorded pid gone — with a distinct
   message naming the start method.
   *Verified by:* a `doctor --json` test over a fixture arena carrying a
   stale-`LIVE` record, asserting `TFT014` fires; and a negative test that it
   does not fire on a healthy arena mid-attach.
   **No new `TFT` id.** `TFT014`'s title already claims this exact ground, a
   second id would be the "second spelling" `CLAUDE.md` forbids, and `0027`'s
   plan has already claimed `TFT020`. `doctor` will need the lock file, which
   `tft014` does not take today; `cmd_participants` (`lib.rs:1768`) is the
   pattern.
7. **The fork handler closes inherited descriptors** (§6.2/§7.3). Carries open
   question 1 and may need its own record — it changes a normative protocol.
   *Verified by:* extend `crates/tf_tree_bench/tests/fork.rs` — fork after
   attach, `SIGKILL` the parent, assert the owner observes `HUP` **while the
   child is still running**, and assert the child's `Tree` is still poisoned and
   its destructors still release nothing of the parent's.
8. **`--force-new`**: either add the flag to `tf_tree_cli` or delete
   `RUNBOOK.md:414`. *Not blocked on this record.*
   *Verified by:* `just artifact-versions`, which already gates that every
   `just <recipe>` reference in docs resolves — the same class of check, one
   surface over.

Documentation, landing with the step that makes each true: §5.1's last paragraph
and §3.3's are amended **first**, in step 0, because piece 2 falsifies both;
§11.3 gains the rows above and its `attach.after_slot_assigned_before_publish`
row's *state* column is corrected (step 3 and step 4); §11.2 gains scenario 2b
(step 3); **§5 gains a note that the joiner, not the owner, writes the arena
record** — §5's "the record is written by the *owner* … before the response is
sent" describes a protocol this code does not implement, and §11.3's row text
inherits the error (step 0, with the rest of the §5-family corrections); §5.1
gains a pointer to the one predicate (step 2); §0.0's rows for §3.9 and reaping
change from unmet to met (step 5).

**§0.0's Reaping row does not become fully true even then.** It reads "**Done** —
any read-write participant reaps", which is true of claims and, after step 5,
true of `live_word` participant records. It stays false of `RESERVED` ones until
open question 6 is answered, and the row has to say which.

---

## Open questions

Resolved before `draft -> ready`. A `ready` doc has none.

1. **Is `Tree::attach_shared(ReadWrite)` a supported deployment shape?** This is
   **not a loose end — it is an input the Decision above already consumed**, and
   listing it here understates it: piece 2's second conjunct exists *only* to
   protect this class, so the predicate was designed around an unanswered
   question. Answer it first and the rest follows; leave it open and the record
   is deciding a shape it cannot justify.

   The byte-less class produces an arena record with no lock byte, so the byte
   conjunct cannot see it. An earlier revision said the `/proc` conjunct "fails
   safe but also fails *silent*: on a host where `/proc` is unreadable, no slot
   is ever reclaimed and the wedge returns quietly". **That has it backwards** —
   see *"the fail-safe claim is false on this code"*: an unmounted `/proc`
   returns `ENOENT`, which `read_start_time` classifies as `NoSuchProcess`, which
   is the *proof of death* branch. On such a host the conjunct votes **dead** for
   everyone and the byte-less participant is evicted while it runs. The failure
   mode is eviction, not a quiet wedge, and it is the one this conjunct was added
   to prevent.

   **Recommendation: answer "no", and collapse the predicate to the byte alone.**
   What that buys: §5.1 and §3.3 need no amendment, so step 0 disappears; the
   `/proc` parsing trap stays a diagnostic rather than becoming a correctness
   dependency; the `ENOENT`, `hidepid` and `start_time == 0` failure modes above
   all vanish rather than needing a third answer and a test each; and the
   predicate becomes one syscall whose answer is the kernel's. What it costs, and
   it is a real cost rather than a formality: `attach_shared` is public API on the
   facade, and `PHASE2.md` §0.0's §5.1 row names fd-inherited trees as a class
   `/proc` is deliberately retained for — so "no" is a retraction of something the
   spec says, and it needs its own line in step 0's amendment rather than a
   silent deprecation. The cheap form is to keep `attach_shared(ReadOnly)`
   unchanged (it registers no record and cannot leak one) and make the
   `ReadWrite` arm either refuse, or take the `u32::MAX` sentinel and lose the
   ability to claim — which is a functional loss that has to be named, because
   `crates/tf_tree_bench/tests/multiprocess.rs:398` claims through it today.

   Not recommended: keeping both conjuncts because two facts feel safer. The
   second fact is not free, it is three new failure modes and an amendment to a
   NORMATIVE section, bought to protect one caller that is a bench test.
   **This changes the predicate, so it gates `ready`.**
2. **Should `reclaim` also clear the lock file's identity record** at
   `4096 + 64·i`? Leaving it means `tf_tree participants` keeps printing a
   truthful `stale` row for a slot that has since been reused, which is
   confusing; clearing it deletes the evidence an operator uses to diagnose. My
   leaning is to leave it and have the next registrant overwrite it, which is
   what `register_at` already does — but that is a leaning, not an answer.
3. **What does a taking-over participant do with its existing slot?**
   `already_attached` routes to `register_any`
   (`tf_tree_ipc/src/open.rs:323–332`), taking a second byte and a second arena
   slot while the first session still holds both. Does the heir reuse its slot,
   and if so which session holds the byte? **This changes step 5's sweep**, so it
   gates `ready` — even though wiring §3.5 itself is out of scope here.
4. **What does the sweep cost?** 64 `F_OFD_GETLK` plus up to 64 `/proc` reads,
   against a 97.5 µs p50 attach. Unmeasured, and this repository does not accept
   a number without the command that produced it. Measure it before step 3, and
   decide then whether it needs a bound (e.g. sweep only on the first
   `NoParticipantSlots`, rather than on every grant).
5. **Can step 7's atfork handler be built inside the async-signal-safety
   constraint?** It needs a lock-free registry of the fds to close, populated
   before any fork; `fork.rs:46–56` is the constraint it has to satisfy. If not,
   the fork case stays a documented limitation and step 7 becomes its own record.
6. **What collects a `RESERVED` record, and at what cost?** This is the question
   the first revision of this record answered by guessing, and the guess put two
   live processes on one slot. §11.3 promises "record cleared by any reaper" and
   nothing in this decision clears one. The three shapes are costed in
   *"`RESERVED` is not a word this predicate may act on"*; the sub-questions a
   chooser has to settle are:
   (a) Does `fill_slot`'s publishing `store` become a `compare_exchange`? That is
   the floor for *any* answer other than "leave it", it costs one CAS on a
   97.5 µs path, and it changes what `register`/`register_at` can return.
   (b) If `RESERVED` gains an incarnation, `fill_slot` must bump `incarnation`
   *before* its state CAS rather than after — which is itself a crash-consistency
   reordering and needs its own loom case and §11.3 walk.
   (c) Is a bounded, rare, permanent one-slot leak the better trade? It would be
   the first place this project accepts an unrepairable state, against §0's
   framing sentence ("there must be no state a dead process can leave behind that
   a live process cannot detect and repair"), so choosing it means amending §0
   and not merely §11.3.
   **This gates `ready`.** It is the only open question whose wrong answer is
   corruption rather than delay.

## What would make this `ready`

- Questions 1, 3 and **6** answered; they change the predicate, the sweep, and
  whether `fill_slot` itself changes. **6 is the blocker** — the others change
  scope, 6 changes whether the shape is sound. **Answer 1 before 6**: if the
  byte-less class is unsupported, the predicate collapses to the byte, step 0
  disappears, and three of this record's failure modes go with it.
- **The `PHASE2.md` §5.1 and §3.3 amendment written and agreed (step 0)**, or
  question 1 answered in the way that removes the need for it. As long as piece 2
  reads the identity triple, this record is asking a NORMATIVE section to mean
  something it does not say, and that must be settled on the page before any code
  cites it. A `ready` record whose predicate contradicts §5.1's last paragraph is
  the same defect as the assigner it was opened about.
- Question 4 measured, with the command.
- **#184's pre-patch numbers re-taken on a provably pre-patch binary.** The
  numbers in *Context* are not mine and the artifact on this host can no longer
  produce them. *(Partly discharged: the orchestrator has since run the same
  harness against a reverted engine and it reports `63 of 64 participant
  slot(s) hold a LIVE record for a process the kernel says is dead` and
  `the arena was being written on only 75/269 observation rounds`, exit 1 —
  which establishes the harness detects the class. It is not yet the paired
  before/after on one binary lineage that this bullet asks for, and whoever
  takes it should record the two commands side by side.)*
- The existing patch explicitly adopted as step 4 — **not landed as an
  independent PR**. It is a crash-consistency protocol change; a green
  `shm_torture` against a harness whose children do not fork and whose owner is
  never killed is not the evidence it needs, and this record's whole reason for
  existing is that the difference matters.

## Review history

A review pass re-derived the ordering and the crash matrix from the code and
**refuted one of this record's own decisions**, which is recorded here rather
than silently edited away:

- **Refuted:** that `reclaim` may accept `RESERVED`, and that doing so closes
  §11.3's `attach.after_slot_assigned_before_publish` row. It does not; the
  interleaving is written out above and the outcome is two live participants on
  one slot index. Piece 1 is narrowed to `live_word(inc)`, the two crash-matrix
  rows that asserted otherwise are rewritten, and the gap becomes open question 6.
- **Refuted:** that `reclaim.probe_then_reoccupied` is unreachable. Pieces 3 and
  4 guarantee two reclaimers; the row is reachable and is safe only because
  `live_word` carries an incarnation.
- **Refuted:** that candidate D "preserves §5.1, literally". The predicate as
  specified was built on `record_is_alive`, which opens with a `state` test.
  Piece 2 now forbids that composition and the preservation claim is qualified.
- **Refuted:** that a `/proc` conjunct "can only ever be conservative". On this
  code an absent `/proc` yields `ENOENT` → `NoSuchProcess` → *proof of death*,
  so the conjunct inverts rather than going silent, and a stored `start_time` of
  `0` makes a live slot unconditionally reclaimable. Both are set out under
  *Candidate D — what it weakens*, and both change what step 2 owes.
- **Refuted:** that `Tree::drop` runs on `SIGTERM`. No handler exists anywhere in
  the workspace, so the operational advice was recommending a signal that leaks
  the slot exactly as `SIGKILL` does.
- **Corrected:** `server.rs:439` → `:440`; §5's owner-writes-the-record model
  versus the shipped joiner-writes-it code, and the §11.3 row text that inherits
  the error; the byte/record index correspondence, which holds on the joiner path
  only; the missing own-slot guard on the sweep; and the `TookOver` arm building
  a second segment.
A second pass took the `/proc` conjunct's admissibility as its subject, and
refuted three more:

- **Refuted:** that "either conjunct answering *unknown* means not reclaimable
  (§6.2)" describes this code. `read_start_time` splits on `io::ErrorKind` and
  maps `ENOENT` to `NoSuchProcess`, the proof-of-death branch. An unmounted
  `/proc` — a hardened container — therefore votes **dead** for every
  participant, and the byte-less class the conjunct exists to protect is evicted
  on sight. The conjunct is not conservative on that host, it is inverted.
- **Refuted:** that an unpopulated `start_time` merely degrades the conjunct to a
  bare-pid check. `process_start_time()` returns `0` on failure and `0` is
  compared as a value, so a real `st` never matches it and the verdict is *dead*
  about a running process. Weaker would have been survivable; inverted is not.
- **Refuted:** that §3.3's "they remain only for the arena's advisory participant
  table" licenses the placement. §3.3's sentence splits on the **path**, and the
  reply split on the table; the assigner runs inside the owner's `serve` loop
  answering a `HelloRequest`, which is the rendezvous path by §3.7's own
  numbering — and this record prices it as such.
- **Escalated:** the §5.1 conflict from "amend as part of step 7" to **step 0**.
  §5.1's last paragraph is an enumeration plus a positive claim that the decision
  falsifies, `PHASE2.md` §1's A1–A8 are the precedent for amending a normative
  section *before* the code that depends on it, and an amendment that trails its
  own implementation is an interpretation. Also noted: §5.1's enumeration
  licenses the triple for "the `--force-new` path", which this record shows does
  not exist, so one of the two permitted uses is fictional.
- **Escalated:** open question 1 from a loose end to an **input the Decision
  already consumed**, with a recommendation to answer it "no" and collapse the
  predicate to the byte alone — which deletes step 0 and all three failure modes
  above, at the cost of a named retraction of `PHASE2.md` §0.0's fd-inherited row
  and of `multiprocess.rs:398`'s ability to claim.

- **Checked and upheld:** the step-ordering table (byte before record, on every
  path that has a byte); the fd-inherited read-write class and its line
  reference; the `granted`-bitmask reasoning; `TFT014`'s blindness and its
  `doctor.rs:348` cause; `--force-new`'s absence; the 97.5 µs p50 attach number,
  which is `PHASE2.md` §12.2's own measured row from `just attach-bench`; the
  "no arena field, no `FORMAT_VERSION` bump" claim, which survives every shape
  costed here including a re-encoded `RESERVED`, because `state_of` masks to the
  low two bits and `layout_hash` hashes strides.
