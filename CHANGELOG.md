# Changelog

Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Released
entries live in [`docs/changelog/`](./docs/changelog/).

**`0.0.x` is not ordinary semver.** Cargo treats every `0.0.x` as incompatible
with every other, so `tf_tree = "0.0.1"` matches `0.0.1` alone. Nothing is
stable: every release may break every other, in the Rust API, the Python API,
the C ABI and the arena format. Pin exactly.

- PyPI has no such rule: `pip install -U transform_tree` moves `0.0.1` to
  `0.0.2`. Pin the wheel.
- `SUPPORT.md`'s "MSRV bump is a minor bump" rule does not apply on `0.0.x`.

What is implemented is defined by the status tables in `docs/`: `## 0.0
Implementation status` in `PHASE2.md`, `PHASE4.md`, `PHASE5.md`; `## 0.0 Status`
in `PHASE7.md`; `PHASE1.md` §13 and `PHASE3.md` §14. Where they disagree with
this file, they win.

---

## [Unreleased]

### Changed

- **Documentation and comments cut by roughly two thirds.** Decision records are
  condensed to their decision and binding consequences, the specs and
  `CLAUDE.md` to their normative content, code comments to the item's contract;
  history lives in git. `0002`/`0003` are deleted (see `docs/PROJECT.md`,
  `docs/PHASE1.md`). `docs/changelog/` holds released entries.

### Breaking

- **`PushError::NonMonotonicStamp` gains `edge: EdgeId`; `ClaimApiError::AlreadyClaimed`
  becomes `{ edge, cause }`; `impl From<ClaimError> for ClaimApiError` is
  removed.** Patterns need `..` or the new fields. Both `Display`s begin `edge N:`.
  C: `tft_error.edge` is now filled for `TFT_ERR_ALREADY_CLAIMED`,
  `TFT_ERR_NON_MONOTONIC` and path-internal `TFT_ERR_TIME_DOMAIN`. See `API.md` R5.
- **`SampleRing::mask` is a method, not a `pub` field** (`tf_tree_core` only).
  Read `ring.mask()`.
- **`tf_tree.ingest/1` becomes `/2`:** `undecodable_channels` is split into
  `filtered_channels` and `non_cdr_channels`. `--max-memory` now counts the
  sort's scratch.
- **C ABI `0.8`:** adds `tft_bridge_close_startup_window` and
  `TFT_BRIDGE_REASON_STARTUP_CONFLICTS = 9`. A window-close halt that reported
  `TFT_BRIDGE_REASON_AUTHORITY_CONFLICT` (5) now reports 9; the action is
  unchanged. Count entry points `tft_tree_frame_count`/`tft_tree_edge_count`
  return `0` on a panic instead of aborting ([`0011`](docs/decisions/0011-the-bridge-clock-guard-and-the-static-conflict-disposition.md)).
- **Python: new exception classes, all subclassing `TfTreeError`:**
  `TimeDomainMismatchError`, `NonMonotonicStampError`, `EdgeAlreadyClaimedError`,
  `ArenaHeldButUnreachableError`, `ArenaAbsentError`, `ChildProcessDetachedError`.
  A `type(e) is TfTreeError` check around `plan`, `lookup`, `push`, `publisher`
  or `open` no longer matches; catch the subclass. Instances you construct carry
  no attributes ([`0058`](docs/decisions/0058-the-fields-a-python-exception-only-printed.md)).
- **`PHASE5.md` §5.4's long-lived per-thread `Guard` is withdrawn**, and its
  region-table clause is retracted: a Phase 6 region costs another
  `FORMAT_VERSION` ([`0032`](docs/decisions/0032-the-region-table-was-not-part-of-the-purchase.md)).

### Changed

- **Python exceptions carry attributes** (`.edge`, `.requested`, `.oldest`,
  `.newest`, `.domain`, `.target`, `.source`, `.cut_at`, `.plan_generation`,
  `.owner_slot`, `.holder_slots` and others) and pickle across `multiprocessing`;
  `Tree.freeze` in a fork child raises instead of `SIGSEGV`
  ([`0058`](docs/decisions/0058-the-fields-a-python-exception-only-printed.md), `PHASE3.md` §4.4, §8.1).
