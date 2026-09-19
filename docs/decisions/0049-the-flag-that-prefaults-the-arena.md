# 0049: the flag that prefaults the arena

**Status:** implemented (2026-09-09; same wave as `0047`, same omission)
**Owner:** @NoeFontana
**Implementation:** #303 — all eight steps: `crates/tf_tree_bench/examples/mlock_probe.rs`, `docs/benchmarks/EVIDENCE.md`'s probe row, `TFT016`'s corrected finding message in `crates/tf_tree_cli/src/checks.rs` and its pin test, `crates/tf_tree_cli/src/hostfacts.rs`, `docs/API.md` §8.3, `docs/PHASE2.md` §7.4's banner and §0.0's row, `docs/PHASE5.md` §6's detection row. Open question 1 is explicitly **not decision-affecting**: the decline rests on `API.md` §8.3's second bullet, which the question does not touch.

## Decision

**No `LockPolicy`. No `mlock` call of any kind in this library. `docs/PHASE2.md` §7.4 stays declined**, because a library that locks memory spends an `RLIMIT_MEMLOCK` budget it cannot see (`docs/API.md` §8.3's second bullet).

§8.3's third bullet is corrected, as measured by `mlock_probe`: "`MLOCK_ONFAULT` would not prefault" is TRUE; "so it adds nothing over §7.1" is FALSE (it retains what §7.1 populates); "on a swapless host the pages are not reclaimable anyway" is UNDETERMINED (question 1). The recommended `mlockall(MCL_CURRENT | MCL_FUTURE)` **prefaults**, undoing [`0024`](./0024-population-is-per-edge-at-take-up.md); the correct spelling adds `MCL_ONFAULT`.

`TFT016`'s message stops predicting `mlockall`'s outcome, which charges the whole address space, not the arena; the comparison and its firing condition are kept.

## Consequences

- `checks::tests::every_tft016_arm_fires_and_the_two_corrected_strings_are_pinned` compares the first `mlockall(...)` under `API.md` §8.3 with the message's; `rg -l mlockall` lists the other sites.
- The probe is `0007` kind 2 (the OS), per [`0048`](./0048-a-kind-is-not-a-crate-name.md); `scripts/evidence-audit.sh` requires its `EVIDENCE.md` row.
- A future `LockPolicy` needs a fallback below `mlock2`'s Linux 4.4 / glibc 2.27 floor.

## Implementation plan

Steps 1-8 landed: the probe and its `EVIDENCE.md` row; `TFT016`'s message and `hostfacts.rs`; `API.md` §8.3, `PHASE2.md` §7.4 and §0.0, `PHASE5.md` §6; `CHANGELOG.md`.

## Open questions

1. Whether global `kswapd` pressure would tear down a shmem arena's PTEs on a swapless host. Until answered, no document may state the swapless clause either way.
