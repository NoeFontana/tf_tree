# 0033: the identity record cannot name a namespace

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** none yet — moved `draft` → `ready` 2026-08-23. This record
exists because the obvious fix reads the wrong process, and because the field it
needs does not fit.

**Amended 2026-08-23 by an audit that re-executed it against `09efc9b` in two
independent worktrees, each reproducing the other's numbers.** What the audit
*confirmed* is left standing and said so in place: all three arms of plan step 1
reproduce, the `[u8; 16]` narrowing is free, and `lstat` is wrong in every arm.
What it changed is four things, none of them the *Decision*'s argument.

1. A **fourth** namespace-shaped false positive, in which `doctor` fires
   `TFT014` **on its own slot** — and which this record's ns-inode guard is
   structurally blind to. Closed by a second guard: *Decision* 4.
2. The `readlink`-vs-`stat` warning, which overstated the failure in one
   direction and understated it in the other: *Decision* 1.
3. The gate claim in *Consequences*: the evidence cited was not `just test`'s
   and could not have been.
4. The blast radius, which was stated as seven files and is at least ten — one
   of them a **runtime panic on a public method**, and one of them the reason
   the gate claim is impossible.

A fifth thing is *added* rather than corrected: the scope this record does not
cover, in the last bullet of *Consequences*, so nobody reads it as fixing the
arena.

## Context

Filed as issue #239. `TFT014` reports a **PID-namespace mismatch** as a **fork
inheritor**, and the two have opposite operator remediations.

**Executed, not derived.** A live, healthy participant inside
`unshare -U --fork --pid`, seen from the host namespace:

```
  [WARN ] TFT014  participant or claim slot leak
          slot 1 pid 1, byte still HELD: a fork inheritor — byte still HELD,
          recorded pid gone. ... Stop the child, or start workers with a start
          method that inherits no descriptors ...
```

The accused process is alive, `state=S`, `uid=dev`; the recorded pid 1 is
root-owned `systemd`. An isolating control varying only the `--pid` flag —
plain host child (silent), `unshare -U --fork` (silent), `unshare -U --fork
--pid` (fires) — puts the cause on the pid namespace and nothing else.

The signature really is shared. A *genuine* surviving fork inheritor was staged
with a purpose-built orphan (every `fork_child` mode `_exit`s its child, so no
existing helper produces one) and prints text character-identical to the false
positive but for the interpolated pid. So this is not a case of two faults that
merely look alike in summary: they are the same two words in the same formatter.

`recorded_given`'s `Ok(_) => R::Gone` arm is **correct for the case it was
written for** — pid in use, start time differs, therefore the recorded process is
gone and the number was reused. It is simply not the only cause of that
observation, and there is no discriminator in the persisted `Identity`: `pid`,
`start_time`, `boot_id`, `mode`, `name`. Only `pid` differs across a namespace,
and its meaning is exactly what changed. The kernel has no per-pid-namespace
boot id, so `boot_id` cannot help.

### Why the obvious fix is worse than the bug

The issue tentatively suggests comparing `/proc/<recorded_pid>/ns/pid` against
doctor's own. **That probe fails open**, and it was made to:

```
recorded pid for slot 1 = 1695322
/proc/1695322 exists: comm=sleep owner=1000 (dev)
readlink /proc/1695322/ns/pid -> pid:[4026531836]
doctor's own ns             -> pid:[4026531836]      <-- "same namespace as me"
```

while the real participant sat at host pid 3394130 with `ns=pid:[4026532487]`.
A recorded pid is **namespace-local**, so in the host namespace it names a
different process. The probe read an unrelated same-uid process, found a matching
namespace, and would have *confirmed* `ForkInheritor` with false confidence.

That is the same "successful read of the wrong process" class that
`recorded_given`'s own doc comment records as the previous shipped bug here. On a
host where the recorded pid is 1 the probe fails closed with `Permission denied`,
which makes it look safe; that is a property of the pid being 1, not of the probe.

`/proc/locks` cannot supply the missing host pid either — OFD locks report
`pid -1`, which `lockfile.rs`'s module docs already state as a design property.

## Decision

**Record the namespace at registration; do not derive it at diagnosis.**

That headline covers steps 1–3. **Step 4 is not an instance of it and is not
meant to be**: it asks nothing about the recorded process, only whether the
observer's `/proc` describes the observer's own namespace, and it is here
because a guard that answers *only* the recorded question leaves `doctor`
accusing itself. Two guards, two questions, one placement — both before the
whole `match probe`.