- **`ShmError`, `FrozenError`, `LayoutError`, `ParticipantError` implement
  `Display` and `Error`**; payloads print as prose, not `Debug`; the payload
  types are nameable from `tf_tree`. Message text is not a compatibility promise
  ([`0059`](docs/decisions/0059-the-arena-errors-that-cannot-describe-themselves.md), `API.md` R5).
- **`Plan::at_many*` read a chunk of sixteen brackets before interpolating**;
  every row stays bit-identical to `Plan::at`
  ([`0060`](docs/decisions/0060-the-batch-fold-that-reads-before-it-interpolates.md)).
- **`tf_tree doctor`:** prints the resolved runtime directory (`--json` gains
  `runtime_dir`); `TFT009` reports a stopped publisher and `not run` when it
  judged nothing; `TFT013` waits out a grace period; `TFT014` no longer accuses a
  live publisher; `--json` is schema-validated (`PHASE5.md` §6).
- **Attach refusals fit the C ABI's 255-byte message buffer**, and the
  `ArenaHeldButUnreachable` text names the create retry
  ([`0055`](docs/decisions/0055-the-recovery-capacity-a-fleet-cannot-add-later.md)).
- **A dead owner is seen when its files close, not at `POLLHUP`;** `owner_lost`'s
  docs now say so (`PHASE2.md` §3.5, [`0057`](docs/decisions/0057-an-owner-is-not-dead-until-its-files-close.md)).
  An owner that dies mid-handshake is retried, not failed.
- **`build_shared` without a lock byte is out of contract**; serve a created
  arena through `Open::open` ([`0031`](docs/decisions/0031-the-participant-record-with-no-byte.md), `PHASE2.md` §3.1).
- **Comments and rustdoc state the contract, not history**; a deletion and
  one-home-per-claim pass over the docs and every crate.

### Added

- **`StaticStore::conflicts_by_edge()`**; loom model `head_publishes_every_stamp_below_it`.
- **Gates:** `just gate2`, `gate5`, `gate4` exit non-zero on FAIL; gate outcomes
  are `0` PASS, `1` FAIL, `2` REFUSED (`scripts/gate-run.sh`); `just no-network`,
  `split-brain-soak`, `reclaim-latency`, `sbom`, and the unsafe-budget census
  ([`0048`](docs/decisions/0048-a-kind-is-not-a-crate-name.md)).
- **Benchmarks:** `at_many` and `at_many_shapes` groups, `bracket_mix`,
  `mlock_probe` (`docs/benchmarks/EVIDENCE.md`).

### Fixed

- **`Tft::ALL` is generated from one table**, so a new check cannot go unrun.
- **Ingest sizes a record body against the file**, not its own header.
- **Two Miri gates, `shm_torture` and the nightly wiring** no longer pass over
  work they did not do; `artifact-versions` reads tracked lockfiles and every
  package-index front page.

## Released versions

Each release's entry is frozen history in its own file under
[`docs/changelog/`](./docs/changelog/) — newest first. **That directory is a
record, not a working document: leave it out of searches and sweeps.** The
release commit dates the `## [X.Y.Z]` section here, and the next release
commit moves it there.

- [`0.0.5`](./docs/changelog/0.0.5.md) — 2026-08-29 (the entry paths that need no toolchain)
- [`0.0.4`](./docs/changelog/0.0.4.md) — 2026-08-22 (the slot a killed participant keeps)
- [`0.0.3`](./docs/changelog/0.0.3.md) — 2026-08-19 (first with a source distribution)
- [`0.0.2`](./docs/changelog/0.0.2.md) — 2026-08-17 (wheels, no sdist)
- [`0.0.1`](./docs/changelog/0.0.1.md) — 2026-08-17 (crates.io only)

[0.0.1]: https://github.com/NoeFontana/tf_tree/releases/tag/v0.0.1
[0.0.2]: https://github.com/NoeFontana/tf_tree/releases/tag/v0.0.2
[0.0.3]: https://github.com/NoeFontana/tf_tree/releases/tag/v0.0.3
[0.0.4]: https://github.com/NoeFontana/tf_tree/releases/tag/v0.0.4
[0.0.5]: https://github.com/NoeFontana/tf_tree/releases/tag/v0.0.5
