# 0062: The host checks `doctor` does not make

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** none

## Context

A proposal asked `tf_tree doctor` to check `/dev/shm` mount flags, OFD-lock
kernel support and AVX2/FMA, and to sweep orphaned lock files with `--clean-stale`.

## Decision

Make none of the four checks, and add no deleting flag.

- **`/dev/shm` `noexec`/`nosuid`:** the arena is a `memfd` passed over the attach
  socket (`tf_tree_ipc::open`), never a file under `/dev/shm`; the mount flags do
  not touch it. `TFT016` already reports the host facts that do (THP, `RLIMIT_MEMLOCK`).
- **OFD locks:** liveness already probes `F_OFD_GETLK` per slot; a kernel without
  it fails that probe with an error, which `TFT014` reports. Linux 3.15 has them.
- **AVX2/FMA:** the engine is portable `f64` (D6) and the SIMD record `0016` is
  withdrawn; no code path depends on an instruction set.
- **`--clean-stale`:** a leftover socket path or lock file is expected and
  harmless, since `open()` lets the ownership byte decide (`IpcError::ServerUnreachable`).
  A sweep deletes files under a user's runtime directory to fix nothing observed,
  and a race with a starting owner is the failure it would add.

## Rationale

Each check answers a question the code never asks. Add one only with a failure it
would have caught, reproduced first.

## Consequences

If a stale file is ever shown to break an open, the response is a read-only
`doctor` line naming it, then a record for a sweep.

## Open questions

- Has any real stale runtime-dir file broken an `open()`? None found so far.