1. A participant reads its own `/proc/self/ns/pid` inode when it writes its
   `Identity` record, and stores it. Executed: readable, cheap, and it differs
   across the boundary (`pid:[4026532489]` vs `pid:[4026531836]`).

   **It must be read with `readlink` and the `pid:[N]` text parsed.** The
   reason is that `readlink` is the only one of the three candidate reads that
   is correct in every arm — it is *strictly dominant*, and that is what makes
   the choice a decision rather than a preference. Re-measured 2026-08-23 across
   four arms, one process per arm doing all three reads in-process:

   ```
   arm                            readlink                stat().ino     lstat().ino
   host                           pid:[4026531836]        4026531836 OK  81341846  WRONG
   unshare -U --fork              pid:[4026531836]        EACCES         81340131  WRONG
   unshare -U --fork --pid        pid:[4026532488]        EACCES         81340134  WRONG
   default `docker run` (no       pid:[4026532489]        4026532489 OK  a procfs  WRONG
     user ns; busybox `stat -L`                                          dentry
     vs `stat`)
   ```

   **This record's earlier wording was wrong in both directions, and the
   correction matters for how an implementer will test it.** It said `statx`
   returns `Permission denied` inside "the namespace"; the refusal in fact comes
   from an **unmapped user namespace** and not from the pid namespace at all —
   `unshare -U --fork`, with no `--pid`, reproduces it, and a **default Docker
   container has a pid namespace and no user namespace, so `stat` succeeds
   there**. An implementer who reaches for `docker` as the nearest container,
   sees `stat` return the right inode, and concludes this paragraph is folklore
   would then ship the `stat` spelling and have it refuse on the one staging the
   regression test actually uses. It also said `lstat` is wrong "inside the
   namespace": `lstat` is wrong **everywhere, including the plain host**, because
   it stats the procfs dentry rather than the `nsfs` inode the link points at,
   and it does so *successfully*. So `fs::metadata().ino()` fails loudly in two
   of four arms and `symlink_metadata().ino()` succeeds *with a plausible wrong
   number* in four of four — the same "successful read of the wrong thing" class
   as the probe this record already rejects, which is why the dominant read is
   the one that is chosen.
2. `doctor` compares the recorded inode against its **own**, never against one
   read through the recorded pid.
3. `recorded_given` gains a **guard before the whole `match probe`**, beside the
   existing `stored_start_time == 0` guard — *not* an arm inserted ahead of
   `Ok(_) => R::Gone`. A recorded namespace that differs from the observer's
   means the pid is not comparable from here, so the verdict is
   `RecordedProcess::Unknown` whatever the probe returned.

   **"Ahead of `Ok(_)`" was the wrong placement, and a measurement caught it.**
   There are two namespace-shaped false positives, not one, and they take
   different arms. The one this record's *Context* stages — a namespaced
   participant seen from the host — takes `Ok(_) => R::Gone`, because the
   recorded pid 1 exists here as `systemd` with a different start time. The
   **mirror case takes the `ENOENT` arm**: a container `doctor` watching a host
   participant, whose recorded pid does not exist in the container's `/proc` at
   all. Executed, with a host publisher alive at pid 3815678:

   ```
   from a container sharing the runtime dir:
     [WARN ] TFT014 ... slot 0 pid 3815678, byte still HELD: a fork inheritor
             ... /proc says the pid ... 3815678, is gone
   isolating control, same binary, same runtime dir, same publisher, on the host:
     19 catalogue checks: 7 passed, 2 fired      <- no TFT014
   ```

   A guard placed before the match covers both; an arm ahead of `Ok(_)` covers
   only the first. `PHASE2.md` §0.0 already records that participants must share
   a PID namespace — this is the second of the two outcomes that row names.

   **Re-executed 2026-08-23 and confirmed, and the confirmation is what makes
   the placement non-negotiable rather than merely preferable.** All three arms
   of plan step 1 reproduce: arm A (namespaced participant, host observer) takes
   `Ok(_) => R::Gone`; arm B (host participant, container observer) and arm C
   (a genuine surviving fork inheritor, staged with the purpose-built orphan)
   **both** take the `ENOENT` arm. Arms A and C render **byte-identical** text —
   1092 bytes each, once the slot number and the interpolated pid are
   normalised. So arm membership carries no information about which fault is
   present: an arm ahead of `Ok(_)` leaves arm B firing, and any fix expressed
   at the `ENOENT` arm silences arm C, which is the one true positive this check
   exists for.

