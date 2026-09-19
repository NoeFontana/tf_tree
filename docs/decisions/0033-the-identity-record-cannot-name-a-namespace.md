# 0033: the identity record cannot name a namespace

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** this PR, all seven plan steps (#239). Moved `draft` →
`ready` 2026-08-23 and `ready` → `implemented` the same day. This record exists
because the obvious fix reads the wrong process, and because the field it needs
does not fit.
## Context

Issue #239. A live participant inside `unshare -U --fork --pid`, seen from the
host, records a pid that on the host is another process (`systemd`, pid 1) with a
different start time, so the probe reports it gone and `TFT014` gives the
remediation *stop the child* for a healthy process.

## Decision

**Record the namespace at registration; do not derive it at diagnosis.**
Step 4 is a separate guard, asking only whether the observer's `/proc`
describes the observer's own namespace. Both guards sit before the whole
`match probe`.

1. A participant reads its own `/proc/self/ns/pid` inode when it writes its
   `Identity` record, and stores it. **It must be read with `readlink` and the
   `pid:[N]` text parsed**: `readlink` is the only read correct in every arm.
   `stat().ino` fails `EACCES` under an unmapped user namespace (`unshare -U
   --fork`; a default Docker container has none, so `stat` succeeds there), and
   `lstat().ino` returns a wrong number everywhere, because it stats the procfs
   dentry, not the `nsfs` inode.
2. `doctor` compares the recorded inode against its **own**, never against one
   read through the recorded pid.
3. `recorded_given` gains a **guard before the whole `match probe`**, beside the
   existing `stored_start_time == 0` guard — *not* an arm ahead of
   `Ok(_) => R::Gone`. A recorded namespace that differs from the observer's
   means the pid is not comparable from here, so the verdict is
   `RecordedProcess::Unknown` whatever the probe returned. Placement is
   non-negotiable: a namespaced participant seen from the host takes
   `Ok(_) => R::Gone`, a host participant seen from a container takes `ENOENT`,
   and so does a genuine fork inheritor.
4. **A second guard, before the same `match probe`: `readlink("/proc/self")`
   against `getpid()`.** They agree exactly when `/proc` describes the observer's
   own pid namespace. On disagreement no recorded pid in the file is comparable,
   *including the observer's own*, so every verdict degrades to
   `RecordedProcess::Unknown`. Without it, a publisher and `doctor` both inside
   one bare `unshare -U --fork --pid` make `doctor` fire `TFT014` on its own
   slot, which step 3's guard cannot see (every recorded inode equals the
   observer's). If the `readlink` itself fails, that is the failed-read case in
   *Consequences*, not `Unknown`-for-everything.

`Unknown` lands on the existing `(LockByte::Held, RecordedProcess::Unknown) =>
None` arm. A false "alive" only delays recovery; a false "dead" is the corruption.
**Zero is "unknown namespace":** a record written before this field keeps
today's behaviour, never "namespace 0".

## Rationale

No third `SlotLeak` variant and no new id (`PHASE5.md` §6); the correct output is
silence. The lock file, not the arena, so `FORMAT_VERSION` and `layout_hash` are
untouched. Inode reuse is harmless: the comparison is against the observer's own
live namespace, so a match is that namespace or a dead one whose participant is
gone. The field means "not provably different", never "the same namespace".

## Consequences

* **Layout.** `name` narrows to `[u8; 16]` at `32..48`; `pid_ns_inode: u64` takes
  `48..56`; `56..64` stay zero (`the_field_offsets_are_pinned`). `comm` is at most
  15 bytes plus NUL; the stride is unchanged; old readers never see the inode.
* **Sites.** `Identity::name_str` falls back to `self.name.len()`, `to_bytes`
  copies into `out[32..32 + self.name.len()]`; `self_comm() -> [u8; 16]` is a
  public break; `open.rs`'s `name_bytes()` pads 16 → 32 because
  `HelloRequest::client_name` (`the_byte_layout_is_pinned`) does not change. A
  failed namespace read yields `0`, never an `IpcError`; a failed read of
  `doctor`'s own `/proc/self/ns/pid` degrades to today's behaviour, not
  `Unknown` for every slot.
* **Unintended verdict move.** A non-`FREE` record with a free byte whose
  process reads `Running` flips to TFT014's *byte free* shape once the guard
  yields `Unknown`; accepted (it needs a matching `start_time`).
* **Gate.** `attach.rs` is compiled only by `just shm-check` (`shm` feature).
* **Not covered.** A runtime that masks `boot_id` is refused upstream with
  `BootIdMismatch`; a different-namespace fork inheritor stays undetectable.
  `ParticipantRecord` gains no namespace field, so `use_ofd_liveness`'s
  fallback, `liveness_for` on a probe-less tree and `Tree::reparent`'s
  `participant_is_alive` still resolve a namespace-local pid: a separate
  decision, which should cite `0029`.

## Implementation plan

1. **A regression test that fails first, four arms** (`tft014_namespace_*` in
   `crates/tf_tree_cli/tests/attach.rs`, behind `just shm-check`):
   - **A** — namespaced participant, `doctor` on the host (`Ok(_)` arm),
     `unshare -U --fork --pid`; moving `doctor` inside turns A into D.
   - **B** — host participant seen from a container (`ENOENT` arm).
   - **C** — a genuine surviving fork inheritor; it must keep firing.
   - **D** — participant and `doctor` both inside one bare
     `unshare -U --fork --pid`; skips loudly where that is refused.

   All four fire `TFT014` before the fix; only **C** after step 4b.
2. **Repack the identity record** as in *Consequences*.
3. **Write the inode at registration, with `readlink`** (*Decision* 1).
4. **The two `recorded_given` guards**, both before the whole `match probe`.
   - **4a**, the recorded-namespace guard (*Decision* 3): arms A and B flip to
     pass, C stays passing.
   - **4b**, the `/proc/self` vs `getpid()` guard (*Decision* 4): arm D flips to
     pass, and still fails after 4a alone.
5. **`docs/PHASE2.md`** §3.3 `NORMATIVE` row and §0.0 row; 6. **`docs/PHASE5.md`**
   §6 wording; 7. **operator prose** in `RUNBOOK.md` and `doctor.rs`'s enum doc.
