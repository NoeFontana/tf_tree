# tf_tree — Phase 2 Implementation Specification: Shared Memory

> **Companion documents:** `docs/PROJECT.md` (roadmap, decision log) and `docs/PHASE1.md` (single-process core). Read §1 before writing any Phase 2 code: it amends the Phase 1 design.

**Deliverable:** the same arena, mapped into N processes, with the *identical unmodified* Phase 1 reader code running against it, plus the lifecycle, liveness and fault-tolerance machinery that makes that safe when processes die at arbitrary points.

**Every mutation protocol in the arena must be crash-consistent: no state a dead process can leave behind may be undetectable or unrepairable by a live one.**

Sections marked **NORMATIVE** are requirements. Syscall behaviour asserted here was verified on Linux 6.18; the probe is in Appendix B.

## 0.0 Implementation status

**Implemented**, except `tf_tree serve` (§9), declined and not scheduled. §10's recorder and §7.4's memory locking are **declined by record** ([`0047`](./decisions/0047-the-recording-this-reader-would-refuse.md), [`0049`](./decisions/0049-the-flag-that-prefaults-the-arena.md)). Where a row and any prose disagree, the row wins.

| Area | Status |
|---|---|
| Amendments A1–A8 (§1) | **Applied** — `FORMAT_VERSION` 3 |
| `MappedArena` — `memfd`, sealed, `MAP_SHARED`, `MADV_DONTFORK`/`HUGEPAGE` (§4) | **Done** (`tf_tree_arena::mapped`, behind `--features shm`) |
| Amendment A2 — topology lock | **Applied, and no longer only in-arena** ([`0029`](./decisions/0029-the-topology-lock-is-a-kernel-lock.md)): on a tree with a lock file `Tree::reparent` takes §3.3's byte 1 first and the arena word second. The `/proc` predicate remains only as a residual that may withhold a steal. |
| `instance_uuid` (§3.6 step 4, A7) | **Done** — header offset 136, in existing padding |
| Discovery, rendezvous, `open()` (§3.1–§3.4) | **Done** (`tf_tree_ipc`, `tf_tree::open`). There is no `tf_tree::open_named`; the named open is `Open::new().name(n)?.open()`. |
| §3.4's `--force-new` escape hatch | **The capability shipped; the flag never existed.** It is `CreatePolicy::Always` on `tf_tree::Open`, and passes iff nothing is serving **and** the ownership byte is free **and** participant byte 0 is free (`the_escape_hatch_creates_over_a_stranded_participant`, `a_live_byte_0_refuses_both_policies`, both `crates/tf_tree/tests/rendezvous.rs`). A flag belongs to the subcommand that owns a topology ([`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md) §1, `tf_tree serve`). §3.4 and §5.1 are read against this row. |
| Attach protocol — `SOCK_SEQPACKET` + `SCM_RIGHTS` (§3.7) | **Done** — owner serves from a thread, not a daemon |
| Ownership migration (§3.5) | **Done; the mechanism is complete and the *trigger* is the caller's, by design.** `Session::take_over_ownership` takes byte 0 on the description the session already holds and `Tree::inherit_ownership` binds and serves the existing segment, returning `Inheritance::{Inherited, OwnerAlive, Contended, ReadOnly, NotApplicable}` ([`0037`](./decisions/0037-a-takeover-is-not-a-second-open.md)). `Tree::owner_lost` polls the attach socket for hangup and then `F_OFD_GETLK`s byte 0: it answers "the arena has no owner", not "my socket is dead" ([`0043`](./decisions/0043-owner-lost-is-a-question-about-the-owner.md)); a `false` or a single non-`Inherited` answer is never final ([`0057`](./decisions/0057-an-owner-is-not-dead-until-its-files-close.md)). Tests: `a_survivor_inherits_ownership_and_the_arena_becomes_joinable_again`, `two_survivors_race_and_exactly_one_inherits`, `scenario_3_an_owner_dying_leaves_readers_working_and_joins_refused`, `a_guard_may_be_held_across_inheriting_ownership`. Owed by the fleet: calling the trigger ([`0055`](./decisions/0055-the-recovery-capacity-a-fleet-cannot-add-later.md)). §3.5's literal reconnect is not implemented. |
| Participant registry — owner-side slot assignment (§5) | **Done.** "The arena slot and the lock byte are the same integer" is checked: `Open::attempt` compares `Session::slot` with `Tree::participant_slot` and returns `OpenError::ParticipantSlotDiverged` before binding ([`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md) step 0c). A `LIVE` record at index *i* whose byte *i* is unheld reads dead to every probe-carrying observer, so `Tree::reap_participants` must not run in a process tree where anything served a `build_shared` arena by hand — **out of contract, permanently** ([`0031`](./decisions/0031-the-participant-record-with-no-byte.md)). |
| §5.1 liveness from `F_OFD_GETLK` | **Done for a tree from `tf_tree::open`** — both arms of `Open::attempt` install the probe and nothing else does. Every other tree keeps `/proc`. |
| §5.1's "no longer on any correctness-critical path" | **False in two places, both `crates/tf_tree/src/tree.rs`**: `use_ofd_liveness`'s fallback when `F_OFD_GETLK` declines to answer, and `liveness_for` for a tree with no probe (heap, `build_shared`, `attach_shared`), where `record_is_alive` decides A8's intern takeover and `Tree::participant_alive`. `Tree::reparent`'s steal is closed for a tree with a lock file ([`0029`](./decisions/0029-the-topology-lock-is-a-kernel-lock.md)) and open for one without. The predicate fails toward "alive". Dependencies: §3.10 (same user, so `hidepid` cannot hide a participant) and a shared PID namespace, since `ParticipantRecord` carries no namespace discriminator ([`0033`](./decisions/0033-the-identity-record-cannot-name-a-namespace.md)). Moving these predicates off the `/proc` triple is a decision record. |
| Reaping (§6.3) | **Done for claims and participant records, by any read-write participant; the participant sweep is on demand.** `Tree::reap_dead` / `reap_participant` for claims; `Tree::reap_participants` ([`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md) step 5) sweeps the table through the one reclamation predicate and is refused on a read-only tree or one with no lock file. The owner's slot assigner and hangup callback are the other collectors. `a_survivor_reaps_the_killed_owners_slot_which_no_hangup_can` pins "must not be owner-only". A forked child keeps the parent's description, so its byte reads held and is deliberately not reclaimed ([`0030`](./decisions/0030-the-atfork-handler-and-inherited-descriptors.md)). |
| Fork poisoning (§7.3) | **Done** — `pthread_atfork` counter; five destructors guarded |
| Per-edge page population (§7.1) | **Done** |
| Memory locking (§7.4) | **Declined by [`0049`](./decisions/0049-the-flag-that-prefaults-the-arena.md).** No `LockPolicy` or `mlock` exists; `docs/API.md` §8.3 gives the reason. `TFT016` compares `RLIMIT_MEMLOCK` to arena size. |
| `tf_tree serve` (was `tf_treed`, §9) | **Not implemented, not scheduled.** Superseded by [`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md) (steps 6–7 not built). |
| `tf_tree_record` (§10) | **Declined by [`0047`](./decisions/0047-the-recording-this-reader-would-refuse.md).** §10(c), the NORMATIVE heap-against-mapped bit-identity test, **is met**: `crates/tf_tree_cli/tests/replay_bit_identity.rs`, run by `just shm-check`, not `just test`. |
| `/tf` ingest bridge; `doctor` / `top` / `participants`; CLI `--attach` | **Done.** Bridge: `docs/PHASE4.md` §5. `tf_tree top`: `docs/PHASE5.md` §0.0's §7 row. |
| Fault injection (§11.3) | **Implemented.** Thirteen of the fourteen rows carry a `crash-points` site (seven in `tf_tree_core`, six in the facade) and all are driven by a test that fires them. Completeness gates: `crash_tests::the_published_site_list_is_the_one_the_tests_arm` (core) and `the_facade_site_list_is_pinned_by_index_and_every_site_has_a_test` (facade). The fourteenth, `reclaim.probe_then_reoccupied`, is an interleaving of two live processes, not an abort site; `loom` and §11.4 reach it. `shm_torture --crash-points` arms these sites; a run without it must not be quoted as §11.3 coverage. |
| `shm_torture` (§11.4) | **Done; it kills the rendezvous owner. Two of §11.4's four invariants are checked continuously, a third at teardown, and the fourth is not implemented — carry that qualifier when quoting “invariants checked continuously”.** `crates/tf_tree_bench/src/bin/shm_torture.rs`: N processes on one arena doing random attach/detach/claim/reap/push/lookup through the real rendezvous, the driver `SIGKILL`ing one several times a second and replacing it. Every child evaluates `owner_lost` and calls `inherit_ownership`; each owner kill must be followed by a *fresh* `Open::new().create(Never)` join inside 10 s and a recorded inheritance, or the run fails (`--no-inherit` is the negative control). Invariants: *no non-unit quaternion or NaN* — every read; *no two writers on one edge* — every push; *no slot leaks* — teardown only, in `check_recovery`; *arena hash stable across quiescent points* — **not implemented**. Recipes: `just shm-torture` (30 min nightly), `shm-torture-asan`, `shm-torture-crash-points` (own nightly job; `--features shm,crash-points`), and `shm-torture-self-test` (in `just shm-check`; proves the detector still detects). An owner kill is deferred, retried every 250 ms (`OWNER_KILL_DEFERRAL_RETRY`), and fails only after `3 × --owner-kill-every` of unbroken deferral; the floor counts owner-kill *attempts*. `--crash-points` refuses when nothing was armed and, separately, when nothing fired; **`aborted` is a floor rather than a count**. A `kill.in_progress` marker makes a child stay instead of detaching for the width of one reap (§3.4 step 4); the run prints the suppressed detaches. The recipes run under `prlimit --core=1:1` ([`0057`](./decisions/0057-an-owner-is-not-dead-until-its-files-close.md) Decision 6). **Twelve of thirteen sites fire in this workload** (`--crash-site NAME[:nth]`); `topo.holding_lock` never (nothing reparents). Per-site coverage lives in the targeted tests. §12.3 gate 3's “every §11.3 crash point recovers” is partly measured here and fully by the targeted tests. |
| §3.8's generous default layout | **Superseded by decision `0004`**, which sizes the arena from declared edges. |

## 0. Scope

### In scope

`MappedArena` (memfd, sealed, `MAP_SHARED`); zero-config discovery and rendezvous with kernel file locks as the election; the `SOCK_SEQPACKET` + `SCM_RIGHTS` attach protocol; ownership migration; the advisory participant registry with OFD locks authoritative for liveness; claims as kernel locks; crash-consistency of every mutation protocol (§1); read-only attach; mapping policy; the `/tf` ingest bridge; `doctor`, `top`, `participants`. Not crates: `tf_treed` is `tf_tree serve` ([`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md)); `tf_tree_record` is declined ([`0047`](./decisions/0047-the-recording-this-reader-would-refuse.md)).