4. **A second guard, before the same `match probe`, answering a different
   question: `readlink("/proc/self")` against `getpid()`.** They agree exactly
   when `/proc` describes the observer's own pid namespace, and disagree when it
   does not — and on disagreement **no** recorded pid in the file is comparable,
   *including the observer's own*, so every verdict degrades to
   `RecordedProcess::Unknown`.

   **This is a fourth false positive; it was not in this record; and the guard
   in step 3 is structurally blind to it.** Staged 2026-08-23 with a publisher
   and `doctor` and nothing else, both inside one `unshare -U --fork --pid`
   whose `/proc` was not remounted:

   ```
   subject — publisher and doctor both inside `unshare -U --fork --pid`:
     publisher pid, as that namespace numbers it = 4
     doctor    pid, as that namespace numbers it = 8
       [WARN ] TFT014  slot 0 pid 4, byte still HELD: a fork inheritor ...
                       (the record is LIVE)
               TFT014  slot 1 pid 8, byte still HELD: a fork inheritor ...
                       (the record is FREE — a read-only participant, D18)
       19 catalogue checks: 6 passed, 5 fired, 8 not run, 0 suppressed

   isolating control — same script, same binaries, same runtime-dir shape,
   host namespace:
     publisher pid = 547313 ; doctor pid = 547317
       19 catalogue checks: 7 passed, 4 fired, 8 not run, 0 suppressed
       <- no TFT014 on either slot
   ```

   **Slot 1 is `doctor`'s own participant slot, and that is measured rather
   than inferred from the ordering.** The pid in the second finding, 8, is the
   pid printed by the process that then `exec`ed the binary — so `doctor` reads
   the record it wrote at attach, decides the process named in it is gone, and
   tells the operator to stop it. The accused process is `doctor`.

   Step 3's guard cannot reach this, and the reason is not an oversight in its
   design. Every process here is in the **same** pid namespace, so every
   recorded inode equals the observer's own and the comparison never fires —
   while the pid the record carries was written by `std::process::id()` and is
   namespace-local, and the `/proc` that `start_time_of` resolves it against is
   still the host's. The two are drawn from different numberings and the guard
   compares neither of them. **The ns-inode guard asks "is the *recorded* process
   comparable to me?"; this one asks "is my `/proc` describing my own namespace
   at all?"** They are different questions and only the second has an answer in
   this staging, which is why this is a second guard and not a widening of the
   first.

   The probe is four lines, and it was measured in three arms:
   `readlink("/proc/self") == getpid()` is **false** inside
   `unshare -U --fork --pid` (`547330` against `8`), **true** on the host
   (`531314` against `531314`), and **true** in a default container — busybox
   `readlink /proc/self` run *as* that container's pid 1 prints `1`. **Measure
   it in one process or not at all:** the first attempt read
   `$(readlink /proc/self)` from a shell and compared it against `$$`, which
   disagreed in the container too — because the command substitution forks, so
   the two halves were about two processes. That is the same
   read-of-the-wrong-process shape this record rejects the `/proc/<recorded>`
   probe for, arriving in the experiment rather than in the code.

   **A real container runtime remounts `/proc`**, which is why the `docker` arm
   is true and why this is chiefly a bare-`unshare` shape rather than a fleet
   shape. **That attribution is corroborated, not isolated**, and the difference
   is worth one sentence: the `docker` arm agrees (`readlink /proc/self` matches
   its pid 1) and the bare-`unshare` arm disagrees, but the control that would
   pin the cause on the remount alone — `unshare -U -m --fork --pid
   --mount-proc` — is refused unprivileged on this host (*"cannot change root
   filesystem propagation: Permission denied"*, and the `-r`/`--map-root-user`
   variants fail on `uid_map` instead). Two agents hit the same refusal. So the
   flag was never varied by itself, and "docker is safe because it remounts" is
   the best available reading of two arms that differ in more than one way. It is still worth the four lines: what it prevents is `doctor`
   accusing itself, which is the most alarming output this tool has, and an
   operator who believes it stops the wrong process — the exact remediation
   inversion this whole record exists to prevent, one layer further in.

   **`Unknown`-for-everything is the right answer here and the wrong one one
   bullet down in *Consequences*, and the difference is worth stating because a
   reader will notice it.** That bullet says a **failed read** of the observer's
   own facts must degrade to *today's* behaviour rather than to `Unknown` for
   every slot, because a check that can never fire is a blind spot traded for a
   false positive. This guard is not a failed read: it is a **successful** one,
   establishing that the pid column of every record in this file is drawn from a
   numbering this `/proc` does not use. There is no verdict left to give, so
   giving none is not a degradation. If the `readlink` itself fails, that is the
   failed-read case and takes the failed-read rule.

Steps 3 and 4 are why this is small at the call site: `Unknown` already lands on
the existing `(LockByte::Held, RecordedProcess::Unknown) => None` arm, so the
check reports nothing rather than reporting a different wrong thing. A
same-namespace fork inheritor, observed from a `/proc` that is its own, is
detected exactly as today.

It also keeps the project's stated bias, which is the reason to prefer `Unknown`
over inventing a third verdict: a false "alive" only delays recovery, a false
"dead" is the corruption.

**Zero is "unknown namespace".** A record written before this field reads back as
zero, which must mean *keep today's behaviour*, never "namespace 0". Old lock
files outlive the process that wrote them.

## Rationale

**Why not a third `SlotLeak` variant.** `PHASE5.md` §6's TFT014 amendment is
explicit — "No new id, and no new arena field. `TFT014`'s title already claims
this ground; a second id would be the second spelling `CLAUDE.md` forbids". A new
*verdict* inside TFT014 is still a normative change to §6, but it is not a new
id, and it does not need one: the correct output here is silence.

**Why the lock file and not the arena.** The identity records live in the lock
file precisely so that a process which cannot reach the arena can still run
`doctor`. Putting the namespace there keeps that property and — the reason it
matters for this project — leaves `FORMAT_VERSION` and `layout_hash` untouched.
CLAUDE.md's "do not add arena fields opportunistically" and `0032`'s open
question about what was reserved are **not engaged by this record.** That is
stated so a reader does not have to work it out.

