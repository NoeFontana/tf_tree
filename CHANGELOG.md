# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

**Versioning, stated before anything else, because `0.0.x` is not ordinary
semver.** Cargo treats every `0.0.x` release as incompatible with every other:
`^0.0.1` — which is what a bare `tf_tree = "0.0.1"` means — matches `0.0.1` and
nothing else. That is deliberate and it is the whole promise. **Nothing in this
release is stable. Every release may break every other**, in the public Rust
API, in the Python API, in the C ABI, and in the arena format. Pin exactly.

Two consequences worth naming rather than leaving to be discovered:

- **PyPI does not have that rule.** PEP 440 gives `0.0.x` no special meaning, so
  `pip install -U transform_tree` will move you from `0.0.1` to `0.0.2` without asking.
  Pin the wheel yourself if the promise above matters to you.
- **`SUPPORT.md`'s "an MSRV bump is a minor-version bump pre-1.0" rule does not
  apply on the `0.0.x` line** — there is no minor slot for it to occupy. The
  argument, and why the resolver already enforces what that rule was written to
  enforce, is in the root `Cargo.toml`'s comment on `[workspace.package]
  version` and in `SUPPORT.md`'s MSRV section.

The single source of truth for what is implemented is the status tables in
`docs/` — `## 0.0 Implementation status` at the head of `PHASE2.md`, `PHASE4.md`
and `PHASE5.md`, and `## 0.0 Status` in `PHASE7.md`. `PHASE1.md` has none because
Phase 1 is implemented whole; `PHASE3.md` has none because it records deviations
inline, in the section each one belongs to — §5.5's buffer-protocol steps are the
example. Where this file and one of those disagree, they are right and this file
is a bug.

---

## [Unreleased]

### Fixed