### Out of scope — NORMATIVE

Everything excluded in §0 of `PHASE1.md` remains excluded. Additionally: network and discovery beyond one host (Phase 6); Python bindings (Phase 3, §14); the `tf2_ros::Buffer` shim (Phase 4; the ingest bridge is one-way); macOS/Windows shared memory (§2); multi-arena federation (one arena per `(runtime_dir, domain, name)`, §3.1); any security boundary against a malicious RW peer (§3.10; say this out loud in the docs); dynamic arena resize (D4).

## 1. Phase 1 amendments — NORMATIVE

Working the crash matrix found eight places where the Phase 1 design was wrong. A6 changes the layout; together they constitute `FORMAT_VERSION = 2` and no version-1 arena may be attached.

### A1 — Pack the topology generation and active index into one atomic word

**Problem.** A writer `SIGKILL`ed after bumping `topo_generation` to odd leaves it odd forever and every reader spins in plan compilation.

**Fix.** The writer mutates an *inactive* block; the active block is never mutated in place, so publication is a single store. `TopoWord(AtomicU64)`: bits 63..8 = generation (monotone), bits 7..0 = active block index; `pack(gen, active) = (gen << 8) | active`.

The writer (holding A2's lock) copies the active block into the next of `TOPO_BLOCKS`, mutates it, and publishes with one `Release` store of `pack(gen + 1, next)`. A reader (plan compilation only; `Plan::at` never touches this) loads the word with `Acquire`, walks the active block bounds-checked under a `max_frames` step budget, and accepts the walk only if a final `Acquire` fence and reload see the same word; after `TOPO_RETRY_LIMIT` attempts it returns `TopologyChurn`.
A dead writer leaves the arena indistinguishable from no write having happened. **`TOPO_BLOCKS = 4`**: a reader is hit only if the writer flips four times mid-read, so `TopologyChurn` is unreachable outside a torture test. **Topology block arrays are `[AtomicU32]` and `[AtomicU16]`.** A non-atomic read racing another process's write is UB even when the value is discarded. **Every index read from a topology block must be bounds-checked and the parent walk capped at `max_frames` steps**, because garbage from a lost race must not panic before the validity check. 

### A2 — The topology mutation lock lives in the arena and is reapable

`TopoLock` is one `#[repr(C, align(64))]` word `owner: AtomicU64` (0 = free, else participant_slot + 1) plus `acquired_at_nanos: AtomicI64`.
Acquire is `compare_exchange(0, slot + 1, AcqRel, Acquire)` with bounded spin. On failure, resolve the owner (§5) and check liveness (§6.2); if dead, CAS to steal. A1 makes an abandoned mutation leave no trace, so stealing needs no rollback. 

### A3 — Claim ownership is a participant slot, not a PID

A writer killed between the state CAS and a separate PID store would leave a permanently leaked edge. One atomic word carries state and identity; the identity is an indirection into a participant record fully written at attach.

`ClaimRecord` is `#[repr(C, align(64))]`: `owner: AtomicU64` (0 = free, else participant_slot + 1; claim and identity publish atomically), `epoch: AtomicU64` (incremented on every reap and every successful claim; fences zombies, A4), `heartbeat: AtomicU64` (advisory; NEVER a reaping trigger, §6.4), `clock_offset_nanos: AtomicI64`.
Claim: `owner.compare_exchange(0, slot + 1, AcqRel, Acquire)`, then `epoch.fetch_add(1, AcqRel)`; the `Publisher` records the epoch. §6.1 supersedes this record's reaping role; the slot indirection stays required. 

### A4 — `push` must verify the claim epoch

A stalled process can be judged dead, reaped, then resume and push to an edge another process owns: `if claim.epoch.load(Relaxed) != self.epoch { return Err(PushError::ClaimRevoked { edge }) }`. With §6.1 the zombie is impossible by construction; keep the check as defence in depth, and do not describe it in comments as the sole barrier.

### A5 — The slot sequence writer forces parity instead of incrementing

A writer killed between the odd and even `seq` stores leaves the slot odd; when the ring wraps the next writer's `s+1` lands even, inverting the protocol. The writer stores `odd = s | 1` (self-heals a stale odd), writes the data, then stores `odd.wrapping_add(1)` with `Release`. Additionally, **claim acquisition normalizes the slot at `head & mask`**.

### A6 — The arena gains a participant table (layout change)

New region of `max_participants * 128` bytes between the claim table and the edge table. `ArenaHeader` gains `participant_table_off: u32`, `max_participants: u32` and `participant_count: AtomicU32` from `_reserved`. Default `max_participants = 64`. **The only amendment that changes the layout.**

### A7 — Header identity fields

`ArenaHeader` gains `owner_start_time: u64` beside `creator_pid`, `boot_id: [u8; 16]` replaces the `u64`, and `instance_uuid: [u8; 16]` joins them (§3.6 step 4): two processes that print different `instance_uuid`s are on different arenas. All fit in `_reserved`.

### A8 — Interning must not spin forever on a dead claimant

A process that wins the hash-slot CAS and dies before publishing the id wedges every future interner of that name (`intern.after_hash_cas_before_id_store`, §11.3). Record the claimant beside the hash, bound the spin, take over from a dead claimant:
Add `claiming: [AtomicU32]` parallel to `hashes`/`ids`: the participant slot that won the hash CAS, + 1, written BEFORE the hash is published. After `INTERN_SPIN_LIMIT` iterations a waiter reads `claiming[i]`; if that claimant is not alive it CASes `claiming[i]` to its own slot and writes the record and id itself. The takeover is idempotent and CAS-guarded. `is_alive` is §6.2's predicate and fails **safe**: an unreadable `/proc` means "alive". Phase 1's `ID_FAILED` sentinel handles the *capacity* failure; A8 handles the *crash* failure. 

## 2. Platform, dependencies, feature gating

**NORMATIVE.** Shared memory is **Linux-only**, requiring **kernel ≥ 3.17** (`memfd_create`, `F_ADD_SEALS`) and **≥ 3.15** (OFD locks, §3.3). Target 5.15 and current stable. `MADV_POPULATE_WRITE` (§7.1) needs ≥ 5.14 and has a fallback. No POSIX abstraction layer: macOS and Windows keep `HeapArena`.

New dependencies: `tf_tree_arena` gains `rustix` (features `shm`, `mm`, `fs`, `net`); `tf_tree_ipc` gains `rustix` and **`libc` for `fcntl(F_OFD_*)` only**; `tf_tree_record` is retired ([`0047`](./decisions/0047-the-recording-this-reader-would-refuse.md)); `tf_tree_core` gains **none**.
`tf_tree_core` gaining a dependency here is a design failure; stop and report it. **The `libc` exception:** `rustix` 1.1 has no OFD locking (its `fcntl_lock` is classic whole-file `F_SETLK`, which §3.3 rejects); scope `libc` to `tf_tree_ipc` and that one call. 

## 3. Discovery, rendezvous, and ownership

A process calls `tf_tree::open()` and either joins the arena on this machine or creates it: no config file, no daemon, no start-order requirement, and no possibility of two processes silently on different arenas.
**Do not implement leader election — borrow the kernel's.** Linux file locks give mutual exclusion, automatic release on holder death, and a way to ask whether anyone holds it, with no timeouts or heartbeats. 

### 3.1 The sharing boundary is the runtime directory — NORMATIVE

Two processes share an arena **if and only if they resolve to the same runtime directory, domain, and name.** This should be the first sentence of the user-facing docs.

The rendezvous files are `<runtime_dir>/<domain>/<name>.lock` (rendezvous and kernel-managed liveness) and `<name>.sock` (`SOCK_SEQPACKET`, owner-bound, FD passing).
`runtime_dir`, first hit wins: `$TF_TREE_RUNTIME_DIR`; `$XDG_RUNTIME_DIR/tf_tree`; `/run/tf_tree` if writable; `/tmp/tf_tree-<uid>` (mode `0700`). **Containers:** sharing the runtime directory is a volume mount; not sharing it is complete isolation. Do **not** use abstract Unix sockets, which tie the boundary to the network namespace. **NORMATIVE check:** `statfs` the runtime directory at open and reject NFS (`0x6969`) and CIFS. 
**An arena no runtime directory names sits outside this boundary, and putting it back inside by hand is out of contract.** `TreeBuilder::build_shared` creates a segment whose fd is the capability: no `.lock`, no `.sock`, and so no byte for its participant record to be judged by. Binding an `OwnerServer` over it publishes it into a rendezvous anyway, and every joining peer judges the creator **dead** and reclaims its record and claims ([`0031`](./decisions/0031-the-participant-record-with-no-byte.md)). The supported way to serve a created arena is `tf_tree::Open`'s create arm, which takes its lock byte *before* it builds (§3.4).

### 3.2 Identity and defaults

`domain` is `$TF_TREE_DOMAIN`, else `$ROS_DOMAIN_ID`, else 0; `name` is `$TF_TREE_NAME`, else `"default"`.

A consumer is `tf_tree::open()` or `tf_tree::Open::new().name("robot")?.open()`: read-only, `CreatePolicy::Never`, joining or failing with `ArenaAbsent`. A consumer that may start before the publisher uses `.await_open(timeout)` ([`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md) §2b). A creator needs `.mode(AttachMode::ReadWrite).create(CreatePolicy::IfAbsent).layout_if_creating(layout)` (`IfAbsent | Never | Always`).
`CreatePolicy::Never` is the default ([`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md) §2a). `AttachMode::ReadOnly` with a creating policy is `OpenError::ReadOnlyCannotCreate`. 

### 3.3 The lock file — NORMATIVE

A small regular file used as a lock substrate with **open file description locks** (`F_OFD_SETLK`, Linux ≥ 3.15), not classic POSIX locks, which are dropped when *any* fd to the file closes anywhere in the process.

| Offset | Meaning |
|---|---|
| byte 0 | **Ownership.** Exclusive. The holder serves the socket. |
| byte 1 | **Topology mutation** (A2). Exclusive, held for one `Tree::reparent`. |
| bytes 2–15 | reserved |
| bytes 16 + *i* | **Participant liveness** for slot *i*. Exclusive, held for the lifetime of the attachment. |
| 4096 + 64·*i* | **Identity record** for slot *i*: pid (`0..4`), start_time (`4..12`), boot_id (`12..28`), mode (`28`), name (`32..48`), pid_ns_inode (`48..56`); `29..32` and `56..64` read zero. Written with `pwrite` before taking the slot lock. Advisory; diagnostics only. |

**`pid_ns_inode` is the `nsfs` inode of the PID namespace its writer's pid is drawn from; `0` means *unknown*** ([`0033`](./decisions/0033-the-identity-record-cannot-name-a-namespace.md)). A recorded `pid` is namespace-local and `boot_id` is identical across namespaces, so an observer compares it against its **own** namespace. This is the lock file, not the arena: no layout hash changed.

Verified: a second process taking a held byte gets `EAGAIN`; a holder that dies is released by the kernel at the end of its exit, after any core dump and address-space teardown ([`0057`](./decisions/0057-an-owner-is-not-dead-until-its-files-close.md)); `F_OFD_GETLK` reports `F_UNLCK` on a free byte and held with **`l_pid = -1`** on a held one.

An OFD lock belongs to a description, so `GETLK` cannot report a PID: the lock file answers *"is anyone alive?"* and *"who?"* comes from the identity records. **`/proc` parsing and PID-reuse defence are off the rendezvous path**; they remain only for the arena's advisory participant table (§5).

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
        backoff; continue   // an owner mid-bind, or another open() in steps 2-4 (0057)
    }

    // 3. DELETED (0037). Do not re-add it.
    // 4. SPLIT-BRAIN CHECK. Is any participant byte locked?
    if any participant byte is held {
        release byte 0; backoff; continue    // yield to the real participant
    }

    // 5. Serve.
    if creating {
        // creator's slot is 0; the ACQUIRE is the check (step 4 is a separate pass)
        if F_OFD_SETLK(participant byte 0, exclusive) fails {
            release the ownership byte; backoff; continue   // step 4, arriving late
        }
        pwrite identity for slot 0
        memfd_create; ftruncate; mmap; init header; seal (§3.6)
    }
    unlink stale sock; bind sock.tmp; chmod; rename -> sock; listen
    return Created
}
on timeout -> Err(ArenaHeldButUnreachable { holder_slots, first_slot, first_pid, ownership_held })
```

There is no takeover branch: a heir is already a participant and registers nothing ([`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md) question 3); §3.5 is a method on the session it already holds, never a re-entry into this algorithm ([`0037`](./decisions/0037-a-takeover-is-not-a-second-open.md)). A fresh `open()` against an ownerless arena with surviving participants times out until a survivor inherits.