**Why not simply suppress TFT014 when the recorded pid is 1.** It is a heuristic
that is wrong in both directions: pid 1 is legitimate on a host, and a namespaced
participant is not always pid 1.

## Consequences

* The identity record grows a field and **it fits, measured.** `name` narrows
  from `[u8; 32]` to `[u8; 16]` at `32..48` and `pid_ns_inode: u64` takes
  `48..56`, leaving `29..32` and `56..64` spare. The kernel caps `comm` at 15
  bytes plus NUL (`TASK_COMM_LEN`); a real record written by a process whose
  binary name is 40 characters long used exactly 15 of the 32, with `47..64`
  zero; and both CLI renderers print at most 15 columns. The narrowing costs no
  diagnostic text at all. **The stride does not change**, so the second page
  stays exactly one page (64 × 64 B). Because every reader NUL-trims, an
  unmodified decoder reads a new record's name correctly and never sees the
  inode, and a new reader sees `0` in an old record — which is already this
  record's "unknown namespace". Verified against the *unmodified*
  `Identity::from_bytes`. **Re-verified independently 2026-08-23 and unchanged:**
  a real record written by a process whose binary basename is **52** characters
  used exactly 15 of the 32 name bytes, with `47..64` zero, and the kernel cap
  was established a second way that does not depend on this codebase at all — a
  `prctl(PR_SET_NAME)` round-trip handed 36 bytes in and read
  `b"abcdefghijklmno"`, 15 bytes, back.
* **`docs/PHASE2.md` §3.3 is edited either way.** Its `4096 + 64·i` row is
  `NORMATIVE` and it **enumerates the fields** — "pid, start_time, boot_id, mode,
  name" — so adding one is a normative edit even though the stride is untouched.
  Widening the stride would be the same edit plus a second page; narrowing is
  strictly cheaper, not free of spec cost.