- **`Tree::lookup`'s per-thread plan cache could serve one tree's compiled plan
  to another** (issue #196). The cache is `thread_local!` and shared by every
  `Tree` on the thread, but its key was `(target, source, generation)` — and all
  three agree across trees as a matter of course, because `FrameId`s are handed
  out in interning order and a built tree's generation is its declared edge
  count. Two trees built from the same names in the same order therefore
  collided, and so did a tree rebuilt after its predecessor was dropped. The key
  now carries an arena identity: a shared segment uses its existing
  `instance_uuid`, so two handles onto one segment still share plans, and every
  other backing takes a process-local counter. **No arena field was added** and
  the per-thread cache's footprint is unchanged — the extra `u64` fits in the
  entry's existing padding, measured at 2176 bytes per entry either way.

  The failure was a wrong number, never a bad read: a stolen plan naming an edge
  index the other arena does not have is refused by `ArenaView::edge`'s bounds
  check with `LookupError::UnknownEdge`.

  **`tf_tree_py` inherited this and is fixed by the same change** — its `lookup`
  calls the Rust facade and keeps no cache of its own, so any Python process
  holding two `tf_tree.Tree` objects on one thread was exposed. The C ABI never
  was: it exposes no collapsed lookup, only `tft_plan_create` and `tft_plan_at`.

- **The liveness predicate could report a running process dead** (issue #194).
  Its own doc comment states the bias it is built on — every ambiguity resolves
  to *alive*, because a false "dead" lets a rescuer take an entry from a running
  process, which is corruption, while a false "alive" only delays recovery — and
  two of its branches produced the other direction.

  A `/proc` that is not mounted fails every open with `ENOENT`, the same errno a
  genuinely dead pid produces, and that was read as proof of death: on such a
  host every participant read dead at once. The classification now depends on a
  one-shot probe of `/proc/self`, because a process that cannot see its own entry
  learns nothing from the absence of anybody else's. The `ENOENT` that remains
  ambiguous is `hidepid=2` hiding another user's process, which `docs/PHASE2.md`
  §3.10's same-user participants rule out — a dependency the code now names.

  Separately, a registrant that could not read its own start time stored `0`, and
  `0` compares unequal to every real start time, so the first reader that *could*
  read `/proc` declared that registrant dead while it was running. A stored `0`
  is now read as *unknown*, and `process_start_time` returns an `Option` so the
  sentinel is written deliberately rather than returned as though it were a
  measurement.

  **No arena field was added.** The record's `start_time` is unchanged and `0`
  was already what a fresh arena's participant region holds, so this is a
  read-side reinterpretation: `FORMAT_VERSION` and `layout_hash` are untouched.
- **`RUNBOOK.md`'s recovery from `ArenaHeldButUnreachable` told an operator to
  use a flag that does not exist** (issue #189). `--force-new` is `PHASE2.md`
  §3.4's name for the escape hatch, and the escape hatch shipped as
  `CreatePolicy::Always` on `tf_tree::Open` — a policy on whoever creates the
  arena, never a command-line flag. `tf_tree_cli` cannot usefully carry one:
  it supplies no `layout_if_creating`, so the create path it would reach ends in
  `OpenError::NoLayoutToCreate`, and it exits, so the arena would not outlive the
  command. The runbook now names the policy and shows the call; `PHASE2.md` §0.0
  carries the status row the prose is read against.

  The same entry described the wrong cause. It blamed a stopped or wedged
  participant; on this build the ordinary cause is an owner that exited while a
  *healthy* survivor kept the arena mapped, because §3.5's takeover has no
  trigger — and the wedge then lasts as long as any survivor does. The recovery
  is to stop every participant, which the entry now says.

- **`PHASE2.md` §5.1 said the identity triple is off the correctness path while
  three paths decided liveness from it** (issue #205). All three are in
  `crates/tf_tree/src/tree.rs`: the OFD probe's fallback, which hands the
  question to `record_is_alive` whenever `F_OFD_GETLK` declines to answer;
  `liveness_for`, which *is* the whole predicate for any tree that never got a
  probe — a heap tree, or one from `Tree::attach_shared`; and `Tree::reparent`,
  which steals A2's topology lock on `participant_is_alive` and never consults
  the probe even when the tree has one. The third was found verifying the first
  two.

  Recorded as a §0.0 status row, not as surgery on §5.1's NORMATIVE prose, on
  the same ground as #189's row: §0.0 outranks prose, and §5.1's wording is
  `0028`'s claimed ground. The row also names §3.10 as the reason a `hidepid=2`
  `ENOENT` is survivable — a dependency #204 put in a code comment and nowhere
  in the spec, which was #194's own complaint one layer down. **No code
  changed**: moving those paths off the triple is a design decision and belongs
  in a record.

- **`tests/frozen.rs`'s litter check failed about files it had not produced.**
  It scanned the shared `std::env::temp_dir()` for any entry whose name
  contained this process's id as a substring, so an unrelated process's scratch
  directories tripped it — roughly two runs in five, making `just shm-check`
  intermittently red. It now matches the `.{stem}.tmp.` prefix `freeze_to`
  actually writes.

### Added

- **`tf_tree_math::slerp` is public**, the shortest-arc quaternion
  interpolation `LerpSlerp` evaluates. It was reachable only by building two
  `Iso3` with throwaway zero translations, which the optimizer does not fold
  away: both arms end in the *same* out-of-line `slerp`, and what the `Iso3`
  one puts in front of it is 256 bytes of stack, two 64-byte isometries written
  out field by field, and a lerp of one zero translation into another. Measured
  as exported `extern "C"` arms at `opt-level = 3` on x86-64, that prologue is
  45 instructions bare (7 against 52) and 28 through the consumer's own
  `nalgebra` adapter (41 against 69). `ScLerp`'s
  kernel `dualquat::screw_pow` has been public since Phase 1; this removes the
  asymmetry rather than opening a new surface.

  **Two behavioural differences from the `Iso3` round trip, at the endpoints
  only** — both because `LerpSlerp::eval` answers `s = 0` and `s = 1` from a
  shortcut that never reaches the kernel:

  - At `s = 1` with `qa·qb < 0` the bare function returns `-qb` where `eval`
    answers `qb`. Same rotation, opposite components. Callers comparing
    quaternion components rather than rotations at `s = 1` will see it.
  - Inside the `1e-6`-rad LERP fallback band the bare function *renormalizes*
    at both endpoints, where `eval`'s shortcut returns its input exactly — a
    departure of about `2.7e-16` for any input whose components do not happen
    to square to exactly `1.0`.

  (The count was **one** here when this entry first landed, while `slerp`'s
  rustdoc, `docs/API.md` §6 row 16 and
  `endpoints_lose_bit_exactness_only_in_the_lerp_fallback` all said two. A third
  difference exists and is not counted with these because no rotation can reach
  it: a `-0.0` component comes back `+0.0` from the kernel and survives `eval`'s
  shortcut. It needs a hand-built `Quat`; `slerp`'s *Endpoints and degenerate
  inputs* section states it.)

- **`tf_tree` re-exports `slerp`.** The commit above made
  `tf_tree_math::slerp` public for a consumer of the *engine*, and the facade
  did not carry it — so that consumer still had to add `tf_tree_math` as a
  second direct dependency and pin it in lockstep with `tf_tree` on a line
  where every release breaks every other, which is worse than the `Iso3` round
  trip they were told to abandon. `tf_tree::slerp` is now the same item as
  `tf_tree_math::slerp`, and **`tf_tree::dualquat`** carries `ScLerp`'s kernel
  beside it — the first revision re-exported only `LerpSlerp`'s and so
  reproduced the same asymmetry one layer up, leaving an `ScLerp` consumer in
  exactly the two-dependency position this closes. The module is re-exported
  rather than the function, so `tf_tree::dualquat::screw_pow` is the same *path*
  as `tf_tree_math::dualquat::screw_pow` and not a second spelling of it
  (`PROJECT.md` §6). On the facade's **stable** tier (no `unstable`
  feature), checked by `tf_tree/tests/math_reexports.rs`, which compiles only
  if the two paths resolve to one function. `ScLerp`'s kernel is deliberately
  not re-exported beside it: `screw_pow` is reached through
  `tf_tree_math::dualquat`, and a bare name at the facade root would be a
  second spelling of that path rather than the same one.

- `just shm-check` runs `cargo nextest run -p tf_tree --features shm --lib`. The
  recipe named integration targets individually and had no `--lib` line, so an
  `shm`-gated unit test in the facade was compiled by clippy and executed by
  nothing.

- `just py-compile` gates the workspace-excluded `tf_tree_py` on a clean
  checkout. It skipped whenever there was no `.venv` — which is every CI runner
  and every first clone — so the dependency `just lint` carries covered nothing
  in the one configuration CI runs. It now falls back to the interpreter on
  `PATH` (PyO3 executes a Python to configure itself; it includes no header, and
  the fallback compiles on a host with no `Python.h`), prefers `.venv` where
  there is one, and skips only where there is no Python at all. It also carries
  `cargo fmt --check` for that crate, which `cargo fmt --all` does not reach
  because the crate is not a workspace member — and `just fmt` gained the
  matching write-mode line, so the formatting `just lint` now enforces has a
  recipe that fixes it. `ci.yml`'s `bindings` job invokes the recipe instead of
  re-spelling its two lines.
- **`just gate4-python` re-derives PHASE5 §12 gate 4's Python arm**, which had
  been a recorded 1.785× that no recipe regenerated — the shape
  `docs/benchmarks/EVIDENCE.md` exists to catch. `frozen_workers` gained
  `--python <interpreter>`, and `crates/tf_tree_bench/python/gate4_worker.py` is
  the worker it drives: same fixture, same stamp grid, same barrier, so the two
  arms differ in the worker's language and nothing else. It **reports** — the
  criterion is stated over the Rust worker and the recipe exits 0 on the FAIL it
  prints. Both arms now name their worker in the verdict line, so a pasted
  transcript carries the qualification the amendment asks for.
- **`tf_tree doctor`'s `TFT014` reports the participant-slot leak its catalogue
  row is named for** (issue #190, `docs/decisions/0028` plan step 6). A
  participant killed without running `Drop` leaves a `LIVE` record over a free
  lock byte; the owner's slot assigner skips such a record, and 64 of them wedge
  the arena at `NoParticipantSlots`. `doctor` now names each one, with the pid
  and how much of the fixed budget is spent. Severity stays **warn**, and
  **nothing is reclaimed** — `0028` is a draft record that exists so no
  reclamation lands before its predicate is settled.

  **It is not the shape #184 measured, and the finding says so.** #191 gave the
  owner a socket-hangup reap (`0028`'s *candidate B*), so a rendezvous joiner
  killed under a running owner has its record released — measured with an
  owner, a read-write joiner and a third observing process: with the joiner
  `SIGKILL`ed, its `state` word had gone `0x6` (`LIVE`) to `0x0` (`FREE`) by the
  observer's first poll 50 ms later. Seeing this finding means the slot was one
  that reap cannot reach, and the message names the five: the owner's own slot,
  a client its `epoll` never watched, an `attach_shared` participant, a
  takeover, and an owner that died inside the callback. Two leave the owner
  dead, so `doctor --attach` cannot be pointed at them at all — the check's doc
  comment and `PHASE5.md` §6's amendment both say which is which.

  **The claim half was blind in that same state and is fixed with it.** It fired
  on `owner_pid == 0`, resolved through `ParticipantTable::identity`, which
  answers for any record whose `state` word reads `LIVE` — which a `SIGKILL`ed
  writer's does until something clears it. `PHASE2.md` §5.1 forbids deciding
  liveness from `state` in as many words. A `Snapshot` now carries the
  participant table with `Tree::participant_alive` already applied — the
  kernel's `F_OFD_GETLK` on the slot's lock byte — and both halves read that one
  answer.

  Three conditions in the title stay undetected, and `PHASE5.md` §6's amendment
  says why each is a gap in the available evidence rather than in the check: a
  `RESERVED` record, which the predicate cannot tell from a healthy joiner
  mid-attach; the fork case, where the kernel truthfully reports the inherited
  byte as held; and a claim whose slot has since been re-granted, where the
  claim word carries the `ClaimRecord`'s own epoch rather than the participant's
  incarnation and so joins to whoever holds the slot now.

  **`TFT014` now skips on `doctor --from-file`, where it reported `pass`.** A
  frozen `.tft` is a byte copy of the whole arena, participant records
  included, so every slot in one names a process that exited when the freeze
  finished — and a file has no assigner for a leaked slot to wedge. A `--json`
  consumer reading `TFT014` on a `.tft` sees `skipped` with a reason where it
  saw `pass`; the `pass` was an all-clear about a question the file cannot be
  asked, and it held only because the old predicate was reading `state`.

## [0.0.2] — 2026-08-17 (wheels, no sdist)

### Fixed

- **The Python binding did not compile on macOS or Windows.** `tf_tree_py`
  imported `OpenError` and `AttachMode` unconditionally and named
  `Open`/`CreatePolicy` in `open_arena`, all of which the facade gates on
  `#[cfg(all(feature = "shm", target_os = "linux"))]`. Since the binding always
  enables `shm`, it is the *target* that decides, and four `wheels.yml` rows —
  both Windows, both macOS — failed with `E0432`/`E0433`. `open_arena` now keeps
  a paired non-Linux arm that refuses with a message naming the platform, on the
  argument `offline.rs` already records for `.tft`: a missing attribute makes a
  portable script fail with `AttributeError` somewhere unrelated.

### Note on 0.0.2's PyPI upload

**0.0.2 published eleven wheels and no source distribution**, and the sdist for
it can never be added under that version — PyPI refused it and the version is
now partially published. `pip install transform_tree` works on every platform
the wheels cover; a platform they do not cover has no source fallback until
0.0.3.

The sdist declared `License-File: LICENSE-APACHE` (and `LICENSE-MIT`, `NOTICE`)
in its `PKG-INFO` and did not contain them. maturin auto-detects those files at
the project root for PEP 639 but its sdist selection follows the cargo package
rules, which never reach the workspace root — the tarball held only
`Cargo.toml`, `PKG-INFO`, `pyproject.toml` and `README.md`. PyPI validates the
pair and returns `400 License-File LICENSE-APACHE does not exist in
distribution`. The wheels were never affected; only the sdist selection is.

`[tool.maturin] include` now names the three files, and `twine check` passes.

### Note on 0.0.1

**0.0.1 is a crates.io-only release.** All five crates are published at that
version and stay published; no wheel exists for it and none can, because the
commit it was cut from is the one that did not compile off Linux. The bug was
invisible until the `v0.0.1` tag ran `wheels.yml` for the first time in the
project's life — no workflow had run here between 2026-07-23 and 2026-08-16, and
that one triggers only on a `v*` tag.

0.0.2 is the first release published from a tag to **both** registries, and the
first to use Trusted Publishing on either: crates.io by OIDC through
`release.yml`, PyPI through its pending publisher in `wheels.yml`. Neither uses
a stored token.

## [0.0.1] — 2026-08-17 (crates.io only)

First publish: the five publishable crates went to crates.io at 00:33 UTC on
2026-08-17. **No wheel** — PyPI refused `tf_tree` as too close to an existing
project, and the distribution was renamed to `transform_tree` for `0.0.2`, so
that is the first version installable with `pip`.

Everything below is *Added*, because there is no previous release for anything
to be changed, deprecated, removed or fixed relative to.

### Added — the engine (Phase 1, `docs/PHASE1.md`)

- A pointer-free, fixed-capacity, `#[repr(C)]` transform arena. No growth, no
  realloc, no `Arc`/`Box`/`Vec` inside an arena structure; `FrameId` and
  `EdgeId` are append-only and tombstoned rather than recycled.
- Lock-free reads: per-edge seqlock sample rings, one writer per edge.
- Compiled lookup plans — `Plan` is resolved once and evaluated many times, and
  evaluation allocates nothing, takes no lock and converts nothing.
- `tf_tree_math`: `no_std`, `#![forbid(unsafe_code)]` SE(3)/SO(3), quaternion and
  dual-quaternion math. Two interpolators, ScLerp (default) and LerpSlerp.
- Integer-nanosecond stamps carrying a clock domain in the type, so mixing a
  sensor clock with a host clock is a compile error rather than a silent wrong
  answer. Cross-domain lookup is an error until Phase 8 supplies alignment.
- `Copy` error types that name the offending edge. No `String` in an error type
  or on a hot path anywhere in the workspace.
- Builder-time edge declaration
  (`docs/decisions/0004-builder-time-edge-declaration.md`), which is what sizes
  the arena.

### Added — shared memory between processes, Linux only (Phase 2, `docs/PHASE2.md`)

Behind the default-off `shm` feature; see *Absent* for the one part of this that
is specified and not wired up.

- A `memfd_create`-backed, sealed, `MAP_SHARED` arena, and a zero-diff read path
  proven by a relocation gate — the same bytes answer the same at a different
  address in a different process.
- Discovery and rendezvous: `tf_tree::open()`, a lock file whose byte 0 is
  ownership, and an attach protocol over `SOCK_SEQPACKET` + `SCM_RIGHTS` served
  by a thread rather than a daemon.
- A participant registry, liveness derived from `F_OFD_GETLK`, edge claims held
  as OFD leases so a dead writer's claim is observable, and reaping by any
  read-write participant.
- Fork poisoning: a `pthread_atfork` counter, with five destructors guarded, so
  a tree inherited across `fork()` refuses rather than corrupting.
- Per-edge page population at take-up. `docs/PHASE2.md` §0.0 records the
  measurement: 66.3 MiB → 3.8 MiB resident on an over-provisioned arena.

### Added — Python bindings (Phase 3, `docs/PHASE3.md`)

Published as the `tf_tree` wheel. The bindings go straight to Rust through PyO3,
not through the C ABI, because typed errors and zero-copy buffers do not survive
a C boundary (`docs/PHASE3.md` §0).

- `tf_tree.open()`, batch lookup with NumPy in and NumPy out and no intermediate
  allocation, and `at_into` writing directly into a caller-owned NumPy array
  (only a NumPy array — see *Partial*).
- The GIL is released above a measured work threshold rather than always or
  never.
- Free-threaded CPython is supported: the module is `#[pymodule(gil_used =
  false)]`, which is what emits the `Py_MOD_GIL_NOT_USED` slot, so importing it
  does not silently re-enable the GIL for the whole process — the failure mode
  that has no error, no warning and no failed import. Wheels are `abi3` for GIL
  builds plus version-specific `cp314t`; `3.13t` is deliberately absent and
  `abi3t` awaits CPython 3.15.
- Hand-written `.pyi` stubs and `py.typed`, checked under strict pyright.

### Added — C, C++ and ROS 2 (Phase 4, `docs/PHASE4.md`)

Of this group, only `at_with_derivatives` reaches a published artifact — it is
part of the `tf_tree` crate. The C ABI, the C++ wrapper and the ROS 2 package are
build-from-source; see *What is and is not published*.

- A two-tier C ABI: `tf_tree.h` (stable) and `tf_tree_unstable.h` (opt-in behind
  `#define TFT_ENABLE_UNSTABLE`), at ABI version 0.5, which is versioned
  independently of this crate version. Handle model, `Copy` error identifiers, a
  panic guard at every boundary, and all five pose layouts in both directions —
  reading needs matrix→quaternion, which needs Shepperd's four-branch method and
  a determinant check, because a reflection and a scaled rotation both convert
  silently into a *valid, different* answer.
- A header-only C++ wrapper, `tf_tree.hpp`, with Eigen and Sophus interop, both
  error modes (exceptions and `-fno-exceptions`), and a CMake package a consumer
  reaches with `find_package(tf_tree CONFIG)`.
- `sample_with_derivatives` / `at_with_derivatives`, pulled forward from Phase 6
  because ScLerp already computes the twist.
- `ros/tf_tree_ros`: a one-way `/tf` and `/tf_static` → arena ingest bridge, as
  an `ament_cmake` package. Ingress only — **it never writes to `/tf`**, which is
  the Phase 7 line and not an oversight.

### Added — offline, observability (Phase 5, `docs/PHASE5.md`)

- **`FORMAT_VERSION = 3`**, `layout_hash 0x3D104195` (`tf_tree doctor
  --explain-version` prints both). The Phase 6 spline regions are already
  declared in the header, absent, so that format break happens once rather than
  again in Phase 6.
- **Frozen `.tft` arenas**: `Tree::freeze_to`, `Tree::open_frozen`, `tf_tree
  freeze`, and `Tree.freeze()` from Python. The arena bytes as a memory-mapped
  file, so sixteen dataloader workers share one copy. A frozen tree answers
  bit-for-bit identically to the live one it came from, and `docs/PHASE5.md` §2
  records that as tested rather than intended — by `crates/tf_tree/tests/frozen.rs`,
  which is compiled only under `--features shm` and so runs in `just shm-check`,
  not in `just test`.
- **Bag ingestion, MCAP only** — `tf_tree ingest --bag`, `tf_tree freeze
  --from-bag`, and `tf_tree_ingest` as a library. Two passes with a
  spill-to-run-file for recordings larger than the memory cap; zstd and lz4
  chunks decode through pure-Rust codecs, so a rosbag2 or Foxglove recording
  ingests with no C build step. A truncated recording is read up to the cut and
  reported as truncated rather than refused, because a SIGKILLed recorder is how
  bags in the field end.
- **Offline Python API**: `tf_tree.open_file()` returns the ordinary `Tree`, so
  there is no parallel offline surface to learn.
- **Diagnostic counters**, default-on behind the `counters` feature, with the
  arena regions declared whether or not the feature is compiled in — so turning
  them off does not fork `layout_hash`.
- **A diagnostics catalogue, `TFT001`–`TFT019`**, behind `tf_tree doctor`, with
  `--json` (schema `tf_tree.doctor/1`), `--exit-code`, `--suppress`, and
  `--from-bag` / `--from-file` so a recording or a frozen index can be diagnosed
  without a running robot. Sixteen of the nineteen can detect something; the
  other three say they cannot rather than passing (see *Partial*).
- **`tf_tree top`**: attaches read-only, refuses `--rw`, and renders per-edge
  rate/staleness/occupancy/writer, the participant list, a rolling event feed and
  a per-edge inter-arrival histogram. `--web` serves the same data over a
  hand-rolled HTTP/1.1 loop on `std::net::TcpListener` with a `default-src 'none'`
  CSP — no new dependency, no CDN.

### What is and is not published

**Published to crates.io:** `tf_tree`, `tf_tree_core`, `tf_tree_math`,
`tf_tree_arena`, `tf_tree_ipc`. **Published to PyPI:** the `tf_tree` wheel.

**Everything else in the repository is `publish = false`**, and two of those are
worth calling out because a reader will otherwise assume they arrive with the
crates:

- **`tf_tree_cli` — the `tf_tree` / `tft` binary — is not published.** So
  `tf_tree tree`, `echo`, `doctor`, `top`, `bench`, `ingest` and `topology` are
  build-from-source in this release, and `freeze` and `participants` are
  build-from-source *with* `--features shm` on Linux, which is the only
  configuration in which those two subcommands exist at all.
  `tf_tree` on crates.io is a library: `cargo install tf_tree` installs no
  command. It does not fail, either — on cargo 1.95.0 it exits **0** after
  `warning: none of the package's binaries are available for install using the
  selected features`, so a script that installs the crate and then looks for a
  `tf_tree` on `PATH` gets a green build and a missing command.
- **`tf_tree_c` — the C ABI — is not published either**, so the C header, the
  C++ wrapper and the CMake package are build-from-source too, as are both
  `ros/` packages (they need `rclcpp`, which only exists inside `docker/tf2`).

**And in the other direction: two of the published crates do carry a binary,
both multi-process test helpers rather than anything to run.** `cargo install
tf_tree_ipc` installs `tf_tree_ipc_child`; `cargo install tf_tree --features
shm` installs `tf_tree_rendezvous_child`. (The plain `cargo install tf_tree`
above installs neither — that target is behind the feature.) Each is named for
its crate because the names they had, `ipc_child` and `rendezvous_child`, are
names neither crate has any business owning in a shared `~/.cargo/bin`, and
crates.io is one-way: a version can be yanked, never deleted. That they install
at all is a residue rather than an oversight — each is spawned by its own test
through `CARGO_BIN_EXE_*`, a compile-time guarantee, and every way to install
*nothing* trades that for a path resolved at run time, in the two most
process-dependent test suites in the repository. Both manifests carry the
measurements.

### Absent, deliberately

Each of these is a decision with an argument behind it, not a gap waiting to be
filled in a patch release.

- **A `tf2_ros::Buffer`-compatible shim, and arena → `/tf` egress.** That is
  Phase 7, and D21 gates it on operating evidence rather than scheduling it.
  `docs/PHASE7.md` §0.0 lists four gates and none is met. That document is a
  requirements artifact; its existence is not permission to build it.
- **Cross-host operation.** Phase 8. Interest-based replication, a delta-coded
  wire format, and clock-domain alignment with reported uncertainty. A
  cross-domain lookup is an error today for exactly this reason.
- **Continuous-time interpolation — cumulative B-splines with analytic
  derivatives.** Phase 6. The arena header already reserves its regions
  (`docs/PHASE5.md` §1.2), which is why this release's format break is the only
  one Phase 6 needs.
- **Visualization.** `docs/PHASE5.md` §8 is a section about not building
  something, with the argument recorded. This is the finished state, not a gap.
- **ROS 1.** There is no path and there will not be one.
- **rosbag2 `.db3` (sqlite3) ingestion.** Refused on a measured dependency
  finding — `rusqlite` vendors C, `prsqlite` records no licence on the crates.io
  index so `cargo deny` refuses it outright, and the remaining readers are header
  parsers. A `.db3` handed to `tf_tree ingest` is *diagnosed* as one, with the
  `ros2 bag convert` remedy, rather than reported as a corrupt MCAP.
- **`tf_tree serve`, and a recorder.** `docs/PHASE2.md` §9 is superseded by
  `docs/decisions/0019`; keeping an owner alive is the operator's job in this
  release, which is what D16 says it should be.

### Partial, and stated rather than papered over

- **Ownership migration when the arena's owner dies (`docs/PHASE2.md` §3.5).**
  The lock-file protocol exists and is tested; **nothing triggers it.** No
  participant watches its client socket for `HUP`. Observable consequence: kill
  the owner and every already-attached process keeps serving lookups exactly as
  specified, but **no new process can join** — it wins the ownership byte, meets
  the split-brain check against the survivors, backs off, and times out with
  `ArenaHeldButUnreachable`, for as long as any survivor lives.
- **Three diagnostic checks cannot detect anything in any configuration, and
  report that instead of passing:** `TFT002` and `TFT003` (owned by
  `tf_tree_bridge::StaticStore`, whose state is process-local) and `TFT004` (no
  arena receipt time is recorded). Eight more skip conditionally, on evidence
  rather than on capability, and each says which condition it hit.
- **`at_into` acquires its output buffer by casting to `numpy.ndarray`, not
  through the buffer protocol** (`docs/PHASE3.md` §5.5's status note). So a
  `memoryview`, or a pinned `torch`/`cupyx` allocation that is not a NumPy array,
  is **refused whatever its layout** — with an error saying so and suggesting
  `np.asarray(...)`, not a wrong answer.
- **`freeze_from_arrays`** — building a frozen index straight from NumPy arrays
  — is not implemented. Unlike the `.db3` row above this is a schedule and not a
  decision: it needs no dependency and no format change.
- **Phase 4's exit criterion is operational and is open.** A real node on real
  hardware for a sustained period, and a written log of every surprise. No amount
  of code closes it, and it is what gates Phase 7.
- **`docs/PHASE4.md` §7's benchmark gate criterion 1** is measured as four
  quotients on an interleaved ladder rather than the single ratio its own table
  names, because the single ratio's denominator moved 43% on edits that did not
  touch it. That section records all four as passing, over twelve pinned runs;
  `docs/decisions/0023` is the record that would change the wording.

### Platforms

- **Linux `x86_64`** is where this is developed and gated.
- **Linux `aarch64`** is a target and not yet evidence: the CI matrix rows exist
  and have never executed.
- **macOS and Windows** get a Python wheel with the single-process engine only —
  no shared memory, no `.tft`, no `tf_tree top` — and nothing tests it. See
  `SUPPORT.md`.

### A note on the gate

**GitHub Actions produced no run for this repository between 2026-07-23 and
2026-08-16**, so most of this release was verified by the `just` recipes run
locally on `x86_64` Linux, and by the `## 0.0 Implementation status` tables
those runs are recorded in — which is what the note at the top of this file
means by naming those tables as the source of truth.

Making the repository public restored CI, and the first runs found three latent
defects that had been invisible for the project's whole life: `c_char`
signedness on `aarch64`, an exported symbol in no header tier, and a container
with no writable `ROS_HOME`. The `ubuntu-24.04-arm` rows now execute and pass
for the first time.
`.github/workflows/ci.yml`'s header carries the evidence for the outage and the
diagnosis. Do not read a green check as verification.

<!-- The tag does not exist until the release commit creates it, so this link
     404s until then. That is the accurate state, not a broken link to fix. -->
[0.0.1]: https://github.com/NoeFontana/tf_tree/releases/tag/v0.0.1