**Step 4 is the whole design.** Without it the owner dies, a fresh process wins the ownership lock before survivors notice the `HUP`, and creates a *second* arena while the survivors keep the first. The check is **deterministic, not a grace period**: if any participant byte is locked a live arena exists and a fresh process must not create one.
**A creator takes participant slot 0, and the acquire is the check.** The facade indexes lock byte and arena record with **one** integer, and `any_participant_held` probes byte 0 first, so byte 0 can be taken for the rest of that scan. Steps 4 and 5 therefore share one `F_OFD_SETLK` on participant byte 0; contention is step 4's condition arriving late and takes step 4's branch. 
**The escape hatch (`CreatePolicy::Always`, §0.0's `--force-new` row) skips step 4 by design** and creates iff nothing is serving **and** the ownership byte is free **and** participant byte 0 is free. Joiners get slots `>= 1`, so the usual reason the three are free is a stranded participant. `Session::release_ownership` (§3.5) frees the ownership byte and keeps byte 0, and the hatch still refuses. It cannot pass a live holder of byte 0 or the ownership byte; the remedy is to stop that process, after which an ordinary `IfAbsent` create works. Never take the path automatically.
The timeout case is correct behaviour: if a participant is `SIGSTOP`ped and never takes over, no new process can join, because the alternative is divergence. The error names the stuck slots and identities. 

### 3.5 Ownership migrates; the data plane never pauses — NORMATIVE

Ownership is a **role**: the arena is the memfd, which lives as long as any mapping; the owner is whichever participant holds byte 0 and the listening socket. A surviving **read-write** participant that observes the owner's hangup inherits the role:

Lookups keep running from the existing mapping. The survivor, in its own loop, polls its own attach socket for `POLLHUP`/`POLLERR`. On hangup it `F_OFD_GETLK`s byte 0 ([`0043`](./decisions/0043-owner-lost-is-a-question-about-the-owner.md): is the *role* vacant?). **Held**: somebody took over, is mid-bind, or a fresh `open()` holds it passing through §3.4 steps 2–4 ([`0057`](./decisions/0057-an-owner-is-not-dead-until-its-files-close.md)); nothing to do this call. **Free**: a read-only attachment cannot be the heir (D18); otherwise `F_OFD_SETLK(byte 0)` **on the description the session already holds**. Acquired: bind a pid-suffixed socket, listen, rename it over the rendezvous path, serve the existing segment fd; on any failure release byte 0 and stay a plain participant. Contended: keep the slot and retry on the next poll; no single non-`Inherited` answer is final.

Five requirements:

1. **The ownership lock is taken on the file description the session already holds. A takeover may not be expressed as a second `open()`.** From a fresh description `F_OFD_GETLK` cannot distinguish "I hold byte *n*" from "a live peer holds byte *n*".
2. **The heir keeps its existing participant slot, byte and arena record and does not register again.** The slot is baked into every claim and topology guard it holds (A3); a second registration would arrange for its own live claims to be reaped.
3. **The heir serves the segment it already has**, never a fresh `memfd_create`, which would fork the tree.
4. **Publication is a `rename`, not unlink-then-bind**, so a client sees the old socket or a listening one, never a half-built one (§3.7).
5. **Serving must stop before byte 0 is released**, or a successor can bind while the old owner still answers handshakes. Expressed as **field declaration order** on `Attachment::Owner` (RFC 1857 drop order), not an `impl Drop`.

**NORMATIVE.** `owner_lost()` answers `true` once the survivor's attach connection has hung up and the last open file description holding byte 0 has closed ([`0043`](./decisions/0043-owner-lost-is-a-question-about-the-owner.md)). For a dying owner that is the end of its exit, including any core dump and address-space teardown; a `fork` child sharing those descriptions holds them until it exits. tf_tree adds no delay, heartbeat or timeout (D17) ([`0057`](./decisions/0057-an-owner-is-not-dead-until-its-files-close.md) Decision 5).
**The trigger is the caller's, and this is NORMATIVE too: no background thread, no daemon, no watcher a user must run** ([`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md)). A survivor evaluates `owner_lost()` in its own loop and nothing evaluates it on its behalf. A fleet whose survivors never call it ends up with an ownerless arena and joiners that time out on `ArenaHeldButUnreachable`; `docs/RUNBOOK.md` says so where an operator will meet it. 
**Recovery capacity is whatever was attached and eligible at the instant the ownership role fell vacant, and it cannot be added afterwards — NORMATIVE** ([`0055`](./decisions/0055-the-recovery-capacity-a-fleet-cannot-add-later.md) part 1). An ownerless arena with any participant byte held admits no new **rendezvous** attachment: §3.7's join cannot start and §3.4's split-brain check refuses an ordinary create. `Tree::attach_shared` and `attach_shared_at` refuse `AttachMode::ReadWrite` (`ShmError::ReadWriteNeedsRendezvous`). `CreatePolicy::Always` still creates in this state but *abandons* the held arena, and its creator is an `Attachment::Owner`, so `inherit_ownership()` answers `NotApplicable`.

Three ways to be ineligible: **read-only** (`inherit_ownership()` answers `ReadOnly`: an owner writes the participant table and a `PROT_READ` mapping cannot, D18); **read-write and never polling** `owner_lost()`; **not attached at that instant** (a supervisor-restarted publisher is polling and still not capacity, because the door closed while it was down).
Nothing in the library enforces any of this: `0055` answered *"no mechanism"* because every candidate is the thread or daemon this section refuses. 
**Lookups do not stop, slow down, or observe anything during a takeover.** `Plan::at` touches the mapping and the `Guard` and nothing else. `inherit_ownership` takes `&self` ([`0044`](./decisions/0044-recovery-the-languages-a-robot-is-written-in-cannot-reach.md)), so a control loop may hold a `Guard` across it (`a_guard_may_be_held_across_inheriting_ownership`).

### 3.6 Creation sequence — verified on Linux 6.18

Creation: (1) `memfd_create("tf_tree.<domain>.<name>", MFD_CLOEXEC | MFD_ALLOW_SEALING)`; (2) `ftruncate`; (3) `mmap` `MAP_SHARED`, **not** `MAP_POPULATE` (§7.1); (4) initialize the header (magic, `format_version`, `layout_hash`, `arena_size`, `instance_uuid`, `boot_id`); (5) `F_ADD_SEALS` with `F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_SEAL`; (6) `MADV_DONTFORK`, which must precede any fork (§7.3); (7) `MADV_HUGEPAGE`, best-effort.
Step 5 is load-bearing (Appendix B). **Sealing against shrink makes `SIGBUS` structurally impossible.** Do not skip it and do not substitute `shm_open`, which cannot be sealed and leaves stale segments in `/dev/shm`. 

### 3.7 Attach

Attach: (1) connect `SOCK_SEQPACKET`; (2) send `HelloRequest`; (3) `recvmsg` a `HelloResponse` plus the `SCM_RIGHTS` fd, or a rejection carrying no fd; (4) check `fstat` size equals `arena_size` and `F_GET_SEALS` includes shrink and grow, refusing an unsealed segment; (5) `mmap` `PROT_READ [| PROT_WRITE]`, `MAP_SHARED`; (6) verify header magic, `format_version`, `layout_hash`, `arena_size`, `boot_id`; (7) `MADV_DONTFORK`, `MADV_HUGEPAGE`; (8) `pwrite` the identity record and `F_OFD_SETLK` the participant byte; (9) **keep the socket open** for the lifetime of the attachment.

**Step 9:** the socket is how a participant learns the *owner* has died, with no polling — **once its attach connection has hung up and the last description holding byte 0 has closed (§3.5), not when the owner is signalled.** The kernel closes a dying process's files only after any core dump and address-space teardown ([`0057`](./decisions/0057-an-owner-is-not-dead-until-its-files-close.md) carries the figures): a small `SIGKILL`ed owner is seen in well under a millisecond, a 1 GiB one in ~100 ms, and one dumping core through a pipe `core_pattern` in ~1 s, during which no survivor can inherit and every fresh join is refused. A `fork` child sharing the owner's descriptions holds the socket and byte 0 until it exits (§6.2; `RUNBOOK.md`). Participant death is detected by the lock file, owner death by the socket; neither involves a timeout.
Message structs are fixed-size `#[repr(C)]`, little-endian, over `SOCK_SEQPACKET`: 
`HelloRequest` carries magic `b"TF_TREE\0"`, `format_version`, `layout_hash`, mode (0 = ReadOnly, 1 = ReadWrite), client pid, start_time, boot_id and name. `HelloResponse` carries magic, status (0 = Ok), `format_version`, `layout_hash`, `participant_slot` (the lock-file byte the client must take), `arena_size`, `instance_uuid` and `owner_pid`.
Rejections: `VersionMismatch`, `LayoutMismatch`, `BootIdMismatch`, `NoParticipantSlots`, `ModeNotPermitted`, `Malformed`. `IpcError::HandshakeRejected` carries the owner's `format_version` and `layout_hash` and not the client's; per-status remedies live in `RUNBOOK.md`'s `HandshakeRejected` table ([`0055`](./decisions/0055-the-recovery-capacity-a-fleet-cannot-add-later.md) step 7, [`0059`](./decisions/0059-the-arena-errors-that-cannot-describe-themselves.md) convention (g)). 

### 3.8 Capacity without planning — NORMATIVE

Fixed capacity (D4) is in tension with zero-config startup: whoever creates the arena fixes the layout. **Virtual capacity is nearly free**: a memfd charges nothing after `ftruncate` and after `mmap` without `MAP_POPULATE`, and only what is touched thereafter. `MADV_WILLNEED` does *not* pre-fault a memfd; use per-edge `MADV_POPULATE_WRITE` (§7.1). `doctor` warns at 80% occupancy of frames, edges or participants, and `ArenaFull` must state the limit and that raising it requires recreating the arena.

### 3.9 Teardown

**A participant dies:** its lock byte releases, its mapping drops, and the owner's hangup callback reaps its participant *record* and every **claim** it held (§6). The remaining stale-claim producers are a dead **owner** and a byte-less `build_shared` participant (out of contract, [`0031`](./decisions/0031-the-participant-record-with-no-byte.md)); for those `Tree::reap()` from a surviving read-write participant is the only collector, reachable from Rust, C, C++ and Python (`tft_tree_reap_dead`, [`0044`](./decisions/0044-recovery-the-languages-a-robot-is-written-in-cannot-reach.md)). **The owner dies:** survivors take over (§3.5); lookups never pause. **The last mapping drops:** the kernel frees the segment. A stale **socket path** may persist and the winner of ownership unlinks it; a stale **lock file** is harmless.

### 3.10 Trust model — NORMATIVE, and state it in the public docs

Participants are **mutually trusting, same-user, cooperating processes**. A read-write participant can corrupt any part of the arena. The design does guarantee:

A **read-only** participant cannot corrupt anything, enforced by the MMU (§8): the only real boundary, and the default. A participant that **crashes** at any instruction cannot corrupt anything or wedge another (§11.3). A participant that **hangs** cannot corrupt anything and cannot be mistaken for a crashed one (§6).

"Shared memory IPC is not a sandbox" belongs in the README.

## 4. `MappedArena`

**NORMATIVE:** the diff against Phase 1 outside `tf_tree_arena` and `tf_tree_ipc` must be **zero lines in the read path**. `PoseSlot`, `EdgeBuffer`, `Plan::at`, bracket search and interning are byte-identical code on a different base pointer.

**The premise is tested.** `crates/tf_tree_bench/tests/relocation.rs` byte-copies a populated arena to a different address and requires **bit-identical** results across every frame pair in the fixture plus frame-name resolution and header validation. Keep it green: a cached absolute address would otherwise surface in another process as a wild read.

`Drop` order is fixed: publish detach in the participant record, `munmap`, close the socket, close the fd, so the owner's reap path never races a half-torn-down participant.

`Tree` is generic over `A: Arena` with no `MappedArena`-specific branches, and `Publisher` cannot be constructed from a `ReadOnly` arena (compile-fail test).

## 5. Participant registry

`ParticipantRecord` is `#[repr(C, align(64))]`, 128 bytes (asserted at compile time): `state: AtomicU32`, `pid: AtomicU32`, `start_time: AtomicU64` (`/proc/<pid>/stat` field 22, defeats PID reuse), `incarnation: AtomicU64`, `attached_at_nanos: AtomicI64`, `heartbeat: AtomicU64`, padding. **`state` is a packed word, not a plain enum:** `state_of(word) = word & 0b11` is the lifecycle (FREE = 0, RESERVED = 1, LIVE = 2) and the incarnation sits above it, `live_word(inc) = (inc << 2) | LIVE`, so testing `state == 2` finds nothing. Published last; there is no `detaching` state: a departing participant goes straight to `FREE`, and "leaving" versus "gone" is the socket's job (D17).

Every field is atomic, because two processes read while a third publishes and neither Miri nor loom crosses a process boundary (§11.1): an arena field two processes touch is atomic. `incarnation` makes a reaped-then-reused slot distinguishable from the same slot still held. Read-only versus read-write is not in the record; D18's enforcement is the MMU.

**The owner assigns the slot; the joiner writes the record.** The owner's accept loop scans for an index whose lock byte the kernel reports free and whose arena record is absent or **collectable** (§5.1's predicate), reclaiming a collectable one — `reclamation_verdict` then `ParticipantTable::reclaim` — *before* granting it, because `fill_slot` CASes from `FREE` ([`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md) step 3). It returns the slot as `HelloResponse.participant_slot` (§3.7). The *joiner* writes its own record **with a CAS, after** taking the lock byte for that slot. A **creator** does the same on the byte `Open::register_creator` *takes* ([`0035`](./decisions/0035-the-creators-slot-is-taken-not-found.md)). **There is no third registrant**: a taker-over keeps its slot, byte and record. A directly-called `TreeBuilder::build_shared` opens no lock file, so there the CAS is the only ordering (§11.3's `attach.after_slot_assigned_before_publish` row). `Tree::attach_shared` / `attach_shared_at` refuse `ReadWrite` and write no record on `ReadOnly`; `TreeBuilder::build` is a heap tree.

`fill_slot` opens with `compare_exchange(FREE, RESERVED)`, writes identity fields under `RESERVED` (where no reader may trust them), and release-stores the live word last. **That publication order makes A3's indirection sound**: a claim can only name a slot some process drove to `LIVE`. A process killed in between leaves `RESERVED`: distinguishable garbage.

### 5.1 Identity is advisory; the lock file is authoritative — NORMATIVE

**Liveness comes from the participant's OFD lock byte (§3.3), never from these records.** Any code deciding liveness from `state` or `heartbeat` is a bug.

`reclamation_verdict` (`crates/tf_tree/src/open.rs`, [`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md) step 2) is the single predicate every reclamation decision goes through and answers from the lock byte alone. It reads `state` only to ask *is there a record here*: **a `FREE` word is very often a live process** (a read-only joiner takes its byte in the handshake and registers no record), so the predicate reports such a slot *unknown*. Properties a changer needs:

