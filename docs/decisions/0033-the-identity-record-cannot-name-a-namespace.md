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
2. `doctor` compares the recorded inode against its **own**, never against one
   read through the recorded pid.
3. `recorded_given` gains an arm **ahead of** `Ok(_) => R::Gone`: a recorded
   namespace that differs from the observer's means the pid is not comparable
   from here, so the verdict is `RecordedProcess::Unknown`, not `Gone`.

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

* The identity record grows a field, and **it does not fit.** 64 bytes with
  exactly 3 spare at `29..32`; a `u64` inode needs 8. Either `name` narrows from
  32 bytes or the stride widens. That is a lock-file layout decision and it is
  this record's open question 1, not an implementation detail.
* `doctor` gains a dependency on reading its own `/proc/self/ns/pid`. If that
  read fails it must degrade to today's behaviour, not to `Unknown` for
  everything — otherwise TFT014 becomes unable to fire at all, which trades a
  false positive for a blind spot.
* One class stays undetectable and should be said out loud: a participant in a
  *different* namespace whose byte really has been inherited by a fork. This
  record makes that report nothing rather than report wrongly.
* `PHASE5.md` §6 needs a spec edit for TFT014's verdict class.

## Implementation plan

1. **A regression test that fails first**, staging both halves in one file: the
   namespaced participant (`unshare -U --fork --pid`, unprivileged, no docker)
   and a genuine surviving fork inheritor via a purpose-built orphan binary.
   Today both produce TFT014; after step 4 only the second does. Verified by the
   test failing on the parent commit.
2. **Widen or repack the identity record**, per open question 1 — with the
   zero-means-unknown rule pinned by a test that reads a pre-change record.
3. **Write the inode at registration.** Verified by reading the raw 64 bytes out
   of the lock file and comparing against `readlink /proc/self/ns/pid`.
4. **The `recorded_given` arm**, ahead of `Ok(_) => R::Gone`. Verified by step 1
   flipping to pass on the namespaced arm and staying passing on the fork arm.
5. **`PHASE5.md` §6 wording** for the verdict class.

## Open questions

Resolved before this moves from `draft` to `ready`.

1. **Where does the inode go in a full 64-byte record?** Narrowing `name` from 32
   to 24 bytes costs diagnostic text in exactly the output this record is about;
   widening the stride changes an on-disk offset table that `lockfile.rs`'s module
   documentation publishes as a layout. Both are answerable by inspection, and
   neither has been chosen here on purpose — the measurement that would settle it
   is how many bytes of `comm` `doctor` actually renders.
2. **Does a container runtime that masks or synthesises `boot_id` change the
   analysis?** Neither the report nor its skeptic tested one; `unshare` shares the
   host's procfs mount, so the "`boot_id` is not a discriminator" check was
   *vacuously* true as executed. The premise holds for a stronger reason — no
   per-namespace boot id exists in the kernel — but `docker/tf2` is a staging
   nobody has used for this, and it is the case the issue gestures at.
3. **Is a namespace inode stable enough to compare?** It is an inode in the `nsfs`
   filesystem, unique while the namespace lives and **reusable after it dies**. A
   dead namespace's inode reappearing on a live one would make a stale record
   compare equal. The window and whether it matters here are unmeasured.
