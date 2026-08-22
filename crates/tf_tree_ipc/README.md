# tf_tree_ipc

Zero-configuration rendezvous for [`tf_tree`](https://crates.io/crates/tf_tree)
shared arenas: runtime-directory discovery, the OFD lock file, `SOCK_SEQPACKET`
descriptor passing, and the `open()` decision machine.

**Linux only.** The crate is `#![cfg(target_os = "linux")]`, so on any other
target it compiles to an empty library rather than failing to build. You
normally reach it through `tf_tree`'s default-off `shm` feature and never name
it yourself.

## What it does

A process calls `open()` and either joins the arena that already exists on this
machine or creates it. No configuration file, no daemon, no start-order
requirement, and **no possibility of two processes silently ending up on
different arenas**.

## The design principle: do not implement leader election, borrow the kernel's

A rendezvous needs exactly three properties — mutual exclusion, automatic
release when the holder dies, and a way to ask whether anyone holds it. Linux
open file description locks provide all three, maintained by the kernel, with no
timeouts, no heartbeats, and no state that can survive a `SIGKILL`. Three things
follow that a heartbeat protocol cannot buy at any price:

* A `SIGKILL`ed participant's lock is released **by the kernel, immediately**.
  There is no timeout to tune and nothing left to reap.
* A `SIGSTOP`ped participant **still holds its lock**, so it can never be
  mistaken for a dead one. A liveness heuristic that is wrong once in a thousand
  hours is exactly the kind of bug that ships.
* "Is anyone alive?" is a kernel fact, not an inference. `/proc` parsing and
  PID-reuse defence leave the correctness path entirely and survive only as
  diagnostics.

## The sharing boundary is a directory

Two processes share an arena **if and only if** they resolve to the same runtime
directory, domain and name:

```text
<runtime_dir>/<domain>/<name>.lock     # rendezvous + kernel-managed liveness
<runtime_dir>/<domain>/<name>.sock     # SOCK_SEQPACKET, owner-bound, FD passing
```

Sharing that directory between containers is a volume mount; not sharing it is
complete isolation. Either way the boundary is inspectable with `ls`, which is
why it is a directory and not an abstract socket namespace.

## Two dependencies, and why the second one is there

`rustix` for the syscalls: raw calls, no C build step. `libc` for exactly one
thing, `fcntl(F_OFD_SETLK)` — `rustix` 1.1 has no OFD locking, and the classic
whole-file locks it does offer are rejected by name in the spec, because they
are dropped when *any* descriptor to the file closes anywhere in the process.

An earlier version issued that syscall by hand and was restricted to x86-64 and
aarch64 by a `compile_error!`, because `struct flock`'s layout and the syscall
numbering are not the same everywhere. That was the wrong trade for the
primitive the whole rendezvous rests on.

## This is not a sandbox

Processes sharing an arena are **mutually trusting, same-user, cooperating
processes**. A read-write participant holds a writable mapping of the same pages
and can corrupt any part of the arena; no checksum would change that. Do not
attach a process you would not run as yourself.
[`SECURITY.md`](https://github.com/NoeFontana/tf_tree/blob/main/SECURITY.md)
draws the line between this and an actual vulnerability.

## Version

**`0.0.x` promises nothing.** Cargo treats every `0.0.x` release as
incompatible with every other, which is the intended signal: pin exactly, and
expect a later release to break. The number is deliberately not repeated here —
this line read `0.0.1` for three releases, because nothing gates a version in
prose. The reasoning is written out in the
repository's [`Cargo.toml`](https://github.com/NoeFontana/tf_tree/blob/main/Cargo.toml)
under `[workspace.package] version`, and the release notes are in
[`CHANGELOG.md`](https://github.com/NoeFontana/tf_tree/blob/main/CHANGELOG.md).

MSRV is **1.87**; see
[`SUPPORT.md`](https://github.com/NoeFontana/tf_tree/blob/main/SUPPORT.md).

## Where the rest of it is

[`docs/PHASE2.md`](https://github.com/NoeFontana/tf_tree/blob/main/docs/PHASE2.md)
§3 is the normative spec for everything above — the runtime directory including
its NFS/CIFS refusal, the lock-file record layout, the `open()` decision
algorithm and its split-brain check, and the handshake.
[`docs/decisions/0005`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0005-the-shared-memory-seam.md)
is why the seam is where it is.

## Licence

Dual [MIT](LICENSE-MIT) / [Apache-2.0](LICENSE-APACHE), at your option. See
[`NOTICE`](NOTICE).
