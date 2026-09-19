# Security policy

## Reporting a vulnerability

Email **noe.fontana.pro@gmail.com** with `tf_tree security` in the subject; no
public issue. Include the version or commit, platform, a reproduction, and what
an attacker gains.

Acknowledgement within **7 days**; if confirmed, a fix or advisory within **90
days**. Coordinated disclosure; no bounty.

## What is in scope

### Shared memory is not a sandbox

Processes sharing an arena are **mutually trusting, same-user, cooperating
processes** (`docs/PHASE2.md` §3.10). A read-write participant can corrupt any
part of the arena; that is **out of scope**.

Three claims *are* in scope:

- **A read-only participant cannot corrupt anything** (enforced by the MMU).
- **A participant that crashes, at any instruction, cannot corrupt the arena or
  wedge anyone else.**
- **A participant that hangs is not mistaken for a crashed one** (liveness is the
  kernel's file-lock answer); reaping a live participant's claims is a vulnerability.

### Also in scope

- **Memory unsafety reachable from safe Rust**, including through the Python
  bindings (`docs/decisions/0007-the-unsafe-budget-and-the-c-abi.md`).
- **Untrusted input causing unsafety**: a recording, a `.tft` file, a peer's header.
- **Rendezvous and lock-file handling**: symlink attacks, predictable paths,
  runtime-directory permissions.

### Out of scope

- A malicious or buggy **read-write** participant corrupting shared state.
- Anything requiring the attacker to already run code as the same user with a
  read-write attachment.
- The `tf_tree_c` C ABI called against its documented contract (`docs/PHASE4.md` §3).

## Network

`tf_tree` opens no outbound network connections; the only library socket is the
`AF_UNIX` rendezvous socket. `just no-network` (`docs/PHASE5.md` §5.1) fails on
any other `socket(2)` under `strace`. Only the latest release is supported
([`SUPPORT.md`](./SUPPORT.md)).
