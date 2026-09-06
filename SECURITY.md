# Security policy

## Reporting a vulnerability

Email **noe.fontana.pro@gmail.com** with `tf_tree security` in the subject line.
Please do not open a public issue for a suspected vulnerability.

Include, as far as you have them: the affected version or commit, the platform,
a reproduction, and what an attacker gains. A reproduction that runs from a
clean checkout is worth more than a description.

**What to expect.** This is a single-maintainer project. The honest commitment
is an acknowledgement within **7 days** and, if the report is confirmed, a fix or
a public advisory with a workaround within **90 days**. If a report is going to
take longer than that, you will be told so rather than left waiting. If you have
had no acknowledgement after 7 days, assume the mail was lost and send it again.

Coordinated disclosure is preferred and there is no bounty programme. Credit is
given in the advisory unless you ask otherwise.

## What is in scope

The threat model is narrow, and reading it first will save you time.

### Shared memory is not a sandbox — by design, and not a vulnerability

Processes sharing a `tf_tree` arena are **mutually trusting, same-user,
cooperating processes** (`docs/PHASE2.md` §3.10). A read-write participant holds
a writable mapping of the same pages as every other participant, so it can
corrupt any part of the arena. No checksum changes that, and none is claimed.

Reports of the form "a malicious writer with a read-write attachment can corrupt
the arena" are therefore **out of scope**: that is the documented model, not a
defect. Do not attach a process you would not run as yourself.

Three claims *are* in scope, because the design does make them:

- **A read-only participant cannot corrupt anything.** This is enforced by the
  MMU, not by convention. A read-only attachment that can mutate another
  participant's view is a vulnerability.
- **A participant that crashes, at any instruction, cannot corrupt the arena or
  wedge anyone else.** A torn write that survives a crash, or a lock that stays
  held after the holder dies, is a vulnerability.
- **A participant that hangs cannot be mistaken for a crashed one.** Liveness is
  the kernel's answer about a file lock, not a heartbeat, so a `SIGSTOP`ped
  publisher keeps its claims. Reaping a live participant's claims is a
  vulnerability.

### Also in scope

- **Memory unsafety reachable from safe Rust.** Any UB reachable without writing
  `unsafe` yourself, including through the Python bindings. `unsafe` in this
  repository is confined to four boundaries
  (`docs/decisions/0007-the-unsafe-budget-and-the-c-abi.md`); a soundness hole in
  any of them is in scope.
- **Anything that lets an *untrusted input* — a recording, a `.tft` file, a
  header from a peer — cause unsafety.** Attaching to an arena validates the
  header before mapping is trusted; a crafted header that gets past validation
  and into a read is a vulnerability, and unlike a hostile co-participant this is
  a real crossing of a trust boundary.
- **Rendezvous and lock-file handling**: symlink attacks, predictable paths, or
  permissions in the runtime directory that let another user on the same host
  interfere.

### Out of scope

- A malicious or buggy **read-write** participant corrupting shared state (see
  above).
- Denial of service by a co-operating participant — a publisher that floods an
  edge is a robot bug, not an attack.
- Anything requiring the attacker to already be running code as the same user
  with a read-write attachment.
- The `tf_tree_c` C ABI called with arguments that violate its documented
  contract. The C ABI validates what it can (`docs/PHASE4.md` §3); a caller that
  passes a pointer it does not own is outside any guarantee C can offer.

## Supported versions

Pre-1.0. Only the latest release is supported, and there are no backports —
see [`SUPPORT.md`](./SUPPORT.md) for the full policy.

## Network

`tf_tree` opens no outbound network connections. The only socket anywhere in the
library is the Phase 2 `AF_UNIX` rendezvous socket, which is a filesystem path on
the local host.

Stated precisely, because the difference matters to anyone evaluating this:
since 2026-09-04 that is **an assertion in CI**, not only a property of the
code. `docs/PHASE5.md` §5.1 requires a test that runs the suite under `strace`
or a seccomp filter and fails if `socket(2)` is called with anything but
`AF_UNIX`; `just no-network` (`scripts/no-network.sh`) is that test, and
`.github/workflows/ci.yml`'s `shm` job runs it on both matrix rows. **This
section said the test was "not implemented" until the day it was**, which is
the shape of claim worth re-reading against the tree rather than trusting.

Read the scope with it, because it is narrower than "everything in this
repository":

- **What is asserted:** every `socket(2)` issued by the test binaries of the
  five published crates — `tf_tree`, `tf_tree_core`, `tf_tree_math`,
  `tf_tree_arena`, `tf_tree_ipc` — names `AF_UNIX`. That is §11's scoping and
  §5.1's amendment, not a narrowing made to get a green run: the sentence above
  is about the library.
- **`tf_tree_cli` is the check's positive control, not a gap.** `tf_tree top
  --web` binds an `AF_INET` listener by construction, on purpose, when an
  operator asks for it. The recipe traces that target *separately* and
  **refuses** unless it finds `AF_INET` there — so "the library opens no network
  socket" and "this check is scoped so narrowly it could not see one" are
  distinguishable outcomes.
- **What it does not cover:** a code path no test takes, a socket the library
  *inherits* rather than creates (`tf_tree_ipc` passes the rendezvous fd, and
  `connect(2)`/`sendto(2)` on a received fd are not traced), a non-Linux target,
  and **every package in this repository other than the five above and
  `tf_tree_cli`** — including `crates/tf_tree_tf2_sys`, the ROS 2
  `tf2::BufferCore` bridge, which is worth naming rather than deriving because
  ROS 2 middleware is DDS over UDP. Those are traced by nothing. (This
  bullet used to enumerate the untraced crates by name; the list shipped
  incomplete, which in a "what it does not cover" bullet is the worst place for
  an omission, so it is stated as the rule the traced set generates.)
  `scripts/no-network.sh` carries the full PROVES / DOES NOT PROVE header.

Verify it yourself if it matters to you: `just no-network` (it needs `strace`,
and refuses rather than skips without it). Do not use
`cargo nextest run --workspace` for this — the workspace suite includes the
CLI's `--web` listener, so a bare `strace` over it reports `AF_INET` sockets
that are the control rather than a violation.

A build of this project that talks to a remote host is a report worth sending.
