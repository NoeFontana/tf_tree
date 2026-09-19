# tf_tree_ipc

[![crates.io](https://img.shields.io/crates/v/tf_tree_ipc.svg?logo=rust)](https://crates.io/crates/tf_tree_ipc)
[![docs.rs](https://img.shields.io/docsrs/tf_tree_ipc?logo=docsdotrs)](https://docs.rs/tf_tree_ipc)
[![Licence](https://img.shields.io/badge/licence-MIT%20OR%20Apache--2.0-blue.svg)](#licence)

Zero-configuration rendezvous for [`tf_tree`](https://crates.io/crates/tf_tree)
shared arenas: runtime-directory discovery, the OFD lock file, `SOCK_SEQPACKET`
descriptor passing, and the `open()` decision machine. **Linux only**: the crate
is `#![cfg(target_os = "linux")]` and compiles empty elsewhere. You normally reach
it through `tf_tree`'s default-off `shm` feature.

A process calls `open()` and either joins the arena on this machine or creates
it — no configuration file, no daemon, no start-order requirement, and **no
possibility of two processes silently ending up on different arenas**.

## Borrow the kernel's locks; do not implement leader election

Linux open file description locks give mutual exclusion, automatic release when
the holder dies, and a way to ask whether anyone holds it, with no timeouts or
heartbeats. So:

* A dead participant's lock is released **by the kernel at the end of its exit**
  (after any core dump and address-space teardown,
  [`docs/decisions/0057`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0057-an-owner-is-not-dead-until-its-files-close.md)).
  Nothing to tune, nothing to reap.
* A `SIGSTOP`ped participant **still holds its lock**, so it is never mistaken
  for a dead one.
* "Is anyone alive?" is a kernel fact; `/proc` parsing survives only as diagnostics.

## The sharing boundary is a directory

Two processes share an arena **if and only if** they resolve to the same runtime
directory, domain and name:

```text
<runtime_dir>/<domain>/<name>.lock     # rendezvous + kernel-managed liveness
<runtime_dir>/<domain>/<name>.sock     # SOCK_SEQPACKET, owner-bound, FD passing
```

Sharing it between containers is a volume mount; not sharing it is complete
isolation; either way it is inspectable with `ls`.

## Dependencies, sandbox, version

`rustix` for raw syscalls; `libc` for exactly one call, `fcntl(F_OFD_SETLK)`,
because `rustix` 1.1 has no OFD locking and classic whole-file locks are dropped
when *any* descriptor to the file closes.

Processes sharing an arena are **mutually trusting, same-user, cooperating
processes**; a read-write participant can corrupt any part of the arena
([`SECURITY.md`](https://github.com/NoeFontana/tf_tree/blob/main/SECURITY.md)).

**`0.0.x` promises nothing**: pin exactly and expect a later release to break
([`CHANGELOG.md`](https://github.com/NoeFontana/tf_tree/blob/main/CHANGELOG.md)).
MSRV is **1.87** ([`SUPPORT.md`](https://github.com/NoeFontana/tf_tree/blob/main/SUPPORT.md)).
The normative spec is
[`docs/PHASE2.md`](https://github.com/NoeFontana/tf_tree/blob/main/docs/PHASE2.md)
§3; the seam is
[`docs/decisions/0005`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0005-the-shared-memory-seam.md).

## Licence

Dual [MIT](LICENSE-MIT) / [Apache-2.0](LICENSE-APACHE), at your option. See
[`NOTICE`](NOTICE).
