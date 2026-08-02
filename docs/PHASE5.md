# tf_tree — Phase 5 Implementation Specification: Offline, Observability, and the Adoption Wedge

> **Companions:** `docs/PROJECT.md` (decision log), `docs/PHASE1.md`–`PHASE4.md`.

**Deliverable:** the artifacts that make `tf_tree` useful to people who have adopted nothing.

Per D28, every user of this phase changes nothing about their robot. They point a tool at a bag, or attach a read-only process to a running system, and get something `tf2` cannot give them. That is the wedge, and it is also what produces the evidence gating Phase 7.

**Framing.** Phases 1–4 built an engine. This phase builds the two products that engine makes possible: a **frozen transform index** that turns a multi-gigabyte bag into a memory-mapped file queryable at native speed from sixteen dataloader workers at once, and an **observability layer** that surfaces a catalogue of transform pathologies which are currently invisible. Neither requires anyone to migrate.

---

## 0.0 Implementation status

**In progress.** This section is the live status table, in the style of
`PHASE2.md` §0.0, and is updated as work lands.

| Area | Status |
|---|---|
| §1 `FORMAT_VERSION = 3`, Phase 6 regions reserved | **Done.** Header 256 → 320 with ≥ 64 bytes still reserved (asserted, not intended); the two counter regions; Phase 6's four header fields, declared absent; `nominal_rate_mhz` and `declared_by_slot` in `EdgeRecord`, `frame_kind` in `FrameRecord`; `layout_hash` `0x9075_90F5` → `0x3D10_4195`; `doctor --explain-version`. **Two of this section's own amendments were wrong and are corrected in place.** |
| §2 Frozen arena (`.tft`) | **Done.** `tf_tree_arena::frozen` writes and maps the container; `Tree::open_frozen`/`Tree::freeze_to` and `tf_tree freeze --from-live` are wired. §2.1's bit-for-bit claim is **tested and holds** (`crates/tf_tree/tests/frozen.rs`). Two amendments below: the container header's size, and the one-sided per-edge span. |
| §3 Bag ingestion | **Partly done — MCAP only.** `tf_tree_ingest` is a new workspace member; §3's opening note rules out `tf_tree_core`/`tf_tree_arena`, and it is not in `tf_tree_cli` because §4's offline Python API needs the same logic and cannot depend on a binary crate. §3.1's two passes **including the spill-to-run-file**, §3.3's MCAP source (schema-based discovery, so remapped topics are found) and every §3.2 row are implemented and gated by `cargo nextest run --workspace`. `tf_tree ingest --bag` needs no features; `tf_tree freeze --from-bag` needs `shm` (the frozen backend does) and is gated by `just shm-check`. **Of the four things that were not done, two are closed and two are now decisions with arguments rather than gaps:** §3.1's run file is built — grouping still handles every recording whose largest single edge fits the cap, and one edge over the cap on its own is spilled, *reduced* in bounded passes and merged, so `IngestError::EdgeExceedsMemoryCap` is gone (see §3.1's amendment, including why the reduce pass is not optional); `--on-clock-reset=split` **stays refused with the argument recorded in §3.2's amendment** — it would turn one ingest into N arenas and change every downstream contract, to do worse what cutting the recording already does; §3.3's rosbag2-sqlite3 source **stays absent on a measured dependency finding** (§3.3's amendment: `rusqlite` vendors C, `prsqlite` has no licence and `cargo deny` refuses it, `sqlite-rs`/`sq3_parser` are header parsers) but a `.db3` is now *diagnosed* as one, with the `ros2 bag convert` remedy, instead of being reported as a corrupt MCAP. **`freeze_from_arrays` is still absent**, and it is the one of the four that is a schedule rather than a decision: it needs no dependency and no format change, only a `tf_tree_py` entry point that builds a `Tree` from NumPy arrays, and its gate is `just py-test` / `just py-lint` on two interpreters rather than `cargo nextest`. `Tree.freeze()` — the way *out* — already exists (§4 row). **`--max-memory` bounds pass two's sort buffers and *not* the arena**, which is the larger of the two at a measured 78 B/sample against the buffers' 64 — the arena is the output and cannot be capped. `ingest::fill` carries the table and `tests/memory.rs` asserts it; an earlier revision claimed "peak memory is the cap either way", which was false. **§0.0's `default-features = false` on `mcap` costs a measured ~1.8× per pass on a compressed recording and nothing else, and §0.0 carries the amendment:** chunks are taken whole (`emit_chunks`) and decoded by pure-Rust `ruzstd`/`lz4_flex` behind a default-on `compression` feature, so a zstd or lz4 recording — what rosbag2 and Foxglove write by default — ingests transparently with no C build step and `PHASE2.md` §2 untouched. `CompressedChunk` now means a codec outside the specification or a `--no-default-features` build, which is what the `mcap compress --compression none` remedy is for. **The price is measured, not asserted:** `survey` over a 160 000-transform recording takes 0.027 s uncompressed, 0.035 s for lz4 and 0.048 s for zstd, and `ruzstd` decodes libzstd frames at about a quarter of libzstd's own rate — multiplied by a pass count that is `1 + groups + spilled edges`, not a flat two. §12's throughput gate is still met by a wide margin, and an earlier revision of this row said the rule "cost nothing", which was the same kind of overclaim as the "peak memory is the cap either way" sentence three clauses up. Decompression is bounded **three** times before anything is allocated: `uncompressed_size` absolutely (64 MiB) and as an expansion ratio (1024×), because neither codec crate bounds total output — and the zstd decoder's *window*, which is a separate field in the codec's own header that neither of those can see. That third bound closed a measured defect: a 26-byte payload of two concatenated zstd frames, with an honest `uncompressed_size` and a correct CRC, decoded to the right answer and drove the allocator to a 134 MiB peak, unaffected by either knob. A truncated *compressed* chunk stays unrecoverable and is reported as truncation, not corruption; zstd is checked against a committed real-libzstd fixture and lz4 against a frame hand-authored from its specification (one frame, not a whole recording — §12 states the remaining asymmetry), and `just ingest-check` — now also run by CI's `test` job — compiles and tests the codec-free configuration that `--workspace` cannot see, plus asserts against the dependency graph that the shipped CLI still links both codecs, which no test inside that crate can. **A truncated recording is read up to the cut** and reported as truncated rather than refused — a SIGKILLed recorder is how bags in the field end. **A review of the spill path found two real defects and they are fixed:** the temporary file's name was derived from the *edge slot*, so two concurrent `fill` calls in one process picked the same path and `truncate(true)` let the second empty the first's inode — silently interleaved samples, no error, and a deterministic collision rather than a race wherever the unlink-at-create cannot run; it is now a process-wide `AtomicU64`. And **the reduce pass's cross-run tie order was asserted by nothing** — two order-inverting edits left the whole workspace green while resolving a duplicate to the wrong pose — so `a_reduce_pass_keeps_the_last_occurrence` now gates it. Two accounting corrections came with them: a reduce pass holds **two** staging buffers and counted one, and the run index (16 B per run) is real memory the cap does not bound and is now reported as `peak_run_index_bytes` rather than omitted. **What is still not gated, and is said rather than papered over:** the *per-run* sort's stability. Swapping it for `sort_unstable_by_key` survives the whole suite, because a run at the caps these tests use is short enough that `sort_unstable` insertion-sorts and is stable in fact; the in-memory sort it mirrors *is* gated, by the two tests that compare the paths. **Five amendments below**: declaration order is canonical, the reset threshold is not the bridge's question, the reset *guard* is per edge, the cap is enforced by two mechanisms and not one, and `split` is a decision. |
| §4 Offline Python API | **Done, including all three of §4.4's deltas**, with §4.2 trimmed and §4.3's *reason* corrected — see the two amendments in those sections. `tf_tree.open_file()` returns the ordinary `Tree`, so §4.1's "no parallel offline API" is structural rather than promised; `Tree.freeze()` is the Python way *out*, which is also what makes §4.1's claim testable from Python at all (`tests/python/test_frozen.py` compares live against frozen bit-for-bit through `plan.at`). Of §4.2's five helpers only `span` is API: `resample` is one line of NumPy over `at`, and `edges`/`gaps`/`manifest` need §3's counting pass and a CBOR reader, neither of which exists. **Gated by `just py-test` (CPython 3.14, 59 passed, 2 skipped) and `just py-test-freethreaded` (3.14t, 61 passed — the two skips are the free-threading tests, which only that interpreter can run) — so §4 does *not* inherit Phase 3's 3.14t gap; `uv` fetches the free-threaded build even though the host interpreter is 3.12.3.** **Of §4.4's three API-contract deltas, item 2 — introspection, `tree.frames()` / `tree.edges()` / `plan.edges()` — has landed** (`API.md` §6 row 8), as the *names* half only, which is what §4.4 authorises; the enumeration lives in `tf_tree_py` rather than on `Tree` and the follow-up to consolidate it with `tf_tree_c::unstable`'s and `tf_tree_cli`'s copies is filed in `frames_impl`'s doc comment. **All three refuse a tree inherited across a `fork()`** rather than describing the poison arena `Tree::view` substitutes, and `Tree.instance_uuid` was brought to the same rule — it returned the all-zero value that elsewhere means "in-process". `Tree.__repr__` is the deliberate exception, because a repr that raises breaks the debugger pane a fork victim is reading; it prints `detached-by-fork`. **Items 1 (`Layout::QuatTwist`) and 3 (`from_parts`/`from_timespec`/`from_ros`) are now done too** — `API.md` §6 rows 7 and 9. Item 1 is a keyword-only `layout=` on `at`/`at_into` serving all four layouts, plus an `interp=` on `build`/`open(create=...)`, whose default moved from `"lerpslerp"` to the engine's own `"sclerp"` (`API.md` §3) so that a Python-built tree answers a twist without one; item 3 is `tf_tree.from_parts` and `tf_tree.from_ros` on the Python side and `tft_stamp_from_parts`/`tft_stamp_from_timespec` on the C side (ABI minor 3 → 4), with **one refusal table asserted on both sides of the boundary** — the successes were never the risk. |
| §5 Diagnostic counters | **Done**, §5.6 included — see its amendment: the capture is structural, not a step. Structs and regions landed with §1; §5.4's `Guard` accumulation, the error-path increments and §5.5's default-on `counters` feature are wired. §5.7's measurement is `cargo run --release -p tf_tree_bench --bin counter_cost`: **no measurable contention at or below the CPU count**, so the sharding fallback is not justified. |
| §6 Diagnostics catalogue `TFT001`–`TFT018` | **Partly done.** All eighteen ids exist and are reported (§6's second amendment appends `TFT017` *unclaimed dynamic edge* and `TFT018` *out-of-order stamps*, so **nothing is reported id-less any more**; the ids are appended and never renumbered, which is what keeps `--suppress` and `--json` compatible); `--json` (schema `tf_tree.doctor/1`), `--exit-code` and `--suppress` are wired. **Fifteen detect** — `TFT001`, `TFT005`–`TFT018` — of which **thirteen run on the reference fixture**: `tf_tree doctor` reports `11 passed, 2 fired, 5 not run` of eighteen — `TFT016` moved from passed to fired when it started reading `transparent_hugepage/shmem_enabled`, the file that governs the live arena's `memfd`, in addition to `transparent_hugepage/enabled`, which does not. Reading only the latter reported this host as healthy while `MappedArena`'s `MADV_HUGEPAGE` was a silent no-op; §2.3's amendment carries the measurement. **Three cannot detect anything in any configuration and say so** rather than passing: `TFT002`/`TFT003` (owned by `tf_tree_bridge::StaticStore`, whose state is process-local) and `TFT004` (no arena receipt time is recorded). **Six more skip conditionally**, on evidence rather than on capability: `TFT001` and `TFT018` (live arena — the rings remember the current claim owner and not the sequence of writers, and the window slides while it is read), `TFT005` (the arena's stamps do not share an epoch with the system clock), **`TFT007` (nothing in *this* arena was comparable — either no edge declares a nominal rate, or the declaring edges have not retained enough intervals to measure one; the skip reason says which)**, `TFT010` (engine built without `counters`) and `TFT016` (non-Linux host). **`TFT007` was in the first group and is now in the second** — the amendment in §6 records how: a topology file's `rate_hz` is carried into `EdgeRecord::nominal_rate_mhz`, with no arena field added and no format bump. The reference fixture sizes its rings by slot count, so it still skips *there*, with a reason about that arena rather than about the system. **A `TFT007` `pass` therefore always means at least one edge was compared** — the second skip condition closes a review finding where a declared-but-unmeasurable arena (`doctor` at bringup, or a publisher that has stopped) reported `pass` having compared nothing, with no note either. **What §6's amendment states and does not solve:** `--discover` writes a *measured* rate into the same `rate_hz` the arena reads as an *intended* one, so a recording of a degraded publisher declares the fault as nominal — a discovered rate is a starting point to review. **`TFT018` also skips on a live arena** — the reconstructed push stream can show an inversion the publisher never made — which is a condition that was previously silently not run and reported as nothing at all. |
| §7 `tf_tree top` | **Done, both halves.** `tf_tree top` exists, attaches read-only and *refuses* `--rw`, and renders all four panes §7 names: per-edge kind/rate/staleness/occupancy/writer, the participant list (arena record ∪ lock-file byte, so read-only participants appear at all), a rolling feed derived from counter deltas, and a per-edge detail view with an inter-arrival histogram. **Built with plain ANSI, not `ratatui` — see the amendment below.** `--web` serves the same [`Sampler`] over a hand-rolled HTTP/1.1 loop on `std::net::TcpListener` (**no new dependency**; a server crate is the third instance of §7's own argument, and the web-view amendment below records it), binding `127.0.0.1:8787` by default. One embedded HTML file, one `/api/tick` JSON endpoint (schema `tf_tree.top/1`), hand-written SVG, **no CDN — enforced by a `default-src 'none'` CSP the server sends, not promised by a comment.** Gated by `cargo nextest run --workspace`: the unit tests in `src/web.rs` plus `crates/tf_tree_cli/tests/web.rs`, which runs the shipped binary and parses the document a browser would receive; `just shm-check` runs the latter again under `--features shm`, which is the build an operator attaches with. **Three defects the amendment names were found by writing those tests and are fixed.** **Not done:** `--web` has no keep-alive, caps itself at 64 concurrent connections and is not a general-purpose server; there is no key handling on either half (see the `ratatui` amendment). |
| §8 Visualization | **Deliberately not built** — this is the finished state, not a gap |
| §9 Benchmark artifact | **Partial.** `just bench-report` emits `report/{results.json,index.html}` with the §9.3 provenance header, all eight §9.2 rows, and all four §9.3 "where we are worse" entries; `Report::validate` makes the honesty rules structural — the tool refuses to write a report that over-claims, rather than relying on whoever wrote it. On this host every comparison row is `UNAVAILABLE` with its own reason, which is §9.3's prescribed output, not a gap in the tool. **Two of those reasons had gone stale and were corrected:** the `.tft` rows said §2 and §3 "are not implemented", citing *this table* as the source of truth, while this table records §2 as Done and §3 as done for MCAP — so the tool was printing a false statement under the one section (§9.3) that is about stating a true one. Both reasons are now derived from `cfg!(all(feature = "shm", target_os = "linux"))` — the frozen backend genuinely is not compiled into `just bench-report`'s build — and **two tests pin the general rule: an unavailable row's reason is about this host or this build, never about the roadmap.** **Not done:** §9.1's container image, the public sample recording, `tf_tree bench compare`'s CLI spelling, and §12 gate 7 (reproducing a committed `results.json` on a clean machine). The CLI spelling's blocker is no longer §3 (which landed) but the crate boundary: `tf_tree_bench` is `publish = false` and carries `criterion`, so a shipped `tf_tree` subcommand reaching it would drag a benchmark harness into every install — `CLAUDE.md` routes that to a decision record, not a PR. **§9.1's *measurement* now exists even though its CLI spelling does not** (`just dds-bench`, `ros/tf_tree_bench_ros`): one publisher, real DDS, §5.2's QoS, N `tf2_ros::TransformListener` consumers against the ingest bridge, warm-up discarded and stated, and the whole input set — publisher plan, bridge topology, query set — *generated* from one `tf_tree_bench::workload` entry so §9.3's "identical data" is structural rather than promised. Measured on this host at 4 consumers: tf_tree 0.86 µs p50 / 9.15 µs p99.9 / 24.65 MiB PSS against the ordinary tf2 deployment's 3.54 / 18.82 / 63.11, and against tf2's *best* case (one composed listener, in the table so the comparison is not a strawman) 1.48 / 13.95 / 24.09. **The arm it cannot run is named in its own output on every run**: there is no multi-process tf_tree consumer, because `tft_bridge_create` builds a *heap* arena with `TreeBuilder::build()` and no second process can attach to it. That is [`0015`](./decisions/0015-the-bridge-fills-a-shared-arena.md), a draft decision record — and it is the arm §9.2's *total RSS across N consumers* row and §12's criterion 4 are both actually about, so §9 is not complete until it lands. **Building it found a real bridge defect**, recorded in `docs/benchmarks/tf2.md`: `tf_tree_bridge::Publisher` is keyed on the resolved *node name* rather than on the GID, so messages arriving before the graph cache resolves claim the edge under an unknown name and `first_writer_wins` then rejects the real publisher permanently — 9 864 of 10 070 transforms dropped and 100 % of lookups failing against a single correctly-declared publisher. Also **not** part of §9 and deliberately not gated: a broader exploratory suite (`just contended-scaling`, `just scale-sweep`, `just soak`, `just bench-run`/`just bench-ab`) covering §11.2's writers-and-pinning row, the width/depth/ring/fan-out axes, multi-minute drift, and A/B comparison of two builds. Those emit `tf_tree.bench-run/1` rather than joining `REQUIRED_ROWS`, because this host fails `Fitness::probe` and a gate that flaps is a gate people learn to ignore. |
| §10 Open-source readiness | **Partial.** Name decision made, measured and recorded ([`0008`](./decisions/0008-the-name-tf-tree.md)): `tf_tree` is free on the crates.io sparse index and on PyPI, and is kept. `LICENSE-MIT`/`LICENSE-APACHE`, `NOTICE`, `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `SECURITY.md` (real address, and an explicit in-scope/out-of-scope boundary around §3.10's trust model) and `SUPPORT.md` (response expectations, platform support, MSRV policy) are in place; `README.md` is rebuilt against the §0.0 tables. **The MSRV was measured and was wrong:** `rust-version = "1.83"` could not build — `blake3` pulls `constant_time_eq 0.4.2`, edition 2024 — so the floor is now **1.85**, with a CI job that reads it from the manifest and builds `--locked` on exactly it. Every `publish = false` crate now states its reason in its own manifest. **The benchmark artifact is now a regression gate** (`just bench-check`, CI job `bench-gate`): `crates/tf_tree_bench/baseline/results.json` is committed, and `bench_report --check-baseline` fails on a withdrawn claim, a dropped §9.2 row, a changed `layout_hash`/`format_version`/build profile, or a directional metric past the slack the baseline itself records. **The comparison ignores every host fact by construction** — CPU model, cores, kernel, governor, load and all `reason` prose — because a gate that fails for the CPU model is a gate people learn to ignore; `src/baseline.rs` carries the split. Making that possible needed `results.json` schema `/2`: `/1`'s bare `{value, unit}` gave a consumer no way to know which direction was an improvement. **On this host the gate holds exactly one number** — the LerpSlerp differential's `max_deviation`, the one row that is host-independent by construction — and that is not a placeholder: `Report::validate` now refuses any row that prints numbers while giving none of them a direction, so a row that becomes measurable arrives gated or not at all. **`docs/PHASE2.md` §11.4's `shm_torture` now exists** (`just shm-torture`, 30 minutes, six processes, `SIGKILL` at 6 Hz) with `just shm-torture-asan` for §11.4's "under ASan" and `just shm-torture-self-test` — the seconds-long half that runs in `just shm-check`. It asserts three things, and the second and third are there because the first revision of this harness had none of them and was **vacuous on most seeds**: that an injected corrupt transform is caught by a process that did not write it; that a run validating too little *fails* instead of printing the same `0 violations` a healthy one does; and that `--inject-violation` finishing clean is itself a failure. **The killed processes are joiners, never the rendezvous owner** — see `docs/PHASE2.md` §0.0's §3.5 row for why, and the §11.4 row for what that costs. **The sanitizer rows are wired to recipes that were run on this host, not to a green tick:** `just tsan` (passes), `just shm-torture-asan` (passes on the fixed harness: 152 936 checked reads — 122 344 of them composing all four edges — 477 kills, no ASan report, over 478 observation rounds), and `just cpp-check` for the C++ UBSan half. **There is no Rust UBSan row and its absence is deliberate:** `rustc -Zsanitizer` accepts address/thread/leak/memory and the CFI variants and has no `undefined`, so §10's "ASan/UBSan/TSan" is a C/C++ checklist and its UBSan half lives where there is C++ to check. The nightly workflow (`.github/workflows/nightly.yml`) carries the torture and sanitizer jobs. **Not done:** the mdBook site, SBOM per release, and release automation (`cargo-dist`, PEP 740 attestations, signed tags) — all three are ceremony until there is a release, and half-building them would leave a publish path nobody has run. |

### What this development environment can and cannot gate

- **ROS 2 is available, in a container**, and an earlier revision of this
  section wrongly said otherwise. `tf_tree/tf2-bench:latest` is
  `FROM ros:lyrical-ros-base` and carries `rosbag2_cpp` with MCAP storage, so
  §3's ingest path can be tested against **real recordings written by a real
  rosbag2 through a real DDS**, not only against synthetic fixtures. Host-side,
  MCAP is self-describing and its Rust reader needs no ROS at all, so fixtures
  work either way.
- **The `mcap` crate must be taken with `default-features = false`.** Its
  defaults are `[zstd, lz4]`, which vendor C through `lz4-sys`/`zstd-sys` and
  violate `PHASE2.md` §2's no-C-build-step rule.

  **Amendment — the consequence is reversed; the rule is not.** This bullet used
  to continue "and the cost is that a zstd- or lz4-compressed recording is
  refused", and §3's status row said the same. That was true only because
  `mcap`'s decompressor factory was the only thing reading a chunk. Two changes
  removed the dependency: `LinearReaderOptions::with_emit_chunks(true)` hands
  chunks over whole so the factory is never reached, and `tf_tree_ingest` then
  decodes them with **pure-Rust codecs of its own** — `ruzstd` and `lz4_flex`,
  behind a default-on `compression` feature. `mcap` keeps
  `default-features = false`, no `*-sys` crate enters the graph, and
  `PHASE2.md` §2 is untouched: what changed is *who owns the decoder*, not
  whether C is permitted.

  So a rosbag2 or Foxglove recording — zstd chunks, the default for both —
  ingests transparently. `IngestError::CompressedChunk` survives for the two
  cases that are still real: a codec name outside the MCAP specification, and a
  `--no-default-features` build. The `mcap compress --compression none` remedy is
  now the answer to those rather than to the ordinary case.

  Three things this bought that are worth stating because they are not free.
  `uncompressed_size` is a number off a disk and is both the allocation size and
  a decompression-bomb bound, so `IngestOptions::max_chunk_uncompressed_bytes`
  (64 MiB) and `max_chunk_expansion_ratio` (1024) are enforced before anything is
  allocated — neither codec crate bounds total output (ruzstd caps its *window*,
  lz4_flex its *per-block* size). A truncated **compressed** chunk is
  unrecoverable, because a partial codec frame is not decodable, and is reported
  as truncation rather than corruption. And the two codecs are not equally
  evidenced, though both are evidenced by something other than their own
  encoder: zstd is checked against a committed fixture compressed by real
  libzstd 1.5.5, and lz4 — with no `lz4` CLI on this host — against an 82-byte
  frame authored by hand from the LZ4 specification, of whose 656 single-bit
  perturbations 651 are caught and the other five accounted for. The remaining
  asymmetry is scope: zstd's evidence is a whole recording, lz4's is one frame
  (`crates/tf_tree_ingest/testdata/ATTRIBUTION.md`).

  `ruzstd` is also what fixes the workspace MSRV at 1.87, which the root
  `Cargo.toml` records. And because `compression` is default-**on**, the
  codec-free configuration is compiled by no `--workspace` command:
  `just ingest-check` builds and tests it explicitly, on both
  `tf_tree_ingest` and `tf_tree_cli`.
- **No GPU**, so anything touching device memory stays untested.
- **CPython 3.12.3 on the host**, no free-threaded build, so §4's additions
  inherit Phase 3's existing coverage gap on 3.14t. `uv` is installed, so
  `just py-setup` can fetch 3.14/3.14t.
- **The benchmark host cannot fairly run §9's comparison.** 4 physical cores
  with SMT and `perf_event_paranoid=4`; Phase 1's own read-scaling gate already
  fails on it (5.35–5.62× against ≥ 6×). The `.tft` rows — 16-worker total Pss,
  open time vs bag parse — *are* measurable here. §9.3 already prescribes the
  right response: omit a row that cannot be measured fairly and say why.
- **GitHub Actions has produced no run since 2026-07-23.** Every gate claimed
  in this document must be run locally through `just` and the arch stated,
  because a green check on a PR is not evidence.

---

## 0. Scope

### In scope

| | |
|---|---|
| `FORMAT_VERSION = 3` | one deliberate layout break, with room reserved for Phase 6 (§1) |
| Frozen arena (`.tft`) | the arena bytes as a file; mmap, share, query (§2) |
| Bag ingestion | MCAP and rosbag2 → arena or `.tft`, two-pass, out-of-order tolerant (§3) |
| Offline Python API | identical to the online API, plus dataset helpers (§4) |
| Diagnostic counters | consumer-side failure counters, always on and free; publish-side derived (§5) |
| Diagnostics catalogue | 16 checks (`TFT001`–`TFT016`), each with a detection rule and a severity (§6). §6's amendments append `TFT017`–`TFT019`, so the shipped catalogue is **19** |
| `tf_tree top` | TUI plus an embedded static web view (§7) |
| Stored-sample iteration | `iter_edge` / `iter_edges` / `frame_path` — audit and export, viewer-neutral (§8.3) |
| Benchmark artifact | one command, reproducible, honest, CI-gated (§9) |
| Open-source readiness | the checklist that has to be true before publishing (§10) |

### Out of scope — NORMATIVE

| Excluded | Why |
|---|---|
| `tf2_ros::Buffer` shim | Phase 7, gated on this phase's evidence |
| Arena → `/tf` egress | Phase 7 |
| Splines | Phase 6 — §1 reserves their layout space now |
| Covariance, CoW branches | **Cut** by [`0009`](./decisions/0009-descoping-phase-6.md) — not deferred. A tree cannot compose a correct covariance; CoW serves the loop-closure use case D2 rejects. |
| Inter-host replication | Phase 8 |
| Compression in `.tft` | Breaks `mmap`, which is the entire point. Revisit only with a block-oriented design and measured demand. |
| A web *framework* | §7. An embedded static page and one JSON endpoint. Nothing that needs npm. |
| **All visualization work** | §8. The user's bag already contains transforms alongside images and LiDAR, and already opens in Rerun or Foxglove. Anything we emit is a re-encoding of data they can already see. |
| A viewer channel, plugin, or SDK dependency | §8.1. Cost with no new information. The missing-transforms case is a Phase 7 egress-bridge problem (§8.4). |

---

## 1. `FORMAT_VERSION = 3` — break it once, deliberately

### 1.1 The honest finding

Phase 5 needs new arena regions. Phases 1–3 are implemented, so this is a real break: every participant must be rebuilt and restarted together, and no version-2 arena may be attached. That is already documented in the Phase 2 runbook as the consequence of a layout change.

**Do it once, now, with room reserved** — rather than three times across Phases 5, 6 and 8. This is the last cheap moment: pre-1.0, small user count, and we already know most of what Phase 6 needs.

### 1.2 What goes in — NORMATIVE

| Region / field | Phase | Size |
|---|---|---|
| `EdgeCounters` array | 5 | `max_edges × 128 B` |
| `ParticipantCounters` array | 5 | `max_participants × 128 B` |
| Per-edge `nominal_rate_mhz: u32` | 5 | in `EdgeRecord` reserved bytes |
| Per-edge `declared_by_slot: u32` | 5 | in `EdgeRecord` reserved bytes |
| ~~`covariance_region_off: u32`, `covariance_stride: u32`~~ | — | **Descoped by [`0009`](./decisions/0009-descoping-phase-6.md).** The eight bytes stay reserved *in place* as `_reserved_covariance`, because closing the gap would move the two spline offsets below. |
| `spline_region_off: u32`, `spline_degree: u8` | **6** | header, zero when absent |
| `frame_kind: u8` (link / sensor / map / virtual) | 5 | `FrameRecord` reserved |
| Header `_reserved` | — | keep ≥ 64 bytes after all of the above |

Regions whose Phase 6 content does not exist yet are declared in the header with offset `0`, meaning absent. Phase 6 then fills them **without another layout change**, because the region table already accounts for them.

**NORMATIVE:** recompute `layout_hash` and bump `FORMAT_VERSION` to 3 in one commit. Ship a `tf_tree doctor --explain-version` that prints both versions and the required action when a mismatch is detected, since this is the error operators will meet during the upgrade.

> **Amendment — this field set does not fit the current header, and the header
> must grow. Plan it; do not discover it.**
>
> `ArenaHeader` is exactly 256 bytes with **48 free**: the named `_reserved: [u8; 8]`
> at offset 128, plus 40 bytes of *implicit* alignment padding at 152..192
> (`instance_uuid` ends at 152; `TopoLock` is `align(64)` so it lands at 192).
> Both are pinned by tests — `header.rs` asserts `size_of == 256` and
> `instance_uuid_occupies_pre_existing_alignment_padding` asserts
> `lock_at == (uuid_at + 16).next_multiple_of(64)`, which exists precisely
> because that padding is what let `instance_uuid` land without a version bump.
>
> The new header fields are `covariance_region_off` + `covariance_stride` +
> `spline_region_off` + `spline_degree` = 13 bytes before alignment, and this
> section then demands **≥ 64 bytes still reserved afterwards**. That is ≥ 77
> against 48 available. It does not fit.
>
> *(The two covariance fields were later descoped by
> [`0009`](./decisions/0009-descoping-phase-6.md). The arithmetic above is left
> as it was, because it is the reason the header is 320 bytes and that decision
> is already published; the freed eight bytes are reserved in place, so nothing
> below it moved. Reclaiming them would move `spline_region_off` off 168 and
> `layout_hash` — which hashes region strides, not header fields — would not
> catch the resulting disagreement between two v3 participants.)*
>
> So `ArenaHeader` grows to **320 bytes**, and three consequences follow that are
> cheap now and expensive later:
>
> 1. `topo_lock` moves off its pinned offset 192, so
>    `instance_uuid_occupies_pre_existing_alignment_padding` must be rewritten
>    rather than deleted — it is the guard that stops the next person assuming
>    there is still slack.
> 2. The header region literal in `layout.rs` (`256usize, // header`) changes.
> 3. `layout_hash` changes **automatically**, because `size_of::<ArenaHeader>()`
>    is its first input. That is correct and wanted here.
>
> **Correction, from implementing it.** This section previously added that the
> hash "is duplicated as the literal `0x9075_90F5` in `tf_tree_ipc`'s wire
> tests, so those move with it". It is not. Those literals are *fixture values*
> in byte-position assertions — they pin that a `SegmentDescriptor`'s
> `layout_hash` field encodes at offset 12, and any distinctive `u32` would do.
> All 83 `tf_tree_ipc` tests pass unchanged across the bump. The one real
> duplicate is the snapshot in `layout.rs`'s own test, which carries the change
> history.
>
> Also note what `layout_hash` does **not** cover: region *offsets*, region
> *count*, and `max_frames`/`max_edges`/`max_participants`. Its `strides` input
> is a hardcoded `[u32; 10]`. **Adding the two counter regions therefore does not
> change the hash unless their strides are explicitly appended.** They are, and
> the array is now `[u32; 12]`.
>
> **Correction, from implementing it.** The stated *reason* — "or a v3 arena
> built with counters and one built without would hash identically and attach to
> each other" — describes a scenario §5.5 and D34 rule out: the regions exist
> whether or not the `counters` feature is compiled in, precisely so that
> disabling it does not fork the layout hash. Those two builds have identical
> layouts and *should* attach. The strides are appended because the hash should
> describe the layout that exists, which is a good enough reason on its own, and
> because it is already correct if the regions ever do become conditional.

### 1.3 Publish-side counters need no storage at all

The obvious design adds push counters to `EdgeTelemetry`. **Do not.** A relaxed `fetch_add` on the push path costs ~5–10 ns against a ~50 ns push — a 10–20% regression on the hottest write in the system, to store something already present:

- **push count** = `EdgeRecord::head`, which is already a monotone counter of every sample ever published.
- **rate, jitter, gaps** = derivable from the stamp array, which is already contiguous and cache-friendly.
- **last publish time** = the newest stamp.

So the entire publish-side diagnostic surface is computed by a reader walking data that already exists. **Publish-side observability is free and the push path is untouched.** Only consumer-side failures need new storage, and those increment on error paths, where cost is irrelevant.

```rust
#[repr(C, align(64))]
pub struct EdgeCounters {                 // consumer-side only
    pub lookups_ok: AtomicU64,
    pub err_extrap_before: AtomicU64,
    pub err_extrap_after: AtomicU64,
    pub err_no_data: AtomicU64,
    pub err_slot_recycled: AtomicU64,
    pub err_slot_contended: AtomicU64,
    pub last_err_nanos: AtomicI64,
    pub worst_extrap_gap_ns: AtomicI64,
    _pad: [u8; 64],
}
```

`ParticipantCounters` mirrors it per participant slot, so `doctor` can answer *which consumer* is failing, not merely that failures exist. Attribution is what makes the diagnostic actionable.

---

## 2. The frozen arena — `.tft`

### 2.1 The file *is* the arena

Phase 1 invariant 2 says no pointers in the arena; every internal reference is an offset. The arena is therefore relocatable by `memcpy` — which means it is also **writable to disk and mappable back with no parsing, no deserialization, and no fixups.**

That property, built for shared memory, turns out to be a file format for free. A frozen `.tft` is a header, a manifest, and the arena bytes. Opening one is an `mmap`.

**NORMATIVE:** the frozen read path uses the **identical** `Plan::at` code as the online path, against a `PROT_READ` mapping. No offline variant of the lookup, no separate index. The bit-identical replay test from Phase 2 §10 extends to cover `HeapArena` / `MappedArena` / `FrozenArena` as three ways of holding the same bytes.

### 2.2 Why this is the wedge

A perception team's dataloader currently does one of three bad things: re-parses the bag in every worker, precomputes poses into a pickle and loses the ability to query at arbitrary times, or runs a ROS node to serve transforms during training.

With a `.tft`: sixteen `DataLoader` workers each `mmap` the same file, the kernel shares one set of clean pages across all of them, and each worker queries at ~50 ns with no IPC. Marginal RSS per worker is approximately zero — **which is the same page-sharing argument as Phase 2's, applied to a use case that requires adopting nothing at runtime.**

Measured basis (Phase 2 §3.8, Linux 6.18): untouched regions of a mapping cost nothing, and touched pages are charged exactly once. Make this a benchmark row (§9): total RSS across 16 workers versus 16 independent bag parses.

### 2.3 File layout — NORMATIVE

```
offset 0        FrozenHeader
                  magic         [u8; 8] = b"TFTFROZ\0"
                  format_version u32     = 3
                  layout_hash    u32
                  file_size      u64
                  manifest_off   u32, manifest_len u32     (CBOR)
                  arena_off      u64                        (2 MiB aligned)
                  arena_size     u64
                  source_digest  [u8;32]  BLAKE3 of the source recording
                  created_unix_ns i64
                  tool_version   [u8; 32]
offset manifest_off   CBOR manifest — frame names, edge list, per-edge time span and
                      sample count, source path, ingest options, tf_tree version
offset arena_off      the arena, byte-identical to an in-memory arena
```

`arena_off` is **2 MiB aligned**, not merely page aligned, so the mapping is eligible for transparent huge pages and `MADV_HUGEPAGE` is meaningful. A 115 MB index on 4 KiB pages needs ~28 000 TLB entries; on 2 MiB pages, 55.

**Amendment — the alignment is correct and the benefit is *unverified*, on any host we have.** `cargo run --release -p tf_tree_bench --features shm --example hugepage_grant` measures the **grant** rather than the request, by reading `ShmemPmdMapped`/`AnonHugePages`/`FilePmdMapped` for the arena's own range out of `/proc/self/smaps`. On this development host, with a 72 MiB arena fully resident, the grant is **0 KiB** — and stays 0 KiB after setting `shmem_enabled` to `advise`.

Three candidate causes were checked and eliminated, so the sentence above is not a hedge:

* **Not our alignment.** The live `memfd` mapping lands 2 MiB aligned anyway (`0x746951c00000`), so the `arena_off` reasoning is sound and nothing in the mapping code is at fault.
* **Not the policy alone.** `enabled` is `[madvise]` and `shmem_enabled` was set to `[advise]` for the counterfactual. Still zero.
* **Not fragmentation.** `/proc/vmstat` shows `thp_file_alloc 0` **and** `thp_file_fallback 0` across the run — the kernel never *attempted* an allocation, so there was nothing for fragmentation to defeat. System-wide `AnonHugePages` and `ShmemHugePages` are both 0 after twelve days of uptime: nothing on this machine has ever received a transparent huge page.

So this host (an EPYC-Milan guest under a hypervisor) cannot grant them at all, and **the TLB-reach claim above remains a projection from page-table arithmetic, not a measurement.** The harness exists so that it becomes one the first time this runs somewhere THP is functional; its `vmstat` table distinguishes "never attempted" from "attempted and beaten by fragmentation", which is the difference between a permanent and a transient condition.

**A live arena is governed by a different sysfs file than this section implies**, and `TFT016` was reading the wrong one — see §6.

The manifest is CBOR rather than a packed struct because it is cold, variable-length, and worth being able to inspect with a generic tool. Everything hot is in the arena.

`source_digest` makes a `.tft` traceable to the recording it came from, which matters the first time a training result cannot be reproduced.

> **Amendment — the container header is 128 bytes, and that has to be a
> constant rather than a `size_of`.**
>
> The field list above gives no total size and no reserved tail. Laid out in the
> stated order it comes to 120 bytes with no implicit padding, so
> `tf_tree_arena::frozen::FROZEN_HEADER_SIZE` pins it at **128 with 8 reserved**,
> asserted by a test. The alternative — letting the manifest start at whatever
> `size_of::<FrozenHeader>()` happened to be — makes the file layout depend on a
> compiler decision, which is exactly the class of accident `layout_hash` exists
> to catch inside the arena and which nothing would have caught out here.
>
> The reserved tail is written zero and **not** checked on read, so a future
> field placed there must be optional by construction: an older reader will
> ignore it rather than refuse the file.

> **Amendment — `arena_off` is 2 MiB aligned for a narrower reason than the
> section gives, and the section's reason is not quite right.**
>
> §2.3 says the alignment makes the mapping "eligible for transparent huge pages".
> More precisely: a huge page can back a mapping only where the virtual address
> and the file offset are congruent modulo 2 MiB. Aligning the file offset is
> *necessary* and is the only half this format controls; the address is the
> kernel's choice. So `MADV_HUGEPAGE` is best-effort here and its failure is not
> an error — which is what the implementation does, and why.

> **Amendment — the manifest's per-edge span is one-sided, and is named
> accordingly.**
>
> §2.3 asks for a "per-edge time span". `newest_ns` is exact. The other end is
> emitted as **`oldest_ns`**, meaning the oldest sample *still retained in the
> ring* — which is not the oldest sample the source contained, because a ring
> that lapped during ingest has already dropped the earlier ones. §3's counting
> pass knows the true span and can add a key for it. A single `span` that
> silently meant "whatever survived" would be worse than a narrower key that
> means what its name says.
>
> Both are `null`, not `0`, for an edge that has never published: a reader cannot
> tell a stamp of zero from "no samples", and epoch-zero stamps are real.

> **Amendment — the per-edge "sample count" is two keys, because there are two
> counts and they are not close.**
>
> §2.3 asks for a per-edge "sample count". `EdgeRecord::head` is the monotone
> count of every sample ever pushed and keeps rising after the ring laps; the
> number of samples the *file* holds is `min(head, retained)`. For a 512-slot
> ring that took 2048 pushes those are 2048 and 511 — and the key sat one line
> above `oldest_ns`, which is already, deliberately, the retained window. A
> consumer computing `samples / (newest_ns - oldest_ns)` read 4 kHz off a 1 kHz
> edge.
>
> So the manifest emits **`samples`** = what the file holds, and
> **`pushes_total`** = what the source produced. Both, not one: their ratio is
> how much the ring dropped during ingest, which is the first thing to check when
> an offline query comes back short. The per-edge map is therefore eight keys.

### 2.4 Read path

```
open, validate FrozenHeader (magic, format_version, layout_hash, file_size)
mmap(arena_off, arena_size, PROT_READ, MAP_PRIVATE | MAP_NORESERVE)
madvise(MADV_HUGEPAGE)                    // best effort
```

`MAP_PRIVATE` on a read-only mapping still shares clean page cache across processes, and removes any possibility of accidental writeback. No socket, no lock file, no participant table — a frozen arena has no writers, so none of Phase 2's coordination applies. `AttachMode` is implicitly and permanently `ReadOnly`.

**NORMATIVE:** `layout_hash` mismatch is a hard error naming both values and stating that the file must be re-frozen. A `.tft` is a cache, not an archive — say so in the docs, and keep the source recording.

### 2.5 Sizing

Freezing computes exact per-edge capacities from a counting pass, so there is no wrap and no wasted space. A 30-minute recording with one 1 kHz edge, four 200 Hz edges, and twenty static edges:

```
1 kHz  × 1800 s = 1.8 M samples × 64 B  = 115 MB
200 Hz × 1800 s × 4 edges = 1.44 M      =  92 MB
stamps                                   =  26 MB
                                          ------
                                          ~233 MB
```

Binary search over 1.8 M samples is ~21 probes against ~12 for an online buffer — still nanoseconds, and the stamp array is dense so most probes hit cache. No special indexing is needed; **resist adding one until a benchmark says otherwise.**

---

## 3. Bag ingestion

### 3.1 Two passes — NORMATIVE

**Pass 1 (count).** Scan the recording; per edge, count samples and record `[t_min, t_max]`; collect frame names; detect edge kind (dynamic vs static) and time domain. Output: an exact `ArenaLayout`.

**Pass 2 (fill).** Re-read, group by edge, **sort by stamp within each edge**, then push in order.

Sorting is required because Phase 1 invariant 6 mandates non-decreasing stamps per edge, and a recording routinely violates it: messages are ordered by log time, not header time, and different publishers interleave. Pushing unsorted would produce a storm of `NonMonotonicStamp` rejections and a silently incomplete index.

Memory: buffer per edge, sort, drain. Peak is roughly the dataset size (~233 MB above). **Cap it** at a configurable `--max-memory` (default 4 GiB) and spill to a temporary run-file with a k-way merge beyond that. Most users never hit it; the ones who do have a 4-hour recording and will not accept an OOM.

> **Amendment — the cap is enforced in two mechanisms, not one, and the run file
> is the second choice.**
>
> §3.1 names one mechanism. There are two, because they bound different things:
>
> 1. **Grouping.** Edges are partitioned into sets whose buffers fit the cap
>    together, and the recording is re-read once per set. This handles every
>    recording whose largest *single* edge fits, costs one sequential re-read
>    per group, and leaves no temporary file to leak, to fill a different
>    filesystem, or to leave behind when the process is killed. It is preferred
>    for exactly those reasons.
> 2. **The run file**, for the case grouping cannot subdivide: one edge over the
>    cap on its own. Spill cap-sized sorted runs, then merge.
>
> The merge needed a third step that §3.1 does not mention and that is not
> optional. **A k-way merge holds at least one sample of every run resident, so a
> single-pass merge over more runs than `cap / 64` exceeds the cap by
> construction** — the very promise the run file exists to keep. So runs are
> *reduced* in passes, merging a bounded fan-in at a time into a fresh file,
> until one merge can hold what is left. The regime is reachable at ordinary
> sizes, not only in theory: 2 200 samples at a 1 KiB cap overran it tenfold
> before the reduce pass existed, which is how the omission was found.
>
> Ties break by run index and runs are merged in contiguous windows, so §3.2's
> "last occurrence in the recording wins" survives being cut into runs and
> re-merged across passes. **The "across passes" half is now a test**
> (`a_reduce_pass_keeps_the_last_occurrence`) and was, for one revision, only
> this paragraph: two edits that inverted the run order inside a reduce window —
> reversing the span list before chunking it, and reversing a chunk on its way
> into the merge — resolved a duplicate to the second-to-last value and left
> every other test in the workspace passing.
>
> **`--max-memory` still bounds the sort buffers and not the arena.** Nothing
> here changes that, and the row in `ingest::fill`'s doc comment and
> `tests/memory.rs` still say so. One further exclusion is now named rather than
> left implicit: the spill path's **run index** — sixteen bytes per sorted run —
> is not bounded by the cap either, because the run count rises as the cap falls.
> It crosses the cap itself at about `cap² / 2048` samples: never at the 4 GiB
> default, and at 6 880 B against a 1 KiB cap in `a_tiny_cap_reduces_in_several_passes`.
> It is measured and reported as `FillStats::peak_run_index_bytes`, beside
> `spilled_bytes` and for the same reason — a cap that quietly excludes an
> allocation stops meaning anything.

### 3.2 Anomalies, all of which occur in real recordings

| Anomaly | Handling |
|---|---|
| Duplicate `(edge, stamp)` | Last wins (Phase 1 invariant 6). Count and report. |
| Stamps far in the future | Warn with the count and the worst offset; keep. |
| Zero stamps (`t == 0`) | Extremely common from misconfigured publishers. Drop, count, report loudly. |
| Backward clock jump | Split into segments; `--on-clock-reset={split,halt}`. `split` produces multiple `.tft` files. |
| Static edge with differing values | Authority policy from Phase 4 §5.7; report both values. |
| Frame declared, never published | Kept in the tree, flagged by `doctor`. |
| Edge kind changes mid-recording | Hard error naming the timestamp. |

The ingest report is a first-class output, not log noise: emit it as JSON alongside the `.tft` and summarize it to the terminal. **For many users the ingest report will be the first thing `tf_tree` ever tells them about their data, and it should be worth reading.**

> **Amendment — frames and edges are declared in canonical order, not
> first-seen order, and §11 is the reason.**
>
> §11 requires that shuffling a recording's messages produce an identical
> result. Declaring frames and edges as they are first encountered **cannot**
> satisfy that: ids are assigned in declaration order, so two ingests of the same
> transforms in a different order produce arenas whose `FrameId`s and `EdgeId`s —
> and therefore whose ring offsets, topology block and
> `LookupError::Extrapolation { edge }` — disagree. The values matched; the
> identities did not, and the shuffle test found it on the first run.
>
> Declaration is therefore sorted by name. The arena becomes a pure function of
> the recording's *content*, and the ingest report becomes diffable between runs,
> which is worth having on its own.

> **Amendment — the clock-reset threshold answers a different question offline
> than online, and a shuffled file is not a recording.**
>
> `tf_tree_bridge`'s `ClockGuard` is reused so a recording and a live system draw
> the same line, but one rule is inverted: a backward stamp is **dropped online
> and kept offline**. Online the ring cannot accept it (invariant 6); offline
> §3.1 sorts, so discarding it would throw away exactly what the sort exists to
> recover. Backward jumps below the threshold are counted as `out_of_order` and
> kept.
>
> The threshold itself is only meaningful because a recording is written in log
> order, so its stamp inversions are milliseconds. §11's shuffle test destroys
> that property by construction and must raise the threshold to run at all —
> which is a fact about the test, not a workaround: a default that admitted a
> whole-recording inversion would miss every real reset.

> **Amendment — the guard is per edge, and a merged `/tf` stream is not a
> clock.**
>
> The paragraph above is right that the *threshold* is shared with the bridge and
> wrong about the *scope*, and the first revision implemented the wrong one: one
> `ClockGuard` over every edge on every topic. That halts at the defaults on the
> most ordinary `/tf` topology there is. A localization node stamps `map -> odom`
> at the scan it processed and publishes hundreds of milliseconds later, while
> `odom -> base_link` is stamped as it is published; both are correct, and their
> stamps interleave by the slower pipeline's latency, which is above the 100 ms
> threshold. A recording of two robots on `/robot1/tf` and `/robot2/tf` — which
> §3.3 explicitly asks be read — fails the same way, by however far their clocks
> differ.
>
> There is no threshold that fixes this, because the quantity being measured is
> not a clock jump: it is the difference between two publishers' latencies, and
> it is unbounded. So the guard is **one per edge**. That is also the only
> monotonicity with a meaning here — §3.1 sorts per edge, and Phase 1 invariant 6
> is a per-edge rule — and it does not weaken the check it exists for: a bag loop
> or a sim reset moves `/clock` itself, so every edge regresses at once.
>
> The error consequently **names the edge**. The old one could not, and said
> "clock reset" about something that was not one.

> **Amendment — `--on-clock-reset=split` stays refused, and this is the
> decision, not the backlog.**
>
> The table above says `split` "produces multiple `.tft` files". Implementing it
> was considered against implementing §3.1's spill file, and only the spill file
> was built. The argument, so nobody has to have it twice:
>
> 1. **The output type changes, everywhere.** Everything downstream of an ingest
>    takes *one* path and yields *one* arena: `--out` is a path, §2.3's container
>    holds one arena, §4.1's `open_file()` returns one `Tree`, a `Plan`'s span is
>    that file's span, `tf_tree top` attaches to one, and §2.3's `source_digest`
>    identifies one recording. `split` makes an ingest produce *N* of those from
>    one input, which turns `--out` into a filename template, the ingest report
>    into a set, and "the index for this bag" into a concept every consumer has
>    to learn. That is a large, permanent surface change.
> 2. **Nothing downstream can be given a segment set to hold.** The natural
>    answer — one `.tft` carrying N segments — is not available: an arena has one
>    time axis per edge (Phase 1 invariant 6) and after a reset the segments'
>    stamps *overlap*, which is precisely why splitting is needed at all. So the
>    segments cannot share an arena, and there is no other container.
> 3. **The user almost never wants N of them.** A backward jump in a recording is
>    a bag loop, a sim reset, or two recordings concatenated. In every one of
>    those the question is about a single stretch of time; the others are noise.
>    `halt` already reports the edge, the stamp, and the magnitude, which is the
>    one number needed to cut the recording — with `mcap filter`, `ros2 bag
>    convert`, or the recorder's own tooling — and ingest the part that matters.
>    The split we would build is a worse version of a cut the user can already
>    make, with a filename convention they did not choose.
> 4. **It is not a lot of code, and that is not the cost.** The counting pass
>    already detects the reset per edge; segmenting it is mechanical. The cost is
>    the API surface in (1) and a format concept in (2), both of which are
>    permanent, against a workflow that is already expressible.
>
> So the variant remains spelled — `--on-clock-reset=split` is accepted by the
> parser and refused with a reason, rather than rejected as an unknown value,
> because a user reading this table must be able to tell "I typed the name wrong"
> from "this is not built". `IngestError::ClockResetSplitUnsupported` and the
> CLI's message both now point at *this* amendment rather than at §0.0's status
> table: it is a decision with an argument, not a row that is waiting to flip.
>
> **What would reopen it:** a user with a recording whose segments they genuinely
> all want indexed, and who cannot cut the source. Nothing in §3 or §9 needs that
> today.

### 3.3 Sources

- **MCAP** — primary. Read `tf2_msgs/msg/TFMessage` via the schema, not by assuming a topic name; support `/tf`, `/tf_static`, and remapped equivalents.
- **rosbag2 sqlite3** — supported, lower priority; convert to MCAP where practical.
- **A running arena** — `tf_tree freeze --from-live --duration 60s` snapshots a live system. Useful for capturing a fault in the field.
- **Python** — `tf_tree.freeze_from_arrays(...)` for users whose poses are not in a bag at all. This is how a non-ROS user gets in the door, and it should exist from day one.

> **Amendment — the rosbag2 sqlite3 source is blocked on the dependency budget,
> and the blocker was measured rather than assumed.**
>
> This is a finding, not a schedule. Reading a `.db3` needs a SQLite reader, and
> every option was checked against the rules this repository already has:
>
> | Candidate | Verdict |
> |---|---|
> | `rusqlite` / `libsqlite3-sys` | Vendors C. `docs/PHASE2.md` §2's no-C-build-step rule already forced `mcap` to `default-features = false`, and §0.0's amendment shows what the rule asks for instead: the zstd and lz4 chunk decoders are pure-Rust crates of ours, at a measured ~1.8× per-pass cost on a compressed recording — a price, and a small one, rather than nothing. Taking C here would abandon that position for one source of one format. |
> | `prsqlite` 0.1.0 | Pure Rust, and **the crates.io index records no licence at all**. `deny.toml`'s `[licenses] allow` list is an allow-list, so `cargo deny check` — part of `just lint` — refuses it outright. Not a judgement call. |
> | `sqlite-rs` 0.3.7 / `sq3_parser` 0.3.3 | Pure Rust, MIT, same author. Both are file-*header* and pager-level parsers: `sqlite-rs`'s public surface is `header`/`io`/`pager` with one undocumented `SqliteConnection`, at 20 % documentation coverage, and `sq3_parser` ships a feature spelled `defaulft`. Neither demonstrates table-row or BLOB iteration, which is the entire job. |
> | Write one here | A read-only b-tree walk plus varint records plus overflow-page chains for a `/tf` message larger than a page — a file format reverse-implemented against fixtures we generate ourselves, in a crate whose reason to exist is *not* reimplementing databases. |
>
> **So it stays absent**, and §3.3's own next clause is the remedy: "convert to
> MCAP where practical". `ros2 bag convert` does it in one command, and the
> container this repository already builds (§0.0) carries both
> `librosbag2_storage_sqlite3.so` and `librosbag2_storage_mcap.so`, so the
> conversion needs nothing new either.
>
> What *is* built is the diagnosis. `tf_tree_ingest::source::is_sqlite` looks at
> the sixteen magic bytes and returns `IngestError::Rosbag2Sqlite`, and the CLI
> prints the conversion command — because the failure this closes is not "we do
> not read `.db3`", it is that handing one to `tf_tree ingest` used to report
> "the file is not a well-formed MCAP recording", which is true and sends the
> user looking for corruption in an intact file. The fixture is a real SQLite
> database with rosbag2's schema (`testdata/rosbag2/`), so the day a reader lands
> it already has the shape it needs.
>
> **What would reopen it:** a pure-Rust, permissively licensed SQLite reader that
> can iterate a table's rows and read BLOBs, at a maturity worth depending on.

---

## 4. Offline Python API

### 4.1 Identical to online — NORMATIVE

```python
ds = tf_tree.open_file("run.tft")            # mmap, microseconds
plan = ds.plan("map", "lidar")
Ts = plan.at(stamps)                          # the same call as online
```

`plan`, `at`, `at_into`, `adaptive`, `latest` — the same objects, the same semantics, the same bit-exact results. A user who learned the online API knows this one. **Do not introduce a parallel offline API**; the point of the frozen arena is that there is nothing to introduce.

### 4.2 Dataset helpers

Additions that only make sense when the whole timeline is present:

```python
ds.span("map", "lidar")            # (t0, t1) over which the plan is answerable
ds.edges()                         # per-edge: rate, jitter, gaps, count, span
ds.gaps("odom", "base_link", threshold_ns=50_000_000)
ds.resample("map", "lidar", t0, t1, hz=100)      # uniform grid, vectorized
ds.manifest                        # source path, digest, ingest options, versions
```

`span` is `LatestCommon` generalized to a range: the interval over which *every* dynamic edge on the plan has data. It is the single most useful offline query, because "why did my lookup fail at t" is nearly always "one edge on the path had not started yet."

> **Amendment — one of these five shipped, and the other four are decisions
> rather than a backlog.**
>
> `span` is there, on `Tree`, so `ds.span("map", "lidar")` is spelled exactly as
> above and works on a live tree too (it reads retained windows, which a live
> arena also has). It returns three distinguishable things, because collapsing
> them loses the answer the caller acts on: `(t0, t1)`; `(t0, t1)` with
> `t0 > t1` when the windows do not overlap — an empty intersection is a real
> answer, not an error; and `None` when every step is static and the plan
> therefore answers at *any* stamp. An edge that has never published raises
> `NoDataError` **naming the edge's two frames**, which is the case §4.2's own
> sentence is about — an `EdgeId` would not be, because the Python surface has no
> way to turn one back into the names the caller typed.
>
> **The arithmetic is `tf_tree_core::Plan::span`, not the binding.** It was
> written in `tf_tree_py` first, and that was wrong twice over. It put a copy of
> the retained-window intersection in the one crate `just test`, `just miri` and
> `just loom` never build — the same mistake §2.3's `manifest` amendment argues
> against in this repository's own words, since the definition of a ring's
> readable window has already changed once. And it walked `ArenaView` directly
> instead of a `Guard`, so it answered where `Plan::at` refuses: from a stale plan
> after a re-parent, and from a fork-poisoned child. `Plan::span` takes a `Guard`
> and calls `check_generation`, so `TopologyChanged` and `ChildDetached` now reach
> the caller; the binding is a forwarder that only re-labels `NoData`. `span` is
> consequently available to the Rust facade, the CLI and the C ABI as well, and
> the branch it could not previously reach — a path with a folded `Step::Static`,
> which no `tf_tree.build` tree can contain — is covered in
> `crates/tf_tree/tests/behavior.rs`.
>
> `resample` is not a binding: it is `plan.at(np.arange(t0, t1, 10**9 // hz))`,
> one line of NumPy over the vectorized call §4.1 insists is the same call. A
> second spelling of an existing path is what §4.1 forbids.
>
> `edges()`, `gaps()` and `manifest` are **not implemented**, and shipping them
> off what is available today would answer a different question than their names
> promise. Per-edge rate and jitter need §3's counting pass: the ring knows what
> it *retained*, not what the source produced, and dividing the one by the other
> is precisely the 4-kHz-off-a-1-kHz-edge error that §2.3's
> `samples`/`pushes_total` amendment already had to correct once. `manifest`
> needs a CBOR *reader*, where the crate has only a writer.

### 4.4 Three API-contract deltas that land here — NORMATIVE

[`API.md`](./API.md) §6 rows 7, 8 and 9. They are grouped here because this is
the phase that opens the Python module anyway; none of them is a new idea and
each is a gap between Python and a surface that already has the feature.

**1. `Layout::QuatTwist` — derivatives reach Python and C (`API.md` §3.3).**
Rust and C have had `at_with_derivatives` since Phase 4
(`tft_plan_at_with_derivatives`, unstable tier); Python was scoped out by
`PHASE4.md` §0 and has had no path to a twist since. It ships as a **fourth
`Layout` variant**, not a fourth method: a contiguous `(N, 13)` write of
`[qw qx qy qz tx ty tz | ωx ωy ωz vx vy vz]`, carried to both bindings by the
layout dispatch that already exists. A separate `at_d` would need its own GIL
threshold (§6.1), its own buffer validation and its own tests, for the same
bytes. `LerpSlerp` returns `DerivativesUnavailable` here exactly as it does from
`at_with_derivatives` — a layout that quietly changed meaning per interpolator
would be the quaternion-order trap in the time axis. On the C side this is one
new `tft_layout` enumerator and therefore a **minor** ABI bump
(`PHASE4.md` §3.6).

> **Status: done, on all three surfaces.** `Layout::QuatTwist` is in
> `tf_tree_core` and `Plan::at_many_into` serves it;
> `TFT_LAYOUT_QVEC7_WXYZ_TWIST6` is in the frozen header, accepted by
> `tft_plan_at` and `tft_plan_at_many`, and reachable by type from the C++
> wrapper as `layout_of<Quat7Twist6>`. **`tf_tree_py` now takes a keyword-only
> `layout=`** on `at` and `at_into` — `"mat4"`, `"quat"`, `"affine32"`,
> `"quat_twist"` — with its own buffer validation and the §6.1 threshold, and
> `LerpSlerp` raises a typed `DerivativesUnavailableError` rather than a finite
> difference.
>
> **Two things the paragraph above did not anticipate.** A twist row is more
> work per element than a pose row (measured at ~1.1x *relative to a pose row*),
> so §6.1's estimate under-shoots it — but the same measurement shows the
> estimate already under-shoots the **pose** row by ~2x, which is
> [`API.md`](./API.md) §3.4's known `NS_PER_STEP_ESTIMATE` error seen from the
> other side. The estimate is therefore deliberately *not* given a
> layout-dependent multiplier: it would correct the smaller of the two errors and
> leave the larger, both sides of the threshold stay far below CPython's switch
> interval either way, and §3.4 is NORMATIVE that `NS_PER_STEP_ESTIMATE` is
> re-derived from `0013`'s re-baseline rather than guessed at. And
> **`tf_tree.build` hard-coded `LerpSlerp`**, so no Python-constructed tree could
> answer a twist at all — `build` and `open(create=...)` now take `interp=`,
> spelled as §4.1's own layout sketch spells it, and **the default moved to
> `"sclerp"`**: `PROJECT.md` §5 D5 forbids making `LerpSlerp` the default without
> a measurement justifying it, and none was ever taken. `API.md` §3 records the
> now-closed divergence.

**2. Introspection: `tree.frames()`, `tree.edges()`, `plan.edges()`
(`API.md` §3.2).** A notebook user currently shells out to the CLI to see what
is in an arena, and this is the phase whose users live in notebooks. These are
tier-1/tier-2 calls, so R2 is not in tension. `plan.depth()` and `tree.span()`
already ship. **`tree.edges()` here is the *names* half only** — the identities
of the edges on a tree — and is a different thing from §4.2's `ds.edges()`,
which promises per-edge rate, jitter and gaps and stays held back until §3's
counting pass exists. Ship the names; do not let them acquire statistics by
adjacency, because a rate computed from a ring is the error §4.2 just finished
refusing.

**3. Exact stamp converters: `from_parts` / `from_timespec` / `from_ros`
(`API.md` §5.1).** `from_sec` exists and carries a lossy-above-10⁷-seconds
warning, and today that warning points nowhere. What users resent is writing
`stamp.sec * 10**9 + stamp.nanosec` in every node; the fix is an exact, total
converter on every surface, none of which takes a float. `tf_tree.from_ros`
converts a `builtin_interfaces/Time` exactly and **never** via `to_sec()`.
`from_sec` stays, keeps its warning, and stays out of every example.

> **Status: done, and the shape is settled by `API.md` §5.1's amendment.** All
> three are `Option`/refusal-returning rather than normalising or wrapping, and
> the refusals are the half worth checking: `tests/python/test_api.py`'s
> `PARTS_TABLE` and `crates/tf_tree_c/tests/abi.rs`'s `PARTS_TABLE` are the same
> ten rows, asserted independently on each side, because a converter that agrees
> with Rust in the middle and disagrees at `i64::MIN` is invisible to any test
> that only checks the middle.
>
> `from_sec` gained the one thing that makes its warning actionable — a sibling
> to name — and its docstring now names both. **Python has no `from_timespec`**:
> `time.clock_gettime_ns()` is already integer nanoseconds, so there is no
> `timespec` on that surface to convert *from*, and adding one would be an entry
> point with no caller. C has both, because a C caller genuinely holds a
> `struct timespec`.

### 4.3 The dataloader pattern

Document it, do not ship a class. A `torch.utils.data.Dataset` subclass would bind us to a framework version for no benefit; the pattern is four lines:

```python
class Frames(Dataset):
    def __init__(self, path): self.path = path; self.ds = None
    def __getitem__(self, i):
        if self.ds is None: self.ds = tf_tree.open_file(self.path)   # per-worker, post-fork
        ...
```

The lazy open matters: it must happen **after** fork, because Phase 3's `register_at_fork` poisoning applies here too. Say so in the docstring with the reason, not just the rule.

> **Amendment — the rule is right and the reason given for it is wrong.**
>
> Phase 3's fork poisoning does **not** apply to a `.tft`. `Tree::from_frozen`
> goes through `fork_gen_for`, which returns `None` for `ArenaBacking::Frozen`
> deliberately: the mapping is `MAP_PRIVATE | PROT_READ` and is not
> `MADV_DONTFORK`, so a child inherits it intact and every offset into it stays
> valid — poisoning it would break `multiprocessing` for offline users to defend
> against a hazard they do not have. `tests/python/test_frozen.py` forks and
> queries the inherited mapping to keep that honest.
>
> The lazy open survives for a different reason, and it is the one the docstring
> now gives: **a `Tree` cannot be pickled**, and a `DataLoader` with
> `num_workers > 0` sends the dataset object to its workers by pickle under
> `spawn` *and* under `forkserver` — which is CPython 3.14's default start
> method on Linux, so this is the common case and not the exotic one. The lazy
> `None` is what keeps the object picklable. Opening per worker is also what
> §2.2's page-sharing argument depends on.

---

## 5. Diagnostic counters

### 5.1 "Telemetry" is the wrong word — NORMATIVE

Rename it everywhere: **counters**, or **diagnostic counters**. Never "telemetry", in the code, the docs, the CLI, or the changelog.

> **Amendment — there is nothing to rename; this is an enforcement item.**
>
> A case-insensitive grep for `telemetr` across the entire repository returns
> **zero hits in code** — no Rust, no Python, no CLI string, no `Cargo.toml`.
> The only occurrences are inside this document and `PHASE4.md` §3.1's
> unstable-header table, i.e. in the specs that introduce the prohibition. The
> `EdgeTelemetry` named in §1.3 above is a hypothetical design being rejected,
> not an existing type.
>
> So the deliverable is **a CI check that keeps the word out**, not a rename
> pass — and the two spec occurrences should be fixed as part of it, since a
> NORMATIVE rule violated by its own document is not a rule anyone will enforce.

In 2026 "telemetry" means the software phones home. A robotics team evaluating a library whose documentation says "telemetry" will assume network egress, and some will block it at procurement without reading further. Nothing here leaves the machine — these are counters in shared memory on the same host — and the name should say so.

Pair the rename with a stronger, testable claim:

**The `tf_tree` *library* opens no network sockets. Ever.** The only socket in the entire library is the Phase 2 `AF_UNIX` rendezvous socket, which is a filesystem path on the local host. Phase 8 replication will add network transport, and when it does it will be an explicitly enabled, separately named component.

**`tf_tree` is also the name of the shipped binary, and the binary has exactly one exception: `tf_tree top --web`.** It binds an `AF_INET` listener, only when an operator types the flag, loopback unless they name another address — see §7's amendment. Nothing a program links can reach it. The headline above is scoped to the library because that is the claim a team evaluating the crate is making a procurement decision about; stating it without the scope would leave the one exception to be discovered rather than read.

**NORMATIVE CI test:** run the full test suite under `strace` (or a seccomp filter) and assert that `socket(2)` is called only with `AF_UNIX`. A promise in a README is worth less than an assertion in CI, and for a library that ships onto robots this is worth asserting.

### 5.2 The two kinds of counter have opposite cost profiles

An earlier draft offered a single `TF_TREE_TELEMETRY=0` runtime opt-out. That was wrong, because it lumped together two things that should be decided separately:

| | Cost | Value |
|---|---|---|
| **Error-path counters** — extrapolation, no-data, recycled, contended | **Zero.** The branch is already taken and an error object is already being constructed. | Irreplaceable. You look at these *after* something went wrong at 3am on Tuesday. |
| **Success denominator** — `lookups_ok` | An atomic increment on the hottest read path, on a per-edge line shared by every reader. | Convenience. It turns an error *count* into an error *rate*. |

Only the second was ever expensive, and it is the less valuable of the two.

### 5.3 Error-path counters are always on, with no runtime switch — NORMATIVE

**Diagnostics that are off by default are diagnostics that do not exist when you need them.** A robot runs for weeks; the failure you care about happened once, unattended. If enabling the counter requires a restart, the incident is already gone — and you will be asked to reproduce it.

They cost nothing, they cannot affect a lookup result, and they are the entire basis of checks `TFT010` and `TFT011`. No environment variable, no runtime flag.

### 5.4 The denominator batches in the `Guard`, so it is also free

The atomic increment is avoidable entirely. A `Guard` already exists, is already per-thread and scoped, and already spans a batch of lookups — so accumulate in it and flush once on `Drop`:

```rust
pub struct Guard<'a> {
    // ...existing fields...
    ok: Cell<u32>,          // plain, non-atomic; Guard is !Sync by construction
}
// Drop: if ok > 0 { edge_counters.lookups_ok.fetch_add(ok, Relaxed) }
```

A `Guard` spanning 1000 lookups pays one relaxed atomic per 1000, per thread — contention is gone and the per-lookup cost is a non-atomic increment on a line already in L1.

**NORMATIVE:** the convenience path (`tree.lookup(...)`, which currently constructs a `Guard` per call) must hold a **long-lived per-thread `Guard`** alongside its existing per-thread plan cache. Otherwise it flushes on every call and reintroduces exactly the contention this design removes. This also amortizes the epoch validation the `Guard` already performs, so it is a win independent of counters.

With that, there is nothing left worth switching off at runtime, and the opt-out is deleted.

### 5.5 The one switch that should exist is compile-time

For a build under safety certification, or a minimal embedded target, the correct knob is not a runtime flag — a runtime flag implies a runtime branch and leaves the code in the binary. It is a **default-on cargo feature**:

```toml
[features]
default = ["counters"]
counters = []
```

Disabling it removes the fields, the increments, and the arena regions' *use* (the regions remain, per D34, so the layout hash does not fork). What executes is then provably nothing, which is what that audience actually needs.

> **A read-only participant keeps no counters, and this section does not say
> so.** D18 makes a consumer's attachment read-only — the MMU is what stops it
> corrupting a robot's transform tree — so *any* write from a read path faults.
> The `Guard` flush is a write from a read path, and it killed a read-only child
> in the multiprocess suite with `SIGSEGV` before the check existed.
>
> The resolution: a read-only participant silently records nothing. It cannot,
> and refusing to run would be far worse than losing a diagnostic. `doctor`
> reports what the *writable* participants recorded, which on a real robot is
> the bridge and every publisher — the processes whose failures the counters are
> mostly about. `ArenaView` carries a `writable` flag, default `false`, so
> forgetting to opt in loses a counter and forgetting to opt out cannot crash
> anything.

### 5.6 Counters are captured in snapshots

Arena counters die with the arena. On a robot, the interesting question is usually "what did the counters say just before this went wrong", so:

**NORMATIVE:** `tf_tree freeze --from-live` copies the counter regions into the `.tft`, and `doctor --json` output is timestamped and appendable. A field snapshot then carries the diagnosis with it, rather than requiring the fault to be reproduced on a bench.

> **Amendment — "into the manifest" was wrong, and §2.1 is why.**
>
> This section originally said the counters go into the `.tft` *manifest*. They
> do not, and should not. §2.1 makes the frozen file an arena image, so freezing
> copies the whole arena — `ArenaLayout::edge_counters()` and
> `participant_counters()` land at their own offsets and are read back through
> the identical `ArenaView::edge_counters` accessor a live arena uses.
>
> That is a stronger guarantee than the original wording asked for. There is no
> code path that can *forget* to copy the counters, because there is no code that
> copies them specifically. And it avoids a second source of truth: a manifest
> copy would be a snapshot of a snapshot, and the first time the two disagreed
> nobody would know which to believe.
>
> The manifest keeps what the arena cannot hold — the source path, the recording
> digest, the ingest options.

### 5.7 What must still be measured

Publish the cost of the non-atomic `Guard` increment, and confirm under sixteen concurrent readers that the flush-on-drop pattern shows no measurable contention. If it somehow does, shard by participant slot (`counters[edge][slot & 7]`) and sum on read — but measure before adding that complexity.

> **Measured.** `cargo run --release -p tf_tree_bench --bin counter_cost`, on
> this host (4 physical cores + SMT). Three runs each, `ns/lookup/thread`:
>
> | threads | 1 | 2 | 4 | 8 |
> |---|---|---|---|---|
> | counters on | 21.1–21.5 | 21.4–22.6 | 22.0–22.8 | 38.0–40.2 |
> | counters off | 18.5–18.9 | 18.5–20.1 | 19.1–19.6 | 34.1–36.8 |
> | ratio | 1.13× | 1.14× | 1.16× | 1.11× |
>
> **The counters cost about 2.6 ns per lookup, and the ratio is flat.** Those
> are two separate findings and only the second answers §5.7's question. A
> constant overhead that does not grow from one thread to eight is not
> contention: the flush is one relaxed atomic per *batch*, so eight threads make
> eight of them rather than eight thousand. **§5.7's sharding fallback is
> therefore not justified** — it would address a cost that is not there.
>
> The 2.6 ns is the `Cell` increment, the `first_dynamic_edge` walk over the
> plan's steps, and the branch on `is_writable`, on a ~19 ns lookup. It is a real
> 14 % and it is what §5.5's compile-time switch exists to remove for a build
> that cannot pay it.
>
> **An earlier revision of this section reported "no measurable contention" and
> an overlap at every row. That measurement was invalid**, and the reason is
> worth recording because nothing in the output showed it: the workspace's
> `tf_tree_core` dependency did not set `default-features = false`, so a
> downstream `--no-default-features` still enabled `counters` and the "control"
> build was the counters-on engine measured twice. The banner printed `OFF` — it
> reads the *bench crate's* features — while the engine counted. Both the
> dependency declaration and the banner are fixed; the numbers above are from
> builds verified with `cargo tree -e features`.
>
> The 16-thread row is still not quoted: it is 2× oversubscribed here, and a
> scheduling artifact answering §5.7's question would be the wrong number.
>
> Batched against per-lookup flushing is **+5.8 ns** *within* the counters-on
> build — an upper bound on the atomic §5.4 removes, since it includes the
> guard's own construction.

---

## 6. The diagnostics catalogue

`tf_tree doctor` exists from Phases 1–2. Phase 5 makes it the product. Each check has a stable identifier so it can be suppressed, tested, and referenced from documentation.

| ID | Check | Severity | Detection |
|---|---|---|---|
| `TFT001` | Multi-publisher conflict on an edge | error | Phase 4 §5.4 counters, or two claim attempts |
| `TFT002` | Static transform republished with a different value | error | ingest / bridge comparison |
| `TFT003` | Edge kind changed (static ↔ dynamic) | error | edge record vs incoming |
| `TFT004` | Clock skew between publishers | warn | per-publisher stamp vs arena receipt time |
| `TFT005` | Stamps in the future | warn | newest stamp vs now, per edge |
| `TFT006` | Zero or absurd stamps | error | value check during ingest and push |
| `TFT007` | Publish rate deviates from nominal | warn | derived from stamps vs `nominal_rate_mhz` |
| `TFT008` | Jitter: p99 inter-arrival ≫ nominal | warn | derived from stamps |
| `TFT009` | Gaps / dropouts | warn | derived from stamps |
| `TFT010` | Extrapolation hotspot | warn | `EdgeCounters` + participant attribution |
| `TFT011` | Ring capacity too small for observed consumer lag | warn | worst extrapolation gap vs buffer span |
| `TFT012` | Disconnected subtree | error | topology walk |
| `TFT013` | Frame declared but never published | info | head == 0 after a grace period |
| `TFT014` | Participant or claim slot leak | warn | Phase 2 lock file vs arena records |
| `TFT015` | Arena occupancy > 80% (frames, edges, participants) | warn | header counters |
| `TFT016` | THP disabled, or `RLIMIT_MEMLOCK` below arena size | info | `/sys`, `getrlimit` |
| `TFT017` | Dynamic edge with no live writer | warn | claim table (added by the amendment below) |
| `TFT018` | Stamps arriving out of monotonic order | error | observed push stream (added by the amendment below) |
| `TFT019` | A wall-clock domain stepped backwards — `TFT018`'s cause, not a publisher fault | warn | `TFT018`'s evidence + the edge's domain tag (added by the amendment below) |

Output modes: human (default, coloured, grouped by severity), `--json` (stable schema, for CI), and `--exit-code` (non-zero if any error-severity check fires) so `doctor` can gate a robot's startup or a CI job.

**`TFT004` deserves special care** — it is the check most likely to find something nobody knew. Compute per-publisher offset between header stamp and arena receipt time, track a rolling median, and report publishers whose median differs from the fleet median by more than a threshold. On a multi-machine robot with imperfect PTP this finds real problems that present as intermittent extrapolation errors.

> **Amendment — `TFT007` is no longer structurally blind. The declared rate
> comes from the topology file, and no arena field was added to get it there.**
>
> §0.0 recorded `TFT007` among the four ids that "cannot detect anything in any
> configuration", because §1.2's `EdgeRecord::nominal_rate_mhz` was reserved and
> never written: every edge read 0, and comparing an observed rate against zero
> fabricates a finding on every edge of a correct arena.
>
> The missing piece was a *declaration*, and Phase 4 had already shipped one.
> `TopologyConfig`'s `rate_hz` — which an operator writes, or
> `tf_tree topology --discover` measures and writes for them — was consumed only
> to size the ring. It is now also carried into the arena:
>
> ```text
> topology.toml rate_hz -> TopologyConfig::builder -> EdgeCfg::nominal_rate_hz
>                       -> TreeBuilder::build -> EdgeRecord::nominal_rate_mhz
> ```
>
> **`FORMAT_VERSION` stays 3 and `layout_hash` does not move**: the field already
> existed, at the offset §1.2 gave it, and this fills it. That is the whole
> reason this was cheap — the expensive half was paid by §1's one deliberate
> break, and §1.2's instruction not to add arena fields opportunistically is
> honoured by adding none.
>
> Four properties of the detection are worth stating, because each is a way it
> could have been wrong:
>
> * **`0` means undeclared, not 0 Hz.** An edge sized by `capacity = N` states no
>   rate and is not compared. When *no* edge declares one, the whole check skips
>   and says which knob supplies the evidence.
> * **The observed rate is the median inter-arrival**, the same statistic the
>   `edges` column prints, so the number an operator sees and the number the
>   check judges cannot differ.
> * **Both directions fire.** Slow is the obvious fault; fast is the quiet one —
>   a ring sized from `rate_hz * history_secs` retains proportionally less
>   history than every consumer was tuned against.
> * **A partial run says so.** `Status` is three-valued and none of them is
>   "ran, half blind", so when some edges declare and others do not, the coverage
>   is disclosed in `Meta.notes` — the same mechanism `TFT015`'s missing
>   participants row uses. **A run that compared *nothing* does not say `pass`**:
>   an arena where edges declare a rate but none has retained enough intervals
>   to measure one — `doctor` seconds after bringup, mid publisher restart, or
>   on an edge whose publisher has stopped dead — **skips**, with a reason
>   naming the missing stream rather than the missing declaration. A `pass`
>   there is the same fabricated assurance as comparing against zero, arrived at
>   from the other side.
>
> **A measured rate is not a declared rate, and `--discover` produces the first
> while the arena reads it as the second.** Before this amendment `rate_hz` was
> a sizing hint and the distinction cost nothing; it is now the statement
> `TFT007` judges the robot against, and the two sources mean opposite things —
> one is intent, one is observation. An integrator who records `/tf` while a
> node is degraded at 12 Hz, runs `tf_tree topology --discover`, and ships the
> result has declared 12 Hz: `doctor` will certify the fault as nominal and fire
> `TFT007` when the publisher is *repaired*. Nothing in the tool can tell the
> two apart, so this is stated rather than solved: **a discovered `rate_hz` is a
> starting point to review, not a declaration.** `--discover` prints each edge's
> sample count to stderr for exactly this review, and an edge whose rate could
> not be measured is emitted as `capacity = N` — declaring nothing — rather than
> as an invented rate.
>
> What this does **not** do: the reference fixture (`tf_tree_bench::fixture`)
> sizes its rings by slot count and declares no rate, so `tf_tree doctor` on the
> fixture still reports `TFT007` as not run. The reason it prints is now about
> *that arena* rather than about the system, which is the whole difference
> between an evidence skip and a capability skip. An arena built from a topology
> file with `rate_hz` — which is what the ROS 2 bridge builds — runs it.

> **Amendment — the catalogue runs to `TFT018`. The two Phase 1 checks that had
> no id have one, and the decision is made rather than deferred.**
>
> §6's table listed sixteen ids and the seven Phase 1 checks mapped onto only
> five of them. `unclaimed-dynamic` and `out-of-order` were reported *id-less*:
> visible, still gating, and explicitly marked as having no identifier, because
> forcing them into `TFT013` or `TFT006` would give an existing id a second
> meaning and inventing new ones was an amendment nobody had made. This is that
> amendment.
>
> **They are appended, not folded in.** The three candidates are each a different
> condition: `TFT013` is *declared and never published to*, which is not
> *published and then abandoned*; `TFT014` is a claim held by a slot whose owner
> is gone, which is not *no claim at all*; `TFT006` judges a stamp's **value**,
> and a stream of entirely plausible stamps can still arrive backwards.
>
> **Appending is additive; renumbering would be a break.** The ids are a public
> contract — `--suppress` takes them, `--json` emits them, a runbook cites them.
> `TFT017`/`TFT018` add two spellings to `--suppress` and two entries to an array
> `--json` consumers already iterate. No existing id changes meaning, and none is
> ever recycled.
>
> **What it buys, concretely.** `unclaimed-dynamic` fires on every dynamic edge
> of an arena whose writers are not attached — a bag-ingested arena, or any
> moment during a publisher restart — and until now there was no way to silence
> it, because `--suppress` had nothing to name. And `out-of-order` was *silently
> not run* on a live arena; it is now a stated skip, with the reason: the live
> push stream is reconstructed from a ring that is being written while it is
> read, so a slot at the old end of the window can already hold the next lap's
> sample, which reads as an inversion on a correctly ordered publisher.
>
> **Severity is preserved exactly** — `TFT017` warn, `TFT018` error — so
> `doctor --exit-code` fails on exactly what it failed on before. That value is
> now stated in two places (the check and the id), so a test compares them:
> `checks::tests::the_two_new_ids_keep_their_phase_1_severities`.
>
> The `uncatalogued` array stays in the `--json` schema with no producer. It is a
> stable key, and it is the shape any future check without an id would take.

> **Amendment — `TFT019`: `CLOCK_REALTIME` is not monotone, and the failure
> reads like our bug.** ([`API.md`](./API.md) §5.3.)
>
> NTP steps and leap seconds move `CLOCK_REALTIME` backwards. `PHASE1.md` §2
> invariant 6 requires per-edge non-decreasing stamps, so a clock step surfaces
> as a **burst of `NonMonotonicStamp` rejections** — entirely correct behaviour
> that reads as a `tf_tree` defect to whoever meets it at 3 a.m., and that
> `TFT018` reports, accurately and unhelpfully, as "a publisher restarted
> without resetting its clock".
>
> **`TFT019` is an attribution, not a second detector.** It fires on exactly
> `TFT018`'s evidence plus one more fact the arena already holds — the edge's
> **declared domain tag** (`EdgeRecord::domain`). A run of rejections
> concentrated in a short window, on an edge whose tag is a **wall clock**, is
> reported as a clock step: the publisher is not at fault and restarting it will
> not help. Where `TFT018` says *what*, `TFT019` says *who*.
>
> **`TFT019` fires only on tag 0**, and on any other tag it **skips with a
> reason naming the tag**, exactly as `TFT007` skips an undeclared rate rather
> than comparing against zero. `Domain` is an open trait, so a user-declared tag
> carries no way to state "this clock can step", and guessing that an unknown tag
> is steady would fabricate an all-clear on the one edge most likely to be a PTP
> driver that lost lock.
>
> When this was written the built-in set was two — `SystemDomain` (tag 0) and
> `SensorDomain` (tag 1) — and that made the skip merely *conservative*: with
> only two built-ins a sim deployment and a steady-clock driver both land on tag
> 0 and both get told their clock stepped, which is [`API.md`](./API.md) §2.5's
> warning arriving as a concrete cost.
>
> > **Amendment — the built-ins are now four, and the skip is correct rather
> > than conservative.** `SimDomain` (tag 2) and `SteadyDomain` (tag 3) exist in
> > `tf_tree_core::plan` (`API.md` §2.5). `TFT019` is **unchanged** and still
> > fires only on tag 0, and that is now the right answer rather than the safe
> > one: a `SteadyDomain` edge cannot have stepped, so a run of rejections there
> > is a real publisher defect and reporting it as a clock step would be exactly
> > the fabricated all-clear this check refuses. Teaching `TFT019` that tag 3 is
> > *provably* steady — skipping with "steady domain, this is a real fault" rather
> > than with "unknown tag" — is a refinement it can now make and has not made.
> >
> > The tag mapping is settled (`sim` 2, `steady` 3, permanently) and the
> > bridge's config file now spells it: `tf_tree_bridge::config::parse_domain`
> > maps all four names, so `domain = "sim"` and `domain = "steady"` are things
> > an operator writes rather than numbers they have to look up
> > (`PHASE4.md` §5.5, whose amendment also records the clause of that section
> > which is still open). **Nothing in this check depends on that, but the field
> > does:** tags 2 and 3 were reachable only by an operator who wrote the
> > integer, and are now reachable by one who wrote the obvious word, so the
> > population of arenas `TFT019` skips on a non-zero tag is about to stop being
> > empty. That makes the refinement above — tag 3 is *provably* steady, so say
> > "this is a real fault" rather than "unknown tag" — worth more than it was
> > when it was written, and it is still not made.
>
> Three things it deliberately does not do:
>
> * **It does not fire on a steady or sim tag** — see above; it skips rather
>   than guesses, and since the amendment those two tags exist, so the skip is
>   right for a stated reason: a steady edge cannot have stepped, so a run of
>   rejections there is a real publisher fault and `TFT018` alone is the honest
>   answer. Sim time has its
>   own, much harder version of this question — a `/clock` reset against a
>   publisher's `transform_tolerance` — and it is settled by
>   [`0012`](./decisions/0012-the-authoritative-clock-jump-signal-and-the-degradation-ladder.md)
>   with an **authoritative** `rcl` signal. `TFT019` must not attempt an
>   inference `0012` spent three rules falsifying. This check is the
>   single-process, no-ROS case, where there is no authoritative signal and no
>   second publisher, and a good diagnostic is the only honest response.
> * **It does not demote `TFT018`.** `TFT018` stays an error and keeps failing
>   `doctor --exit-code`; `TFT019` is a warn that explains it. Rejected pushes
>   are lost data whatever caused them.
> * **It does not reuse the bridge's counter.** `dropped_non_monotonic` is a
>   *bridge* counter (`PHASE4.md` §5.5's ladder), not an `EdgeCounters` field, and
>   an arena with no bridge in front of it has none. The evidence is the observed
>   push stream, which is what `TFT018` already reconstructs — including its
>   stated skip on a live arena, which `TFT019` inherits rather than works around.
>
> The catalogue therefore runs to **`TFT019`**, appended by the same rule the
> `TFT017`/`TFT018` amendment establishes: ids are a public contract, appending
> is additive, and none is ever recycled or given a second meaning.
>
> **Paired with a documentation line, not just a check:** anything published at
> rate should declare a steady or PTP domain rather than the system wall clock —
> `SteadyDomain` (tag 3) since the amendment above, or, `Domain` being an open
> trait, a driver's own unit struct and `TAG` for a PTP-disciplined clock
> (`API.md` §2.5). The check tells an operator what happened; the doc line is how
> the next robot avoids it. `RUNBOOK.md`'s `NonMonotonicStamp` section carries
> both.

---

## 7. `tf_tree top`

A live read-only participant. TUI first (`ratatui`), with an embedded static web view behind `--web`.

TUI panes: topology with per-edge rate/staleness/occupancy and writer identity; a participant list with mode, PID, attach time, and failure counts; a rolling diagnostics feed; and a per-edge detail view with an inter-arrival histogram.

**NORMATIVE constraints on the web view:** a single embedded HTML file plus one JSON endpoint, no build step, no npm, no CDN. Charts in hand-written SVG. The moment this needs a frontend toolchain it becomes a maintenance liability that outlives its usefulness, and a small-team infrastructure project cannot afford that.

Bind to loopback by default. Serving robot state on `0.0.0.0` by default would be a security bug in someone's deployment.

> **Amendment — `ratatui` is not used, and the reason is the same one §7 gives
> for the web view.**
>
> This section forbids a frontend toolchain for the web half on the grounds that
> a dependency which outlives its usefulness is a maintenance liability a small
> team cannot afford. That argument does not stop at the browser. `ratatui`
> plus `crossterm` is a transitive tail inside a workspace whose dependency
> budget is a stated hard rule, carried so that four panes of fixed-width text
> can be drawn — and the implementation draws them in about thirty lines of
> `ESC[H` / `ESC[K` / `ESC[J`, in `crates/tf_tree_cli/src/top.rs`.
>
> **What that costs is real.** There is no key handling, because raw mode means
> `termios`, which means `libc`, which is both a dependency and an `unsafe`
> boundary `tf_tree_cli` does not have (`#![forbid(unsafe_code)]`). So the
> per-edge detail view is selected with `--edge <id|name>` rather than by moving
> a cursor, and there is no alternate screen — restoring one on `SIGINT` needs a
> signal handler, and a `top` that wedges an operator's terminal on Ctrl-C is
> worse than one that leaves its last frame in the scrollback. If interactive
> selection is later judged worth a `libc` dependency, that is a decision
> record, not a drive-by `cargo add`.
>
> **Two things §7 does not say, which the implementation had to decide:**
>
> * **Ages are against the reference clock `doctor` uses, decided the same
>   way.** §0.0 already records that an arena's stamps need not share an epoch
>   with the system clock — it is why `TFT005` skips on the reference fixture —
>   so a staleness column computed unconditionally against `SystemTime::now()`
>   reads as decades on a boot-relative arena. The reference is therefore
>   `checks::Clock::decide`: a majority vote of the per-edge newest stamps
>   against the host clock, falling back to the **median** newest stamp when the
>   arena's stamps are in some other domain. Not the *maximum* — that hands the
>   definition of "now" to the single worst publisher, so one
>   nanoseconds-into-a-seconds-field overshoot makes every healthy edge read ~54
>   years stale and the broken one read `0.0`. That failure is the one
>   `checks.rs` was already fixed for, and `top` prints `Clock::label()` in its
>   header so an operator can see that the two tools agreed on a reference. The
>   same applies to a participant's `attached_at_nanos`, which is the *arena's*
>   clock and routinely disagrees with the publishers' stamps; that column shows
>   `epoch?` rather than a negative age.
> * **The "observes without perturbing" claim is a test, not a sentence.**
>   `top::tests::capturing_the_arena_moves_no_counter` reads a populated arena
>   five times and asserts no edge counter moved, with a real lookup afterwards
>   to show the counters it is watching are ones that move. The claim was prose
>   for one revision, and a `tree.lookup` added to `Capture::from_tree` left
>   every other test passing.
> * **Frame names and lock-file `comm` are sanitized before they reach the
>   terminal.** Frame names are arbitrary UTF-8 (`intern_core` validates only
>   the hash) and `comm` is bytes another process wrote; both are interpolated
>   into a full-screen ANSI frame. `catalogue::json_escape` already guards the
>   JSON path against the same input — `top::sanitize` is the ANSI path's half,
>   and it is also what keeps `--color never` producing escape-free text and
>   `{:<30}` producing aligned columns.
> * **The participant pane is the arena table ∪ the lock file.** A read-only
>   participant writes no arena participant record — it cannot, its mapping is
>   `PROT_READ` (D18) — so a pane built from the arena alone shows only the
>   writers, and `top` would be missing from its own output. §5.6's amendment
>   is the same fact from the counters' side.
>
> **Rates are observed and are never presented as a deviation.**
> `nominal_rate_mhz` is always 0 (§0.0, `TFT007`), so `top` shows a
> stamp-derived median rate and a wall-clock-derived head-advance rate side by
> side and compares neither against a declared one.

> **Amendment — the `--web` half: what it is, and the four things §7 does not
> say that the implementation had to decide.**
>
> **No HTTP crate, for the reason this section already gives twice.** §7 forbids
> a frontend toolchain for the page and the `ratatui` amendment extends that to
> the terminal; a server crate is the same argument a third time. `hyper`/`axum`
> pull a `tokio` runtime into a workspace whose `CLAUDE.md` says "no
> `async`/runtime", and `tiny_http` is still a dependency to keep current — to
> answer two routes that serve one constant and one string. `--web` is
> `std::net::TcpListener` with a `std::thread::scope`d thread per connection and
> no keep-alive. The socket half of `crates/tf_tree_cli/src/web.rs` — `bind`,
> `read_head`, `respond`, `handle`, `serve` and their classifiers — is **152
> lines of code**, blank and comment lines excluded; the file as a whole is 391,
> and the remaining 239 are the JSON document, which a server crate would not
> have written either. Both numbers are measured and each says what it counts,
> because "~150 lines" of an unspecified region is the kind of claim that is off
> by two times without anyone noticing. **The dependency budget is unchanged: the
> `--web` commit adds no crate to any manifest.** What that costs is stated in
> `serve`'s own doc — it is not a general-purpose server and must never be
> pointed at a network.
>
> * **This is the only network socket in the repository, and §11's "no network"
>   test must be scoped to say so.** §5.1 is NORMATIVE that "`tf_tree` opens no
>   network sockets. Ever." That sentence is about the *library* and stays
>   literally true — nothing in `tf_tree`, `tf_tree_core`, `tf_tree_arena` or
>   `tf_tree_ipc` can reach this code. The `AF_INET` socket lives in the CLI and
>   exists only when an operator types `--web`. §11's proposed
>   `socket(2)`-is-only-`AF_UNIX` assertion is therefore a claim about the
>   library's suite; a version of it that ran over the CLI would have to encode
>   this exception, and that is a worse test than a narrower one.
> * **Loopback is not a boundary a browser respects, so there is a `Host`
>   guard.** §7 asks for a loopback default and that is necessary but not
>   sufficient: any page the operator visits can `fetch http://127.0.0.1:8787/`,
>   and while CORS makes the response opaque, **DNS rebinding** makes
>   `evil.example` resolve to `127.0.0.1` on its second lookup, at which point
>   the page is same-origin and can read every frame name and pid in the arena.
>   An `Origin` check does not help — a rebound page's origin *is*
>   `evil.example`. The fix is that a loopback bind refuses any request whose
>   `Host` is not a loopback name, missing `Host` included. The guard is scoped
>   to a loopback bind: an operator who typed `--web 0.0.0.0:8787` asked for a
>   reachable server, gets a stderr warning saying what it exposes, and would
>   otherwise have every request refused.
> * **"No CDN" is enforced by the browser, not promised by a comment.** Every
>   response carries `Content-Security-Policy: default-src 'none'` with
>   `connect-src 'self'` for the one `fetch`, so a `<script src>` added to the
>   page in a year's time does not load. Two tests scan the page for the three
>   ways it could acquire an external dependency without an absolute URL
>   (protocol-relative `src`, `@import`, dynamic `import()`) and for the four
>   string-to-DOM paths, because frame names are arbitrary UTF-8 and this page is
>   the one place in the repository where they meet an HTML parser.
> * **A poll arriving inside one interval is answered from the previous
>   document, and that is correctness rather than politeness.** One `Sampler`
>   holds all the per-tick state, and every delta in the document
>   (`delta_head`, `delta_errors`, `observed_hz`) is a difference between two of
>   its observations. Two browser tabs polling at 1 Hz would take alternate
>   observations, so each would see half the samples and **every rate on both
>   pages would read half of what the arena is doing** — wrong, silently, with
>   no error anywhere. Caching within a tick also makes an F5 or a `watch curl`
>   free rather than a perturbation.
>
> **Three defects were found by writing the tests, not by reading the code, and
> two of them would have shipped.** They are recorded because each is a general
> shape:
>
> 1. **A peer that connects and says nothing is an outage.** With the
>    connections handled inline, a port scanner holding one socket stopped the
>    operator's view for as long as it held it. A read timeout was the first
>    answer and was **not sufficient**: it bounds the outage *per connection*, so
>    five silent sockets still cost `5 x IO_TIMEOUT` in series — measured, 10.0 s
>    against a baseline of 0.008 s — linear in the number of peers and bounded by
>    nothing. The fix is a `std::thread::scope`d thread per connection, capped at
>    `MAX_CONNECTIONS = 64`, with the read timeout retained to retire the socket
>    and free the slot. The general shape: **a per-item deadline sets the slope
>    of a denial of service, it does not remove it.** Both properties are pinned
>    — `silent_peers_do_not_delay_the_operators_poll` fails at ~10 s if the
>    handling goes back inline, and `a_client_that_never_speaks_does_not_wedge_the_server`
>    fails if the read timeout goes. Note that the second one has to hold its
>    silent socket *open* across the assertion; closing it first ends the handler
>    by EOF and the timeout never has to fire.
> 2. **A header assertion that the page's own prose satisfied.** The CSP test
>    searched the whole response for the header string, and `web/index.html`'s
>    file comment quotes the header it documents. Deleting the
>    `Content-Security-Policy` line from the server left the test passing. Header
>    assertions are now made against the response head only.
> 3. **A docstring naming a failure that could not happen.** The non-finite-rate
>    test claimed a zero median interval would put `inf` in the document; it
>    cannot, because `IntervalStats::rate_hz` returns `None` unless the median is
>    positive and `observed_hz` returns `None` unless the elapsed time is. The
>    guard is worth keeping and the test is worth keeping — as a claim about a
>    guard, which is what it now says.

---

## 8. Visualization — deliberately not built

### 8.1 The reasoning

Two earlier drafts of this section proposed a `tf_tree.rerun` module, then a well-known-schema MCAP "viewer channel." **Both were solving a problem that does not exist**, and the section is now a record of why nothing is built.

A user with a bag already has images, LiDAR, *and* transforms in one MCAP. They open it in Rerun or Foxglove and see all of it, including the transform tree, with no involvement from us. Any transform channel `tf_tree` writes carries the same poses that came out of that bag. It is a re-encoding, not new information, and it costs a protobuf dependency, a schema to keep current, a CI job against two viewers, and a support surface — to show the user something they can already see.

The general form, worth remembering because it will recur: **`tf_tree`'s value is entirely in things a viewer cannot show.** How fast a query is answered, whether the answer is correct, and what is wrong with the transform tree. None of those are rendering problems.

### 8.2 What is genuinely not visible in a viewer — and where it goes

| Information | Not in the bag | Surface |
|---|---|---|
| Clock skew between publishers | ✓ | `doctor` `TFT004` |
| Extrapolation hotspots, with consumer attribution | ✓ | `doctor` `TFT010`, `EdgeCounters` |
| Multi-publisher conflicts | ✓ | `doctor` `TFT001` (Phase 4 bridge) |
| Rate deviation, jitter, gaps | ✓ | `doctor` `TFT007`–`TFT009` |
| Buffer undersizing vs observed lag | ✓ | `doctor` `TFT011` |
| Live per-edge state | ✓ | `tf_tree top` (§7) |

All of it already has a home in §6 and §7. **None of it wants a 3D viewer** — it is tabular, time-series, and per-publisher, which is exactly what a TUI and a JSON schema are good at.

### 8.3 What survives — and it is not viewer-specific

Two iteration methods, justified independently of any viewer. They are how a user exports, audits, or computes statistics over stored data, and they happen to also make a twenty-line Rerun snippet possible for anyone who wants one.

```python
ds.iter_edge(edge, t0, t1)     # -> (stamp_ns, pose) at STORED sample times, not interpolated
ds.iter_edges(t0, t1)          # -> interleaved across edges, time-ordered
ds.frame_path("lidar")         # -> ["world", "base_link", "lidar"]  root-to-leaf chain
```

**NORMATIVE:** `iter_edge` yields stored samples. Everything else in the API interpolates; this one deliberately does not, because "what was actually published" and "what would be interpolated at time t" are different questions and conflating them makes audit impossible.

`frame_path` is a plain frame chain with no viewer branding. It is already needed for error messages and for `doctor` output; exposing it costs nothing.

Ship a Rerun snippet in the documentation's examples directory if it is useful to us. **It is an example, never a module** — no optional dependency, no API surface, no obligation when upstream changes.

### 8.4 If `tf_tree` ever becomes the source of truth

The one scenario where transforms are genuinely missing from a recording is a post-Phase-7 deployment where nodes publish to `tf_tree` instead of `/tf`, so the bag has no transform topic at all.

That is solved by the **Phase 7 egress bridge** (arena → `/tf`), not by a recorder schema: publishing back to `/tf` makes every existing tool work — Rerun, Foxglove, Lichtblick, RViz, PlotJuggler, `ros2 bag` — with no viewer-specific code anywhere. Resolve it there, once, and only when that deployment actually exists.

### 8.5 One idea explicitly parked

Annotating an existing recording with `tf_tree`'s *analysis* — writing a diagnostics channel into a copy of the bag so that extrapolation failures appear on a viewer's timeline aligned with the video — would be genuinely additive, because those markers are not in the source. It also requires rewriting bags, has no requester, and is speculative. **Parked, unbuilt, recorded here so it is not reinvented as a viewer integration.**

## 9. The benchmark artifact

### 9.1 It is a product, not a script

```
tf_tree bench compare --bag run.mcap --consumers 16 --duration 120s --out report/
```

Runs both stacks on the same data: N `tf2` consumers versus one bridge plus N `tf_tree` consumers. Emits `report/index.html`, `report/results.json` (stable schema, CI-diffable), and the exact environment description needed to reproduce.

Ship a container image and a small public sample recording so a stranger can run it in one command. **A benchmark nobody can reproduce persuades nobody**, and this artifact's job is to answer the two questions that actually block adoption — is it faster, and is it correct — in one place.

### 9.2 Required rows

| Measurement | Both stacks |
|---|---|
| CPU per consumer at steady state | %CPU |
| **Total RSS across N consumers** | MB |
| Lookup latency | p50, p99, p99.9 |
| Publish → visible-to-consumer | p50, p99.9 |
| Scaling curve, N = 1…16 | throughput, CPU |
| Frozen `.tft`: 16 dataloader workers, total RSS | MB, vs 16 bag parses |
| `.tft` open time vs bag parse time | ms |
| Differential agreement (`LerpSlerp`) | max deviation |
| **Facade `Plan::at` from a separate crate vs in-crate**, depth 3 | ratio, gated at 5% |

The last row is `tf_tree` against itself and belongs in this table anyway: it is the only measurement of the path an **embedder** actually compiles. `PHASE4.md` §7 gates the C ABI at 5% against native in-crate Rust, and nothing gates native *out-of-crate* Rust, which is what a user's node links. [`API.md`](./API.md) §2.3 makes the row and the gate normative, along with the `#[inline]` attributes and the LTO guidance that are how it is passed. Report it with the embedder's default profile, **not** this workspace's — `[profile.release]` here sets `lto = "thin"`, which is precisely what hides the effect.

### 9.3 Honesty requirements — NORMATIVE

The first skeptical reader will look for a thumb on the scale, and finding one ends the project's credibility permanently.

- Identical QoS, identical executor configuration, identical DDS vendor and version, all recorded in the report.
- Both stacks warmed; discard the first N seconds; state N.
- Report `tf2` version, ROS distro, RMW implementation, kernel, CPU model, and THP setting.
- **Report where `tf_tree` is worse**, in the same table and not in a footnote: arena memory floor (an idle arena costs more than an idle `tf2` buffer), attach latency, the operational cost of a format bump, and the bridge as an additional process to supervise.
- Publish the harness source in the same repository. No private benchmark.

If a row cannot be measured fairly, omit it and say why. An honest gap is worth more than a favourable number nobody trusts.

---

## 10. Open-source readiness

Phase 5 is where the repository becomes publishable, so this is a deliverable, not an afterthought.

- **Name check before anything else.** Confirm `tf_tree` is available on crates.io and PyPI, and decide deliberately whether the proximity to ROS's `tf` / `tf2` package names helps discovery or invites confusion. Renaming after 1.0 is not an option; renaming now is an afternoon.
- Apache-2.0 / MIT dual (D30), license headers, `NOTICE`, SBOM per release.
- `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `SECURITY.md` with a real disclosure address.
- **A stated support policy**, honestly scoped. A small-team infrastructure project dies from unanswered issues more often than from bad design. Say what is supported, what is best-effort, and what the response expectation is. Under-promising is fine; silence is not.
- **MSRV policy** and a CI matrix pinning it.
- Documentation site (mdBook): a first-five-minutes path that works — `pip install tf_tree`, three lines, a real result — before any architecture prose.
- CI: the full Phase 1–5 suites on `x86_64` and `aarch64`, ASan/UBSan/TSan, Miri, loom, the nightly `shm_torture`, the benchmark artifact as a regression gate.
- Release automation: `cargo-dist` or equivalent, maturin wheels per Phase 3 §10, PEP 740 attestations, signed tags.

---

## 11. Test plan

- **Three-way bit-identity:** replay one recording into `HeapArena`, `MappedArena`, and `FrozenArena`; identical query set; assert bit-identical `f64`. This extends Phase 2 §10 and is the core correctness claim of the frozen format.
- **Ingest anomalies:** a synthetic corpus containing every row of §3.2, asserting the exact ingest-report output.
- **Out-of-order ingest:** shuffle a recording's messages; the resulting `.tft` must be byte-identical to one built from the ordered source.
- **Spill path:** ingest with `--max-memory` set below the dataset size; result identical to the in-memory path. **Four tests, because §3.1's amendment splits this into three mechanisms and the third has two properties:** grouping (`capped_memory_matches_the_uncapped_path`), one edge spilled to a run file and merged in a single pass (`an_oversized_edge_spills_and_matches_the_in_memory_path`), a cap small enough that the runs must be *reduced* in several passes first (`a_tiny_cap_reduces_in_several_passes`), and a duplicate `(edge, stamp)` that a reduce pass **re-merges** (`a_reduce_pass_keeps_the_last_occurrence`). The third is not redundant: a single-pass merge over that many runs exceeds the cap tenfold, and only that test observes it. Nor is the fourth: the first two tests are the only ones with duplicates and neither reaches the reduce loop, while the third is the only one that reduces and its fixture has no duplicates — so §3.2's "last wins" across a reduce pass was, until it landed, held by a comment. Every one of these asserts the reported peak from **both** sides; `peak <= cap` alone is satisfied by a path that reports nothing.
- **Chunk decompression:** the newest class §3.3 grew, and it has four parts that are deliberately not interchangeable. **Conformance against real libzstd** (`a_real_libzstd_recording_ingests`, against the committed `testdata/zstd_conformance.mcap`, which the test fails *loudly* on if absent rather than skipping) is a different claim from **round-trip** (`a_zstd_recording_ingests_identically`, `an_lz4_recording_ingests_identically`): an encoder and a decoder from the same crate can agree with each other and both disagree with the zstd `rosbag2` and Foxglove link. **Every bomb guard asserts the allocation and not only the error** — `a_lying_uncompressed_size_is_refused_before_it_allocates` and `a_high_expansion_ratio_is_refused` check `scratch.capacity() == 0` first, because an error alone is also what a reader returns *after* allocating a gigabyte and failing to decode into it; the third guard, on the zstd decoder's declared window, is asserted by `a_zstd_frame_demanding_an_oversized_window_is_refused` and bounded from below by `the_window_floor_admits_what_a_real_zstd_encoder_declares`, because a window bound set too tight refuses ordinary recordings. **Both length disagreements, each written with and without a CRC** (`each_codec_round_trips_and_catches_both_length_disagreements`): `uncompressed_crc == 0` means "not computed" per the specification and real writers emit it, so only the CRC-free rows leave the length comparison as the sole witness. And **the codec-free configuration**, which `--workspace` compiles nowhere and which `just ingest-check` therefore gates on its own, together with a dependency-graph assertion that the shipped CLI's default build still links both codecs. **The asymmetry, stated here because a future implementer reads this list first: the two codecs' conformance evidence differs in kind.** zstd's is a whole recording written by real libzstd. lz4's — there being no `lz4` CLI on the build host — is `a_hand_authored_lz4_frame_decodes_per_the_specification`: 82 bytes written from the LZ4 frame and block formats, asserted not to be what `lz4_flex`'s own encoder emits, and shown load-bearing by `a_single_flipped_bit_in_the_lz4_vector_is_caught` (651 of 656 single-bit perturbations caught, the five don't-cares enumerated against the format text). That is a conformance claim rather than a round-trip one, but it covers one frame and not a file; `testdata/ATTRIBUTION.md` records what would close the rest.
- **Multi-process page sharing:** 16 processes mapping one `.tft`; assert total RSS is within 1.2× of a single process, measured from `/proc/*/smaps_rollup` `Pss`.
- **Fork safety:** a `DataLoader` with `num_workers=16` under all three start methods.
- **Counter contention:** 16 concurrent readers on one edge, each holding a long-lived `Guard`; assert no measurable throughput difference against a `counters`-disabled build.
- **No network:** full suite under `strace`/seccomp, asserting `socket(2)` is only ever `AF_UNIX` (§5.1). **Scoped to the library's suite** — `tf_tree top --web` is an `AF_INET` listener by construction, and §7's web-view amendment records why that exception belongs in this sentence rather than inside the assertion.
- **Convenience-path guard reuse:** assert `tree.lookup` in a loop performs O(1) atomic flushes, not O(n).
- **Diagnostics:** one test per check ID, each with a fixture that triggers exactly that check and no other.
- **`doctor --json`:** schema-validated; adding a check must not break an existing consumer.
- **Web view:** loopback binding asserted; no outbound network requests (assert on the served HTML). **Both are implemented, and two more were needed**: the `Host` guard that makes the loopback bind mean something against DNS rebinding, and an end-to-end test that parses the document a browser receives — every unit test stubs the sampler, so the hand-formatted JSON was otherwise never once parsed.
- **`iter_edge` returns stored samples:** push a known irregular sequence, iterate, and assert the exact stamps come back — no resampling, no interpolation, no reordering.

---

## 12. Gate

1. **Three-way bit-identity passes.**
2. `.tft` open time under **10 ms** for a 233 MB index (it is an `mmap` plus header validation; anything more means work is happening that should not).
3. Frozen lookup p50 within **20%** of online (accounting for the deeper binary search).
4. **16 workers sharing one `.tft`: total Pss within 1.2× of one worker.**
5. Ingest throughput ≥ **10× real time** on a representative recording. **Currently held by nobody: `crates/tf_tree_bench/benches/` has no ingest benchmark, so `just bench-check` cannot see a regression on this path.** Measured by hand it passes with a wide margin — 0.048 s for 160 000 transforms through a zstd recording composes to roughly 200× real time for a four-hour bag at 100 Hz × 50 transforms — but a hand measurement is not a gate. Adding one needs a corpus that is *not* produced by `ruzstd`'s own encoder, which understates a real recording's decode cost by about 1.3×; `crate::fixture::compress_records` carries that number.
6. Every §6 check has a passing fixture test.
7. Benchmark artifact runs from the published container on a clean machine and reproduces the committed `results.json` within tolerance.
8. §10 checklist complete, including the name decision.

Criterion 4 is the wedge's central claim, and criterion 7 is what makes it believable to anyone outside the team.

---

## 13. Definition of done

- [ ] `FORMAT_VERSION = 3` shipped in a single commit, with Phase 6 regions reserved and `doctor --explain-version`
- [ ] Publish-side observability derived, not counted — push path unchanged and benchmarked to prove it
- [ ] Nothing in the codebase, CLI, or docs is called "telemetry"
- [ ] `socket(2)` restricted to `AF_UNIX`, asserted in CI
- [ ] Error-path counters always on with no runtime switch; `counters` cargo feature is the only knob
- [ ] Convenience path holds a long-lived per-thread `Guard`
- [ ] `freeze --from-live` captures counters into the manifest
- [ ] `.tft` format implemented, 2 MiB-aligned arena, CBOR manifest, `source_digest`
- [ ] Three-way bit-identity test green in CI
- [ ] Offline Python API is the *same* API; no parallel surface introduced
- [ ] Ingest report emitted as JSON and human summary; every §3.2 anomaly covered by a fixture
- [ ] All 16 diagnostic checks implemented with stable IDs, `--json`, and `--exit-code`
- [x] `tf_tree top` TUI plus embedded web view with no build step, loopback-bound
- [ ] `iter_edge` / `iter_edges` / `frame_path` present on both live and frozen arenas, with `iter_edge` yielding stored samples
- [ ] No viewer dependency, channel, schema, or plugin anywhere in the repository
- [ ] Benchmark artifact reproducible from a published container by someone outside the team
- [ ] "Where we are worse" section present in the benchmark report
- [ ] §10 open-source checklist complete, name decision made and recorded
- [ ] §12 gate met, or a written explanation of which criterion failed and by how much
- [ ] `docs/PHASE6.md` written, carrying forward the reserved regions and the Phase 4 surprise log