* **The blast radius, definitively — and one site is silent.** This bullet said
  *"seven files, and two are easy to miss"*. The 2026-08-23 audit walked every
  reference at `09efc9b`; the list below replaces the count. It is ordered by how
  loudly each site announces itself, because that ordering is the actionable
  fact: **two of the code sites compile**, and everything else is a compile
  error an implementer cannot miss. One of the two is quiet (`:125`, a panic on
  a `pub` method that production data never reaches), the other is instantly
  fatal (`:142`, a panic on *every* `to_bytes` call). The last three sub-items
  are not code sites at all — one is a *conditional* no-change, and the other two
  are documents.

  - **`crates/tf_tree_ipc/src/identity.rs:125` — silent and quiet, and it is
    a panic, not a wrong answer.** `name_str` hard-codes its fallback:

    ```rust
    let end = self.name.iter().position(|b| *b == 0).unwrap_or(32);
    core::str::from_utf8(&self.name[..end]).unwrap_or("<non-utf8>")
    ```

    With `name: [u8; 16]` and no NUL anywhere in it, `end == 32` and
    `&self.name[..32]` panics. Executed as a standalone reproduction of exactly
    those two lines, with the control that isolates the NUL as the cause:

    ```
    subject (16 bytes, no NUL):    thread 'main' panicked:
                                   range end index 32 out of range for slice of
                                   length 16                          exit 101
    control (16 bytes with a NUL): node                               exit 0
    ```

    **It is not reachable from data this codebase writes.** `self_comm`
    (`crates/tf_tree_ipc/src/procstat.rs:130`) copies at most `out.len()` bytes
    of a `comm` the kernel caps at 15, so every record this workspace produces
    carries a NUL — which is also why the narrowing is free, and the two facts
    are the same fact. It is reachable through `Identity::from_bytes`, which is
    `pub`, validates `pid != 0` and nothing else, and decodes a file any process
    on the box with the right uid can `pwrite` into. So: corrupt or adversarial
    bytes, not production data. The fix is `unwrap_or(self.name.len())`, which is
    additionally the spelling that cannot rot the next time this field moves.
  - **`crates/tf_tree_ipc/src/identity.rs:142` — the second site that compiles,
    and unlike `:125` it fires on the very first call.** `to_bytes` writes the
    name with

    ```rust
    out[32..64].copy_from_slice(&self.name);
    ```

    Both sides are slices, so narrowing `name` type-checks; `copy_from_slice`
    then compares lengths at run time and panics — *"source slice length (16)
    does not match destination slice length (32)"*, exit 101 — on **every**
    `to_bytes()`, which is every `write_identity`, which is every `open()` that
    registers. So it is not a latent hazard like `:125`; it is a change that
    compiles and then cannot rendezvous at all. Named separately because the
    ordering of this list is its point, and an earlier draft of this amendment
    said `:125` was *the* silent site. The fix is
    `out[32..32 + self.name.len()].copy_from_slice(&self.name);` — the same
    cannot-rot spelling as `:125`'s.
  - **`crates/tf_tree_ipc/src/identity.rs` — seven edits in the one file, the
    two tests counting as one.**
    `:84` the `name` field plus the new `pub pid_ns_inode: u64` (`0` = unknown
    namespace); `:102` and `:118`, `of_self` and `of_self_best_effort` — and in
    **both** a failed namespace read must yield `0` and **not** an `IpcError`,
    because `of_self`'s own doc comment says the record is advisory and
    `of_self_best_effort` exists precisely so that an unreadable `/proc` cannot
    fail an `open()`; `:125` the panic above; `:142` `to_bytes`; `:161-162`
    `from_bytes`; and the two tests that build a 32-byte name, `:204` and `:229`
    — of which `the_field_offsets_are_pinned` (`:221`, asserting `b[32..64]` at
    `:237`) needs a **new** assertion that bytes `56..64` are zero, since that
    test is the only place in the workspace where this layout is pinned at all
    and the new tail would otherwise be pinned by nothing.
  - **`crates/tf_tree_ipc/src/lockfile.rs:550-558` — not in this record's list,
    and it is why the gate bullet above matters.**
    `identity_records_round_trip_at_the_specified_offsets` builds an `Identity`
    with `let mut n = [0u8; 32];`. `tf_tree_ipc` has **no** `shm` feature — it has
    no `[features]` block — so this test is compiled by plain
    `cargo nextest run --workspace`, and until it is fixed `just test` does not
    run: it does not build.
  - **`crates/tf_tree_ipc/src/procstat.rs:127-136`.** `self_comm() -> [u8; 16]`
    is a `pub` signature, re-exported at `crates/tf_tree_ipc/src/lib.rs:121`, so
    this is a public break on a **publishing** crate and takes the `0.0.x`
    treatment `CLAUDE.md` describes. Its `:127` doc prose hard-codes *"the 32
    bytes an identity record has for it"* and is a second edit in the same
    function. Callers workspace-wide are exactly three — `identity.rs:102`,
    `identity.rs:118`, `crates/tf_tree/src/open.rs:1548` — which is what makes
    the break cheap in-tree and does not make it cheap out of tree. The new
    namespace reader belongs here too, beside `boot_id` and `self_start_time`,
    which is where this crate keeps its `/proc` readers.
  - **`crates/tf_tree/src/open.rs:1547-1549`.** `name_bytes()` must pad 16 → 32.
    `HelloRequest::client_name` is **wire** bytes `56..88` of an 88-byte datagram
    (`crates/tf_tree_ipc/src/wire.rs:170`, `:187`, `:210`), pinned by
    `the_byte_layout_is_pinned` at `wire.rs:417` and by §3.7, and it does **not**
    change here. This is the one site where the two 32s in this change are
    unrelated to each other, which is exactly why it is the one an implementer
    will try to "simplify".
  - **`crates/tf_tree_cli/tests/attach.rs` — three, exactly as this record
    already said.** An earlier draft of this amendment claimed the type error was
    a single site, `comm()`'s return type at `:396` (with its `:395` doc line
    *"A 32-byte `comm` field"*), and that the literals truncated silently. That
    is backwards and was caught in review: narrowing `Identity::name` alone
    produces **three** `E0308`s, at the three literals — `:472`
    `"a-writer-that-died"` (18 B), `:553` `"a-parent-that-forked"` (20 B), `:638`
    `"a-forked-consumer"` (17 B) — because each assigns `comm()`'s `[u8; 32]` to
    a `[u8; 16]` field, and **none** at `:396`, because a helper returning
    `[u8; 32]` is well-typed on its own. Narrowing `comm()` too is a choice the
    implementer makes, not one the compiler demands, and it is only *after* that
    choice that the literals go quiet.
    They must be shortened either way, for a reason this record did not give:
    written at offset 32, a 17–20-byte name is the **only** thing in this
    repository that puts nonzero bytes in `48..56`, which is precisely the range
    that becomes `pid_ns_inode`. Left alone they would hand a zero-means-unknown
    compatibility test a fabricated namespace — the test passing or failing for a
    reason that has nothing to do with the field. Each of the three `Identity`
    literals additionally needs `pid_ns_inode: 0`, three more compile errors at
    the same three lines.
  - **`crates/tf_tree_cli/src/lib.rs`.** `:1913-1921`, the two guards, beside the
    existing `stored_start_time == 0` guard at `:1919` and ahead of the
    `match probe` at `:1922`; `:1857-1870`, `recorded_process`, which is where
    the observer's own inode and its own `/proc`-is-mine answer are read, and
    where a **failed** read degrades to today's behaviour and never to
    `Unknown`-for-everything; `:1874-1911`, the doc comment, whose *"Same three
    inputs, same arms, same bias"* (`:1879`) stops being true the moment there
    are five inputs; and `~:2769-2813`, eight `recorded_given` call sites in one
    unit test, every one of which grows arguments.
  - **`crates/tf_tree_cli/src/doctor.rs:261-266` — not in this record.**
    `RecordedProcess`'s own enum doc repeats the same claim in the same words
    (*"same three inputs … same arms, same bias"*, `:265`). Two copies of a claim
    and one edit named is how the stale half survives a change; both are named
    here.
  - **`crates/tf_tree_cli/src/checks.rs` — no change *for the shape this record
    is about*, and the qualifier is load-bearing.** For a **held** byte the fix
    lands without touching the check that fires: the `state == SlotState::Free`
    early return (`:1320`) only fires on `RecordedProcess::Gone`, and the main
    match's `(LockByte::Held, Running | Unknown)` arm (`:1330`) returns `None`.
    A reader who does not know that will go looking for an edit that is not there.

    But the guard is **not** verdict-neutral, and an earlier draft of this
    amendment said "no change" flat. `slot_leak`'s other arm reads
    `(LockByte::Free, Gone | Unknown) => Some(SlotLeak::Abandoned)` (`:1326`),
    and the function's own doc table states it deliberately at `:1269` — *"the
    byte alone is the leak signature, and §5.1 says the byte is the fact"*. So a
    slot with a **non-`FREE` record, a free byte, and a recorded process that
    reads `Running` today** flips from silence to TFT014's *byte free* shape once
    the guard degrades it to `Unknown`. Accepted rather than fixed, and the
    reachability is why: getting `Running` out of a namespaced participant needs
    the host process at the recorded ns-local pid to have a *matching*
    `start_time`, which is the pid-reuse collision the identity triple exists to
    exclude. It is named here so the next reader does not discover it as a
    surprise, and it is the one verdict this record moves that it did not intend
    to.
  - **Docs.** `docs/PHASE2.md:380` — §3.3's `4096 + 64·i` row, `NORMATIVE`,
    enumerating the field set (already in the plan). `docs/PHASE2.md:37` — §0.0's
    status row, **not** in the plan, and `CLAUDE.md` says §0.0 outranks the
    prose: it currently reads *"Derived from the code and from §3.1's own text,
    not reproduced — `unshare --fork --pid` is unavailable in the environment
    this was written in"*, and that is no longer true of either outcome that row
    names. `docs/PHASE5.md` §6's TFT014 amendment — the arm list at
    `~:1455-1472`; the catalogue row at `:957` needs **no** change, because the
    id, the severity and the evidence column are all unaffected, and a reader
    should not have to check. `docs/RUNBOOK.md:414` and `:623` — **not** in the
    plan — are the operator-facing *byte still HELD* prose, which is the text an
    operator acts on and which today tells them to stop a child that may not
    exist.