- It **skips this process's own slot**, because `F_OFD_GETLK` reports only conflicting locks.
- It **observes the `state` word before it probes the byte**: the `Acquire` load of a live word synchronises with `fill_slot`'s `Release`, so a later byte probe must see the byte held. Reversed, or taken from one up-front `held_participants()` mask, it erases a published record.
- It is **sound only because** every rendezvous participant holds a byte and the byte at index `slot` belongs to the record at index `slot`.
- It is **total over the rendezvous population, not over the table**: a `build_shared` creator served by hand reads dead to it (`a_byteless_creators_record_reads_dead_and_is_reaped_while_it_publishes`), which [`0031`](./decisions/0031-the-participant-record-with-no-byte.md) answered *out of contract*.

A second copy of this predicate is the defect `0028` was opened about.

**The ordering above binds a *probe*; A2's topology lock takes an *acquire* — NORMATIVE.** `Tree::reparent` takes an exclusive `F_OFD_SETLK` on **byte 1** and holds it for the whole mutation, so its order is byte-then-word ([`0029`](./decisions/0029-the-topology-lock-is-a-kernel-lock.md)). The invariant: **the topology word is CASed non-zero only while its process holds byte 1, and byte 1 is released only after the word is; a process holding byte 1 that observes a non-zero word is looking at a holder that is either dead or has no lock file.** The `/proc` triple decides only the second, and only to withhold a steal. Anything that reverses either order, or uses `held_participants()`'s mask on this path, gives that invariant up.

