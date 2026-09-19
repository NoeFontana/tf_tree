# 0033: the identity record cannot name a namespace

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** this PR, all seven plan steps (#239). Moved `draft` →
`ready` 2026-08-23 and `ready` → `implemented` the same day. This record exists
because the obvious fix reads the wrong process, and because the field it needs
does not fit.
## Context

Issue #239: a participant inside `unshare -U --fork --pid` records a pid that is
another process on the host, so `TFT014` blames a healthy one.

## Decision

**Record the namespace at registration; do not derive it at diagnosis.**

1. A participant stores its `/proc/self/ns/pid` inode in its `Identity` record,
   **read with `readlink` and the `pid:[N]` text parsed** (`stat`/`lstat` fail or
   return the wrong inode).
2. `doctor` compares the recorded inode against its **own**, never one read
   through the recorded pid.
3. `recorded_given` gains a guard **before the whole `match probe`**, beside the
   `stored_start_time == 0` guard: a differing recorded namespace yields
   `RecordedProcess::Unknown` whatever the probe returned.
4. **A second guard before the same `match probe`: `readlink("/proc/self")`
   against `getpid()`.** They agree exactly when `/proc` is the observer's own pid
   namespace; otherwise every verdict degrades to `Unknown`. A failed `readlink`
   degrades to pre-`0033` behaviour.

**Zero is "unknown namespace"**: a pre-`0033` record keeps today's behaviour. No
new `SlotLeak` variant or id.

## Consequences

* `name` narrows to `[u8; 16]` at `32..48`; `pid_ns_inode: u64` takes `48..56`
  (`the_field_offsets_are_pinned`); `self_comm() -> [u8; 16]` is a public break;
  `name_bytes()` pads 16 → 32 (`the_byte_layout_is_pinned`). `FORMAT_VERSION` and
  `layout_hash` are untouched.
* `attach.rs` is compiled only by `just shm-check`.
* **Not covered.** A different-namespace fork inheritor stays undetectable;
  `use_ofd_liveness`'s fallback, `liveness_for` and `Tree::reparent`'s
  `participant_is_alive` still resolve a namespace-local pid (cite `0029`).

## Implementation plan

1. **Regression tests, four arms** (`tft014_namespace_*` in
   `crates/tf_tree_cli/tests/attach.rs`): **A** namespaced participant, `doctor`
   on the host; **B** host participant seen from a container; **C** a genuine fork
   inheritor (must keep firing); **D** both inside one `unshare -U --fork --pid`.
2. Repack the identity record. 3. Write the inode at registration.
4. The guards: **4a** (*Decision* 3) flips A, B; **4b** (*Decision* 4) flips D.
5. `PHASE2.md` §3.3, §0.0; 6. `PHASE5.md` §6; 7. `RUNBOOK.md`, `doctor.rs`.