* **`just test` does not cover the `attach.rs` half of this change — and the
  number this bullet used to cite was not `just test`'s.** It read: *"with the
  narrowing applied and those three literals still wrong, `just test` finished
  `160 tests run: 160 passed`"*. At `09efc9b`, `cargo nextest list --workspace`
  is **878** testcases and `-p tf_tree_cli` alone is **162**, so a 160-test run
  was neither — it was something narrower than a single crate. **The conclusion
  survives, re-established from the code rather than from that run:**
  `crates/tf_tree_cli/tests/attach.rs:12` is
  `#![cfg(all(feature = "shm", target_os = "linux"))]`, `tf_tree_cli`'s
  `default = ["counters", "compression"]` does not include `shm`
  (`crates/tf_tree_cli/Cargo.toml:93`), and the only justfile lines that build
  that target with the feature are `just shm-check`'s. So those fixtures are
  compiled there and nowhere else, and a plan that leans on `just test` ships
  the break.

  **The evidence as cited was not merely thin, it was impossible**, and that is
  the part worth keeping. A real `just test` with the narrowing applied would
  not have printed `160 passed`; it would have failed to **compile**, on
  `crates/tf_tree_ipc/src/lockfile.rs:550`, whose `let mut n = [0u8; 32];` sits
  in a plain `#[cfg(test)]` module of a crate that has no `[features]` block at
  all — its manifest says so in a comment, and that comment is the reason the
  site is easy to walk past. See the blast radius below.
* **This record does not make containers safe, and the gap is measured.** A
  runtime that masks or synthesises `/proc/sys/kernel/random/boot_id` is refused
  by the *handshake* with `BootIdMismatch`, and told that nothing in the arena is
  alive and it should be removed — about a healthy arena. That is upstream of
  `doctor`, a different failure with a worse remediation, and it needs its own
  issue. Named here only so nobody reads `0033` as covering it.
* `doctor` gains a dependency on reading its own `/proc/self/ns/pid`. If that
  read fails it must degrade to today's behaviour, not to `Unknown` for
  everything — otherwise TFT014 becomes unable to fire at all, which trades a
  false positive for a blind spot.
* One class stays undetectable and should be said out loud: a participant in a
  *different* namespace whose byte really has been inherited by a fork. This
  record makes that report nothing rather than report wrongly.
