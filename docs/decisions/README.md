# Architectural decisions

A significant architectural change starts as a decision record in this folder, not as a PR. The PR(s) implementing it link back here. Skeleton: [`template.md`](./template.md).

Cite [`docs/PROJECT.md`](../PROJECT.md) (D1-D22 in §5) and [`docs/PHASE1.md`](../PHASE1.md) for the architecture and the Phase 1 contract. [`docs/API.md`](../API.md) is the API contract a public-surface decision is checked against; [`docs/PHASE2.md`](../PHASE2.md) §1 holds amendments A1-A8.

A `path.rs:NNN` citation in a frozen record is unreliable: read the symbol the surrounding prose names. `CLAUDE.md`: cite a symbol, never a line number.

Records are indexed by number below. The table does not restate status; read it from the record:

```sh
grep -m1 -H '^\*\*Status:' docs/decisions/0*.md
```

| Record | Decided |
|---|---|
| [`0001`](./0001-record-architectural-decisions.md) | the meta-decision that this folder exists. |
| [`0004`](./0004-builder-time-edge-declaration.md) | builder-time edge declaration; arena sized from declared edges. Not consolidated into a spec; this record is where it lives. |
| [`0005`](./0005-the-shared-memory-seam.md) | the `tf_tree` to `tf_tree_ipc` seam: fd passing, claims as leases, reaping, fork poisoning. Amends D16; `0017` amends its `forbid(unsafe_code)` commitment. |
| [`0006`](./0006-the-eight-phase-roadmap.md) | the eight-phase roadmap, D21/D22, and the alias table for decision numbers `PHASE4.md`/`PHASE5.md` cite. Phase 6 amended by `0009`. |
| [`0007`](./0007-the-unsafe-budget-and-the-c-abi.md) | the unsafe budget as a criterion, not a crate list; places the C ABI in `tf_tree_c`. |
| [`0008`](./0008-the-name-tf-tree.md) | crate name `tf_tree` kept; PyPI distribution is `transform_tree`, import name `tf_tree`. |
| [`0009`](./0009-descoping-phase-6.md) | Phase 6 cut to one item: covariance and copy-on-write branches cut, URDF leaves the engine, B-splines stay. |
| [`0010`](./0010-naming-the-record-size-refusal.md) | `IngestError::RecordTooLarge` for records past the 256 MiB ingest ceiling. |
| [`0011`](./0011-the-bridge-clock-guard-and-the-static-conflict-disposition.md) | bridge static-conflict disposition and `Strict` startup window; its clock half is superseded by `0012`. |
| [`0012`](./0012-the-authoritative-clock-jump-signal-and-the-degradation-ladder.md) | authoritative `rcl` clock-jump callbacks first, inference demoted to common-mode rejection; supersedes `0011`'s clock half. |
| [`0013`](./0013-the-benchmark-gate-never-interpolated.md) | benchmark gate queries off-grid stamps so interpolation runs; depth-3 figure is 192.7 ns, not 150 ns. |
| [`0014`](./0014-the-push-heartbeat-is-a-store.md) | the push heartbeat is a plain store, not a locked RMW; `push` 8.66 ns to 4.65 ns. |
| [`0015`](./0015-the-bridge-fills-a-shared-arena.md) | the bridge owns and fills a shared arena when ROS is the source of truth; scoped by `0019`. |
| [`0016`](./0016-portable-simd-and-the-dependency-budget.md) | portable SIMD and the dependency budget; `target-cpu=x86-64-v3` measured 8-14% slower on `at_many` and rejected. |
| [`0017`](./0017-owned-handles-and-the-lifetime-rule.md) | `Tree::claim_owned` and `OwnedWriter`; no type a user stores carries a lifetime (`API.md` §2.1). |
| [`0018`](./0018-blocking-waits-belong-in-the-shim.md) | no blocking primitive in the arena; the `tf2`-shaped timeout is a predicted sleep in the caller (D18). |
| [`0019`](./0019-one-binary-and-topology-you-can-wait-for.md) | no `tf_treed` binary; `tf_tree serve` subcommand; read-only attach implies `CreatePolicy::Never`. Supersedes `PHASE2.md` §9. |
| [`0020`](./0020-the-consumer-side-of-the-arena-refusal.md) | `tft_tree_open` reports `TFT_ERR_ARENA_UNAVAILABLE` for a publisher that has not started, not `TFT_ERR_INTERNAL`. |
| [`0021`](./0021-the-idle-arena-is-resident-because-of-its-alignment.md) | idle arena is ~100% resident because `HeapArena`'s 64-byte alignment routes to `calloc` only at alignment 16 or less. |
| [`0022`](./0022-the-per-call-guard-and-the-unwatched-gate.md) | build nothing: the C ABI's per-call `Guard` cost is accepted. |
| [`0023`](./0023-the-gate-that-could-not-gate.md) | `PHASE4.md` §7 criterion 1 re-cut into three pinned rungs plus a control at `[profile.embedder]`. |
| [`0024`](./0024-population-is-per-edge-at-take-up.md) | page population is per-edge at take-up, not per-arena; `PHASE2.md` §7.1 corrected. |
| [`0025`](./0025-what-build-the-tf2-ratio-gate-speaks-for.md) | what build the tf2 ratio gate speaks for: `ratio.rs`'s floor holds for this workspace's build, not a default consumer's. |
| [`0026`](./0026-the-corpus-shape-of-a-frozen-index.md) | a frozen `.tft` is one file per episode; corpus identity lives in an external index. |
| [`0027`](./0027-the-48-byte-frame-name-store.md) | `FrameRecord::name` is 48 bytes and over-length names are refused, not truncated. |
| [`0028`](./0028-the-slot-a-killed-participant-keeps.md) | the slot a killed participant keeps: reclaim by kernel-lock liveness, participant reaping. Implemented and frozen. |
| [`0029`](./0029-the-topology-lock-is-a-kernel-lock.md) | the topology lock is a kernel lock; `Tree::reparent` decides holder liveness by `F_OFD_GETLK`, one predicate per tree. |
| [`0030`](./0030-the-atfork-handler-and-inherited-descriptors.md) | the atfork handler closes inherited descriptors so a forked child cannot hold the owner's lock byte or socket (`0028` step 7). |
| [`0031`](./0031-the-participant-record-with-no-byte.md) | `build_shared` participants with no lock byte: the boundary goes where the call is. |
| [`0032`](./0032-the-region-table-was-not-part-of-the-purchase.md) | the region table was not part of the Phase 5 header purchase; the region break is still owed (`PROJECT.md` §5.1). |
| [`0033`](./0033-the-identity-record-cannot-name-a-namespace.md) | the identity record cannot name a namespace; `tft014_namespace_*` arms guard cross-namespace attach. |
| [`0034`](./0034-the-depth-bound-priced-two-slots-the-same.md) | `MAX_DEPTH` priced the compiled plan but was enforced on the raw walk. |
| [`0035`](./0035-the-creators-slot-is-taken-not-found.md) | the creator's participant slot is taken atomically with the scan, not found in a second pass. |
| [`0036`](./0036-the-receipt-time-the-format-already-reserved.md) | receipt time in the reserved format field; `TFT004` clock-skew detection. |
| [`0037`](./0037-a-takeover-is-not-a-second-open.md) | an ownership takeover is a method on an attached session, not a second `open()`; §3.5's shape. |
| [`0038`](./0038-the-domain-a-binding-cannot-name.md) | the time domain is a plan-handle tag, so C, C++ and Python can read arenas not on tag 0. |
| [`0039`](./0039-extrapolation-you-cannot-fail-to-notice.md) | `ExtrapPolicy::{Hold, ConstantTwist}` reachable through `at_extrapolating`, which reports the extrapolation distance. |
| [`0040`](./0040-the-error-that-cannot-be-returned.md) | hand-written `Display` and `core::error::Error` on the five `tf_tree_core` error enums; errors stay `Copy`. |
| [`0041`](./0041-python-declares-a-topology-the-way-everything-else-does.md) | Python `build`/`open` accept topology-config text wherever a pair list was accepted; no Python `Builder`. |
| [`0042`](./0042-the-cacheline-the-arena-never-asked-for.md) | `Iso3` drops `align(64)`; `Plan` 4160 to 2064 bytes. |
| [`0043`](./0043-owner-lost-is-a-question-about-the-owner.md) | `owner_lost` asks whether the arena has an owner (hangup then `F_OFD_GETLK` on byte 0), not whether this socket is dead. |
| [`0044`](./0044-recovery-the-languages-a-robot-is-written-in-cannot-reach.md) | `inherit_ownership` takes `&self`; recovery reaches C, C++ and Python. |
| [`0045`](./0045-the-spin-that-can-starve-what-it-waits-for.md) | `FrameTable::wait_for_publish`'s final spin can starve a single-core publisher. |
| [`0046`](./0046-the-consumer-the-crate-boundary-was-drawn-for.md) | `tf_tree_ingest` stays a library crate; `ingest_bag` returns the ordinary `Tree`. |
| [`0047`](./0047-the-recording-this-reader-would-refuse.md) | the recording crate `PHASE2.md` §10 promised does not exist; the NORMATIVE bit-identity test does. |
| [`0048`](./0048-a-kind-is-not-a-crate-name.md) | an unsafe-budget kind is a property, not a crate name; there is no fifth kind. |
| [`0049`](./0049-the-flag-that-prefaults-the-arena.md) | `LockPolicy` and `mlock2` stay declined; `PHASE2.md` §7.4's body is stale. |
| [`0050`](./0050-what-ten-times-real-time-divides.md) | what ten-times-real-time divides: `Survey::span_ns()`, with a corpus density floor. |
| [`0051`](./0051-the-licence-travels-with-the-artifact-not-the-file.md) | no per-file licence headers; the licence travels with the crate and wheel. |
| [`0052`](./0052-the-first-five-minutes-nobody-runs.md) | the documented first-five-minutes path is separated from `just quickstart`, which builds from source. |
| [`0053`](./0053-the-branchless-bracket-that-branches.md) | `SampleRing::bracket`'s mask-select compiles to a branch, not `cmov`. |
| [`0054`](./0054-the-conflicts-the-bridge-counts-and-the-arena-cannot-see.md) | `TFT002`/`TFT003` detect nothing in any `doctor` configuration; the bridge's counts reach the catalogue. |
| [`0055`](./0055-the-recovery-capacity-a-fleet-cannot-add-later.md) | recovery capacity a fleet cannot add later: an ownerless arena admits no new rendezvous attachment. |
| [`0056`](./0056-the-participant-numerator-is-the-lock-files.md) | `TFT015`'s `participants` row counts the lock file's bytes, not an arena-side counter. |
| [`0057`](./0057-an-owner-is-not-dead-until-its-files-close.md) | an owner is not dead until its files close; `owner_lost` is not microsecond-fast. |
| [`0058`](./0058-the-fields-a-python-exception-only-printed.md) | Python exceptions carry structured attributes on the instance's `__dict__`. |
| [`0059`](./0059-the-arena-errors-that-cannot-describe-themselves.md) | `Display` and `core::error::Error` for `ShmError`, `FrozenError`, `LayoutError` and `ParticipantError`. |
| [`0060`](./0060-the-batch-fold-that-reads-before-it-interpolates.md) | a two-phase chunked `fold_batch` (read every bracket, then interpolate) as the SoA batch lever `0016` called unreachable. |
| [`0061`](./0061-a-freeze-protects-the-argument-not-the-pointers.md) | a freeze protects the argument, not `path.rs:NNN` pointers. |

