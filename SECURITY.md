# Security policy

## Reporting a vulnerability

Email **noe.fontana.pro@gmail.com** with `tf_tree security` in the subject line.
Please do not open a public issue for a suspected vulnerability. Include the
affected version or commit, the platform, a reproduction (ideally from a clean
checkout), and what an attacker gains.

This is a single-maintainer project: acknowledgement within **7 days** (resend if
you hear nothing) and, if confirmed, a fix or public advisory with a workaround
within **90 days**. Coordinated disclosure is preferred; there is no bounty.
Credit is given unless you ask otherwise.

## What is in scope

### Shared memory is not a sandbox — by design, and not a vulnerability

Processes sharing an arena are **mutually trusting, same-user, cooperating
processes** (`docs/PHASE2.md` §3.10). A read-write participant holds a writable
mapping of the same pages as everyone else and can corrupt any part of the
arena; no checksum changes that. "A malicious read-write writer can corrupt the
arena" is **out of scope**. Do not attach a process you would not run as yourself.

Three claims *are* in scope:

- **A read-only participant cannot corrupt anything** (enforced by the MMU). A
  read-only attachment that can mutate another participant's view is a vulnerability.
- **A participant that crashes, at any instruction, cannot corrupt the arena or
  wedge anyone else.** A torn write that survives a crash, or a lock held after
  its holder dies, is a vulnerability.
- **A participant that hangs is not mistaken for a crashed one.** Liveness is the
  kernel's answer about a file lock, not a heartbeat, so reaping a live
  participant's claims is a vulnerability.

### Also in scope

- **Memory unsafety reachable from safe Rust**, including through the Python
  bindings. `unsafe` is confined to the boundaries of
  `docs/decisions/0007-the-unsafe-budget-and-the-c-abi.md`; a soundness hole in
  any is in scope.
- **Untrusted input causing unsafety** — a recording, a `.tft` file, a header
  from a peer. A crafted header that passes attach validation and reaches a read
  crosses a real trust boundary.
- **Rendezvous and lock-file handling**: symlink attacks, predictable paths, or
  runtime-directory permissions that let another user interfere.

### Out of scope

- A malicious or buggy **read-write** participant corrupting shared state.
- Denial of service by a cooperating participant (a flooding publisher is a robot bug).
- Anything requiring the attacker to already run code as the same user with a
  read-write attachment.
- The `tf_tree_c` C ABI called against its documented contract (`docs/PHASE4.md` §3).

## Supported versions

Only the latest release; no backports ([`SUPPORT.md`](./SUPPORT.md)).

## Network

`tf_tree` opens no outbound network connections; the only library socket is the
`AF_UNIX` rendezvous socket. `docs/PHASE5.md` §5.1 requires this to be a test:
`just no-network` (`scripts/no-network.sh`) runs the five published crates' test
binaries under `strace` and fails on any `socket(2)` that is not `AF_UNIX`, in
`ci.yml`'s `shm` job. Its scope is narrower than the repository — not code paths
no test takes, inherited sockets, non-Linux targets, or other packages;
`scripts/no-network.sh`'s PROVES / DOES NOT PROVE header is the full list. It
needs `strace` and refuses rather than skips without it; do not run
`cargo nextest run --workspace` under `strace` instead, since the CLI's `--web`
listener is the check's positive control. A build that talks to a remote host is
a report worth sending.