* `PHASE5.md` §6 needs a spec edit for TFT014's verdict class.
* **The scope this record does not cover, stated because it will otherwise be
  read as covering it.** `0033` changes `doctor`'s `TFT014` report and nothing
  else. The arena's `ParticipantRecord` gains **no** namespace discriminator, so
  the three paths `docs/PHASE2.md:37` itself calls corrupting still resolve a
  namespace-local pid against the observer's `/proc`: **(1)**
  `use_ofd_liveness`'s fallback, where the `(pid, start_time, boot_id)` triple
  decides A8 claim liveness whenever `F_OFD_GETLK` declines to answer; **(2)**
  `liveness_for` for a tree with no probe, where `record_is_alive` is the
  *entire* predicate; **(3)** `Tree::reparent`'s `participant_is_alive`, which
  reads `/proc` even on a tree that holds a probe. All three are in
  `crates/tf_tree/src/tree.rs`, and each false "dead" there is the corrupting
  direction §6.2 forbids rather than a wrong sentence in a diagnostic.

  **That is the right scope, and the reason is the one this record already argues
  in *Rationale* rather than a new one.** The identity records live in the lock
  file so that a process which cannot map the arena can still run `doctor`; a
  namespace field there costs no `FORMAT_VERSION` and no `layout_hash`, which is
  what makes this record cheap. The same field in `ParticipantRecord` is an arena
  field, and an arena field engages `CLAUDE.md`'s *"do not add arena fields
  opportunistically"*, `0032`'s open question about what the region table
  actually reserved, and a `FORMAT_VERSION` bump — three arguments this record
  does not make and must not borrow the conclusion of. It is a separate decision,
  and it should cite `0029`, since two of the three paths are the ones that record
  is already about.

## Implementation plan

1. **A regression test that fails first.** **Four arms, not three** — open
   question 2's container experiment found a second false positive on a
   different code path, and the 2026-08-23 audit found a fourth on neither.
   They are lettered here, not numbered, and **A/B/C keep the meaning they have
   in *Context* and *Decision*** so that "arm C" names the same staging
   everywhere in this record; the new one is appended as D rather than inserted:
   - **A** — the namespaced participant seen from the host (`Ok(_)` arm),
     `unshare -U --fork --pid`, unprivileged, no docker;
   - **B** — a **host** participant seen from a container (`ENOENT` arm),
     `docker run` with a bind-mounted runtime dir, executed;
   - **C** — a genuine surviving fork inheritor, which needs a purpose-built
     orphan binary because every `fork_child` mode `_exit`s its child. **This is
     the true positive** and it must keep firing after every step below;
   - **D** — participant *and* `doctor` both inside one bare `unshare -U --fork
     --pid`, which fires `TFT014` on every slot including `doctor`'s own. The
     arm *Decision* 4 exists for, and the only one step 4a does not fix.

   Today all four produce `TFT014`; after step 4b only **C** does. Verified by
   the test failing on the parent commit.

   **Arm A must be staged with `doctor` on the host, and this is a constraint on
   the test rather than a stylistic note.** Moving `doctor` inside the namespace
   turns arm A into arm D, which the ns-inode guard alone does **not** silence —
   so a test written that way reports the step-4a fix as not working, and, once
   both guards land, passes for a reason that has nothing to do with the fix it
   is pinning. A and D differ by where the observer stands and by nothing else,
   which is what makes them a usable pair.

   **Note where this can live:** arms B and D need a runtime dir and `shm`, so
   they belong behind `just shm-check`, not `just test` — see the gate bullet in
   *Consequences*.
2. **Repack the identity record**: `name` to `[u8; 16]` at `32..48`,
   `pid_ns_inode: u64` at `48..56`, per open question 1. Pin zero-means-unknown
   with a test that reads a pre-change record, and add the assertion that bytes
   `56..64` are zero to `the_field_offsets_are_pinned`. **Work the blast-radius
   list in *Consequences*, not the count that used to be here** — in particular
   `identity.rs:125`'s `unwrap_or(32)`, which is the one site that does not
   announce itself, and `lockfile.rs:550`, which is the one site that breaks
   plain `just test`. Verified by `just test` **and** `just shm-check`: the first
   covers `tf_tree_ipc`, which has no features, and only the second compiles
   `attach.rs`.
3. **Write the inode at registration, with `readlink`** — not `stat`, and never
   `lstat`; see Decision step 1. Verified by reading the raw 64 bytes out of the
   lock file and comparing against `readlink /proc/self/ns/pid`.
4. **The two `recorded_given` guards**, both before the whole `match probe`
   rather than as arms ahead of `Ok(_) => R::Gone`.
   - **4a**, the recorded-namespace guard (*Decision* 3). Verified by step 1's
     arms **A** and **B** flipping to pass and arm **C** staying passing.
   - **4b**, the `readlink("/proc/self")` vs `getpid()` guard (*Decision* 4).
     Verified by step 1's arm **D** flipping to pass — **and by arm D still
     failing after 4a alone**, which is the measurement that shows the two guards
     answer different questions rather than one question twice.

   Both land in `crates/tf_tree_cli/src/lib.rs:1913-1921`; `checks.rs` is
   untouched, for the reason *Consequences* gives.