`(pid, start_time, boot_id)` remains the identity triple for diagnostics and the forced-create path (`CreatePolicy::Always`). A bare PID is not an identity. `start_time` is read by `read_start_time` (`crates/tf_tree/src/tree.rs`) feeding `alive_given`, the predicate §0.0's row records; `client_start_time` in the attach `Hello` is consumed by no reader.

**The parsing trap — NORMATIVE.** Field 2 is `comm`, which may contain spaces *and parentheses*; splitting on whitespace and taking index 21 silently returns another field. Locate the **last** `)` (`raw.rfind(')')`), parse from `rp + 2` and take `nth(19)`. Appendix B has the failing naive parse; include that case as a unit test against a fixture string.

## 6. Liveness and reaping

### 6.1 Claims are kernel locks — NORMATIVE

**`claim(edge)` takes an exclusive OFD lock on `CLAIM_BASE + edge_id` in the lock file**, held for the life of the `Publisher`. Reaping is `F_OFD_GETLK` says free ⇒ definitively dead, and the zombie writer (§A4) is **impossible by construction**. A `SIGSTOP`ped or GC-stalled writer **still holds its lock**, so it cannot be reaped while alive and a second claimer gets `EdgeAlreadyClaimed`.

**Two sources of truth, one authoritative.** `ClaimRecord` remains for diagnostics and for readers asking who publishes an edge, but **the lock file is authoritative**: claim = take the lock, then write the record; reap = lock free and record held ⇒ clear the record. Any decision from `ClaimRecord` alone is a bug. **A4 is retained but downgraded** to defence in depth; its comment must say so.

### 6.2 Fork is still the exception — NORMATIVE

OFD locks are held by the open file description, which **survives `fork`**: parent and child both "hold" every claim and both pass A4's epoch check. `MADV_DONTFORK` (§7.3) closes this: the child has no mapping and faults loudly. `MADV_DONTFORK` and OFD claims are a matched pair; a comment at each site must say so.

### 6.3 What remains of reaping

Arena-side cleanup after a death, by any read-write participant, all steps idempotent:

For each edge whose `ClaimRecord` says held and whose `F_OFD_GETLK(CLAIM_BASE + edge)` reports free (the holder is definitively dead): bump `claim.epoch` (fences a buggy `Publisher`), normalize the slot parity at `head & mask` (A5 repair), and CAS `claim.owner` from the stale value to 0 (racing reapers are harmless). For each populated participant record whose lock byte is free: clear it.

The owner runs this on socket `HUP`; others run it lazily when a claim appears held. **Reaping must not be owner-only** — that would leak every claim held when the owner died.

### 6.4 Heartbeats are diagnostics only — NORMATIVE

`heartbeat` and `clock_offset_nanos` remain in `ClaimRecord` and are **never** a reaping trigger. Neither detects a hang: a live process that stopped publishing is found from its *stamps* (`TFT009`, `TFT008`).

**Write schedules differ** ([`0036`](./decisions/0036-the-receipt-time-the-format-already-reserved.md)). `heartbeat` is bumped on **every** push inside `SampleRing::push` ([`0014`](./decisions/0014-the-push-heartbeat-is-a-store.md)). `clock_offset_nanos` needs a wall-clock reading, so `tf_tree`'s `EdgeWriter` **samples** it on a claim's **first** push and then once every `max(nominal_rate_mhz / 1000, 1)` pushes; an edge declaring no rate gets a fixed **1024**. A claim clears the field it inherits. **`0` means never sampled and is written by nothing**: an offset computing to zero is stored as `1`. The clock is read **in the facade, after the ring write returns**, never inside `SampleRing::push`, where it would widen the seqlock window into readers' `SlotContended` retries. **Only a `SystemDomain` (tag 0) edge records anything**, since `wall clock - stamp` is an offset only where both share an epoch. It stores the *offset*, not a receipt time, because the ring's newest stamp may belong to a later push than the receipt. Cost: `just push-sampler-cost`; `docs/PHASE1.md` §11.2 tabulates it.

Reaping on staleness would be actively unsafe: a 0.2 Hz map-to-odom correction is indistinguishable from a hung writer under any timeout short enough to be useful. **Do not add such a policy**, not even opt-in.

