# 0049: the flag that prefaults the arena

**Status:** implemented (2026-09-09; same wave as `0047`, same omission)
**Owner:** @NoeFontana
**Implementation:** #303 — all eight steps: `crates/tf_tree_bench/examples/mlock_probe.rs`, `docs/benchmarks/EVIDENCE.md`'s probe row, `TFT016`'s corrected finding message in `crates/tf_tree_cli/src/checks.rs` and its pin test, `crates/tf_tree_cli/src/hostfacts.rs`, `docs/API.md` §8.3, `docs/PHASE2.md` §7.4's banner and §0.0's row, `docs/PHASE5.md` §6's detection row. Open question 1 is explicitly **not decision-affecting**: the decline rests on `API.md` §8.3's second bullet, which the question does not touch.

## Context

`docs/PHASE2.md` §7.4 specified `LockPolicy::{ None, Populate (default), Locked }`, with `Locked` calling `mlock2(MLOCK_ONFAULT)`. It was never implemented; `docs/API.md` §8.3 and `PHASE2.md` §0.0 declined it, with no decision record and a probe-less §8.3.

## Decision

**No `LockPolicy`. No `mlock` call of any kind in this library. §7.4 stays declined.**

### The decline rests on §8.3's second bullet, which survives everything

A library that locks memory spends an `RLIMIT_MEMLOCK` budget it cannot see, and the arena is deliberately over-provisioned (§3.8), so the embedding application decides. The decision is about who decides, not what the flag does.

### §8.3's third bullet is corrected, clause by clause, at three different strengths

Measured with `crates/tf_tree_bench/examples/mlock_probe.rs`:

| clause | verdict |
|---|---|
| *"`MLOCK_ONFAULT` would not prefault"* | **TRUE** |
| *"so it adds nothing over §7.1"* | **FALSE as a mechanism claim** |
| *"on a swapless host the pages are not reclaimable anyway"* | **UNDETERMINED** |

- `mlock2(MLOCK_ONFAULT)` on an untouched shmem mapping leaves `Rss=0`.
- §7.1 populates once; `MLOCK_ONFAULT` retains: `MADV_PAGEOUT` returns `EINVAL` while locked and reclaims after `munlock`.
- Organic reclaim of a shmem arena under memcg pressure was not observed (file-backed positive control was); global `kswapd` pressure is untested. The clause is a reclaim-policy conclusion stated as a mechanism fact.

### The recommended incantation is wrong

`mlockall(MCL_CURRENT | MCL_FUTURE)` **prefaults** (`mlock_probe mlockall`), which is per-arena population at address-space granularity and undoes [`0024`](./0024-population-is-per-edge-at-take-up.md). The correct spelling is `mlockall(MCL_CURRENT | MCL_FUTURE | MCL_ONFAULT)` (`mlock_probe mlockall-onfault`).

### `TFT016`'s message stops predicting `mlockall`'s outcome

`mlockall` charges the whole address space, not the arena, so the arena-versus-`RLIMIT_MEMLOCK` comparison does not predict it. The comparison is kept (the limit below the arena is a real finding) and its firing condition is unchanged; the message no longer predicts a call whose other term it cannot see and says its silence is not a clearance. `tf_tree_cli` is `#![forbid(unsafe_code)]`, so it grows no address-space term.

## Rationale

- The decline survives the `retention` finding: it measures what the flag does, and §8.3's second bullet is about who may spend the budget. Implementing it would also put an OS-boundary `unsafe` call in the engine (`0007`).
- The probe is a cargo example so `scripts/evidence-audit.sh` requires a backticked row in `docs/benchmarks/EVIDENCE.md`; it is registered as a probe, not run by a recipe, because every arm is a property of the kernel and host. It carries its own `unsafe` posture, `0007` kind 2 (the OS), per [`0048`](./0048-a-kind-is-not-a-crate-name.md).
- The warm/cold ratio is not published; the per-fault figure (~8.1 µs) is.

## Consequences

- `docs/API.md` §8.3's recommendation changes everywhere it is spelled, including the shipped operator string. `checks::tests::every_tft016_arm_fires_and_the_two_corrected_strings_are_pinned` drives every arm from synthetic `HostFacts` and compares the first `mlockall(...)` under `API.md` §8.3 (whitespace and backticks normalised) with the message's. Other sites are guarded by review; `rg -l mlockall` is the list.
- `docs/PHASE2.md` §7.4 carries an amendment banner; §0.0's row cites `fn tft016`'s `match host.memlock` arm by name.
- `docs/PHASE5.md` §6's `TFT016` row reads `/proc/self/limits`, not `getrlimit`.
- The probe is kind 2 in `0048`'s index.
- A future `LockPolicy` needs a fallback below `mlock2`'s Linux 4.4 / glibc 2.27 floor.

## Implementation plan

1. `crates/tf_tree_bench/examples/mlock_probe.rs`, each `mlockall` arm in its own process.
2. `docs/benchmarks/EVIDENCE.md` probe row, gated by `bash scripts/evidence-audit.sh`.
3. `crates/tf_tree_cli/src/checks.rs`: `TFT016`'s message.
4. `crates/tf_tree_cli/src/hostfacts.rs`: module doc.
5. `docs/API.md` §8.3: the flag on bullet 2, bullet 3 rewritten to the three verdicts.
6. `docs/PHASE2.md`: §7.4 banner, §0.0 row.
7. `docs/PHASE5.md`: §6's `TFT016` detection row.
8. `CHANGELOG.md` entry; the index row is added centrally, as in `0047` step 8.

## Open questions

1. Whether global `kswapd` pressure would tear down a shmem arena's PTEs on a swapless host. Until answered, no document may state the swapless clause either way.