5. **`docs/PHASE2.md`**: §3.3's `NORMATIVE` row at `:380`, which enumerates the
   field set, **and** §0.0's row at `:37`, whose *"not reproduced"* is now false
   of both outcomes it names — §0.0 outranks the prose, so leaving it is worse
   than leaving §3.3. **This record makes that edit**, and says so because
   `0029:364` also reproduced the collision and also observes that the row can be
   retired: two records seeing the same stale sentence is how it survives both.
6. **`docs/PHASE5.md` §6 wording** for the verdict class (`~:1455-1472`). The
   catalogue row at `:957` is unchanged; say so in the commit so the next reader
   does not re-check it.
7. **The operator-facing prose**: `docs/RUNBOOK.md:414` and `:623`, and
   `crates/tf_tree_cli/src/doctor.rs:261-266`'s enum doc. This step is last
   because it is the one that can be written only once the arms are known to be
   silenced, and it is not optional: `RUNBOOK.md:623` is the sentence an operator
   acts on.

## Open questions

~~Resolved before this moves from `draft` to `ready`.~~ **All three resolved
2026-08-23 by measurement.** They are kept with their answers rather than deleted,
because two of them changed the Decision.

1. **RESOLVED: narrow `name` to `[u8; 16]`; it is free, and it is
   backward-compatible in both directions.** ~~Where does the inode go in a full
   64-byte record?~~

   Linux caps `comm` at 15 bytes of content — `proc_pid_comm(5)`: "Strings longer
   than TASK_COMM_LEN (16) characters (including the terminating null byte) are
   silently truncated" — confirmed two ways on 6.8.0, including a `prctl`
   round-trip that handed back 15 of 36 bytes. A real record, written by a
   participant whose binary was named with 40 characters:

   ```
   +32  61 5f 70 75 62 6c 69 73 68 65 72 5f 77 69 74 00  |a_publisher_wit.|
   +48  00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00  |................|
   ```

   Fifteen bytes used of thirty-two. Both renderers agree: `tf_tree participants`
   and `tf_tree top` each printed `a_publisher_wit` and neither can receive more.

   So `name` goes to `32..48` and `pid_ns_inode: u64` to `48..56`. **No version
   field is needed**, verified against the unmodified decoder: an old reader
   NUL-trims and reads a new record's name correctly; a new reader sees `0` in an
   old record, which is already this record's "unknown namespace" marker.

   Widening the stride to 128 is strictly worse and is rejected: same normative
   §3.3 edit, plus it turns a region that is exactly one page into two, and the
   lock file carries no version field to make the change detectable.

2. **RESOLVED: no. A default container does not mask `boot_id`, and no namespace
   type can.** ~~Does a container runtime that masks or synthesises `boot_id`
   change the analysis?~~

   `man 7 namespaces` enumerates the eight types and what each isolates; none is
   the boot id — the Time namespace isolates the boot and monotonic *clocks*, not
   the id. Executed: `docker run --rm alpine` printed the host's boot_id verbatim
   (`5169c48c-…`) while its pid namespace differed (`pid:[4026532489]`). Docker
   29.1.3, overlay2.

   So the premise holds and `docker/tf2` is safe as it stands. What the same
   experiment *did* turn up is the third false positive now recorded in Decision
   step 3, and the `BootIdMismatch` handshake refusal now in *Consequences* — a
   runtime that masks the file breaks the rendezvous, upstream of `doctor`, and
   is not this record's to fix.

3. **RESOLVED: reuse is immediate and, for containers, total — and it is
   harmless here.** ~~Is an nsfs inode stable enough to compare?~~

   ```
   200 x  unshare -U --fork --pid sh -c 'readlink /proc/self/ns/pid'
     200 runs, 33 distinct values; first repeat at run 9, reusing run 1's
     most frequent value seen 14 times; range 4026532487..4026532521

   10 x   docker run --rm busybox readlink /proc/self/ns/pid
     all ten returned pid:[4026532491]        1 distinct of 10
   ```

   A freed inum is handed straight back. (The first attempt at showing that got
   `False`, because it killed the `unshare` wrapper rather than the process
   group, so the namespace still had a live member and nothing had been freed.)

   **The control is what closes the question:** forty namespaces created and held
   alive *at once* gave forty distinct inodes, zero duplicates. Reuse is caused by
   destruction and by nothing else — inums are unique among **live** namespaces.

   The comparison is against the observer's **own** namespace, which is alive by
   construction. So a match means either the observer's own namespace — correct,
   and the point — or one that is **dead**, and a dead pid namespace has no live
   members, so the recorded participant really is gone, which is exactly what
   `Gone` asserts. The reused-inode case lands on the right verdict by the wrong
   route. The root namespace's inum is never freed, so a host observer is immune
   outright.

   **No pairing field is needed and none is added.** Pairing with `boot_id` was
   considered and rejected as redundant. What the measurement *does* forbid is any
   future use of this field to conclude two namespaces are the **same** one: it
   establishes "not provably different", which is all `Unknown` needs.