## 7. Mapping policy

### 7.1 Page population is per-edge, not per-arena — NORMATIVE

`MAP_POPULATE` over a generous arena would charge memory nobody declared. So populate at **take-up** granularity:

- `mmap` **without** `MAP_POPULATE`.
- When an edge is **taken up**, `madvise(MADV_POPULATE_WRITE|READ)` (Linux ≥ 5.14) over its stamp and pose ranges; older kernels touch one byte per page. The moments are `Tree::claim` for a writer and plan compilation for a reader, both off the query path by D3 ([`0024`](./decisions/0024-population-is-per-edge-at-take-up.md)).
- On attach, populate the header, frame table, topology blocks, claim table, participant table, edge table and both counter regions. **Not the two ring arenas**, 99.8% of a large arena.

**`MADV_WILLNEED` does not work here** (zero change in charged pages on a memfd). §12 requires first-access-after-attach rows with population on and off.

### 7.2 Huge pages

`madvise(MADV_HUGEPAGE)`, best-effort. THP must be `madvise` or `always`; `doctor` reports the setting and the benchmark reports both configurations.

### 7.3 `MADV_DONTFORK` — NORMATIVE, and easy to forget

A `MAP_SHARED` mapping survives `fork()`, and the child inherits the parent's `Publisher` structs with their claim epochs, so both processes pass A4 and write the same edge. `madvise(base, len, MADV_DONTFORK)` at attach, before any fork, removes the mapping from the child, which faults loudly. The child must re-attach for its own slot and claims. **Document this prominently**: Python's `multiprocessing` defaults to `fork` on Linux (§14).

### 7.4 Memory locking

> **AMENDED by [`0049`](./decisions/0049-the-flag-that-prefaults-the-arena.md). There is no `LockPolicy` and no `mlock` call in this library, and none is owed.** The reason is `docs/API.md` §8.3's second bullet: a library that locks memory spends an `RLIMIT_MEMLOCK` budget it cannot see. `TFT016` reports the limit against arena size.

## 8. Read-only attachment

**NORMATIVE:** `AttachMode::ReadOnly` maps `PROT_READ` only and is **the default for any participant that does not declare an intent to publish.** A buggy perception node *cannot* corrupt the transform tree, enforced by hardware; lead with this in the documentation.

Permitted, on the identical code path: `plan`, `at`, `at_many`, `at_adaptive`, resolving an existing frame name. Refused: interning a new frame (`Err(FrameNotDeclared)`, since interning writes), reaping, and `claim`/`push` (not expressible: `Publisher` construction requires `ReadWrite`, compile-fail test). No heartbeat is written; the socket carries liveness.

`FrameNotDeclared` must say "no publisher has declared this frame yet", not "unknown frame".

## 9. `tf_treed`

> **SUPERSEDED by [`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md). There is no `tf_treed` binary. The capability is `tf_tree serve`, a subcommand of the shipped binary, and it is an escalation, not a prerequisite.** Liveness, reaping and owner death need no daemon (§3.5). Pre-declaration is fixed without one: read-only attach implies `CreatePolicy::Never`, consumers wait with `Open::await_open` and `Tree::await_frames`, and `frame_headroom`/`edge_headroom` cover late frames. What survives as `tf_tree serve --config <topology.toml>`: create and seal from the config, pre-declare, hold the arena open, export metrics, drain on `SIGTERM` leaving the segment alive. Per [`0009`](./decisions/0009-descoping-phase-6.md), `--config` takes the topology config only; there is no URDF.

## 10. Recording and replay — the correctness harness

> **§10(a) *Record* and §10(b) *Replay* are DECLINED by [`0047`](./decisions/0047-the-recording-this-reader-would-refuse.md). There is no `tf_tree_record` crate and no `record`/`replay` subcommand, and none is owed. §10(c), the NORMATIVE test, is met — `crates/tf_tree_cli/tests/replay_bit_identity.rs`.** The read half shipped as `tf_tree_ingest` under [`0006`](./decisions/0006-the-eight-phase-roadmap.md); read `docs/PHASE5.md` §0.0's §3 row before assuming a substitute for the regression corpus.

- **The test that matters — NORMATIVE.** Replay one recording into a `HeapArena` and a `MappedArena`, run an identical query set against both, and assert **bit-identical `f64` results**. Lookups are pure functions of `(plan, stamp, buffer contents)`, so any difference means the shared-memory path is not the same code.

## 11. Test plan

### 11.1 What Miri and loom can and cannot do

**Neither crosses a process boundary**, so `MappedArena` cannot be tested by either. Run every Phase 1 loom test against a `HeapArena` with A1–A5 applied, cover the multi-process dimension by fault injection, and add loom cases for: two threads racing `try_reap` (at most one `Reaped::Yes`); reap concurrent with `push` from the reaped `Publisher` (`ClaimRevoked`, or completes before the epoch bump); topology mutation concurrent with plan compilation across four blocks (one consistent block or `TopologyChurn`); claim, reap, re-claim, zombie push (the zombie always fails).

### 11.2 Multi-process integration harness

Real child processes, coordinated via pipes, asserting on arena state. Required scenarios:

1. 1 owner, 1 writer, 14 read-only readers. Sustained 1 kHz for 60 s. Zero errors, zero divergence.
2. Attach/detach churn: 32 processes for 60 s while a writer publishes. Slots must not leak.
   - **2b.** `SIGKILL` a read-write participant 128 times against a 64-slot arena; every attach must succeed (`slot_recycling_under_abnormal_exit`, `crates/tf_tree/tests/rendezvous.rs`).
3. Owner dies mid-run: participants continue for 60 s; new attach fails cleanly; reaping still functions.
4. `FORMAT_VERSION` / `layout_hash` mismatch: rejected with the correct status and a message naming both values.
5. Read-only participant attempts every write operation: all fail, arena bytes unchanged (hash before and after).
6. 64 participants, then a 65th: `NoParticipantSlots`, and the message says how to raise the limit.
7. **Thundering herd:** 32 processes `open()` simultaneously with no arena. Exactly one creates; 31 join; all 32 see the same `instance_uuid`.
8. **Ownership migration:** kill the owner mid-run. A survivor takes over; a new process joins the *same* arena (identical `instance_uuid`); a reader thread running throughout observes zero failed lookups and no excursion beyond steady-state p99.9.
9. **Split-brain attempt:** kill the owner and immediately start a fresh process. It must block on §3.4 step 4 and then join. **Two distinct `instance_uuid`s on one `(runtime_dir, domain, name)` is a hard failure.** Run a thousand times; it is the most important race in the phase.
10. **Stuck participant:** `SIGSTOP` the only participant, then `open()` from a fresh process. Must fail with `ArenaHeldButUnreachable` naming the stuck slot — never create a second arena. `SIGCONT`, then a subsequent `open()` succeeds.
11. **Domain isolation:** two arenas under different domains, and two under different runtime dirs, never observe each other.

### 11.3 Fault injection — the core of this phase

**NORMATIVE.** A build-time `crash-points` feature places named, deterministic abort sites (`crash_point!("name")`) in every mutation protocol.

Armed by `TF_TREE_CRASH_AT=<name>:<nth_hit>`, which `abort()`s (not `panic!`, whose unwinding runs `Drop` and defeats the test). One test per site:

| Crash point | The state it leaves behind must be repairable |
|---|---|
| `push.after_seq_odd` | slot odd, `head` unbumped → A5 self-heals on next claim |
| `push.after_data_before_seq_even` | as above; sample invisible because `head` never moved |
| `push.after_seq_even_before_head` | sample fully written but unpublished → invisible, then overwritten |
| `topo.after_copy_before_publish` | inactive block dirty, word unchanged → **no observable effect** (A1) |
| `topo.holding_lock` | Placed in `Tree::reparent`, after A2's word is CASed and *before* `set_parent` (`a_killed_topology_holder_leaves_a_word_the_next_acquirer_steals`). Byte released by the kernel; word left stale and overwritten by the next acquirer ([`0029`](./decisions/0029-the-topology-lock-is-a-kernel-lock.md)); stealing needs no rollback (A1). Holder classes: **live with a lock file** — refused by the byte whatever `/proc` says; **dead with a lock file** — stealable; **no lock file** (`build_shared`, [`0031`](./decisions/0031-the-participant-record-with-no-byte.md)) — decided by the triple alone; **fork inheritor** ([`0030`](./decisions/0030-the-atfork-handler-and-inherited-descriptors.md)) — a dead parent's lock is not stealable while the child lives, an availability failure reported by `TFT014` |
| `claim.after_cas` | claim held by a dead participant → reapable via slot indirection (A3) |
| `intern.after_hash_cas_before_id_store` | hash slot claimed, id unpublished → next interner spins, then takes over (A8) |
| `attach.after_slot_assigned_before_publish` | Placed in `participant::fill_slot`, between the `FREE -> RESERVED` CAS and the `live_word` store (`attach_after_slot_assigned_before_publish_aborts_at_the_named_point`). Slot `RESERVED` → record cleared by any reaper: `ParticipantTable::reclaim` accepts any observed word, `RESERVED` included ([`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md) step 1), and both the hangup callback and the slot assigner act on one. The window is ~12 ns, so §11.2's `..._collects_a_record_left_reserved_by_a_killed_registrant` tests *stage* the word. On the byte-less `build_shared` path no reclaimer ever runs |
| `hangup.after_probe_before_cas` | Placed in the owner's hangup callback between the `state` load and `reclaim` (`a_killed_owner_in_its_hangup_callback_leaves_the_role_inheritable`). One CAS, no torn state; the assigner (next grant) and `Tree::reap_participants` form the same verdict later |
| `reclaim.after_probe_before_cas` | Placed in `Tree::reap_participants` between `reclamation_verdict` and the CAS (`a_killed_sweeper_leaves_the_record_for_the_next_one`). Nothing published → idempotent; at most one racing CAS succeeds |
| `reclaim.probe_then_reoccupied` | **Not an abort site**: an interleaving between two *live* processes, which `loom` and §11.4 reach. A reclaimer holding a verdict formed before the slot was freed, re-granted and re-occupied: for `live_word(inc)` the CAS fails on the differing incarnation; for `RESERVED`, which carries none, it **can** succeed, bounded by the byte rather than the word, so the outcome is a **spurious free, never a second occupant**. `ParticipantTable::reclaim`'s doc comment carries the precondition |
| `open.after_ownership_lock_before_bind` | Placed with `open.after_create_before_bind` (`a_creator_killed_before_or_after_the_arena_exists_leaves_nothing_behind`). Ownership lock released by the kernel → the next `open()` proceeds; **no arena created twice** |
| `open.after_create_before_bind` | Placed before `use_ofd_liveness`, so the abandoned tree holds only the segment. Arena exists, nothing serving, no byte held → next `open()` creates fresh; the orphan memfd is freed with its last mapping |
| `takeover.after_ownership_lock_before_bind` | ownership released; another participant takes over; joiners retry. **`another participant` is a precondition, not an outcome**: the role is re-taken only by a process *already attached*. With **zero** attached read-write survivors the state is absorbing; `a_killed_heir_leaves_the_role_for_the_next_survivor` keeps a second heir attached and `shm_torture` defers an owner kill rather than taking the last one |