## Lifecycle

Four statuses on one document type; no folder moves.

- **draft**: open questions present. **A draft authorises nothing**: no spec §0.0 row, amendment banner, code comment or status table may cite one as settled (`scripts/artifact-versions.py` checks this).
- **ready**: every open question the *Decision* depends on is resolved (one explicitly scoped as not decision-affecting may stay open) and the implementation plan is concrete.
- **implemented**: code shipped, PRs linked, document frozen.
- **superseded by NNNN**: replaced; the document stays in place.

### Gates

- **draft to ready** is the architectural review: questions resolved, alternatives named in *Rationale*, plan detailed enough that the implementer invents nothing.
- **implemented** is the immutability lock. A record is not edited to match the code; a new record supersedes it. The only edits are the status line and an amendment banner signed and dated by a later record.

A `ready` record is the contract: the *Decision* is implemented as stated, each plan step lands as one PR in order with its listed verification, and an open question found mid-implementation means stop and ask.

Numbers are sequential four-digit and never renumbered. Filenames are `NNNN-kebab-case-noun-phrase.md`.

## Opt-in extensions

Documented, unimplemented; adopt only when needed: a CLI crate, fuzzing, benchmarks, a reference-impl oracle, real-model integration tests, Diataxis user docs, design docs, a `slow.yml` release workflow.
