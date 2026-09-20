# Changelog

Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Released
entries live in [`docs/changelog/`](./docs/changelog/).

**`0.0.x` is not ordinary semver.** Cargo treats every `0.0.x` as incompatible
with every other; every release may break the Rust API, Python API, C ABI and
arena format. Pin exactly, including the PyPI wheel. What is implemented is
defined by the status tables in `docs/`; they win over this file.

---

## [Unreleased]

### Breaking

- **`PushError::NonMonotonicStamp` gains `edge: EdgeId`; `ClaimApiError::AlreadyClaimed`
  becomes `{ edge, cause }`; `impl From<ClaimError> for ClaimApiError` is removed.**
  `tft_error.edge` is now filled for the C equivalents. See `API.md` R5.
- **`SampleRing::mask` is a method, not a `pub` field** (`tf_tree_core` only).
- **`tf_tree.ingest/1` becomes `/2`:** `undecodable_channels` is split into
  `filtered_channels` and `non_cdr_channels`; `--max-memory` counts sort scratch.
- **C ABI `0.8`:** adds `tft_bridge_close_startup_window` and
  `TFT_BRIDGE_REASON_STARTUP_CONFLICTS = 9`, reported where a window-close halt
  reported 5. `tft_tree_frame_count`/`tft_tree_edge_count` return `0` on a panic
  ([`0011`](docs/decisions/0011-the-bridge-clock-guard-and-the-static-conflict-disposition.md)).
- **Python: new `TfTreeError` subclasses** (`TimeDomainMismatchError`,
  `NonMonotonicStampError`, `EdgeAlreadyClaimedError`, `ArenaHeldButUnreachableError`,
  `ArenaAbsentError`, `ChildProcessDetachedError`); catch the subclass
  ([`0058`](docs/decisions/0058-the-fields-a-python-exception-only-printed.md)).
- **`PHASE5.md` §5.4's long-lived per-thread `Guard` is withdrawn**; a Phase 6
  region costs another `FORMAT_VERSION`
  ([`0032`](docs/decisions/0032-the-region-table-was-not-part-of-the-purchase.md)).

### Changed

- **Documentation and comments cut by about 85%** over two passes: records, specs, tests and
  code comments keep the decision and the contract; released entries moved to
  `docs/changelog/`; `0002`/`0003` deleted; no line-number citations remain. The C
  headers' doc text is shortened to match; no signature changes.
- **Python exceptions carry attributes and pickle**; `Tree.freeze` in a fork child
  raises instead of `SIGSEGV` ([`0058`](docs/decisions/0058-the-fields-a-python-exception-only-printed.md)).
- **`ShmError`, `FrozenError`, `LayoutError`, `ParticipantError` implement
  `Display` and `Error`**; message text is not a compatibility promise
  ([`0059`](docs/decisions/0059-the-arena-errors-that-cannot-describe-themselves.md)).
- **`Plan::at_many*` read sixteen brackets before interpolating**; rows stay
  bit-identical to `Plan::at` ([`0060`](docs/decisions/0060-the-batch-fold-that-reads-before-it-interpolates.md)).
- **`tf_tree doctor`:** prints the runtime directory (`--json` gains `runtime_dir`);
  `TFT009` reports a stopped publisher; `TFT013` waits out a grace period; `TFT014`
  no longer accuses a live publisher (`PHASE5.md` §6).
- **Attach refusals fit the C ABI's 255-byte buffer** ([`0055`](docs/decisions/0055-the-recovery-capacity-a-fleet-cannot-add-later.md)).
- **A dead owner is seen when its files close, not at `POLLHUP`**; an owner that
  dies mid-handshake is retried ([`0057`](docs/decisions/0057-an-owner-is-not-dead-until-its-files-close.md)).
- **`build_shared` without a lock byte is out of contract**; use `Open::open`
  ([`0031`](docs/decisions/0031-the-participant-record-with-no-byte.md)).
- **Documentation cut by roughly two thirds**; `0002`/`0003` are deleted.

### Added

- **`just top-cpu`** asserts `tf_tree top` at 10 Hz uses under 0.5% of a core (0.30% measured).
- **README gains a "Measured against `tf2`" table**, every figure from `docs/benchmarks/tf2.md`.
- **`just book`** builds the docs as an mdBook (`docs/SUMMARY.md`), sorted into start/operating/reference/explanation/decisions; no file moved.
- **`StaticStore::conflicts_by_edge()`**; loom model `head_publishes_every_stamp_below_it`.
- **Gates:** `just gate2`, `gate5`, `gate4` exit non-zero on FAIL (`scripts/gate-run.sh`:
  `0` PASS, `1` FAIL, `2` REFUSED); `just no-network`, `split-brain-soak`,
  `reclaim-latency`, `sbom`, unsafe-budget census ([`0048`](docs/decisions/0048-a-kind-is-not-a-crate-name.md)).
- **Benchmarks:** `at_many`, `at_many_shapes`, `bracket_mix`, `mlock_probe`.

### Fixed

- **`Tft::ALL` is generated from one table**, so a new check cannot go unrun.
- **Ingest sizes a record body against the file**, not its own header.
- **Two Miri gates no longer pass over work they did not do.**

## Released versions

Frozen history, newest first, in [`docs/changelog/`](./docs/changelog/); leave it
out of searches and sweeps.

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