**`intern.after_hash_cas_before_id_store` needs amendment A8** (§1), covered by a loom test.

### 11.4 `shm_torture`

Nightly CI, 30 minutes: N processes, random attach/detach/claim/reap/push/lookup, random `SIGKILL` at 1–10 Hz, a random crash point armed in 10% of children. Invariants: no reader ever observes a non-unit quaternion or a NaN; no two writers ever hold one edge; participant and claim slots never leak; the arena hash is stable across quiescent points. Run it under ASan and with `TF_TREE_PARANOID=1`, which validates quaternion normalization and stamp monotonicity on every read. §0.0's `shm_torture` row states which invariants are checked how.

> **The crash-point clause runs nightly** as its own job (`nightly.yml`'s `crash-points`): it is a different build (`--features shm,crash-points`) with gentler parameters (5 minutes, 10 children, 2 Hz). The binary **refuses** a `--crash-points` run with `armed 0` and separately one with `armed N, aborted 0`, because a job reads an exit status, not the `§11.3:` line.
>
> **The harness pauses its own churn for the width of one reap.** Between the driver's `kill()` and `wait()` the owner is *undetectably* dead (`exit_files()` runs after `exit_mm()`) and a survivor that leaves cannot return (§3.4 step 4), so the pool can drain. A `kill.in_progress` marker makes a child stay instead of detaching; it suppresses a detach, never a kill, an inheritance or a violation. `--victim-ballast-mb` and `--stop-owner-ms` are the positive controls.
>
> **The torture recipes run children under `prlimit --core=1:1 --`** ([`0057`](./decisions/0057-an-owner-is-not-dead-until-its-files-close.md) Decision 6), so a pipe `core_pattern` helper does not hold an armed owner's files open; they do not exercise recovery across a core dump.

## 12. Benchmarks and the gate

### 12.1 Fixture

The Phase 1 24-frame robot tree, plus 1 writer process (4 dynamic edges) and 1–16 read-only consumer processes each running 4 reader threads, cores pinned, `isolcpus` if available. Compare against ROS 2 `tf2` with an equivalent tree over the default DDS, same rates.

### 12.2 Required measurements

| Benchmark | Report | Measured |
|---|---|---|
| first access after attach, per-edge population on vs off | p99.9, both | **Half done — `just attach-bench`.** The **on** arm is measured; the **off** arm is absent (`populate_hot()` is unconditional inside `attach_shared_inner`) and arrives with `0022`'s B2-prime. |
| aggregate read throughput, 1→16 consumer processes | scaling curve | **Done — `just shm-scaling`; the curve is in [`docs/benchmarks/tf2.md`](./benchmarks/tf2.md).** The curve stops at 8 because 16 processes on 4 cores measures the scheduler. |
| attach time, cold and warm | p50 | **Done — `just attach-bench`, which does not run §3.7's rendezvous** (`Tree::attach_shared(dup, ReadOnly)` on a duplicated memfd). Figures: `docs/benchmarks/EVIDENCE.md`. |
| owner kill → new owner serving | p50, p99 | **Done — `just owner-migration`.** Timed from the outside: the driver stamps its `SIGKILL` and retries `Open::new().create(Never)` until one succeeds; of a small `SIGKILL`ed owner only ([`0057`](./decisions/0057-an-owner-is-not-dead-until-its-files-close.md); §3.7 step 9). |
| lookup latency across an ownership migration | p99.9 during vs steady-state | **Measured — `just owner-migration` — and the quotient is only weakly evaluable**: its sensitivity falls as its sample count rises, so it cannot be both stable and sensitive. Re-cutting the criterion is a decision record. **Load-bearing:** *zero failed lookups* in every run, and the stall count against steady state. |

### 12.3 The gate — NORMATIVE

Proceed to Phase 3 if:

1. **Cross-process depth-3 p50 within 10% of the in-process baseline, p99.9 within 25%.** The central claim of the phase; if it fails, the mapping policy (§7) is wrong, not the design.
2. **Aggregate read throughput scales ≥ 12× from 1 to 16 consumer processes.**
3. **Zero corrupt reads across the full `shm_torture` run**, and every §11.3 crash point recovers. Not negotiable.
4. **Kill → re-claimable p99 under 10 ms. MET, and gated in CI — `just gate reclaim-latency may-refuse`, in `ci.yml`'s `shm` job on both matrix rows.** The figure is of a small `SIGKILL`ed victim; a large dirty or dumping victim pays its teardown first ([`0057`](./decisions/0057-an-owner-is-not-dead-until-its-files-close.md)). **`may-refuse`, not `must-pass`:** `reclaim_latency` exits `2` for its own non-vacuity refusal (INVALID, not FAIL), a statement about the runner; the refusal emits a `::warning::`, and `scripts/gate-run.sh` owns the policy ([`PHASE5.md` §9.3](./PHASE5.md#93-honesty-requirements--normative)).
4b. **Ownership migration is invisible to the data plane:** lookup p99.9 during a migration within 5% of steady state, and zero failed lookups. **Partly met, on a criterion that is only weakly evaluable** (`just owner-migration`, wired into no CI workflow; §12.2's row carries the argument). Re-cutting it is a decision record.

    A lookup against an actively-written ring can transiently refuse with an *inverted* window (`oldest` past `newest`) at ~1 in 4 × 10⁷: `SampleCursor::sample` (`crates/tf_tree_core/src/sample.rs`) reads the two bounds with independent `Relaxed` loads and no seqlock, deliberately. The refusal is correct; only the reported pair is inconsistent, and fixing it changes a hot-path read, which is a decision record. `owner_migration` excludes these from 4b's tally.
4c. **Scenario 9 of §11.2 passes 1000 consecutive runs with a single `instance_uuid`.**
5. Total RSS across 16 consumers under 1.2 × arena size.

### 12.4 What the numbers are actually for

Latency is the engineering gate; **CPU-per-consumer and RSS are the industrial argument** and belong in the README: under `/tf` every consumer deserializes every transform into a private replica, under `tf_tree` CPU is O(1) in consumers and RSS is one arena.

## 13. Failure modes and runbook

`docs/RUNBOOK.md` owns the symptom / cause / response table. Every row must correspond to a `doctor` check and a distinct error type. Rows: `LayoutMismatch`, `BootIdMismatch`, `ConnectionRefused`, `NoParticipantSlots` (capacity is fixed at construction, D4; raising `DEFAULT_MAX_PARTICIPANTS` means rebuilding every participant together), `FrameNotDeclared` on a read-only participant (`Tree::await_frames`), `ClaimRevoked`, `EdgeAlreadyClaimed`, `SlotContended` / `SlotRecycled`, `TopologyChurn`, and `SIGBUS` in a lookup (**structurally impossible with sealing, §3.6**; if it happens the segment was not sealed). `HelloStatus::BootIdMismatch` from the §3.7 handshake is not `BootIdMismatch`: a serving owner answered, so the processes disagree about the boot; `RUNBOOK.md`'s `HandshakeRejected` row is the triage.

## 14. Phase 3 handoff — constraints you must not break

> **Superseded in part by [`PHASE3.md`](./PHASE3.md) §1**, which corrects item 5 and adds a constraint this section missed. Read it before acting on items 4 or 5.

1. **`fork` safety.** `multiprocessing` defaults to `fork` on Linux; `MADV_DONTFORK` means the child's mapping is gone. Phase 3 must register an `os.register_at_fork(after_in_child=...)` hook that poisons every inherited `Tree` handle so the child gets a Python exception, not a segfault.
2. **GIL and liveness.** The socket carries liveness, so a long GIL-held pause does not risk reaping. Do not add heartbeat-based reaping (§6.4).
3. **Read-only by default.** Python `attach()` defaults to `ReadOnly` with `CreatePolicy::Never`.
4. **`tf_tree.open()` with no arguments must work in a notebook.**
5. **Distribution: `abi3` wheels via maturin**, plus the additions in `PHASE3.md` §1.1.

## 15. Definition of done

- [x] Amendments A1–A8 applied to Phase 1; all Phase 1 tests still pass unchanged — §0.0's first row
- [x] `FORMAT_VERSION` bumped, with a documented compatibility table — `tf_tree doctor --explain-version` prints the build's `format_version` and `layout_hash` and what a mismatch requires (rebuild and restart every participant together; there is no compatibility layer)
- [x] Diff in `tf_tree_core`'s read path against Phase 1: **zero lines** — structural: the read path is written against `ArenaView` and never names a backend. `another_process_reads_the_same_arena_bit_identically` (`crates/tf_tree_bench/tests/multiprocess.rs`) is the executable half
- [x] `tf_tree_core`'s normal-kind third-party dependencies are exactly `blake3`, `bytemuck` and `libm` (D14)
- [x] All §11.2 integration scenarios pass in CI on x86-64 **and aarch64** — all eleven are in `crates/tf_tree/tests/rendezvous.rs` except 4 (`a_layout_mismatch_names_the_owners_hash_and_sends_no_fd`, `a_version_mismatch_outranks_a_layout_mismatch` in `tf_tree_ipc`) and 5 (`read_only_refuses_mutation_instead_of_faulting`); 8 is `a_survivor_inherits_ownership_and_the_arena_becomes_joinable_again`. They run under `just shm-rendezvous` in the `shared memory` job on `[ubuntu-latest, ubuntu-24.04-arm]`. Scenario 1's and 2's 60 s soaks are not reproduced (`just shm-torture` is the sustained arm)
- [x] Scenario 9 (split-brain) passes 1000 consecutive runs — `just split-brain-soak`; one run is `scenario_9_a_split_brain_attempt_never_produces_a_second_arena`. **§11.2's prediction is wrong and the test says so:** the newcomer is **refused** with `ArenaHeldButUnreachable`, not joined, because migration is caller-driven ([`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md)). The test asserts refused is fine, joined is fine, a second `instance_uuid` never is
- [x] `tf_tree::open()` joins-or-creates correctly from any start order — `scenario_7_a_thundering_herd_produces_exactly_one_arena`
- [x] `doctor` prints `instance_uuid` and the resolved runtime dir, and works without the arena — `crates/tf_tree_cli/tests/doctor_runtime_dir.rs`
- [x] Every §11.3 crash point has a test proving recovery — thirteen of fourteen rows carry a site and all thirteen are driven by a test; the fourteenth is not an abort site (§0.0's fault-injection row)
- [~] `shm_torture` runs 30 minutes nightly, clean, under ASan — **the ASan arm is not clean.** It failed on the scheduled 2026-09-10 run on the population condition [`0055`](./decisions/0055-the-recovery-capacity-a-fleet-cannot-add-later.md) is about (`Classification: POPULATION`, `Heirs attached at this kill: 0`), not on an ASan report or an invariant. What the box waits on is whether `shm_torture` guarantees an heir at every kill, a harness decision nobody has taken. Three arms: the 30-minute `SIGKILL` soak (`nightly.yml`'s `torture`), the `crash-points` job, and ASan in the `sanitizers` matrix at `--duration 30m --children 4 --kill-hz 4`
- [x] `HeapArena` / `MappedArena` replay produces **bit-identical** results (§10) — `crates/tf_tree_cli/tests/replay_bit_identity.rs`; the test is `#![cfg(all(feature = "shm", target_os = "linux"))]`: **`just test` does not run it; `just shm-check` does**. §10's tooling is DECLINED ([`0047`](./decisions/0047-the-recording-this-reader-would-refuse.md))
- [x] §12.3 gate met, or a written explanation of which criterion failed and by how much — on a 4-physical-core / 8-thread host. Three states: **met**, **failed by a stated margin**, and **not evaluable on this hardware** (`docs/PHASE5.md` §9.3's `Sensitivity` axes; `tf_tree_bench`'s `Ground` enum).

  | criterion | verdict | evidence |
  |---|---|---|
  | 1. cross-process depth-3 p50 within 10%, p99.9 within 25% | **not evaluable here** | `just mp-bench` refuses: the host was busier than its threshold |
  | 2. read throughput scales ≥ 12× from 1 to 16 processes | **not evaluable here; the ceiling is arithmetic** | `just shm-scaling`: 3.72× at 8 on 4 physical cores |
  | 3. zero corrupt reads; every §11.3 crash point recovers | **met** | `just shm-torture`: `0 violation(s)` |
  | 4. kill → re-claimable p99 under 10 ms | **met, 0.182 ms** | `just reclaim-latency`, 200 trials |
  | 4b. migration invisible to the data plane | **partly met, weakly evaluable** | §12.2's argument |
  | 4c. split-brain, 1000 runs | **met** | `just split-brain-soak`: 1000/1000 clean |
  | 5. RSS across 16 consumers under 1.2 × arena | **not evaluable; the criterion should be re-cut** | at 8 processes unique resident 19.9 MiB against a 1368 KiB arena: it compares whole-process RSS against the arena alone |
- [~] `tf_tree serve` ships with a systemd unit and a container example — **out of scope by [`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md)** (steps 6–7 not built, not scheduled). `~` because this is work declined, not owed.
- [x] `docs/RUNBOOK.md` complete; every row maps to a `doctor` check (rows for unimplemented Phase 2 errors are marked as such)
- [~] `docs/PHASE3.md` written and carrying §14 forward; the measured numbers land with §12

## Appendix A — implementation order

1. **A1–A8 against `HeapArena`**, with the loom tests, before touching a syscall.
2. **The lock file and `open()`**, including the §3.4 split-brain check and ownership migration; build §11.2 scenarios 7–11 alongside, scenario 9 first.
3. **`MappedArena` + attach protocol**; assert the zero-line-diff property in the read path.
4. **Claims as OFD locks; arena-side reaping; the crash-point harness.**
5. Replay bit-identity test (done; `tf_tree_record` declined, [`0047`](./decisions/0047-the-recording-this-reader-would-refuse.md)). 6. `tf_tree serve`, last, possibly never ([`0019`](./decisions/0019-one-binary-and-topology-you-can-wait-for.md) §2). 7. `doctor` / `top` / `participants`. 8. The `/tf` ingest bridge: the bridge claims each edge on first sight and applies `FirstWriterWins` (default, loud diagnostic naming both ROS publishers) or `LastWriterWins`. 9. Benchmarks and the gate.

Do not proceed past step 4 until every §11.3 crash point recovers and §11.2 scenario 9 passes a thousand consecutive runs.

## Appendix B — kernel behaviour probe

Verified on Linux 6.18. Re-run on the target kernel; the sealing results in §3.6 are load-bearing. On a memfd with a writable mapping: sealing `SHRINK|GROW` succeeds, sealing `WRITE` is `EBUSY`, `ftruncate` shrink after sealing is `EPERM`, `F_GET_SEALS` is `0x7`, `MADV_DONTFORK` and `MADV_HUGEPAGE` return 0. Probe: `memfd_create(MFD_ALLOW_SEALING)`, `ftruncate`, `mmap(MAP_SHARED)`, then the `fcntl`/`madvise` calls above in that order.

**The `/proc` parsing trap (§5.1), as a test fixture.** For a process whose `comm` is `evil) proc`, the naive whitespace split returns field 12's value where field 22 was intended:

```
raw    = "1234 (evil) proc) S 1 1234 1234 0 -1 4194304 1 2 3 ... 39"
naive  : raw.split()[21]                       -> 12    WRONG
robust : raw[raw.rindex(')')+2:].split()[19]   -> 13    correct
```
