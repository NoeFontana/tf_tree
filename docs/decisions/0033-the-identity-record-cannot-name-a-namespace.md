# 0033: the identity record cannot name a namespace

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** none — this record exists because the obvious fix reads the
wrong process, and because the field it needs does not fit.

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

1. A participant reads its own `/proc/self/ns/pid` inode when it writes its
   `Identity` record, and stores it. Executed: readable, cheap, and it differs
   across the boundary (`pid:[4026532489]` vs `pid:[4026531836]`).

   **It must be read with `readlink` and the `pid:[N]` text parsed — not with
   `stat`.** Inside a user namespace with no uid map, `readlink` returns
   `pid:[4026532513]` while `statx` returns `Permission denied` and `lstat`
   returns `80100317` — a procfs dentry inode, not the nsfs one. So
   `fs::metadata().ino()` fails and `symlink_metadata().ino()` succeeds *with a
   plausible wrong number*, which is the same class of defect as the probe this
   record already rejects. (The refusal comes from the **user** namespace without
   a uid map, not from the pid namespace; `unshare -U --fork` alone reproduces it.
   The staging this record uses has both.)
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

Step 3 is why this is small at the call site: `Unknown` already lands on the
existing `(LockByte::Held, RecordedProcess::Unknown) => None` arm, so the check
reports nothing rather than reporting a different wrong thing. A same-namespace
fork inheritor is detected exactly as today.

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
  `Identity::from_bytes`.
* **`docs/PHASE2.md` §3.3 is edited either way.** Its `4096 + 64·i` row is
  `NORMATIVE` and it **enumerates the fields** — "pid, start_time, boot_id, mode,
  name" — so adding one is a normative edit even though the stride is untouched.
  Widening the stride would be the same edit plus a second page; narrowing is
  strictly cheaper, not free of spec cost.
* **The narrowing moves more than the three obvious call sites.** Seven files, and
  two are easy to miss: `identity.rs`'s own `the_field_offsets_are_pinned` test,
  and three `attach.rs` fixtures that write 17–20-byte synthetic names.
  `tf_tree_ipc::self_comm()` becomes `[u8; 16]` — a public signature on a
  publishing crate, so a `0.0.x` break — and `tf_tree::open::name_bytes()` must
  pad 16 → 32, because `HelloRequest::client_name` is **wire** bytes `56..88`
  under §3.7 with its own pinned test and does not change here.
* **`just test` does not cover this change.** The `attach.rs` fixtures are
  `shm`-gated, so with the narrowing applied and those three literals still
  wrong, `just test` finished `160 tests run: 160 passed`. `just shm-check` is
  the gate; a plan that leans on `just test` would ship the break.
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

## Implementation plan

1. **A regression test that fails first.** **Three arms, not two** — open
   question 2's container experiment found a second false positive on a different
   code path:
   - the namespaced participant seen from the host (`Ok(_)` arm) —
     `unshare -U --fork --pid`, unprivileged, no docker;
   - a **host** participant seen from a container (`ENOENT` arm) — `docker run`
     with a bind-mounted runtime dir, executed;
   - a genuine surviving fork inheritor, which needs a purpose-built orphan
     binary because every `fork_child` mode `_exit`s its child.

   Today all three produce TFT014; after step 4 only the third does. Verified by
   the test failing on the parent commit. **Note where it can live:** the
   container arm needs a runtime dir and `shm`, so it belongs behind
   `just shm-check`, not `just test` — see the gate note in *Consequences*.
2. **Repack the identity record**: `name` to `[u8; 16]` at `32..48`,
   `pid_ns_inode: u64` at `48..56`, per open question 1. Pin zero-means-unknown
   with a test that reads a pre-change record, and expect seven files — including
   `identity.rs`'s own offset-pinning test and three `attach.rs` fixtures whose
   synthetic names are too long. Verified by `just shm-check`, which is the only
   gate that compiles those fixtures.
3. **Write the inode at registration, with `readlink`** — not `stat`; see
   Decision step 1. Verified by reading the raw 64 bytes out of the lock file and
   comparing against `readlink /proc/self/ns/pid`.
4. **The `recorded_given` guard**, before the whole `match probe` rather than as
   an arm ahead of `Ok(_) => R::Gone`. Verified by step 1 flipping to pass on both
   namespace arms and staying passing on the fork arm.
5. **`docs/PHASE2.md` §3.3's NORMATIVE row**, which enumerates the field set.
6. **`docs/PHASE5.md` §6 wording** for the verdict class.

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
